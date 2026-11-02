/// Logical hop bound for one flow's inbox.
///
/// # Why not `usize`
///
/// A raw `capacity: usize` lets a 64-bit host pass an absurd value and
/// pretend it is a memory contract. An embeddable M:N runtime must pick
/// deliberate bounds: enough hops for request/reply bursts, not enough to
/// grow RSS until OOM when a slow consumer sits under a fast producer.
///
/// `MIN = 1` (a mailbox that cannot hold a hop is not a mailbox).
/// `MAX = 1 << 20` (~1M hops) is a hard ceiling, not a recommendation —
/// the default is [`MailboxCapacity::DEFAULT`] (256).
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MailboxCapacity(u32);

impl MailboxCapacity {
    pub const MIN: u32 = 1;
    pub const MAX: u32 = 1 << 20;
    /// Default logical bound used by [`super::MailboxConfig::DEFAULT`].
    pub const DEFAULT: MailboxCapacity = MailboxCapacity(256);

    /// `Some` iff `value` is in `MIN..=MAX`.
    pub const fn new(value: u32) -> Option<Self> {
        if value < Self::MIN || value > Self::MAX {
            None
        } else {
            Some(Self(value))
        }
    }

    #[inline]
    pub const fn get(self) -> usize {
        self.0 as usize
    }

    #[inline]
    pub const fn get_u32(self) -> u32 {
        self.0
    }
}

impl Default for MailboxCapacity {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Byte budget for one flow's inbox, enforced alongside
/// [`MailboxCapacity`].
///
/// # Why a hop count is not a memory bound
///
/// [`MailboxCapacity`] bounds *how many* hops an inbox holds. Since ABI v4
/// a hop may carry [`crate::Value::Str`] / [`crate::Value::Bytes`], and the
/// `.bf` decoder accepts blobs up to 1 MiB, so "256 hops" is anywhere from
/// ~12 KiB of scalars to ~256 MiB of blobs. A count alone therefore does
/// not bound RSS — which is the whole point of bounding the mailbox.
///
/// Cost per hop comes from [`crate::Value::memory_size`]; read its doc for
/// why `Arc`-shared payloads are deliberately over-charged.
///
/// # Choosing the default
///
/// `DEFAULT` is 4 MiB, which must stay **strictly greater** than the
/// largest single hop the `.bf` decoder will produce (`MAX_BLOB` in
/// [`crate::decode`]'s module, 1 MiB) plus its envelope. A budget equal to
/// the maximum payload size would make a legal, decodable constant
/// permanently undeliverable under the default config — a bound should
/// refuse abuse, not valid inputs. `byte_budget_default_fits_one_max_hop`
/// guards that relationship.
///
/// `MIN = 1 KiB` (an inbox that cannot hold a handful of scalar hops is
/// not an inbox). `MAX = 1 GiB` is a hard ceiling, not a recommendation.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MailboxBytes(usize);

impl MailboxBytes {
    pub const MIN: usize = 1 << 10;
    pub const MAX: usize = 1 << 30;
    /// Default byte budget used by [`super::MailboxConfig::DEFAULT`]:
    /// 4 MiB. See the type doc for why it is not 1 MiB.
    pub const DEFAULT: MailboxBytes = MailboxBytes(1 << 22);

    /// `Some` iff `value` is in `MIN..=MAX`.
    pub const fn new(value: usize) -> Option<Self> {
        if value < Self::MIN || value > Self::MAX {
            None
        } else {
            Some(Self(value))
        }
    }

    #[inline]
    pub const fn get(self) -> usize {
        self.0
    }
}

impl Default for MailboxBytes {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_and_over_max() -> Result<(), &'static str> {
        assert!(MailboxCapacity::new(0).is_none());
        assert!(MailboxCapacity::new(MailboxCapacity::MAX + 1).is_none());
        let min = MailboxCapacity::new(1).ok_or("min capacity")?;
        assert_eq!(min.get(), 1);
        let max = MailboxCapacity::new(MailboxCapacity::MAX).ok_or("max capacity")?;
        assert_eq!(max.get_u32(), MailboxCapacity::MAX);
        Ok(())
    }

    #[test]
    fn default_is_compile_time_valid() {
        assert_eq!(MailboxCapacity::DEFAULT.get(), 256);
        assert!(MailboxCapacity::new(256).is_some());
    }

    #[test]
    fn byte_budget_rejects_out_of_range() -> Result<(), &'static str> {
        assert!(MailboxBytes::new(0).is_none());
        assert!(MailboxBytes::new(MailboxBytes::MIN - 1).is_none());
        assert!(MailboxBytes::new(MailboxBytes::MAX + 1).is_none());
        let min = MailboxBytes::new(MailboxBytes::MIN).ok_or("min bytes")?;
        assert_eq!(min.get(), MailboxBytes::MIN);
        Ok(())
    }

    #[test]
    fn byte_budget_default_holds_a_full_scalar_inbox() {
        // DEFAULT must not reject a mailbox filled to DEFAULT capacity with
        // scalar hops, or the byte bound would shadow the hop bound.
        let worst = MailboxCapacity::DEFAULT.get() * std::mem::size_of::<crate::Value>();
        assert!(MailboxBytes::DEFAULT.get() > worst);
    }

    #[test]
    fn byte_budget_default_fits_one_max_hop() {
        // `MAX_BLOB` in `bytecode::format` (private) caps a decoded
        // `Str`/`Bytes` constant at 1 MiB. The default budget must fit one
        // of those *plus* its envelope, or the decoder would happily accept
        // a constant that no mailbox could ever receive.
        const MAX_BLOB: usize = 1_048_576;
        let biggest = crate::Value::bytes(vec![0u8; MAX_BLOB]);
        assert!(biggest.memory_size() <= MailboxBytes::DEFAULT.get());
    }
}
