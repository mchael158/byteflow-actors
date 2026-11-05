//! Delegation, attenuation, and revocation — fail-closed at the Cap layer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use byteflow::{Cap, CapRights, CapTarget, NativeMask, RevocationCell};

#[test]
fn chained_attenuation_never_adds_admin() {
    let cell = RevocationCell::new();
    let root = Cap::root(
        CapTarget::Flow(1),
        CapRights::SEND.union(CapRights::FLOW),
        Some(NativeMask::from_indices(32, &[0, 1])),
        &cell,
    );
    let mid = root.attenuate(CapRights::from_bits(u32::MAX), Some(&NativeMask::full(32)));
    let leaf = mid.attenuate(CapRights::ADMIN.union(CapRights::SEND), None);
    assert!(!leaf.rights().contains(CapRights::ADMIN));
    assert!(leaf.rights().is_subset_of(root.rights()));
    if let Some(mask) = leaf.native_mask() {
        assert!(!mask.allows(31));
    }
}

#[test]
fn revoking_issuer_invalidates_every_derived_cap() {
    let cell = RevocationCell::new();
    let root = Cap::root(CapTarget::Flow(9), CapRights::SEND, None, &cell);
    let a = root.attenuate(CapRights::SEND, None);
    let b = a.attenuate(CapRights::SEND, None);
    assert!(b.is_valid(&cell));
    cell.revoke();
    assert!(!root.is_valid(&cell));
    assert!(!a.is_valid(&cell));
    assert!(!b.is_valid(&cell));
}

#[test]
fn concurrent_revoke_races_still_fail_closed() -> Result<(), String> {
    for _ in 0..32 {
        let cell = Arc::new(RevocationCell::new());
        let cap = Cap::root(CapTarget::Flow(3), CapRights::SEND, None, cell.as_ref());
        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();
        for _ in 0..6 {
            let cell = Arc::clone(&cell);
            let worker_cap = cap.clone();
            let stop = Arc::clone(&stop);
            handles.push(thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    let _ = worker_cap.is_valid(cell.as_ref());
                }
            }));
        }
        cell.revoke();
        stop.store(true, Ordering::Release);
        for handle in handles {
            handle.join().map_err(|_| "worker panicked".to_string())?;
        }
        if cap.is_valid(cell.as_ref()) {
            return Err("revoked cap must not validate".into());
        }
    }
    Ok(())
}
