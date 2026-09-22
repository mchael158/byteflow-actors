//! Std-only M:N run queue (no `crossbeam-deque`).
//!
//! Global injector + per-worker FIFO locals with stealers. Semantics match
//! what the scheduler needs: push ready flows, pop locally first, then
//! steal from the injector, then from peer workers. All synchronization is
//! `std::sync::Mutex` — fail-closed via [`super::sync_lock`].
//!
//! Steal outcomes are only [`Steal::Empty`] / [`Steal::Success`]: under a
//! mutex queue there is no transient "retry" state like classic
//! work-stealing deques.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::sync_lock;

/// Outcome of a steal attempt under mutex queues (Empty / Success).
#[derive(Debug)]
pub enum Steal<T> {
    Empty,
    Success(T),
}

/// Shared FIFO for newly spawned / externally woken flows.
pub struct Injector<T> {
    inner: Mutex<VecDeque<T>>,
}

impl<T> Injector<T> {
    pub fn new() -> Self {
        Injector {
            inner: Mutex::new(VecDeque::new()),
        }
    }

    pub fn push(&self, task: T) {
        if let Ok(mut q) = sync_lock::lock(&self.inner, "Injector::push") {
            q.push_back(task);
        }
        // Poison: drop the task (fail-closed). Caller already woke workers;
        // a poisoned runtime is shutting down or is unusable.
    }

    pub fn steal(&self) -> Steal<T> {
        match sync_lock::lock(&self.inner, "Injector::steal") {
            Ok(mut q) => match q.pop_front() {
                Some(t) => Steal::Success(t),
                None => Steal::Empty,
            },
            Err(_) => Steal::Empty,
        }
    }
}

impl<T> Default for Injector<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-worker FIFO. Owned by one OS thread; peers steal via [`Stealer`].
pub struct Worker<T> {
    queue: Arc<Mutex<VecDeque<T>>>,
}

impl<T> Worker<T> {
    pub fn new_fifo() -> Self {
        Worker {
            queue: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    pub fn push(&self, task: T) {
        if let Ok(mut q) = sync_lock::lock(&self.queue, "Worker::push") {
            q.push_back(task);
        }
    }

    pub fn pop(&self) -> Option<T> {
        match sync_lock::lock(&self.queue, "Worker::pop") {
            Ok(mut q) => q.pop_front(),
            Err(_) => None,
        }
    }

    pub fn stealer(&self) -> Stealer<T> {
        Stealer {
            queue: Arc::clone(&self.queue),
        }
    }
}

/// Handle for stealing work from another worker's local queue.
pub struct Stealer<T> {
    queue: Arc<Mutex<VecDeque<T>>>,
}

impl<T> Stealer<T> {
    pub fn steal(&self) -> Steal<T> {
        match sync_lock::lock(&self.queue, "Stealer::steal") {
            Ok(mut q) => match q.pop_front() {
                Some(t) => Steal::Success(t),
                None => Steal::Empty,
            },
            Err(_) => Steal::Empty,
        }
    }
}

impl<T> Clone for Stealer<T> {
    fn clone(&self) -> Self {
        Stealer {
            queue: Arc::clone(&self.queue),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injector_fifo_order() {
        let inj = Injector::new();
        inj.push(1u32);
        inj.push(2);
        assert!(matches!(inj.steal(), Steal::Success(1)));
        assert!(matches!(inj.steal(), Steal::Success(2)));
        assert!(matches!(inj.steal(), Steal::Empty));
    }

    #[test]
    fn local_then_stealer() {
        let w = Worker::new_fifo();
        let s = w.stealer();
        w.push(10u32);
        w.push(20);
        assert_eq!(w.pop(), Some(10));
        assert!(matches!(s.steal(), Steal::Success(20)));
        assert!(matches!(s.steal(), Steal::Empty));
    }
}
