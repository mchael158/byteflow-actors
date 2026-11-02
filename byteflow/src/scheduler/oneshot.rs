use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::error::{report_fault, RuntimeError};
use super::sync_lock;

/// A single-value, single-producer/single-consumer handoff, used to deliver
/// a flow's terminal [`super::process::FlowOutcome`] to whoever holds
/// its [`super::handle::FlowHandle`].
///
/// This is intentionally not a general MPSC channel — every flow has
/// exactly one completion event and exactly one handle — so it is just a
/// `Mutex<Slot<T>>` + `Condvar`, with no allocation beyond the shared
/// `Arc` and no dependency on a channel crate for something this narrow.
///
/// # Disconnect is part of the contract
///
/// A flow can be destroyed without ever producing an outcome — the runtime
/// shuts down while it sleeps in the timer, sits in a worker deque, or is
/// parked in its mailbox. Its [`Sender`] is then dropped without a `send`.
///
/// A oneshot that only ever notified on `send` would leave the embedder's
/// thread blocked on the condvar permanently, indistinguishable from a flow
/// that is merely slow. So dropping the `Sender` without sending is a
/// first-class event: it wakes the joiner with
/// [`RuntimeError::Abandoned`]. Fail-closed applies to liveness too, not
/// just to correctness.
///
/// # Bounded waiting is part of the contract too
///
/// [`Receiver::join`] blocks until the flow terminates, which is fine for a
/// `main` that has nothing else to do and wrong for everyone else: a control
/// loop, a watchdog, a driver, a test harness, or any embedder with its own
/// event loop cannot commit a thread to an event whose arrival time is
/// decided by bytecode. [`Receiver::try_join`] polls without waiting and
/// [`Receiver::join_deadline`] waits under a hard bound, so "the flow is
/// slow or stuck" is an answer the caller can receive and act on.
///
/// Those two take `&self` precisely so the receiver *survives* a timeout and
/// can be retried; only the unbounded [`Receiver::join`] consumes.
struct Inner<T> {
    slot: Mutex<Slot<T>>,
    cvar: Condvar,
}

/// Shared state, as an explicit state machine rather than a bag of flags.
///
/// The point is that the illegal combinations cannot be written down: there
/// is no way to represent "value present *and* already collected", or
/// "abandoned *and* holding a value". Every accessor below is a total match
/// over these four states, so a new state cannot be added without the
/// compiler pointing at every place that must decide what it means.
///
/// ```text
/// Pending ──send──────────────▶ Ready(T) ──collect──▶ Taken
///    │                                                  ▲
///    └──sender dropped, no send──▶ Abandoned            (terminal)
/// ```
enum Slot<T> {
    /// No value yet; the sender still exists, so one may still arrive.
    Pending,
    /// The value arrived and nobody has collected it.
    Ready(T),
    /// The sender was dropped without sending: no value will ever arrive.
    Abandoned,
    /// The value arrived and was already handed to a caller.
    Taken,
}

/// Inspect the slot without waiting.
///
/// `None` means `Pending` — the *only* state a caller may retry, and
/// therefore the only one that justifies blocking. Every other state is
/// terminal and produces a definite answer.
fn collect<T>(slot: &mut Slot<T>) -> Option<Result<T, RuntimeError>> {
    // `Taken` is written speculatively so the value can be moved out of
    // `Ready` through the `&mut`, then restored for the states that keep
    // their meaning. Nothing between the two writes can panic (unit variant
    // construction only), so the slot is never left lying about its state.
    match std::mem::replace(slot, Slot::Taken) {
        Slot::Ready(value) => Some(Ok(value)),
        Slot::Pending => {
            *slot = Slot::Pending;
            None
        }
        Slot::Abandoned => {
            *slot = Slot::Abandoned;
            Some(Err(RuntimeError::Abandoned("oneshot")))
        }
        Slot::Taken => Some(Err(RuntimeError::AlreadyCollected("oneshot"))),
    }
}

/// Result of a bounded wait: either the value, or "not yet".
///
/// Deliberately not `Option<Result<T, _>>`: the nesting reads the same for
/// "still running" and "completed with an error", and this is exactly the
/// distinction a caller must not fumble.
#[derive(Debug)]
pub enum JoinState<T> {
    /// The flow terminated and this is its value.
    Ready(T),
    /// The bound elapsed with the flow still running. The receiver is intact
    /// — retry with a later deadline.
    Pending,
}

pub struct Sender<T> {
    inner: Arc<Inner<T>>,
    /// Set only after the value is actually stored.
    ///
    /// `send` consumes `self`, so `Drop` runs on the success path too and
    /// cannot infer "dropped without sending" from the drop alone. This flag
    /// is that distinction. `mem::forget` would also suppress the drop, but
    /// it would leak the `Arc` strong count and with it `Inner`.
    sent: bool,
}

pub struct Receiver<T> {
    inner: Arc<Inner<T>>,
}

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let inner = Arc::new(Inner {
        slot: Mutex::new(Slot::Pending),
        cvar: Condvar::new(),
    });
    (
        Sender {
            inner: inner.clone(),
            sent: false,
        },
        Receiver { inner },
    )
}

impl<T> Sender<T> {
    /// Deliver the value. Consumes `self`: a `Sender` can only send once,
    /// enforced at the type level rather than by a runtime "already sent"
    /// check.
    ///
    /// Mutex poison → [`report_fault`] and drop the value. `sent` stays
    /// false in that case, so the [`Drop`] below still reports the
    /// disconnect and a joiner gets [`RuntimeError::Abandoned`] instead of
    /// blocking forever on a value that was lost.
    pub fn send(mut self, value: T) {
        match sync_lock::lock(&self.inner.slot, "oneshot::send") {
            Ok(mut slot) => {
                debug_assert!(
                    matches!(*slot, Slot::Pending),
                    "oneshot slot must be Pending before send: one sender, sends once"
                );
                *slot = Slot::Ready(value);
                self.sent = true;
                drop(slot);
                self.inner.cvar.notify_one();
            }
            Err(e) => report_fault(e),
        }
    }
}

/// Report the disconnect so a blocked [`Receiver::join`] wakes up.
///
/// This is the difference between a runtime that abandons a flow *loudly*
/// (shutdown while it was suspended, a lost value after mutex poison) and
/// one that leaves the embedder's thread parked on a condvar that will
/// never be notified again.
impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if self.sent {
            return;
        }
        match sync_lock::lock(&self.inner.slot, "oneshot::disconnect") {
            Ok(mut slot) => {
                // Guarded transition, not a blind write: only a slot that is
                // still waiting for a value may be declared abandoned. A
                // `Ready`/`Taken` slot must never be clobbered into losing a
                // delivered outcome.
                if matches!(*slot, Slot::Pending) {
                    *slot = Slot::Abandoned;
                    drop(slot);
                    self.inner.cvar.notify_one();
                }
            }
            // Nothing left to do: the lock that guards the wakeup is itself
            // broken. `join` surfaces the same poison to its caller.
            Err(e) => report_fault(e),
        }
    }
}

impl<T> Receiver<T> {
    /// Take the value if it is already there, without ever waiting.
    ///
    /// The building block for embedders that own their own loop and cannot
    /// hand a thread over to a blocking call: poll, do other work, poll
    /// again. Costs one uncontended mutex acquisition.
    pub fn try_join(&self) -> Result<JoinState<T>, RuntimeError> {
        let mut slot = sync_lock::lock(&self.inner.slot, "oneshot::try_join")?;
        match collect(&mut slot) {
            Some(result) => result.map(JoinState::Ready),
            None => Ok(JoinState::Pending),
        }
    }

    /// Wait for the value until `deadline`, then give up and report
    /// [`JoinState::Pending`] with the receiver still usable.
    ///
    /// # Why the bound is an `Instant` and not a `Duration`
    ///
    /// A condvar wait can return spuriously, so the wait must sit in a loop.
    /// Feeding the *original* duration back into each turn of that loop
    /// restarts the full budget on every spurious wakeup, and the resulting
    /// "timeout" has no upper bound at all — the failure mode is invisible
    /// in testing and unbounded in production. Anchoring on an absolute
    /// deadline and recomputing the remainder makes the bound hold no
    /// matter how many times the wait is interrupted.
    pub fn join_deadline(&self, deadline: Instant) -> Result<JoinState<T>, RuntimeError> {
        let mut slot = sync_lock::lock(&self.inner.slot, "oneshot::join_deadline")?;
        loop {
            if let Some(result) = collect(&mut slot) {
                return result.map(JoinState::Ready);
            }
            // `None` when the deadline is already behind us.
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Ok(JoinState::Pending);
            };
            if remaining.is_zero() {
                return Ok(JoinState::Pending);
            }
            // The `WaitTimeoutResult` is deliberately discarded: a value can
            // land between the sender's notify and our reacquisition of the
            // mutex, so the slot is the only authority on what happened. The
            // deadline check above is what terminates this loop.
            let (guard, _) = sync_lock::wait_timeout(
                &self.inner.cvar,
                slot,
                remaining,
                "oneshot::join_deadline",
            )?;
            slot = guard;
        }
    }

    /// [`Receiver::join_deadline`] with the deadline measured from now.
    pub fn join_timeout(&self, timeout: Duration) -> Result<JoinState<T>, RuntimeError> {
        match Instant::now().checked_add(timeout) {
            Some(deadline) => self.join_deadline(deadline),
            // `Instant + Duration` panics on overflow. A timeout that runs
            // past the monotonic clock's range means "effectively never", so
            // honour that as an unbounded wait instead of panicking on a
            // value the caller is entitled to pass.
            None => self.wait_forever().map(JoinState::Ready),
        }
    }

    /// Block the calling (native) thread until the value is available.
    /// Only ever called from an embedder's own thread (e.g. `main`) waiting
    /// on a top-level flow — never from inside a worker thread, which
    /// must never block on anything but its own park/steal loop.
    ///
    /// Returns [`RuntimeError::Abandoned`] if the flow was destroyed before
    /// producing an outcome, and [`RuntimeError::PoisonedLock`] if the
    /// oneshot mutex is poisoned. Never blocks on a value that can no longer
    /// arrive — but *does* block indefinitely on a flow that is merely slow,
    /// which is why [`Receiver::join_timeout`] exists.
    pub fn join(self) -> Result<T, RuntimeError> {
        self.wait_forever()
    }

    /// Unbounded wait shared by [`Receiver::join`] and the overflow path of
    /// [`Receiver::join_timeout`]. Takes `&self` so neither has to consume.
    fn wait_forever(&self) -> Result<T, RuntimeError> {
        let mut slot = sync_lock::lock(&self.inner.slot, "oneshot::join")?;
        loop {
            if let Some(result) = collect(&mut slot) {
                return result;
            }
            slot = sync_lock::wait(&self.inner.cvar, slot, "oneshot::wait")?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// How long a correct implementation may take to wake a joiner. Chosen
    /// far above any plausible scheduling delay: this bound exists to turn
    /// a regression into a *failure* rather than a suite that hangs.
    const WAKE_BUDGET: Duration = Duration::from_secs(5);

    #[test]
    fn join_returns_the_sent_value() -> TestResult {
        let (tx, rx) = channel::<u32>();
        tx.send(42);
        match rx.join() {
            Ok(v) => {
                assert_eq!(v, 42);
                Ok(())
            }
            Err(e) => Err(format!("expected the value, got {e}").into()),
        }
    }

    #[test]
    fn a_value_sent_before_the_drop_still_wins() -> TestResult {
        // `send` consumes the sender, so `sender_gone` may already be set by
        // the time `join` looks. The value must still take precedence.
        let (tx, rx) = channel::<u32>();
        tx.send(7);
        match rx.join() {
            Ok(v) => {
                assert_eq!(v, 7);
                Ok(())
            }
            Err(e) => Err(format!("a sent value must outrank the disconnect: {e}").into()),
        }
    }

    #[test]
    fn dropping_the_sender_before_the_join_reports_abandoned() -> TestResult {
        let (tx, rx) = channel::<u32>();
        drop(tx);
        match rx.join() {
            Err(RuntimeError::Abandoned(_)) => Ok(()),
            Err(e) => Err(format!("expected Abandoned, got {e}").into()),
            Ok(_) => Err("a dropped sender cannot yield a value".into()),
        }
    }

    #[test]
    fn try_join_is_pending_while_the_flow_is_still_running() -> TestResult {
        let (tx, rx) = channel::<u32>();
        match rx.try_join()? {
            JoinState::Pending => {}
            JoinState::Ready(v) => return Err(format!("nothing was sent, got {v}").into()),
        }
        drop(tx);
        Ok(())
    }

    #[test]
    fn the_value_is_handed_out_exactly_once() -> TestResult {
        let (tx, rx) = channel::<u32>();
        tx.send(9);
        match rx.try_join()? {
            JoinState::Ready(v) => assert_eq!(v, 9),
            JoinState::Pending => return Err("the value was already sent".into()),
        }
        // A second poll must not claim the flow was abandoned: it completed
        // normally and was already observed.
        match rx.try_join() {
            Err(RuntimeError::AlreadyCollected(_)) => Ok(()),
            Err(e) => Err(format!("expected AlreadyCollected, got {e}").into()),
            Ok(_) => Err("the value must not be handed out twice".into()),
        }
    }

    #[test]
    fn a_timeout_leaves_the_receiver_usable() -> TestResult {
        let (tx, rx) = channel::<u32>();

        match rx.join_timeout(Duration::from_millis(30))? {
            JoinState::Pending => {}
            JoinState::Ready(v) => return Err(format!("nothing was sent, got {v}").into()),
        }

        // The whole point of `&self`: the receiver survived the expired
        // bound and can still collect the outcome.
        tx.send(4);
        match rx.join_timeout(Duration::from_millis(30))? {
            JoinState::Ready(v) => {
                assert_eq!(v, 4);
                Ok(())
            }
            JoinState::Pending => Err("the value was sent before this wait".into()),
        }
    }

    #[test]
    fn abandonment_outranks_a_pending_timeout() -> TestResult {
        let (tx, rx) = channel::<u32>();
        drop(tx);
        // A destroyed flow is a definite answer, not "not yet".
        match rx.join_timeout(Duration::from_secs(30)) {
            Err(RuntimeError::Abandoned(_)) => Ok(()),
            Err(e) => Err(format!("expected Abandoned, got {e}").into()),
            Ok(_) => Err("a dropped sender cannot yield a value".into()),
        }
    }

    /// The regression test for the reason `join_deadline` is anchored on an
    /// `Instant`: a duration fed back into each turn of the wait loop would
    /// restart the full budget on every spurious wakeup, so a steady stream
    /// of them would make the "timeout" never expire.
    #[test]
    fn the_deadline_holds_under_a_storm_of_spurious_wakeups() -> TestResult {
        let (tx, rx) = channel::<u32>();
        let inner = rx.inner.clone();
        let stop = Arc::new(AtomicBool::new(false));

        let halt = stop.clone();
        let noise = thread::spawn(move || {
            while !halt.load(Ordering::SeqCst) {
                // Notifying without touching the slot is, by definition, a
                // spurious wakeup for the waiter.
                inner.cvar.notify_all();
                thread::sleep(Duration::from_millis(1));
            }
        });

        let bound = Duration::from_millis(150);
        let started = Instant::now();
        let state = rx.join_timeout(bound);
        let elapsed = started.elapsed();

        stop.store(true, Ordering::SeqCst);
        if noise.join().is_err() {
            return Err("the notifier thread panicked".into());
        }
        drop(tx);

        match state? {
            JoinState::Pending => {}
            JoinState::Ready(v) => return Err(format!("nothing was sent, got {v}").into()),
        }
        // Upper bound generous enough to never flake on a loaded CI box, and
        // still orders of magnitude below the "never returns" regression.
        if elapsed > Duration::from_secs(5) {
            return Err(format!("the bound was {bound:?} but the wait took {elapsed:?}").into());
        }
        // Lower bound: it must actually have waited, not bailed out at once.
        if elapsed < Duration::from_millis(100) {
            return Err(format!("returned after {elapsed:?}, before the bound").into());
        }
        Ok(())
    }

    #[test]
    fn dropping_the_sender_wakes_a_joiner_that_is_already_blocked() -> TestResult {
        let (tx, rx) = channel::<u32>();
        let finished = Arc::new(AtomicBool::new(false));

        let flag = finished.clone();
        let joiner = thread::spawn(move || {
            let outcome = rx.join();
            flag.store(true, Ordering::SeqCst);
            outcome
        });

        // Let the joiner reach the condvar, so this exercises the wakeup
        // path and not just the flag check done before waiting.
        thread::sleep(Duration::from_millis(50));
        drop(tx);

        // Poll rather than joining the thread directly: if the wakeup
        // regresses, this reports a failure instead of blocking forever.
        let deadline = Instant::now() + WAKE_BUDGET;
        while !finished.load(Ordering::SeqCst) {
            if Instant::now() > deadline {
                return Err("join() never returned after the sender was dropped".into());
            }
            thread::sleep(Duration::from_millis(10));
        }

        match joiner.join() {
            Ok(Err(RuntimeError::Abandoned(_))) => Ok(()),
            Ok(Err(e)) => Err(format!("expected Abandoned, got {e}").into()),
            Ok(Ok(_)) => Err("a dropped sender cannot yield a value".into()),
            Err(_) => Err("the joining thread panicked".into()),
        }
    }
}
