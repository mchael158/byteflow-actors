/// Per-mailbox counters (Relaxed atomics would over-synchronize a hot
/// inbox). Updated only while the mailbox mutex is held — same critical
/// section as enqueue / park — then copied out for host scrapes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MailboxStats {
    pub enqueued: u64,
    pub dequeued: u64,
    /// Total refusals under [`super::OverflowPolicy::Reject`], whichever
    /// bound was hit.
    pub rejected: u64,
    /// Subset of [`Self::rejected`] caused by the byte budget rather than
    /// the hop count. `rejected - rejected_byte_limit` is therefore the
    /// hop-count share — the two have different remedies, so an operator
    /// needs to tell them apart.
    pub rejected_byte_limit: u64,
    pub dropped_newest: u64,
    pub dropped_oldest: u64,
    /// Hops queued **right now** (occupancy, not a counter).
    pub queued_messages: usize,
    /// Bytes charged to the queued hops right now. Compare against the
    /// configured [`super::MailboxBytes`] to see how close an inbox is to
    /// its budget before it starts refusing.
    pub queued_bytes: usize,
}
