//! Chase-Lev work-stealing deque (single owner, many stealers).
//!
//! The owner pushes and pops at the bottom. Stealers take from the top.
//! Those two ends meet only on the last task, so a busy worker does not
//! take a mutex to run its own queue. `unsafe` stays in this module;
//! the parent module exposes the safe wrapper.
//!
//! Buffer growth is intentionally absent: the ring has a fixed power-of-two
//! capacity. A full push returns the task so the caller can spill it onto
//! the global injector. That avoids retiring old buffers while a stealer
//! still holds the previous pointer.
//!
//! Algorithm: Chase and Lev, "Dynamic Circular Work-Stealing Deque" (SPAA
//! 2005), with the Lé et al. correction for weak memory (PPoPP 2013).

#![allow(unsafe_code)]

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicI64, Ordering};

use super::Steal;

/// Slots in one worker's local ring. Power of two so the index is a mask.
pub(super) const LOCAL_CAP: usize = 256;

struct Slot<T> {
    value: UnsafeCell<MaybeUninit<T>>,
}

/// Owner-at-bottom, stealers-at-top ring.
///
/// `T: Send` because a stealer moves the task onto another thread.
pub(super) struct LocalQueue<T> {
    top: AtomicI64,
    bottom: AtomicI64,
    slots: Box<[Slot<T>]>,
}

// Safety: a live index is written by the owner before `bottom` is published,
// and moved out by exactly one thread. The loser of the last-task race
// `mem::forget`s its bitwise copy, so `Drop` runs once. Concurrent reads of
// that slot have no concurrent write. `unsafe impl` is required because
// `UnsafeCell` is `!Sync` and stealers share the ring.
unsafe impl<T: Send> Send for LocalQueue<T> {}
unsafe impl<T: Send> Sync for LocalQueue<T> {}

impl<T> LocalQueue<T> {
    pub(super) fn new() -> Self {
        let mut slots = Vec::with_capacity(LOCAL_CAP);
        for _ in 0..LOCAL_CAP {
            slots.push(Slot {
                value: UnsafeCell::new(MaybeUninit::uninit()),
            });
        }
        Self {
            top: AtomicI64::new(0),
            bottom: AtomicI64::new(0),
            slots: slots.into_boxed_slice(),
        }
    }

    /// Tasks the owner has published and nobody has taken yet.
    ///
    /// Racy by nature: a stealer may win the last task between the two
    /// loads. Callers use it as a hint (yield fairness), not as a bound.
    pub(super) fn has_tasks(&self) -> bool {
        let b = self.bottom.load(Ordering::Relaxed);
        let t = self.top.load(Ordering::Acquire);
        b.wrapping_sub(t) > 0
    }

    /// Owner only. `Err(task)` means the ring is full; the task was not stored.
    pub(super) fn push(&self, task: T) -> Result<(), T> {
        let b = self.bottom.load(Ordering::Relaxed);
        let t = self.top.load(Ordering::Acquire);
        if b.wrapping_sub(t) >= LOCAL_CAP as i64 {
            return Err(task);
        }
        // Safety: `b - t < CAP`, so slot `b` is not in the live range.
        // Only the owner writes `bottom`, so no other push targets `b`.
        unsafe { self.write(b, task) };
        std::sync::atomic::fence(Ordering::Release);
        self.bottom.store(b.wrapping_add(1), Ordering::Relaxed);
        Ok(())
    }

    /// Owner only. `None` if the ring is empty or a stealer took the last task.
    pub(super) fn pop(&self) -> Option<T> {
        let b = self.bottom.load(Ordering::Relaxed).wrapping_sub(1);
        self.bottom.store(b, Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::SeqCst);
        let t = self.top.load(Ordering::Relaxed);
        if b < t {
            self.bottom.store(t, Ordering::Relaxed);
            return None;
        }
        // Safety: `b` was published by a previous push and is still inside
        // the ring (`b >= t`). A stealer only reads index `top`.
        let task = unsafe { self.read(b) };
        if b > t {
            return Some(task);
        }
        let won = self
            .top
            .compare_exchange(t, t.wrapping_add(1), Ordering::SeqCst, Ordering::Relaxed)
            .is_ok();
        self.bottom.store(t.wrapping_add(1), Ordering::Relaxed);
        if won {
            Some(task)
        } else {
            // The stealer owns the only copy that will be dropped.
            std::mem::forget(task);
            None
        }
    }

    pub(super) fn steal(&self) -> Steal<T> {
        let t = self.top.load(Ordering::Acquire);
        std::sync::atomic::fence(Ordering::SeqCst);
        let b = self.bottom.load(Ordering::Acquire);
        if t >= b {
            return Steal::Empty;
        }
        // Safety: `t < b`, so a push has released this slot. The CAS below
        // decides which stealer keeps the bitwise copy.
        let task = unsafe { self.read(t) };
        if self
            .top
            .compare_exchange(t, t.wrapping_add(1), Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
        {
            Steal::Success(task)
        } else {
            std::mem::forget(task);
            Steal::Retry
        }
    }

    /// # Safety
    /// `index` must not be live for another writer, and must be `>= 0`.
    unsafe fn write(&self, index: i64, task: T) {
        let slot = self.slot(index);
        unsafe { (*slot).write(task) };
    }

    /// # Safety
    /// `index` must refer to an initialized slot. The caller keeps at most
    /// one of the bitwise copies (see [`Self::pop`] / [`Self::steal`]).
    unsafe fn read(&self, index: i64) -> T {
        let slot = self.slot(index);
        unsafe { (*slot).assume_init_read() }
    }

    fn slot(&self, index: i64) -> *mut MaybeUninit<T> {
        debug_assert!(index >= 0);
        let i = (index as usize) & (LOCAL_CAP - 1);
        self.slots[i].value.get()
    }
}

impl<T> Drop for LocalQueue<T> {
    fn drop(&mut self) {
        // Last `Arc` owner: stealers and the worker are gone.
        let t = *self.top.get_mut();
        let b = *self.bottom.get_mut();
        let mut i = t;
        while i < b {
            let slot = self.slot(i);
            unsafe { (*slot).assume_init_drop() };
            i = i.wrapping_add(1);
        }
    }
}
