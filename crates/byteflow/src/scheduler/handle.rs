use std::time::{Duration, Instant};

use super::oneshot::{self, JoinState};
use super::process::{FlowId, FlowOutcome};
use super::worker::is_on_worker;

/// Why a host-side join refused to wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinError {
    /// [`FlowHandle::join`] (or a bounded variant) was called on a worker
    /// thread. That deadlocks the M:N pool if the target needs this worker.
    CalledFromWorker,
}

impl std::fmt::Display for JoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CalledFromWorker => {
                f.write_str("FlowHandle::join called from a Byteflow worker thread")
            }
        }
    }
}

impl std::error::Error for JoinError {}

/// A reference to a spawned **flow**, returned by
/// [`super::runtime::Runtime::spawn`].
///
/// Holding a `FlowHandle` does not keep the flow alive — it is a way to
/// (a) read its [`FlowId`] for addressing with `Send`, and (b) block the
/// *calling native thread* until it finishes via [`FlowHandle::join`].
/// Dropping without joining is fine (fire-and-forget).
pub struct FlowHandle {
    pub(crate) id: FlowId,
    pub(crate) receiver: oneshot::Receiver<FlowOutcome>,
}

impl FlowHandle {
    pub fn id(&self) -> FlowId {
        self.id
    }

    /// Block the current (native) thread until the flow terminates.
    ///
    /// **Never call from inside a worker / from a native invoked by bytecode.**
    /// That case is machine-checked: this returns
    /// [`FlowOutcome::Failed`] with [`JoinError::CalledFromWorker`] rather
    /// than deadlocking the pool.
    ///
    /// Returns [`FlowOutcome::Failed`] rather than blocking forever if the
    /// flow was destroyed without producing an outcome — most commonly
    /// `Runtime::shutdown` while the flow was suspended, since shutdown does
    /// not drain flows out of the timer or the worker deques. The message
    /// comes from [`super::error::RuntimeError::Abandoned`], so it is
    /// distinguishable from a flow that genuinely faulted.
    pub fn join(self) -> FlowOutcome {
        if is_on_worker() {
            return FlowOutcome::Failed(JoinError::CalledFromWorker.to_string());
        }
        match self.receiver.join() {
            Ok(outcome) => outcome,
            Err(e) => FlowOutcome::Failed(e.to_string()),
        }
    }

    /// Same as [`Self::join`], but surfaces [`JoinError`] instead of folding
    /// it into [`FlowOutcome::Failed`].
    pub fn join_checked(self) -> Result<FlowOutcome, JoinError> {
        if is_on_worker() {
            return Err(JoinError::CalledFromWorker);
        }
        Ok(match self.receiver.join() {
            Ok(outcome) => outcome,
            Err(e) => FlowOutcome::Failed(e.to_string()),
        })
    }

    /// The outcome if the flow has already terminated, `None` if it is still
    /// running. Never waits.
    ///
    /// For embedders that drive their own loop and cannot surrender a thread
    /// to [`FlowHandle::join`]. Takes `&self`, so it can be polled until it
    /// answers; note that the outcome is handed out exactly once, and a
    /// further poll after that reports
    /// [`super::error::RuntimeError::AlreadyCollected`] as
    /// [`FlowOutcome::Failed`] rather than repeating it.
    pub fn try_join(&self) -> Option<FlowOutcome> {
        if is_on_worker() {
            return Some(FlowOutcome::Failed(JoinError::CalledFromWorker.to_string()));
        }
        Self::settle(self.receiver.try_join())
    }

    /// Wait up to `timeout` for the flow to terminate; `None` if it is still
    /// running when the bound elapses.
    ///
    /// This is the variant to reach for in anything with a deadline — a
    /// control loop, a watchdog, a test harness — since it is the only join
    /// whose worst-case duration the caller chooses.
    pub fn join_timeout(&self, timeout: Duration) -> Option<FlowOutcome> {
        if is_on_worker() {
            return Some(FlowOutcome::Failed(JoinError::CalledFromWorker.to_string()));
        }
        Self::settle(self.receiver.join_timeout(timeout))
    }

    /// [`FlowHandle::join_timeout`] against an absolute deadline, for callers
    /// that already track one and must not have it drift across repeated
    /// waits.
    pub fn join_deadline(&self, deadline: Instant) -> Option<FlowOutcome> {
        if is_on_worker() {
            return Some(FlowOutcome::Failed(JoinError::CalledFromWorker.to_string()));
        }
        Self::settle(self.receiver.join_deadline(deadline))
    }

    /// Collapse a bounded-wait result into the handle's public vocabulary:
    /// `None` for "still running", `Some(Failed)` for an infrastructure
    /// error, since an embedder polling a handle has no separate error
    /// channel to report one on.
    fn settle(
        state: Result<JoinState<FlowOutcome>, super::error::RuntimeError>,
    ) -> Option<FlowOutcome> {
        match state {
            Ok(JoinState::Ready(outcome)) => Some(outcome),
            Ok(JoinState::Pending) => None,
            Err(e) => Some(FlowOutcome::Failed(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::worker::WorkerGuard;
    use super::*;

    #[test]
    fn join_from_worker_is_rejected() {
        let _guard = WorkerGuard::enter();
        // Synthetic handle: receiver will never complete; we must not block.
        let (_tx, rx) = oneshot::channel::<FlowOutcome>();
        let handle = FlowHandle {
            id: FlowId(1),
            receiver: rx,
        };
        match handle.join_checked() {
            Err(JoinError::CalledFromWorker) => {}
            other => panic!("expected CalledFromWorker, got {other:?}"),
        }
    }
}
