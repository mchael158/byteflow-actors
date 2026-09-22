use crate::bytecode::Value;

use super::{MailboxFullReason, OverflowPolicy, WaitFilter};

/// FIFO hop storage with two **logical** bounds independent of physical
/// allocation: a hop count and a byte budget.
///
/// # Logical vs physical
///
/// `limit` is how many hops this inbox may hold. Physical capacity is
/// how many slots the ring currently reserved. A flow that receives one
/// hop with `limit = 4096` must **not** pay for 4096 slots up front.
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
/// the filtered take ([`Self::take`]) instead of handing out raw slots:
/// there is no way to remove a hop without going through the accounting.
///
/// # Safe growable ring
///
/// Storage is `Vec<Option<Value>>` with `head` / `len` — no `unsafe`,
/// compatible with `#![forbid(unsafe_code)]`.
pub(crate) struct MailboxQueue {
    buf: Vec<Option<Value>>,
    head: usize,
    len: usize,
    limit: usize,
    bytes: usize,
    byte_limit: usize,
}

impl MailboxQueue {
    pub(crate) fn new(limit: usize, byte_limit: usize) -> Self {
        debug_assert!(limit >= 1);
        debug_assert!(byte_limit >= 1);
        Self {
            buf: Vec::new(),
            head: 0,
            len: 0,
            limit,
            bytes: 0,
            byte_limit,
        }
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.len
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
        self.len >= self.limit
    }

    #[inline]
    fn would_exceed_bytes(&self, cost: usize) -> bool {
        self.bytes.saturating_add(cost) > self.byte_limit
    }

    #[inline]
    fn capacity(&self) -> usize {
        self.buf.len()
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn pop_front(&mut self) -> Option<Value> {
        if self.len == 0 {
            return None;
        }
        let value = self.buf[self.head].take();
        let cap = self.capacity();
        self.head = if cap == 0 { 0 } else { (self.head + 1) % cap };
        self.len -= 1;
        if self.len == 0 {
            self.head = 0;
        }
        value
    }

    fn push_back(&mut self, value: Value) {
        let cap = self.capacity();
        debug_assert!(cap > 0 && self.len < cap);
        let idx = (self.head + self.len) % cap;
        self.buf[idx] = Some(value);
        self.len += 1;
    }

    fn remove_at(&mut self, logical: usize) -> Option<Value> {
        if logical >= self.len {
            return None;
        }
        let cap = self.capacity();
        debug_assert!(cap > 0);
        let idx = (self.head + logical) % cap;
        let value = self.buf[idx].take();
        // Shift later elements toward the hole (preserves FIFO order).
        let mut i = logical;
        while i + 1 < self.len {
            let from = (self.head + i + 1) % cap;
            let to = (self.head + i) % cap;
            self.buf[to] = self.buf[from].take();
            i += 1;
        }
        self.len -= 1;
        if self.len == 0 {
            self.head = 0;
        }
        value
    }

    /// Remove one hop matching `filter`, preserving the relative order of
    /// everything else (FIFO skip — non-matching hops are never dropped).
    pub(crate) fn take(&mut self, filter: WaitFilter) -> Option<Value> {
        let value = match filter {
            WaitFilter::Any => self.pop_front(),
            other => {
                let mut found = None;
                let cap = self.capacity();
                if cap == 0 {
                    return None;
                }
                for i in 0..self.len {
                    let idx = (self.head + i) % cap;
                    if let Some(v) = &self.buf[idx] {
                        if other.matches(v) {
                            found = Some(i);
                            break;
                        }
                    }
                }
                self.remove_at(found?)
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
            if !self.reserve_for_push(self.limit) {
                return Err(MailboxFullReason::MessageLimit);
            }
            self.bytes = self.bytes.saturating_add(cost);
            self.push_back(value);
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
                while (self.is_full() || self.would_exceed_bytes(cost)) && !self.is_empty() {
                    if let Some(old) = self.pop_front() {
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
                if !self.reserve_for_push(self.limit) {
                    return Err(MailboxFullReason::MessageLimit);
                }
                self.bytes = self.bytes.saturating_add(cost);
                self.push_back(value);
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
        while (self.is_full() || self.would_exceed_bytes(cost)) && !self.is_empty() {
            if let Some(old) = self.pop_front() {
                self.bytes = self.bytes.saturating_sub(old.memory_size());
            }
        }
        let cap = self.limit.max(self.len.saturating_add(1));
        let _ = self.reserve_for_push(cap);
        self.bytes = self.bytes.saturating_add(cost);
        self.push_back(value);
    }

    /// Physical growth: double current capacity, never past `capacity`.
    fn reserve_for_push(&mut self, capacity: usize) -> bool {
        if self.len < self.capacity() {
            return true;
        }
        let current = self.capacity();
        let next = current.max(1).saturating_mul(2).min(capacity);
        if next <= current {
            return self.len < capacity;
        }
        let mut new_buf = Vec::with_capacity(next);
        new_buf.resize_with(next, || None);
        if current > 0 {
            for (i, slot) in new_buf.iter_mut().enumerate().take(self.len) {
                let old_idx = (self.head + i) % current;
                *slot = self.buf[old_idx].take();
            }
        }
        self.buf = new_buf;
        self.head = 0;
        true
    }
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
        match msg.payload.as_int() {
            Some(i) => i,
            None => 0,
        }
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

    #[test]
    fn payload_kind_filter_skips_non_matching() {
        let mut q = MailboxQueue::new(8, ROOMY);
        let _ = q.enqueue(
            Value::Message(Message::new(1, 1, 1, Value::Int(1))),
            OverflowPolicy::Reject,
        );
        let _ = q.enqueue(
            Value::Message(Message::new(1, 2, 1, Value::str("hi"))),
            OverflowPolicy::Reject,
        );
        // Str wire tag = 7
        let got = q.take(WaitFilter::PayloadKind(7));
        assert!(matches!(
            got.as_ref().and_then(|v| v.as_message()),
            Some(m) if m.payload.as_str() == Some("hi")
        ));
        assert_eq!(q.len(), 1);
    }
}
