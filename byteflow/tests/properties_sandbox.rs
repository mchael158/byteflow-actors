//! Property-style sandbox invariants (in-house PRNG — no extra crates).

use std::sync::Arc;
use std::thread;

use byteflow::prng::XorShift64;
use byteflow::{
    decode_with, verify_with, Cap, CapRights, CapTarget, MemoryBudget, RevocationCell, TrustLevel,
    VerifyConfig,
};

#[test]
fn used_memory_never_exceeds_limit_under_contention() -> Result<(), String> {
    let budget = Arc::new(MemoryBudget::new(2_048));
    let mut handles = Vec::new();
    for seed in 0..8u64 {
        let budget = Arc::clone(&budget);
        handles.push(thread::spawn(move || {
            let mut rng = XorShift64::new(0xBEEF_0000 ^ seed);
            for _ in 0..400 {
                let n = rng.next_u32_inclusive(1, 32) as usize;
                if budget.try_charge(n).is_ok() {
                    budget.release(n / 2 + 1);
                }
            }
        }));
    }
    for h in handles {
        h.join().map_err(|_| "worker".to_string())?;
    }
    assert!(budget.used() <= budget.limit());
    Ok(())
}

#[test]
fn delegated_rights_are_always_subset() {
    let cell = RevocationCell::new();
    let mut rng = XorShift64::new(0xCA95_0001);
    for _ in 0..256 {
        let parent_bits = rng.next_u32();
        let want_bits = rng.next_u32();
        let parent = Cap::root(
            CapTarget::Flow(1),
            CapRights::from_bits(parent_bits),
            None,
            &cell,
        );
        let child = parent.attenuate(CapRights::from_bits(want_bits), None);
        assert!(child.rights().is_subset_of(parent.rights()));
        assert_eq!(child.rights().bits(), parent_bits & want_bits);
    }
}

#[test]
fn revoked_capability_denies_authorization() {
    let cell = RevocationCell::new();
    let cap = Cap::root(CapTarget::Flow(9), CapRights::SEND, None, &cell);
    assert!(cap.is_valid(&cell));
    cell.revoke();
    assert!(!cap.is_valid(&cell));
}

#[test]
fn untrusted_and_restricted_reject_authority_injection() {
    let mut rng = XorShift64::new(0xBADC_0FFE);
    for _ in 0..64 {
        let len = rng.next_u32_inclusive(0, 128) as usize;
        let mut data = vec![0u8; len];
        rng.fill_bytes(&mut data);
        for trust in [TrustLevel::Untrusted, TrustLevel::Restricted] {
            if let Ok(chunk) = decode_with(&data, TrustLevel::Trusted) {
                let _ = verify_with(&chunk, VerifyConfig { trust });
            }
            let _ = decode_with(&data, trust);
        }
    }
}
