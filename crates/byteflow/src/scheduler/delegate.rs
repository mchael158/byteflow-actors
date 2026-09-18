//! Opcode DELEGATE — attenuation as a first-class ISA operation.
//!
//! `DELEGATE dst, src_cap, rights_mask, native_mask_cap?`
//!
//! # Authority path (do not bypass)
//!
//! ```text
//! Opcode::Delegate
//!        │
//!        ▼
//! exec_delegate()          ← revocation + NATIVE gate
//!        │
//!        ▼
//! Cap::attenuate()         ← rights ∩ + native ∩ ; same epoch
//!        │
//!        ▼
//! CapTable insert → Value::Cap
//! ```
//!
//! The ISA requests an operation; [`Cap::attenuate`] is the only place that
//! transforms authority. There is **no separate grant** — only intersection.
//!
//! # Formal invariants
//!
//! For every successful `DELEGATE(src, want_rights, want_native)`:
//!
//! - `rights(result) ⊆ rights(src)`
//! - `native(result) ⊆ native(src)` (see [`Cap::attenuate`])
//! - never `rights(result) ⊃ rights(src)` nor `native(result) ⊃ native(src)`
//!
//! If `src` is revoked at the check (or becomes revoked before use):
//! `Err(SourceRevoked)`. A Cap produced mid-revoke still carries the old
//! epoch and is immediately invalid against the bumped cell — fail-closed.
//!
//! # `want_native` vs `NATIVE`
//!
//! `want_native = Some(...)` means “attenuate the native mask”, **not**
//! “grant NATIVE”. Specifying a mask without holding [`CapRights::NATIVE`]
//! is rejected as [`DelegateError::SourceLacksNative`].

use crate::bytecode::{Cap, CapRights, NativeMask, RevocationCell};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegateError {
    SourceRevoked,
    /// Caller passed `want_native = Some(...)` but `src` lacks
    /// [`CapRights::NATIVE`]. Specifying a native attenuation requires that
    /// right; it does not invent native authority.
    SourceLacksNative,
}

impl std::fmt::Display for DelegateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DelegateError::SourceRevoked => f.write_str("delegate: source capability revoked"),
            DelegateError::SourceLacksNative => f.write_str(
                "delegate: cannot specify native-mask attenuation without NATIVE right",
            ),
        }
    }
}

impl std::error::Error for DelegateError {}

/// Security gate for `Opcode::Delegate`. All authority math is
/// [`Cap::attenuate`]; this wrapper only enforces revocation and the
/// native-mask precondition.
pub fn exec_delegate(
    src: &Cap,
    src_cell: &RevocationCell,
    want_rights: CapRights,
    want_native: Option<&NativeMask>,
) -> Result<Cap, DelegateError> {
    if !src.is_valid(src_cell) {
        return Err(DelegateError::SourceRevoked);
    }
    if want_native.is_some() && !src.rights.contains(CapRights::NATIVE) {
        return Err(DelegateError::SourceLacksNative);
    }
    Ok(src.attenuate(want_rights, want_native))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{CapTarget, NativeMask, RevocationCell};

    #[test]
    fn cannot_escalate_via_delegate_regardless_of_requested_mask() {
        let cell = RevocationCell::new();
        let src = Cap::root(CapTarget::Flow(1), CapRights::SEND, None, &cell);
        let out = match exec_delegate(
            &src,
            &cell,
            CapRights::SEND.union(CapRights::ADMIN).union(CapRights::NATIVE),
            None,
        ) {
            Ok(c) => c,
            Err(err) => panic!("delegate of live SEND cap must succeed: {err}"),
        };
        assert!(out.rights.contains(CapRights::SEND));
        assert!(!out.rights.contains(CapRights::ADMIN));
        assert!(!out.rights.contains(CapRights::NATIVE));
        assert!(out.rights.is_subset_of(src.rights));
    }

    #[test]
    fn rights_result_is_always_intersection() {
        let cell = RevocationCell::new();
        for src_bits in 0u32..=255 {
            for want_bits in 0u32..=255 {
                let src = Cap::root(
                    CapTarget::Flow(1),
                    CapRights::from_bits(src_bits),
                    None,
                    &cell,
                );
                let out = match exec_delegate(
                    &src,
                    &cell,
                    CapRights::from_bits(want_bits),
                    None,
                ) {
                    Ok(c) => c,
                    Err(err) => panic!("live cap must delegate: {err}"),
                };
                assert_eq!(out.rights.bits(), src_bits & want_bits);
                assert!(out.rights.is_subset_of(src.rights));
            }
        }
    }

    #[test]
    fn native_mask_can_only_be_narrowed() {
        let cell = RevocationCell::new();
        let src_mask = NativeMask::from_indices(64, &[1, 3, 5, 7]);
        let requested = NativeMask::from_indices(64, &[3, 5, 9, 11]); // 9,11 not in src
        let src = Cap::root(
            CapTarget::Flow(1),
            CapRights::NATIVE,
            Some(src_mask.clone()),
            &cell,
        );
        let out = match exec_delegate(&src, &cell, CapRights::NATIVE, Some(&requested)) {
            Ok(cap) => cap,
            Err(err) => panic!("live NATIVE capability should delegate: {err}"),
        };
        let got = out.native_mask.as_ref().expect("native mask present");
        assert!(got.is_subset_of(&src_mask));
        assert!(got.allows(3));
        assert!(got.allows(5));
        assert!(!got.allows(1)); // dropped by request
        assert!(!got.allows(9)); // never in source
        assert!(!got.allows(11));
    }

    #[test]
    fn cannot_delegate_native_from_non_native_source() {
        let cell = RevocationCell::new();
        let src = Cap::root(CapTarget::Flow(1), CapRights::SEND, None, &cell);
        let requested = NativeMask::from_indices(32, &[0, 1]);
        assert!(matches!(
            exec_delegate(&src, &cell, CapRights::SEND, Some(&requested)),
            Err(DelegateError::SourceLacksNative)
        ));
    }

    #[test]
    fn want_native_none_keeps_source_mask() {
        let cell = RevocationCell::new();
        let src_mask = NativeMask::from_indices(32, &[2, 4]);
        let src = Cap::root(
            CapTarget::Flow(1),
            CapRights::NATIVE.union(CapRights::SEND),
            Some(src_mask.clone()),
            &cell,
        );
        let out = match exec_delegate(&src, &cell, CapRights::SEND, None) {
            Ok(c) => c,
            Err(err) => panic!("{err}"),
        };
        // SEND-only result still inherits the (unchanged) mask payload;
        // NATIVE bit was intersected away.
        assert!(!out.rights.contains(CapRights::NATIVE));
        let got = out.native_mask.as_ref().expect("mask preserved");
        assert!(got.is_subset_of(&src_mask));
        assert_eq!(got.allows(2), src_mask.allows(2));
        assert_eq!(got.allows(4), src_mask.allows(4));
    }

    #[test]
    fn delegated_cap_shares_revocation_cell_epoch() {
        let cell = RevocationCell::new();
        let src = Cap::root(
            CapTarget::Flow(1),
            CapRights::SEND.union(CapRights::NATIVE),
            Some(NativeMask::from_indices(8, &[0])),
            &cell,
        );
        let child = match exec_delegate(&src, &cell, CapRights::SEND, None) {
            Ok(c) => c,
            Err(err) => panic!("{err}"),
        };
        assert_eq!(child.epoch(), src.epoch());
        assert!(child.is_valid(&cell));
        cell.revoke();
        assert!(!src.is_valid(&cell));
        assert!(!child.is_valid(&cell));
    }

    #[test]
    fn revoked_source_cannot_delegate() {
        let cell = RevocationCell::new();
        let src = Cap::root(CapTarget::Flow(1), CapRights::SEND, None, &cell);
        cell.revoke();
        assert!(matches!(
            exec_delegate(&src, &cell, CapRights::SEND, None),
            Err(DelegateError::SourceRevoked)
        ));
    }
}
