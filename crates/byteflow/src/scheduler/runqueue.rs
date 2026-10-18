//! M:N run queue.
//!
//! Each worker owns a Chase-Lev deque ([`Worker`]): the owner pushes and
//! pops at the bottom without a mutex, peers steal from the top. The global
//! [`Injector`] is the multi-producer FIFO for spawns and wakeups. Workers
//! move a batch off it under one lock, then run from the local deque.
//!
//! A panic while holding the injector used to poison the mutex and leave
//! every flow already in the deque unreachable. The critical section only
//! touches a [`VecDeque`], whose updates stay valid if they unwind, so the
//! injector clears that poison and keeps the queued flows. `push` still
//! returns the task when the lock cannot be taken at all.

mod chase_lev;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use self::chase_lev::LocalQueue;

/// How many injector tasks one worker pulls under a single lock.
pub(crate) const INJECTOR_BATCH: usize = 32;

/// Outcome of a steal attempt.
#[derive(Debug)]
pub enum Steal<T> {
    Empty,
    Success(T),
    /// Lost the race with another stealer or the owner. The task is still queued.
    Retry,
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

    /// Enqueue `task`. A poisoned injector is cleared first, so `task` joins
    /// the flows that were already queued instead of being dropped.
    pub fn push(&self, task: T) {
        self.lock_queue("Injector::push").push_back(task);
    }

    /// Take up to `limit` tasks from the front (oldest first).
    ///
    /// A poisoned lock is cleared. Flows already in the deque come out in
    /// FIFO order instead of staying trapped until process exit.
    pub fn steal_batch(&self, limit: usize) -> Vec<T> {
        if limit == 0 {
            return Vec::new();
        }
        let mut q = self.lock_queue("Injector::steal_batch");
        let n = limit.min(q.len());
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            match q.pop_front() {
                Some(task) => out.push(task),
                None => break,
            }
        }
        out
    }

    pub(crate) fn drain(&self) -> Vec<T> {
        self.lock_queue("Injector::drain").drain(..).collect()
    }

    /// The deque under this mutex only changes through `VecDeque` methods,
    /// which stay valid if they unwind. A poisoned guard is cleared so those
    /// flows can still be run or finalized.
    fn lock_queue(&self, where_: &'static str) -> std::sync::MutexGuard<'_, VecDeque<T>> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                self.inner.clear_poison();
                super::error::report_fault(super::error::RuntimeError::PoisonedLock(where_));
                poisoned.into_inner()
            }
        }
    }

    /// Poison the mutex. Tests use this to force [`Self::push`] to return the task.
    #[cfg(test)]
    pub(crate) fn poison_for_test(&self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Ok(_guard) = self.inner.lock() {
                panic!("poison injector");
            }
        }));
    }
}

impl<T> Default for Injector<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-worker Chase-Lev deque. One OS thread owns [`Self::push`] / [`Self::pop`];
/// peers steal via [`Stealer`].
///
/// Owner pop is LIFO (the task just pushed). Steal takes the oldest task.
pub struct Worker<T> {
    queue: Arc<LocalQueue<T>>,
}

impl<T> Worker<T> {
    pub fn new() -> Self {
        Worker {
            queue: Arc::new(LocalQueue::new()),
        }
    }

    /// `Err(task)` means the local ring is full. The caller spills onto the injector.
    #[must_use = "a full local deque returns the task"]
    pub fn push(&self, task: T) -> Result<(), T> {
        self.queue.push(task)
    }

    pub fn pop(&self) -> Option<T> {
        self.queue.pop()
    }

    /// `true` when a stealer or the owner might still observe a task.
    pub fn has_tasks(&self) -> bool {
        self.queue.has_tasks()
    }

    pub fn stealer(&self) -> Stealer<T> {
        Stealer {
            queue: Arc::clone(&self.queue),
        }
    }
}

/// Handle for stealing work from another worker's local queue.
pub struct Stealer<T> {
    queue: Arc<LocalQueue<T>>,
}

impl<T> Stealer<T> {
    pub fn steal(&self) -> Steal<T> {
        self.queue.steal()
    }

    pub(crate) fn drain(&self) -> Vec<T> {
        let mut out = Vec::new();
        loop {
            match self.steal() {
                Steal::Success(task) => out.push(task),
                Steal::Empty => break,
                Steal::Retry => {}
            }
        }
        out
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    use super::*;

    #[test]
    fn injector_fifo_order() {
        let inj = Injector::new();
        inj.push(1u32);
        inj.push(2);
        assert_eq!(inj.steal_batch(1), vec![1]);
        assert_eq!(inj.steal_batch(1), vec![2]);
        assert!(inj.steal_batch(1).is_empty());
    }

    #[test]
    fn injector_batch_keeps_fifo() {
        let inj = Injector::new();
        inj.push(1u32);
        inj.push(2);
        inj.push(3);
        assert_eq!(inj.steal_batch(2), vec![1, 2]);
        assert_eq!(inj.steal_batch(1), vec![3]);
    }

    #[test]
    fn poisoned_injector_keeps_the_tasks_already_queued() {
        let inj = Injector::new();
        inj.push(1u32);
        inj.push(2);
        inj.poison_for_test();
        inj.push(3);
        assert_eq!(inj.steal_batch(2), vec![1, 2]);
        assert_eq!(inj.drain(), vec![3]);
        assert!(inj.steal_batch(1).is_empty());
    }

    #[test]
    fn owner_pops_newest_stealer_takes_oldest() {
        let w = Worker::new();
        let s = w.stealer();
        assert!(w.push(10u32).is_ok());
        assert!(w.push(20).is_ok());
        assert_eq!(w.pop(), Some(20));
        assert!(matches!(s.steal(), Steal::Success(10)));
        assert!(matches!(s.steal(), Steal::Empty));
    }

    #[test]
    fn full_local_deque_returns_the_task() {
        let w = Worker::<u32>::new();
        let mut n = 0u32;
        loop {
            if w.push(n).is_err() {
                break;
            }
            n += 1;
            assert!(n <= super::chase_lev::LOCAL_CAP as u32, "ring did not fill");
        }
        assert_eq!(n, super::chase_lev::LOCAL_CAP as u32);
        assert!(w.pop().is_some());
        assert!(w.push(n).is_ok());
    }

    #[test]
    fn owner_and_thieves_conserve_every_task() {
        const N: u32 = 2_000;
        let worker: Worker<Box<u32>> = Worker::new();
        let done = Arc::new(AtomicBool::new(false));
        let stolen = Arc::new(Mutex::new(Vec::new()));
        let mut joins = Vec::new();
        for _ in 0..2 {
            let stealer = worker.stealer();
            let done = Arc::clone(&done);
            let stolen = Arc::clone(&stolen);
            joins.push(thread::spawn(move || {
                let mut local = Vec::new();
                loop {
                    match stealer.steal() {
                        Steal::Success(v) => local.push(*v),
                        Steal::Retry => {}
                        Steal::Empty => {
                            if done.load(Ordering::Acquire) {
                                match stealer.steal() {
                                    Steal::Success(v) => local.push(*v),
                                    Steal::Retry => {}
                                    Steal::Empty => break,
                                }
                            } else {
                                thread::yield_now();
                            }
                        }
                    }
                }
                if let Ok(mut guard) = stolen.lock() {
                    guard.extend(local);
                }
            }));
        }

        let mut owned = Vec::new();
        for i in 0..N {
            let mut task = Box::new(i);
            loop {
                match worker.push(task) {
                    Ok(()) => break,
                    Err(returned) => {
                        task = returned;
                        if let Some(v) = worker.pop() {
                            owned.push(*v);
                        } else {
                            thread::yield_now();
                        }
                    }
                }
            }
            if i % 3 == 0 {
                if let Some(v) = worker.pop() {
                    owned.push(*v);
                }
            }
        }
        while let Some(v) = worker.pop() {
            owned.push(*v);
        }
        done.store(true, Ordering::Release);
        for join in joins {
            let _ = join.join();
        }
        // Thieves may have stopped on an empty observation just as the
        // owner finished. Anything still queued belongs in the result.
        if let Some(v) = worker.pop() {
            owned.push(*v);
        }
        for v in worker.stealer().drain() {
            owned.push(*v);
        }

        let mut all = owned;
        if let Ok(guard) = stolen.lock() {
            all.extend(guard.iter().copied());
        }
        all.sort_unstable();
        let expect: Vec<u32> = (0..N).collect();
        assert_eq!(all, expect);
    }
}
