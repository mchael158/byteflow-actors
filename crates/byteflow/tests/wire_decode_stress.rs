//! Random bytes must not panic wire or BFH1 decode paths.

use byteflow::prng::XorShift64;
use byteflow::{decode_frame, read_wire_frame, DEFAULT_MAX_WIRE_BYTES};

#[test]
fn read_wire_frame_never_panics_on_arbitrary_bytes() {
    let mut rng = XorShift64::new(0x0123_4567_89AB_CDEF);
    for _ in 0..256 {
        let len = rng.next_u32_inclusive(0, 512) as usize;
        let mut data = vec![0u8; len];
        rng.fill_bytes(&mut data);
        let mut cursor = std::io::Cursor::new(data);
        let _ = read_wire_frame(&mut cursor, DEFAULT_MAX_WIRE_BYTES);
    }
}

#[test]
fn decode_frame_never_panics_on_arbitrary_bytes() {
    let mut rng = XorShift64::new(0xFEDC_BA98_7654_3210);
    for _ in 0..256 {
        let len = rng.next_u32_inclusive(0, 512) as usize;
        let mut data = vec![0u8; len];
        rng.fill_bytes(&mut data);
        let _ = decode_frame(&data);
    }
}
