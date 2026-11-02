//! Fail-closed locking for scheduler shared state.
//!
//! # Why we do **not** recover from poison
//!
//! `std::sync::Mutex` marks itself poisoned when a thread panics while holding
//! the guard. That means the protected value *may* be mid-update
//! (directory shard, mailbox queue, supervisor tables, oneshot slot, …).
//!
//! Silently calling `PoisonError::into_inner()` and continuing is the wrong
//! trade-off for a concurrent runtime: you schedule and deliver messages
//! against potentially inconsistent tables. High-integrity practice
//! (fail closed) is to abort the operation with a loud diagnostic so the
//! fault cannot cascade as silent corruption.
//!
//! flow-level panics are already isolated by `catch_unwind` around
//! `Vm::run` and never hold these locks across that boundary. Poison here
//! therefore implies a **scheduler / host bug**, not a buggy actor.
//!
//! These helpers return [`RuntimeError::PoisonedLock`] instead of panicking
//! so worker loops can [`report_fault`] and exit cleanly, and API boundaries
//! can propagate `Result`. We still never call `into_inner()`.

use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

use super::error::RuntimeError;

/// Acquire `m`, or [`RuntimeError::PoisonedLock`] (integrity fault).
#[inline]
pub(super) fn lock<'a, T>(
    m: &'a Mutex<T>,
    where_: &'static str,
) -> Result<MutexGuard<'a, T>, RuntimeError> {
    m.lock().map_err(|_| RuntimeError::PoisonedLock(where_))
}

/// Wait on `cvar`, or poison error if the wait observes a poisoned mutex.
#[inline]
pub(super) fn wait<'a, T>(
    cvar: &Condvar,
    guard: MutexGuard<'a, T>,
    where_: &'static str,
) -> Result<MutexGuard<'a, T>, RuntimeError> {
    cvar.wait(guard)
        .map_err(|_| RuntimeError::PoisonedLock(where_))
}

/// Timed wait, or poison error if the wait observes a poisoned mutex.
#[inline]
pub(super) fn wait_timeout<'a, T>(
    cvar: &Condvar,
    guard: MutexGuard<'a, T>,
    timeout: Duration,
    where_: &'static str,
) -> Result<(MutexGuard<'a, T>, std::sync::WaitTimeoutResult), RuntimeError> {
    cvar.wait_timeout(guard, timeout)
        .map_err(|_| RuntimeError::PoisonedLock(where_))
}
