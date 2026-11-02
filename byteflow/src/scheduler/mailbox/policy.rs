/// What happens when a hop would exceed the mailbox's **logical** capacity.
///
/// # Why there is no `Block`
///
/// Parking the **OS worker** on a full mailbox would stall every other flow
/// on that thread — the opposite of an M:N scheduler. Backpressure that
/// waits belongs to scheduler-level `WAITING_SEND` (one parked sender
/// woken per freed slot), not to a queue method that holds a worker.
///
/// This revision therefore only offers **non-blocking** overflow:
///
/// | Policy | Effect |
/// |--------|--------|
/// | [`Reject`](Self::Reject) | Hop is not accepted; caller sees [`super::MailboxFull`] / [`crate::SendError::MailboxFull`] |
/// | [`DropNewest`](Self::DropNewest) | Incoming hop discarded; queue unchanged |
/// | [`DropOldest`](Self::DropOldest) | Oldest queued hop dropped, incoming enqueued (still FIFO among survivors) |
///
/// Default is [`Reject`](Self::Reject): fail-closed. Silent drop is an
/// explicit embedder choice.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OverflowPolicy {
    #[default]
    Reject = 0,
    DropNewest = 1,
    DropOldest = 2,
}

/// Memory + overflow contract for every mailbox spawned by a [`crate::Runtime`].
///
/// Fields are private so an out-of-range capacity or byte budget cannot
/// sneak in. Construct with [`MailboxConfig::new`] or
/// [`MailboxConfig::DEFAULT`], then narrow the byte budget with
/// [`MailboxConfig::with_bytes`].
///
/// Both bounds apply; the first one reached refuses the hop. See
/// [`super::MailboxBytes`] for why the hop count alone is not a memory
/// bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MailboxConfig {
    capacity: super::MailboxCapacity,
    bytes: super::MailboxBytes,
    overflow: OverflowPolicy,
}

impl MailboxConfig {
    /// Compile-time-valid default: 256 hops, 4 MiB, reject on overflow.
    pub const DEFAULT: MailboxConfig = MailboxConfig {
        capacity: super::MailboxCapacity::DEFAULT,
        bytes: super::MailboxBytes::DEFAULT,
        overflow: OverflowPolicy::Reject,
    };

    /// Capacity + policy, with the default byte budget.
    ///
    /// The byte budget is a separate builder step rather than a third
    /// parameter so embedders that only care about hop count are not
    /// forced to reason about bytes to keep compiling.
    pub const fn new(capacity: super::MailboxCapacity, overflow: OverflowPolicy) -> Self {
        Self {
            capacity,
            bytes: super::MailboxBytes::DEFAULT,
            overflow,
        }
    }

    /// Replace the byte budget.
    pub const fn with_bytes(self, bytes: super::MailboxBytes) -> Self {
        Self { bytes, ..self }
    }

    #[inline]
    pub const fn capacity(&self) -> super::MailboxCapacity {
        self.capacity
    }

    #[inline]
    pub const fn bytes(&self) -> super::MailboxBytes {
        self.bytes
    }

    #[inline]
    pub const fn overflow(&self) -> OverflowPolicy {
        self.overflow
    }
}

impl Default for MailboxConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::mailbox::{MailboxBytes, MailboxCapacity};

    #[test]
    fn default_is_reject_256() {
        let c = MailboxConfig::DEFAULT;
        assert_eq!(c.capacity().get(), 256);
        assert_eq!(c.bytes().get(), MailboxBytes::DEFAULT.get());
        assert_eq!(c.overflow(), OverflowPolicy::Reject);
    }

    #[test]
    fn new_preserves_parts() -> Result<(), &'static str> {
        let cap = MailboxCapacity::new(4).ok_or("cap")?;
        let c = MailboxConfig::new(cap, OverflowPolicy::DropOldest);
        assert_eq!(c.capacity().get(), 4);
        assert_eq!(c.overflow(), OverflowPolicy::DropOldest);
        assert_eq!(c.bytes().get(), MailboxBytes::DEFAULT.get());
        Ok(())
    }

    #[test]
    fn with_bytes_narrows_only_the_budget() -> Result<(), &'static str> {
        let cap = MailboxCapacity::new(4).ok_or("cap")?;
        let budget = MailboxBytes::new(8192).ok_or("bytes")?;
        let c = MailboxConfig::new(cap, OverflowPolicy::DropOldest).with_bytes(budget);
        assert_eq!(c.bytes().get(), 8192);
        assert_eq!(c.capacity().get(), 4);
        assert_eq!(c.overflow(), OverflowPolicy::DropOldest);
        Ok(())
    }
}
