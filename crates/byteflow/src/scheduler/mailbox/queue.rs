use std::collections::VecDeque;

use crate::bytecode::Value;

use super::{MailboxFullReason, OverflowPolicy, WaitFilter};

/// FIFO hop storage with two **logical** bounds independent of physical
/// allocation: a hop count and a byte budget.
///
/// # Logical vs physical
///
/// `limit` is how many hops this inbox may hold. `VecDeque` capacity is
/// how much the allocator currently reserved. A flow that receives one hop
/// with `limit = 4096` must **not** pay for 4096 slots up front.
///
/// Growth is geometric and capped at `limit` (`reserve_for_push`). That
/// keeps hot mailboxes from reallocating on every push without pre-paying
/// the worst-case footprint.
///
/// # Why bytes are tracked here and not by the caller
///
/// `bytes` must move in lockstep with every push **and** pop, or the
/// budget drifts until the inbox wedges (a leaked charge is never
/// refunded, so the mailbox rejects forever). That is why this type owns
/// the filtered take ([`Self::take`]) instead of handing out `&mut
/// VecDeque`: there is no way to remove a hop without going through the
/// accounting.
///
/// # Why `VecDeque`, not an `unsafe` ring
///
/// A dedicated `MaybeUninit` ring would be a natural next step, but this
/// crate is `#![forbid(unsafe_code)]`. The queue is a `pub(crate)`
/// abstraction so a later ring can replace `inner` without touching
/// FlowCap, Ask, or the worker loop.
pub(crate) struct MailboxQueue {
    inner: VecDeque<Value>,
    limit: usize,
    bytes: usize,
    byte_limit: usize,
}

impl MailboxQueue {
    pub(crate) fn new(limit: usize, byte_limit: usize) -> Self {
        debug_assert!(limit >= 1);
        debug_assert!(byte_limit >= 1);
        Self {
            inner: VecDeque::new(),
            limit,
            bytes: 0,
            byte_limit,
        }
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.inner.len()
    }

    /// Bytes currently charged to this inbox (see
    /// [`Value::memory_size`]).
    #[inline]
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    /// True iff `cost` can occupy an empty inbox (count and byte budget).
    #[inline]
    pub(crate) fn can_ever_fit(&self, cost: usize) -> bool {
        cost <= self.byte_limit && self.limit >= 1
    }

    #[inline]
    fn is_full(&self) -> bool {
        self.inner.len() >= self.limit
    }

    #[inline]
    fn would_exceed_bytes(&self, cost: usize) -> bool {
        self.bytes.saturating_add(cost) > self.byte_limit
    }

    /// Remove one hop matching `filter`, preserving the relative order of
    /// everything else (FIFO skip — non-matching hops are never dropped).
    pub(crate) fn take(&mut self, filter: WaitFilter) -> Option<Value> {
        let value = match filter {
            WaitFilter::Any => self.inner.pop_front(),
            other => {
                let idx = self.inner.iter().position(|v| other.matches(v))?;
                self.inner.remove(idx)
            }
        };
        if let Some(v) = &value {
            self.bytes = self.bytes.saturating_sub(v.memory_size());
        }
        value
    }

    /// Try to accept `value` under `policy`.
    ///
    /// `Err` carries which bound refused the hop. When both are at their
    /// limit the hop count is reported first, because it is the bound
    /// embedders configure most often and the one they read in
    /// [`super::MailboxCapacity`].
    pub(crate) fn enqueue(
        &mut self,
        value: Value,
        policy: OverflowPolicy,
    ) -> Result<EnqueueEffect, MailboxFullReason> {
        let cost = value.memory_size();
        if !self.is_full() && !self.would_exceed_bytes(cost) {
            if !reserve_for_push(&mut self.inner, self.limit) {
                return Err(MailboxFullReason::MessageLimit);
            }
            self.bytes = self.bytes.saturating_add(cost);
            self.inner.push_back(value);
            return Ok(EnqueueEffect::Enqueued);
        }
        let reason = if self.is_full() {
            MailboxFullReason::MessageLimit
        } else {
            MailboxFullReason::ByteLimit
        };
        match policy {
            OverflowPolicy::Reject => Err(reason),
            // "Drop the newest" is satisfiable no matter how large the
            // incoming hop is: the hop discarded *is* the incoming one.
            OverflowPolicy::DropNewest => Ok(EnqueueEffect::DroppedNewest),
            OverflowPolicy::DropOldest => {
                let mut dropped = false;
                while (self.is_full() || self.would_exceed_bytes(cost)) && !self.inner.is_empty() {
                    if let Some(old) = self.inner.pop_front() {
                        self.bytes = self.bytes.saturating_sub(old.memory_size());
                        dropped = true;
                    }
                }
                // An empty inbox that still cannot fit `cost` means the hop
                // is larger than the entire budget. Evicting the queue
                // bought nothing, so refuse instead of pretending it landed.
                if self.would_exceed_bytes(cost) {
                    return Err(MailboxFullReason::ByteLimit);
                }
                self.bytes = self.bytes.saturating_add(cost);
                self.inner.push_back(value);
                Ok(if dropped {
                    EnqueueEffect::DroppedOldest
                } else {
                    EnqueueEffect::Enqueued
                })
            }
        }
    }

    /// System hops (`DOWN`) must not be lost to overflow. Evict oldest
    /// user hops first so a DOWN does not permanently wedge the budget.
    /// A single hop larger than the whole budget still over-charges —
    /// losing DOWN is worse.
    pub(crate) fn force_push(&mut self, value: Value) {
        let cost = value.memory_size();
        while (self.is_full() || self.would_exceed_bytes(cost)) && !self.inner.is_empty() {
            if let Some(old) = self.inner.pop_front() {
                self.bytes = self.bytes.saturating_sub(old.memory_size());
            }
        }
        let cap = self.limit.max(self.inner.len().saturating_add(1));
        let _ = reserve_for_push(&mut self.inner, cap);
        self.bytes = self.bytes.saturating_add(cost);
        self.inner.push_back(value);
    }
}

/// Physical growth: double current `VecDeque` capacity, never past `limit`.
fn reserve_for_push(queue: &mut VecDeque<Value>, capacity: usize) -> bool {
    if queue.len() < queue.capacity() {
        return true;
    }
    let current = queue.capacity();
    let next = current.max(1).saturating_mul(2).min(capacity);
    if next <= current {
        return queue.len() < capacity;
    }
    queue.reserve(next - current);
    true
}

/// What [`MailboxQueue::enqueue`] did (stats + [`super::Delivery`] mapping).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EnqueueEffect {
    Enqueued,
    DroppedNewest,
    DroppedOldest,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{Message, Value};

    /// Byte budget large enough that hop-count tests never trip it.
    const ROOMY: usize = 1 << 20;

    fn hop(n: u64) -> Value {
        Value::Message(Message::new(1, n, 1, n))
    }

    fn blob(len: usize) -> Value {
        Value::bytes(vec![0u8; len])
    }

    fn hop_payload_int(msg: &Message) -> i64 {
        msg.payload.as_int().unwrap_or(0)
    }

    fn hop_payload(q: &mut MailboxQueue) -> Result<i64, &'static str> {
        match q.take(WaitFilter::Any) {
            Some(v) => match v.as_message() {
                Some(m) => Ok(hop_payload_int(m)),
                None => Err("expected Message"),
            },
            None => Err("queue empty"),
        }
    }

    #[test]
    fn reject_at_limit() {
        let mut q = MailboxQueue::new(2, ROOMY);
        assert_eq!(
            q.enqueue(hop(1), OverflowPolicy::Reject),
            Ok(EnqueueEffect::Enqueued)
        );
        assert_eq!(
            q.enqueue(hop(2), OverflowPolicy::Reject),
            Ok(EnqueueEffect::Enqueued)
        );
        assert_eq!(
            q.enqueue(hop(3), OverflowPolicy::Reject),
            Err(MailboxFullReason::MessageLimit)
        );
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn drop_newest_keeps_old() -> Result<(), &'static str> {
        let mut q = MailboxQueue::new(1, ROOMY);
        let _ = q.enqueue(hop(1), OverflowPolicy::DropNewest);
        assert_eq!(
            q.enqueue(hop(2), OverflowPolicy::DropNewest),
            Ok(EnqueueEffect::DroppedNewest)
        );
        assert_eq!(hop_payload(&mut q)?, 1);
        Ok(())
    }

    #[test]
    fn drop_oldest_slides() -> Result<(), &'static str> {
        let mut q = MailboxQueue::new(2, ROOMY);
        let _ = q.enqueue(hop(1), OverflowPolicy::DropOldest);
        let _ = q.enqueue(hop(2), OverflowPolicy::DropOldest);
        assert_eq!(
            q.enqueue(hop(3), OverflowPolicy::DropOldest),
            Ok(EnqueueEffect::DroppedOldest)
        );
        assert_eq!(hop_payload(&mut q)?, 2);
        assert_eq!(hop_payload(&mut q)?, 3);
        Ok(())
    }

    #[test]
    fn byte_limit_rejects_long_before_the_hop_count() {
        // 64 hop slots, but only room for ~one 3 KiB blob.
        let mut q = MailboxQueue::new(64, 4096);
        assert_eq!(
            q.enqueue(blob(3000), OverflowPolicy::Reject),
            Ok(EnqueueEffect::Enqueued)
        );
        assert_eq!(
            q.enqueue(blob(3000), OverflowPolicy::Reject),
            Err(MailboxFullReason::ByteLimit)
        );
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn take_refunds_the_charge() {
        let mut q = MailboxQueue::new(64, 4096);
        let _ = q.enqueue(blob(3000), OverflowPolicy::Reject);
        assert!(q.bytes() >= 3000);
        assert!(q.take(WaitFilter::Any).is_some());
        assert_eq!(q.bytes(), 0);
        // Budget freed, so an equally large hop fits again.
        assert_eq!(
            q.enqueue(blob(3000), OverflowPolicy::Reject),
            Ok(EnqueueEffect::Enqueued)
        );
    }

    #[test]
    fn selective_take_refunds_the_right_charge() {
        let mut q = MailboxQueue::new(64, ROOMY);
        let first = hop(1);
        let first_cost = first.memory_size();
        let _ = q.enqueue(first, OverflowPolicy::Reject);
        let _ = q.enqueue(blob(3000), OverflowPolicy::Reject);
        let _ = q.enqueue(hop(2), OverflowPolicy::Reject);
        let charged = q.bytes();
        // Tag 1 matches the Message hops, never the blob.
        assert!(q.take(WaitFilter::Tag(1)).is_some());
        assert_eq!(q.bytes(), charged - first_cost);
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn hop_larger_than_the_whole_budget_is_refused_even_by_drop_oldest() {
        let mut q = MailboxQueue::new(64, 2048);
        let _ = q.enqueue(hop(1), OverflowPolicy::DropOldest);
        // Evicting everything still cannot make room, so refuse rather
        // than report a delivery that silently never happened.
        assert_eq!(
            q.enqueue(blob(4000), OverflowPolicy::DropOldest),
            Err(MailboxFullReason::ByteLimit)
        );
    }

    #[test]
    fn force_push_evicts_oldest_to_protect_the_budget() {
        let mut q = MailboxQueue::new(8, 4096);
        for n in 0..3 {
            assert_eq!(
                q.enqueue(blob(1000), OverflowPolicy::Reject),
                Ok(EnqueueEffect::Enqueued),
                "blob {n} should fit"
            );
        }
        q.force_push(blob(3000));
        assert!(q.bytes() <= 4096 + 3000);
        assert!(q.len() <= 2);
    }

    #[test]
    fn drop_oldest_evicts_as_many_as_the_byte_budget_needs() {
        let mut q = MailboxQueue::new(64, 4096);
        for n in 0..3 {
            assert_eq!(
                q.enqueue(blob(1000), OverflowPolicy::DropOldest),
                Ok(EnqueueEffect::Enqueued),
                "blob {n} should fit"
            );
        }
        // A 3 KiB blob needs two of the three 1 KiB blobs evicted.
        assert_eq!(
            q.enqueue(blob(3000), OverflowPolicy::DropOldest),
            Ok(EnqueueEffect::DroppedOldest)
        );
        assert!(q.bytes() <= 4096);
    }
}
