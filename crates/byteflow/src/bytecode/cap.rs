//! Capability model — Phase 3.
//!
//! A [`CapId`] is still the bytecode-visible token. Authorization lives in a
//! [`Cap`] record (target + rights + optional native mask + epoch).
//!
//! Design:
//! - Rights are a fixed bitset (no dynamic strings): cheap to check, cheap
//!   to attenuate, easy to audit.
//! - Every Cap carries an `epoch`. One revocation counter per issuer
//!   (flow, native table, scheduler) invalidates every derived Cap in O(1)
//!   without scanning the live-flow table.
//! - [`Cap::attenuate`] is the **only** way to derive a new Cap from an
//!   existing one. There is no constructor that lets a flow synthesize
//!   rights it does not already hold — that is where S7 closes: a
//!   [`NativeMask`] only shrinks, never grows, on any code path.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Unpredictable capability token. Never a flow id.
///
/// `0` is reserved as [`CapId::NONE`] (“no grant”, e.g. host hops without a
/// reply address). Minted ids are never zero.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CapId(u128);

/// Why [`CapId::random`] could not produce a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapIdError {
    /// Platform entropy unavailable (see [`crate::entropy`]).
    Entropy,
}

impl CapId {
    /// Placeholder used on unauthenticated / host-injected hops.
    pub const NONE: CapId = CapId(0);

    /// Draw a non-zero id from platform entropy (`std` only). Never panics.
    pub fn random() -> Result<Self, CapIdError> {
        for _ in 0..8 {
            let mut bytes = [0u8; 16];
            if crate::entropy::fill_bytes(&mut bytes).is_err() {
                return Err(CapIdError::Entropy);
            }
            let raw = u128::from_le_bytes(bytes);
            if raw != 0 {
                return Ok(CapId(raw));
            }
        }
        Err(CapIdError::Entropy)
    }

    #[inline]
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub const fn as_u128(self) -> u128 {
        self.0
    }

    /// Wire / trusted-decode path only. Does **not** insert into any table.
    #[inline]
    pub(crate) const fn from_raw(raw: u128) -> Self {
        CapId(raw)
    }
}

impl fmt::Debug for CapId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CapId({:032x})", self.0)
    }
}

impl fmt::Display for CapId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_none() {
            f.write_str("cap#none")
        } else {
            write!(f, "cap#{:032x}", self.0)
        }
    }
}

impl std::error::Error for CapIdError {}

impl fmt::Display for CapIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CapIdError::Entropy => f.write_str("capability CSPRNG unavailable"),
        }
    }
}

/// Index into a [`crate::NativeTable`].
pub type NativeIdx = u32;

/// Fixed-width rights bitset. Keep this an explicit allow-list: every new
/// bit here must be audited against the threat-model doc before production.
///
/// Low two bits stay `SEND` / `ASK` so ABI-era 0.9 addressing tokens remain
/// compatible (`SEND_ASK == 0b11`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct CapRights(u32);

impl CapRights {
    pub const NONE: CapRights = CapRights(0);
    pub const SEND: CapRights = CapRights(1 << 0);
    pub const ASK: CapRights = CapRights(1 << 1);
    pub const RECV: CapRights = CapRights(1 << 2);
    pub const SPAWN: CapRights = CapRights(1 << 3);
    pub const LINK: CapRights = CapRights(1 << 4);
    pub const MONITOR: CapRights = CapRights(1 << 5);
    pub const ADMIN: CapRights = CapRights(1 << 6);
    pub const NATIVE: CapRights = CapRights(1 << 7);

    /// Addressing grant minted on `SelfPid` / `Spawn` / `grant_cap`.
    pub const ADDRESSING: CapRights =
        CapRights(Self::SEND.0 | Self::ASK.0 | Self::LINK.0 | Self::MONITOR.0);

    /// Default child request from the high-level assembler (`Fn::spawn`).
    /// Does **not** include `ADMIN`. The raw opcode with `c = 0` is confined
    /// (`NONE`); the assembler writes this mask so existing actor samples
    /// keep working without an implicit second grant path.
    pub const FLOW: CapRights = CapRights(
        Self::SEND.0
            | Self::ASK.0
            | Self::RECV.0
            | Self::SPAWN.0
            | Self::LINK.0
            | Self::MONITOR.0
            | Self::NATIVE.0,
    );

    /// Host-spawned root authority (init). Same as [`Self::FLOW`] — `ADMIN`
    /// is never in the default set; the embedder mints it explicitly.
    pub const ROOT: CapRights = Self::FLOW;

    /// Historical alias: `SEND | ASK`.
    pub const SEND_ASK: CapRights = CapRights(Self::SEND.0 | Self::ASK.0);

    #[inline]
    pub const fn empty() -> Self {
        CapRights(0)
    }

    #[inline]
    pub const fn union(self, other: CapRights) -> CapRights {
        CapRights(self.0 | other.0)
    }

    #[inline]
    pub const fn intersect(self, other: CapRights) -> CapRights {
        CapRights(self.0 & other.0)
    }

    #[inline]
    pub const fn contains(self, other: CapRights) -> bool {
        self.0 & other.0 == other.0
    }

    #[inline]
    pub const fn is_subset_of(self, other: CapRights) -> bool {
        self.0 & !other.0 == 0
    }

    #[inline]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Low 8 bits for the `Spawn` / `Delegate` immediate operand.
    #[inline]
    pub const fn bits_u8(self) -> u8 {
        self.0 as u8
    }

    #[inline]
    pub const fn from_bits(bits: u32) -> Self {
        CapRights(bits)
    }

    #[inline]
    pub const fn from_u8(bits: u8) -> Self {
        CapRights(bits as u32)
    }
}

/// Bitset over `NativeTable` indices, sized once at boot to
/// `NativeTable::len()`. Shared via `Arc` because attenuation clones the Cap
/// far more often than it clones the mask.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeMask(Arc<[u64]>);

impl NativeMask {
    pub fn empty(native_count: usize) -> Self {
        let words = native_count.div_ceil(64);
        Self(Arc::from(vec![0u64; words].into_boxed_slice()))
    }

    pub fn full(native_count: usize) -> Self {
        let words = native_count.div_ceil(64);
        let mut v = vec![u64::MAX; words];
        // Clear tail bits above `native_count`, otherwise `intersect` is
        // imprecise near the upper bound.
        let rem = native_count % 64;
        if rem != 0 {
            if let Some(last) = v.last_mut() {
                *last &= (1u64 << rem) - 1;
            }
        }
        Self(Arc::from(v.into_boxed_slice()))
    }

    pub fn from_indices(native_count: usize, idxs: &[NativeIdx]) -> Self {
        let words = native_count.div_ceil(64);
        let mut v = vec![0u64; words];
        for &i in idxs {
            let (w, b) = (i as usize / 64, i as usize % 64);
            if w < v.len() && (i as usize) < native_count {
                v[w] |= 1u64 << b;
            }
        }
        Self(Arc::from(v.into_boxed_slice()))
    }

    pub fn word_len(&self) -> usize {
        self.0.len()
    }

    #[inline]
    pub fn allows(&self, idx: NativeIdx) -> bool {
        let (w, b) = (idx as usize / 64, idx as usize % 64);
        match self.0.get(w) {
            Some(word) => word & (1u64 << b) != 0,
            None => false,
        }
    }

    /// Attenuation primitive: result ⊆ self AND ⊆ requested, always.
    /// Bitwise AND — there is no OR path on this type.
    pub fn intersect(&self, requested: &NativeMask) -> NativeMask {
        let n = self.0.len().min(requested.0.len());
        let words: Vec<u64> = self
            .0
            .iter()
            .zip(requested.0.iter())
            .take(n)
            .map(|(a, b)| a & b)
            .collect();
        // If lengths differ, zip already truncated; pad remaining from the
        // shorter side is all zeros under AND — nothing to keep.
        NativeMask(Arc::from(words.into_boxed_slice()))
    }

    /// `true` iff every bit set in `self` is also set in `other`.
    pub fn is_subset_of(&self, other: &NativeMask) -> bool {
        let n = self.0.len().max(other.0.len());
        (0..n).all(|idx| {
            let a = match self.0.get(idx) {
                Some(w) => *w,
                None => 0,
            };
            let b = match other.0.get(idx) {
                Some(w) => *w,
                None => 0,
            };
            a & !b == 0
        })
    }
}

/// What a capability addresses. Bytecode never sees this enum — only the
/// opaque [`CapId`] token.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum CapTarget {
    /// Delivery / link / monitor / self-authority for one flow (`u64` = FlowId).
    Flow(u64),
    /// Native-table authority (S7). Rarely minted; usually the mask lives on
    /// a flow-targeted Cap that also has [`CapRights::NATIVE`].
    NativeTable,
    /// Exclusive target of [`CapRights::ADMIN`]: kill / inspect / quota top-up.
    Scheduler,
}

impl CapTarget {
    pub fn flow_id(self) -> Option<u64> {
        match self {
            CapTarget::Flow(id) => Some(id),
            CapTarget::NativeTable | CapTarget::Scheduler => None,
        }
    }
}

/// Authorization record stored in the runtime Cap table, never in bytecode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cap {
    pub target: CapTarget,
    pub rights: CapRights,
    /// Meaningful only when `rights.contains(NATIVE)`.
    pub native_mask: Option<NativeMask>,
    /// Incremented by [`RevocationCell::revoke`]. A Cap is valid iff
    /// `self.epoch == issuer.epoch()`.
    epoch: u64,
}

/// One revocation counter per capability issuer (a flow, the native table,
/// the scheduler). Revoke is O(1); every derived Cap dies with it.
#[derive(Debug)]
pub struct RevocationCell(AtomicU64);

impl RevocationCell {
    pub fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    pub fn epoch(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }

    pub fn revoke(&self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

impl Default for RevocationCell {
    fn default() -> Self {
        Self::new()
    }
}

impl Cap {
    /// Trusted root mint (runtime / host only). Bytecode cannot call this.
    pub fn root(
        target: CapTarget,
        rights: CapRights,
        native_mask: Option<NativeMask>,
        cell: &RevocationCell,
    ) -> Self {
        Self {
            target,
            rights,
            native_mask,
            epoch: cell.epoch(),
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn is_valid(&self, cell: &RevocationCell) -> bool {
        self.epoch == cell.epoch()
    }

    /// Produce a strictly narrower-or-equal Cap. Never panics, never grants
    /// a bit the parent did not have. This is the single choke point the
    /// rest of the model sits on — keep it boring and obvious.
    ///
    /// # Invariants
    ///
    /// - `rights(result) ⊆ rights(self)` (`intersect`)
    /// - `native(result) ⊆ native(self)`:
    ///   - `(Some(src), Some(want))` → `src ∩ want`
    ///   - `(Some(src), None)` → keep `src` (no native change requested)
    ///   - `(None, _)` → `None` (cannot invent a mask)
    /// - `result.epoch == self.epoch` — same [`RevocationCell`]; revoke
    ///   invalidates the whole derivation chain. No fresh cell is minted.
    pub fn attenuate(&self, want_rights: CapRights, want_native: Option<&NativeMask>) -> Cap {
        let rights = self.rights.intersect(want_rights);
        let native_mask = match (&self.native_mask, want_native) {
            (Some(cur), Some(req)) => Some(cur.intersect(req)),
            (Some(cur), None) => Some(cur.clone()),
            (None, _) => None,
        };
        Cap {
            target: self.target,
            rights,
            native_mask,
            epoch: self.epoch,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_is_zero_and_random_is_not() -> Result<(), CapIdError> {
        assert!(CapId::NONE.is_none());
        let a = CapId::random()?;
        let b = CapId::random()?;
        assert!(!a.is_none());
        assert!(!b.is_none());
        assert_ne!(a, b);
        Ok(())
    }

    #[test]
    fn from_raw_is_crate_internal() {
        assert_eq!(CapId::from_raw(0), CapId::NONE);
        assert!(!CapId::from_raw(1).is_none());
    }

    #[test]
    fn send_ask_bits_stay_compatible() {
        assert_eq!(CapRights::SEND.bits(), 0b01);
        assert_eq!(CapRights::ASK.bits(), 0b10);
        assert_eq!(CapRights::SEND_ASK.bits(), 0b11);
        assert!(CapRights::ADMIN.bits() > 0b11);
    }

    #[test]
    fn attenuation_never_escalates() {
        let cell = RevocationCell::new();
        let nm = NativeMask::from_indices(128, &[3, 5, 9]);
        let parent = Cap::root(
            CapTarget::Flow(1),
            CapRights::SEND.union(CapRights::NATIVE),
            Some(nm),
            &cell,
        );

        let child = parent.attenuate(CapRights::SEND.union(CapRights::ADMIN), None);
        assert!(!child.rights.contains(CapRights::ADMIN));
        assert!(child.rights.contains(CapRights::SEND));

        let wide = NativeMask::full(128);
        let child2 = parent.attenuate(CapRights::NATIVE, Some(&wide));
        let child_mask = child2.native_mask.as_ref();
        assert!(child_mask.is_some_and(|m| m.allows(3)));
        let parent_mask = parent.native_mask.as_ref();
        assert!(parent_mask.is_some_and(|m| !m.allows(7)));
    }

    #[test]
    fn attenuation_is_subset_for_all_byte_masks() {
        let cell = RevocationCell::new();
        for src in 0u32..=255 {
            for want in 0u32..=255 {
                let parent = Cap::root(CapTarget::Flow(1), CapRights::from_bits(src), None, &cell);
                let child = parent.attenuate(CapRights::from_bits(want), None);
                assert!(
                    child.rights.is_subset_of(parent.rights),
                    "escalation src={src:#x} want={want:#x} got={:#x}",
                    child.rights.bits()
                );
                assert_eq!(child.rights.bits(), src & want);
            }
        }
    }

    #[test]
    fn chained_delegate_never_gains_bits() {
        let cell = RevocationCell::new();
        let mut cap = Cap::root(
            CapTarget::Flow(1),
            CapRights::FLOW,
            Some(NativeMask::from_indices(32, &[0, 1, 2])),
            &cell,
        );
        let adversarial = [
            CapRights::ADMIN,
            CapRights::NATIVE.union(CapRights::ADMIN),
            CapRights::from_bits(u32::MAX),
            CapRights::NONE,
            CapRights::FLOW,
        ];
        for want in adversarial {
            cap = cap.attenuate(want, Some(&NativeMask::full(32)));
            assert!(cap.rights.is_subset_of(CapRights::FLOW));
            assert!(!cap.rights.contains(CapRights::ADMIN));
            if let Some(mask) = &cap.native_mask {
                assert!(!mask.allows(7));
            }
        }
    }

    #[test]
    fn revocation_kills_all_derived_caps() {
        let cell = RevocationCell::new();
        let root = Cap::root(CapTarget::Flow(1), CapRights::SEND, None, &cell);
        let child = root.attenuate(CapRights::SEND, None);
        assert!(child.is_valid(&cell));
        cell.revoke();
        assert!(!child.is_valid(&cell));
        assert!(!root.is_valid(&cell));
    }
}
