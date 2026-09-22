use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::error::report_fault;
use super::mailbox::{Mailbox, WaitEpoch};
use super::process::{Flow, FlowId};
use super::sync_lock;

/// What to do when a timer entry's deadline is reached.
enum TimerPayload {
    /// A `Sleep`-suspended flow: just make it runnable again. Its `pc`
    /// already points past the `Sleep` instruction, so no register
    /// writeback is needed.
    WakeSleeper(Box<Flow>),
    /// A `ReceiveTimeout`-suspended flow, parked *inside its own
    /// mailbox* rather than held here directly (see
    /// [`super::mailbox::Mailbox`]'s doc comment). We only hold enough to
    /// find it again: its id (for logging/metrics), a handle to the
    /// mailbox, and the [`WaitEpoch`] of the park this deadline belongs
    /// to — without that epoch a late deadline would wake whichever wait
    /// happens to be current and write `Unit` into the previous wait's
    /// register.
    WakeReceiver {
        pid: FlowId,
        mailbox: Arc<Mailbox>,
        dest_reg: u8,
        epoch: WaitEpoch,
    },
}

struct TimerEntry {
    deadline: Instant,
    payload: TimerPayload,
}

// Ordering is by deadline only — two entries with the same deadline are
// "equal" for heap-ordering purposes even though they wake different
// processes. That's intentional: `BinaryHeap` only needs a total order to
// stay a valid heap, and we never rely on it to deduplicate entries.
impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline
    }
}
impl Eq for TimerEntry {}
impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.deadline.cmp(&other.deadline)
    }
}

/// A single global `Sleep`/`ReceiveTimeout` deadline queue.
///
/// # On the choice of a binary heap over a real timing wheel
///
/// Design notes §19 calls for a hierarchical timing wheel (O(1) insert/
/// cancel, bucketed by coarse deadlines) — the right choice at BEAM/Tokio
/// scale, where timer churn is enormous. A `BinaryHeap<Reverse<TimerEntry>>`
/// behind one mutex is `O(log n)` insert and pop, which is the simpler,
/// well-understood structure and is more than fast enough to validate the
/// scheduling model end-to-end (this is exactly the "don't build everything
/// at once" ordering the design notes argue for in §36 — a wheel is a
/// drop-in replacement for this module's internals later, since nothing
/// outside `timer.rs` knows how deadlines are stored).
pub struct TimerWheel {
    heap: Mutex<BinaryHeap<Reverse<TimerEntry>>>,
    cvar: Condvar,
    shutdown: Mutex<bool>,
}

/// `Instant + Duration` can overflow (far-future delays). Never panic.
fn deadline_from_now(delay: Duration) -> Instant {
    Instant::now()
        .checked_add(delay)
        .or_else(|| Instant::now().checked_add(Duration::from_secs(60 * 60 * 24 * 365 * 30)))
        .unwrap_or_else(Instant::now)
}

impl TimerWheel {
    pub fn new() -> Arc<Self> {
        Arc::new(TimerWheel {
            heap: Mutex::new(BinaryHeap::new()),
            cvar: Condvar::new(),
            shutdown: Mutex::new(false),
        })
    }

    pub fn schedule_sleep(&self, delay: Duration, flow: Box<Flow>) {
        let entry = TimerEntry {
            deadline: deadline_from_now(delay),
            payload: TimerPayload::WakeSleeper(flow),
        };
        self.push(entry);
    }

    /// Arm a `ReceiveTimeout` deadline for the park identified by `epoch`.
    /// Obtain `epoch` from the [`Mailbox::park`] call that installed the
    /// wait — never fabricate or reuse one.
    pub fn schedule_receive_timeout(
        &self,
        delay: Duration,
        pid: FlowId,
        mailbox: Arc<Mailbox>,
        dest_reg: u8,
        epoch: WaitEpoch,
    ) {
        let entry = TimerEntry {
            deadline: deadline_from_now(delay),
            payload: TimerPayload::WakeReceiver {
                pid,
                mailbox,
                dest_reg,
                epoch,
            },
        };
        self.push(entry);
    }

    fn push(&self, entry: TimerEntry) {
        match sync_lock::lock(&self.heap, "TimerWheel::push") {
            Ok(mut heap) => {
                heap.push(Reverse(entry));
                self.cvar.notify_one();
            }
            Err(e) => report_fault(e),
        }
    }

    pub fn shutdown(&self) {
        match sync_lock::lock(&self.shutdown, "TimerWheel::shutdown") {
            Ok(mut flag) => *flag = true,
            Err(e) => report_fault(e),
        }
        self.cvar.notify_all();
    }

    /// Runs on a single dedicated OS thread (spawned by
    /// [`super::runtime::Runtime`]) for the life of the runtime.
    /// Pops every entry whose deadline has passed, resolves it into a
    /// runnable flow, and pushes that flow onto the shared global
    /// injector queue so any idle worker can pick it up — the timer thread
    /// itself never runs flow code.
    ///
    /// Mutex poison → [`report_fault`] and exit the drive loop (fail-closed).
    pub fn drive(self: &Arc<Self>, shared: &super::runtime::Shared) {
        loop {
            let mut heap = match sync_lock::lock(&self.heap, "TimerWheel::drive") {
                Ok(h) => h,
                Err(e) => {
                    report_fault(e);
                    return;
                }
            };
            let shutting_down = match sync_lock::lock(&self.shutdown, "TimerWheel::drive/shutdown")
            {
                Ok(g) => *g,
                Err(e) => {
                    report_fault(e);
                    return;
                }
            };
            if shutting_down {
                return;
            }
            match heap.peek() {
                None => {
                    match sync_lock::wait_timeout(
                        &self.cvar,
                        heap,
                        Duration::from_millis(250),
                        "TimerWheel::idle",
                    ) {
                        Ok((guard, _)) => {
                            drop(guard);
                        }
                        Err(e) => {
                            report_fault(e);
                            return;
                        }
                    }
                }
                Some(Reverse(top)) => {
                    let now = Instant::now();
                    if top.deadline <= now {
                        let Reverse(entry) = match heap.pop() {
                            Some(e) => e,
                            None => continue,
                        };
                        drop(heap);
                        self.fire(entry, shared);
                    } else {
                        let wait_for = top.deadline - now;
                        match sync_lock::wait_timeout(
                            &self.cvar,
                            heap,
                            wait_for,
                            "TimerWheel::wait",
                        ) {
                            Ok((guard, _)) => drop(guard),
                            Err(e) => {
                                report_fault(e);
                                return;
                            }
                        }
                    }
                }
            }
        }
    }

    fn fire(&self, entry: TimerEntry, shared: &super::runtime::Shared) {
        match entry.payload {
            TimerPayload::WakeSleeper(flow) => {
                shared.injector.push(flow);
            }
            TimerPayload::WakeReceiver {
                pid,
                mailbox,
                dest_reg,
                epoch,
            } => {
                match mailbox.take_parked_at(epoch) {
                    Ok(Some(flow)) => {
                        debug_assert_eq!(
                            flow.id, pid,
                            "timer fired for a mailbox owned by a different flow"
                        );
                        if let Err(e) = shared.ask_waits.remove_asker(flow.id) {
                            report_fault(e);
                        }
                        // This deadline still owns the current wait (see
                        // `Mailbox::take_parked_at`): deliver `Unit` as the
                        // "no message arrived in time" result.
                        if let Some(flow) = super::finalize::resume_or_fail(
                            shared,
                            flow,
                            dest_reg,
                            crate::bytecode::Value::Unit,
                        ) {
                            shared.injector.push(flow);
                        }
                    }
                    Ok(None) => {
                        // A hop already ended that wait, or the flow has
                        // since parked on a different `Receive` this
                        // deadline does not own.
                    }
                    Err(e) => report_fault(e),
                }
            }
        }
        match sync_lock::lock(&shared.notify.0, "TimerWheel::fire/notify") {
            Ok(_guard) => shared.notify.1.notify_all(),
            Err(e) => report_fault(e),
        }
    }
}
