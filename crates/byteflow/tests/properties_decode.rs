//! Property-style decode / verify stress tests (in-house PRNG — no external crates).

use byteflow::prng::XorShift64;
use byteflow::{
    decode, decode_with, encode, verify, verify_with, Program, TrustLevel, VerifyConfig,
};

#[test]
fn decode_arbitrary_bytes_never_panics() {
    let mut rng = XorShift64::new(0xDEC0_0001);
    for _ in 0..128 {
        let len = rng.next_u32_inclusive(0, 511) as usize;
        let mut data = vec![0u8; len];
        rng.fill_bytes(&mut data);
        let _ = decode(&data);
        let _ = decode_with(&data, TrustLevel::Untrusted);
        let _ = decode_with(&data, TrustLevel::Trusted);
    }
}

#[test]
fn verify_after_decode_never_panics() {
    let mut rng = XorShift64::new(0xDEC0_0002);
    for _ in 0..128 {
        let len = rng.next_u32_inclusive(0, 511) as usize;
        let mut data = vec![0u8; len];
        rng.fill_bytes(&mut data);
        if let Ok(chunk) = decode_with(&data, TrustLevel::Trusted) {
            let _ = verify(&chunk);
            let _ = verify_with(
                &chunk,
                VerifyConfig {
                    trust: TrustLevel::Untrusted,
                },
            );
            let _ = verify_with(
                &chunk,
                VerifyConfig {
                    trust: TrustLevel::Trusted,
                },
            );
        }
    }
}

#[test]
fn encode_decode_roundtrip_preserves_verify() -> Result<(), String> {
    let mut rng = XorShift64::new(0xDEC0_0003);
    for _ in 0..64 {
        let imm = rng.next_i32();
        let mut program = Program::new("prop-roundtrip");
        program.function("main", 0, |f| {
            let r = f.load_i32(imm);
            f.return_(r);
        });
        let chunk = program.build();
        verify(&chunk).map_err(|e| e.to_string())?;
        let bytes = encode(&chunk);
        let decoded = decode(&bytes).map_err(|e| e.to_string())?;
        verify(&decoded).map_err(|e| e.to_string())?;
        if decoded.functions.len() != chunk.functions.len() {
            return Err("function count mismatch".into());
        }
        if decoded.constants.len() != chunk.constants.len() {
            return Err("constant count mismatch".into());
        }
    }
    Ok(())
}
