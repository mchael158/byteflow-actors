//! Host-side bytecode **integrity fingerprint** (Phase 4).
//!
//! [`fingerprint_bf`] is a deterministic 32-byte digest over a `.bf` buffer.
//! It is an **integrity fingerprint**, not cryptographic attestation: it does
//! not use a secret key, MAC, or signature scheme. Hosts that want signed
//! module attestation must layer that on top (non-goal for Byteflow itself).

use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};

use crate::bytecode::{decode, Chunk, FormatError};

/// Fixed domain separator mixed into every round (not a secret).
const FINGERPRINT_DOMAIN: u64 = 0xBF01_464C_4F57_7630; // "BF01FLOW v0"

/// Deterministic 32-byte integrity fingerprint of a BFV0 module buffer.
///
/// Built by expanding several [`DefaultHasher`] rounds (SipHash-1-3 with the
/// hasher's fixed constructor keys) into 32 bytes. Suitable for host-side
/// "did this buffer change?" checks — **not** for cryptographic attestation.
pub fn fingerprint_bf(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (lane, chunk) in out.chunks_mut(8).enumerate() {
        let mut state = mix_lane(bytes, lane as u64, 0);
        for round in 1u64..=4 {
            state = mix_lane(bytes, lane as u64, round ^ state);
        }
        chunk.copy_from_slice(&state.to_le_bytes());
    }
    out
}

fn mix_lane(bytes: &[u8], lane: u64, round: u64) -> u64 {
    let mut hasher = DefaultHasher::new();
    FINGERPRINT_DOMAIN.hash(&mut hasher);
    lane.hash(&mut hasher);
    round.hash(&mut hasher);
    (bytes.len() as u64).hash(&mut hasher);
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Failure of [`decode_attested`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttestError {
    /// Buffer fingerprint did not match the expected digest.
    DigestMismatch {
        expected: [u8; 32],
        actual: [u8; 32],
    },
    /// Fingerprint matched but BFV0 decode failed (`TrustLevel::Untrusted`).
    Decode(String),
}

impl fmt::Display for AttestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AttestError::DigestMismatch { expected, actual } => {
                write!(
                    f,
                    "bytecode fingerprint mismatch: expected {}, actual {}",
                    hex32(expected),
                    hex32(actual)
                )
            }
            AttestError::Decode(msg) => write!(f, "attested decode failed: {msg}"),
        }
    }
}

impl std::error::Error for AttestError {}

fn hex32(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Fingerprint `bytes`, compare to `expected`, then [`decode`] (untrusted).
///
/// Digest is checked **before** decode so a wrong expected digest never
/// spends work on a hostile buffer beyond the fingerprint pass.
pub fn decode_attested(bytes: &[u8], expected: &[u8; 32]) -> Result<Chunk, AttestError> {
    let actual = fingerprint_bf(bytes);
    if &actual != expected {
        return Err(AttestError::DigestMismatch {
            expected: *expected,
            actual,
        });
    }
    decode(bytes).map_err(|e: FormatError| AttestError::Decode(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{encode, Program};

    #[test]
    fn roundtrip_fingerprint_then_decode_attested() -> Result<(), AttestError> {
        let mut program = Program::new("attest-demo");
        program.function("main", 0, |f| {
            let a = f.load_i32(41);
            let b = f.load_i32(1);
            let sum = f.add(a, b);
            f.return_(sum);
        });
        let chunk = program.build();
        let bytes = encode(&chunk);
        let digest = fingerprint_bf(&bytes);
        let decoded = decode_attested(&bytes, &digest)?;
        assert_eq!(decoded.name, chunk.name);
        assert_eq!(decoded.code.len(), chunk.code.len());
        Ok(())
    }

    #[test]
    fn wrong_digest_fails_before_decode() {
        let mut program = Program::new("attest-bad");
        program.function("main", 0, |f| {
            let v = f.load_i32(1);
            f.return_(v);
        });
        let bytes = encode(&program.build());
        let mut bad = fingerprint_bf(&bytes);
        bad[0] ^= 0xFF;
        match decode_attested(&bytes, &bad) {
            Err(AttestError::DigestMismatch { .. }) => {}
            other => panic!("expected DigestMismatch, got {other:?}"),
        }
    }

    #[test]
    fn fingerprint_is_deterministic() {
        let bytes = b"BFV0-not-a-real-module-but-stable";
        assert_eq!(fingerprint_bf(bytes), fingerprint_bf(bytes));
    }
}
