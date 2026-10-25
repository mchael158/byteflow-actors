use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::error::report_fault;
use super::mailbox::{Mailbox, WaitEpoch};
use super::process::{Flow, FlowId};
use super::runtime::enqueue_flow;
use super::sync_lock;

/// Slots per wheel level. A power of two so the slot index is a bit mask.
const SLOTS: usize = 256;
const SLOT_SHIFT: u32 = 8;
/// Six levels of 256 slots at 1 ms cover more than `Instant` can express.
const LEVELS: usize = 6;
const TICK_NS: u64 = 1_000_000;

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
    /// Test marker. Production never constructs one.
    #[cfg(test)]
    Probe(u64),
}

struct TimerEntry {
    deadline: Instant,
    payload: TimerPayload,
}

struct Slot {
    entries: Vec<TimerEntry>,
    earliest: Option<Instant>,
}

/// Hierarchical timing wheel. Insert is O(1). A wake scans the fixed slot
/// grid, not the timer count, and only the slots whose earliest deadline
/// has passed are opened.
struct Wheel {
    levels: Vec<Vec<Slot>>,
    /// Tick of the last observed `now`. New inserts bucket against it.
    cursor: u64,
    origin: Instant,
}

impl Wheel {
    fn new() -> Self {
        let mut levels = Vec::with_capacity(LEVELS);
        for _ in 0..LEVELS {
            let mut slots = Vec::with_capacity(SLOTS);
            for _ in 0..SLOTS {
                slots.push(Slot {
                    entries: Vec::new(),
                    earliest: None,
                });
            }
            levels.push(slots);
        }
        Self {
            levels,
            cursor: 0,
            origin: Instant::now(),
        }
    }

    fn tick_of(&self, when: Instant) -> u64 {
        let nanos = when.saturating_duration_since(self.origin).as_nanos();
        let ticks = nanos / u128::from(TICK_NS);
        u64::try_from(ticks).unwrap_or(u64::MAX)
    }

    fn observe(&mut self, now: Instant) {
        self.cursor = self.tick_of(now);
    }

    fn insert(&mut self, entry: TimerEntry) {
        let when = self.tick_of(entry.deadline).max(self.cursor);
        let (level, slot) = place(self.cursor, when);
        let bucket = &mut self.levels[level][slot];
        bucket.earliest = Some(match bucket.earliest {
            Some(prev) if prev <= entry.deadline => prev,
            _ => entry.deadline,
        });
        bucket.entries.push(entry);
    }

    /// Entries with `deadline <= now`, earliest first. Later entries stay
    /// in the wheel, rebucketed against `now`.
    fn collect_due(&mut self, now: Instant) -> Vec<TimerEntry> {
        self.observe(now);
        let mut due = Vec::new();
        loop {
            let mut opened = false;
            for level in 0..LEVELS {
                for slot in 0..SLOTS {
                    let ready = matches!(
                        self.levels[level][slot].earliest,
                        Some(earliest) if earliest <= now
                    );
                    if !ready {
                        continue;
                    }
                    opened = true;
                    let batch = std::mem::take(&mut self.levels[level][slot].entries);
                    self.levels[level][slot].earliest = None;
                    for entry in batch {
                        if entry.deadline <= now {
                            due.push(entry);
                        } else {
                            self.insert(entry);
                        }
                    }
                }
            }
            if !opened {
                break;
            }
        }
        due.sort_by_key(|entry| entry.deadline);
        due
    }

    fn next_deadline(&self) -> Option<Instant> {
        let mut best = None;
        for level in &self.levels {
            for slot in level {
                if let Some(earliest) = slot.earliest {
                    best = Some(match best {
                        Some(prev) if prev <= earliest => prev,
                        _ => earliest,
                    });
                }
            }
        }
        best
    }

    fn drain_sleepers(&mut self) -> Vec<Flow> {
        let mut flows = Vec::new();
        for level in &mut self.levels {
            for slot in level.iter_mut() {
                let batch = std::mem::take(&mut slot.entries);
                slot.earliest = None;
                for entry in batch {
                    if let TimerPayload::WakeSleeper(flow) = entry.payload {
                        flows.push(*flow);
                    }
                }
            }
        }
        flows
    }
}

/// Level whose window contains `when - cursor`, and the slot inside it.
fn place(cursor: u64, when: u64) -> (usize, usize) {
    let when = when.max(cursor);
    let delta = when - cursor;
    let mut level = 0usize;
    let mut horizon = SLOTS as u64;
    while level + 1 < LEVELS && delta >= horizon {
        level += 1;
        horizon = horizon.saturating_mul(SLOTS as u64);
    }
    let shift = (level as u32) * SLOT_SHIFT;
    let slot = ((when >> shift) as usize) & (SLOTS - 1);
    (level, slot)
}

/// A single global `Sleep`/`ReceiveTimeout` deadline queue.
///
/// Deadlines sit in a hierarchical wheel: each level has [`SLOTS`] buckets
/// and one slot of level `n` spans 256 slots of level `n - 1`. Insert touches
/// one bucket. The timer thread sleeps until the earliest deadline, then
/// opens only the buckets that have reached it.
pub struct TimerWheel {
    wheel: Mutex<Wheel>,
    cvar: Condvar,
    shutdown: Mutex<bool>,
}

/// `Instant + Duration` can overflow (far-future delays). Never panic, and
/// never turn a delay that does not fit into a deadline of `now` — that
/// wakes the flow immediately. The old 30-year clamp did the same kind of
/// shortening for every `Duration` past that mark.
fn deadline_from_now(delay: Duration) -> Instant {
    let now = Instant::now();
    match now.checked_add(delay) {
        Some(deadline) => deadline,
        None => farthest_deadline(now),
    }
}

/// Latest `Instant` that can still be added onto `now`.
fn farthest_deadline(now: Instant) -> Instant {
    let mut lo = 0u64;
    let mut hi = u64::MAX;
    let mut best = 0u64;
    while lo <= hi {
        let mid = lo + (hi - lo) / 2;
        if now.checked_add(Duration::from_nanos(mid)).is_some() {
            best = mid;
            if mid == u64::MAX {
                break;
            }
            lo = mid + 1;
        } else if mid == 0 {
            break;
        } else {
            hi = mid - 1;
        }
    }
    match now.checked_add(Duration::from_nanos(best)) {
        Some(deadline) => deadline,
        None => now,
    }
}

impl TimerWheel {
    pub fn new() -> Arc<Self> {
        Arc::new(TimerWheel {
            wheel: Mutex::new(Wheel::new()),
            cvar: Condvar::new(),
            shutdown: Mutex::new(false),
        })
    }

    /// `Err` returns the flow. The wheel lock was poisoned, so the sleeper
    /// was not stored and must be finalized by the caller.
    pub fn schedule_sleep(&self, delay: Duration, flow: Box<Flow>) -> Result<(), Box<Flow>> {
        let entry = TimerEntry {
            deadline: deadline_from_now(delay),
            payload: TimerPayload::WakeSleeper(flow),
        };
        match self.push(entry) {
            Ok(()) => Ok(()),
            Err(entry) => match entry.payload {
                TimerPayload::WakeSleeper(flow) => Err(flow),
                TimerPayload::WakeReceiver { .. } => {
                    unreachable!("schedule_sleep only inserts a sleeper")
                }
                #[cfg(test)]
                TimerPayload::Probe(_) => {
                    unreachable!("schedule_sleep only inserts a sleeper")
                }
            },
        }
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
    ) -> bool {
        let entry = TimerEntry {
            deadline: deadline_from_now(delay),
            payload: TimerPayload::WakeReceiver {
                pid,
                mailbox,
                dest_reg,
                epoch,
            },
        };
        self.push(entry).is_ok()
    }

    fn push(&self, entry: TimerEntry) -> Result<(), TimerEntry> {
        match sync_lock::lock(&self.wheel, "TimerWheel::push") {
            Ok(mut wheel) => {
                wheel.insert(entry);
                self.cvar.notify_one();
                Ok(())
            }
            Err(e) => {
                report_fault(e);
                Err(entry)
            }
        }
    }

    pub fn shutdown(&self) {
        match sync_lock::lock(&self.shutdown, "TimerWheel::shutdown") {
            Ok(mut flag) => *flag = true,
            Err(e) => report_fault(e),
        }
        self.cvar.notify_all();
    }

    /// Take every sleeper out of the wheel. Receive-timeout entries are
    /// dropped: those flows live in the mailbox and are drained there.
    pub(crate) fn drain(&self) -> Vec<Flow> {
        match sync_lock::lock(&self.wheel, "TimerWheel::drain") {
            Ok(mut wheel) => wheel.drain_sleepers(),
            Err(e) => {
                report_fault(e);
                Vec::new()
            }
        }
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
            let due = {
                let mut wheel = match sync_lock::lock(&self.wheel, "TimerWheel::drive") {
                    Ok(wheel) => wheel,
                    Err(e) => {
                        report_fault(e);
                        return;
                    }
                };
                let shutting_down =
                    match sync_lock::lock(&self.shutdown, "TimerWheel::drive/shutdown") {
                        Ok(flag) => *flag,
                        Err(e) => {
                            report_fault(e);
                            return;
                        }
                    };
                if shutting_down {
                    return;
                }
                let now = Instant::now();
                let due = wheel.collect_due(now);
                if due.is_empty() {
                    let wait_for = match wheel.next_deadline() {
                        Some(deadline) => {
                            let left = deadline.saturating_duration_since(Instant::now());
                            if left.is_zero() {
                                drop(wheel);
                                continue;
                            }
                            left
                        }
                        None => Duration::from_millis(250),
                    };
                    match sync_lock::wait_timeout(&self.cvar, wheel, wait_for, "TimerWheel::wait") {
                        Ok((guard, _)) => drop(guard),
                        Err(e) => {
                            report_fault(e);
                            return;
                        }
                    }
                    continue;
                }
                due
            };
            for entry in due {
                self.fire(entry, shared);
            }
        }
    }

    fn fire(&self, entry: TimerEntry, shared: &super::runtime::Shared) {
        match entry.payload {
            TimerPayload::WakeSleeper(flow) => {
                enqueue_flow(shared, flow);
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
                            enqueue_flow(shared, flow);
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
            #[cfg(test)]
            TimerPayload::Probe(_) => {}
        }
        match sync_lock::lock(&shared.notify.0, "TimerWheel::fire/notify") {
            Ok(_guard) => shared.notify.1.notify_all(),
            Err(e) => report_fault(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delay_that_does_not_fit_stays_far_in_the_future() {
        let before = Instant::now();
        let deadline = deadline_from_now(Duration::MAX);
        let thirty_years = Duration::from_secs(60 * 60 * 24 * 365 * 30);
        let capped = match before.checked_add(thirty_years) {
            Some(t) => t,
            None => before,
        };
        assert!(deadline > Instant::now());
        assert!(
            deadline > capped,
            "a delay past 30 years must not collapse to that clamp"
        );
    }

    #[test]
    fn a_short_deadline_fires_and_a_later_one_stays() {
        let mut wheel = Wheel::new();
        let now = Instant::now();
        wheel.insert(TimerEntry {
            deadline: now,
            payload: TimerPayload::Probe(1),
        });
        wheel.insert(TimerEntry {
            deadline: now + Duration::from_secs(3),
            payload: TimerPayload::Probe(2),
        });
        let due = wheel.collect_due(now + Duration::from_millis(10));
        assert_eq!(due.len(), 1);
        assert!(matches!(due[0].payload, TimerPayload::Probe(1)));
        let later = wheel.collect_due(now + Duration::from_secs(3));
        assert_eq!(later.len(), 1);
        assert!(matches!(later[0].payload, TimerPayload::Probe(2)));
    }

    #[test]
    fn a_deadline_past_the_first_level_still_fires() {
        let mut wheel = Wheel::new();
        let now = Instant::now();
        let when = now + Duration::from_secs(2);
        let (level, _) = place(wheel.cursor, wheel.tick_of(when).max(wheel.cursor));
        assert!(level >= 1, "2s must leave the 256ms level");
        wheel.insert(TimerEntry {
            deadline: when,
            payload: TimerPayload::Probe(7),
        });
        assert!(wheel
            .collect_due(now + Duration::from_millis(20))
            .is_empty());
        let due = wheel.collect_due(when);
        assert_eq!(due.len(), 1);
        assert!(matches!(due[0].payload, TimerPayload::Probe(7)));
    }

    #[test]
    fn equal_deadlines_both_fire() {
        let mut wheel = Wheel::new();
        let now = Instant::now();
        wheel.insert(TimerEntry {
            deadline: now,
            payload: TimerPayload::Probe(1),
        });
        wheel.insert(TimerEntry {
            deadline: now,
            payload: TimerPayload::Probe(2),
        });
        let due = wheel.collect_due(now);
        assert_eq!(due.len(), 2);
    }
}
