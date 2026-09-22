//! Deterministic PRNG for in-crate property-style tests (no external crates).
//!
//! Used by integration tests (`tests/properties_*`) and CapTable stress
//! suites. Not for CapId entropy — see [`crate::entropy`].

/// xorshift64* — enough for stress loops; not for CapId entropy.
#[derive(Clone, Debug)]
pub struct XorShift64(u64);

impl XorShift64 {
    pub fn new(seed: u64) -> Self {
        // Avoid the all-zero state (xorshift lock).
        XorShift64(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn next_u32(&mut self) -> u32 {
        self.next_u64() as u32
    }

    pub fn next_i64(&mut self) -> i64 {
        self.next_u64() as i64
    }

    pub fn next_i32(&mut self) -> i32 {
        self.next_u64() as i32
    }

    /// Inclusive `lo..=hi` when `lo <= hi`.
    pub fn next_u32_inclusive(&mut self, lo: u32, hi: u32) -> u32 {
        if lo >= hi {
            return lo;
        }
        let span = (hi - lo) as u64 + 1;
        lo + (self.next_u64() % span) as u32
    }

    pub fn fill_bytes(&mut self, buf: &mut [u8]) {
        let mut i = 0;
        while i < buf.len() {
            let word = self.next_u64().to_le_bytes();
            let take = (buf.len() - i).min(word.len());
            buf[i..i + take].copy_from_slice(&word[..take]);
            i += take;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_seed_is_forced_nonzero_and_deterministic() {
        let mut a = XorShift64::new(0);
        let mut b = XorShift64::new(0);
        let x = a.next_u64();
        let y = b.next_u64();
        assert_ne!(x, 0);
        assert_eq!(x, y);
        assert_eq!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn width_helpers_and_fill_are_wired() {
        let mut rng = XorShift64::new(0x71E5_7001);
        let _ = rng.next_u32();
        let _ = rng.next_i64();
        let _ = rng.next_i32();
        assert_eq!(rng.next_u32_inclusive(5, 5), 5);
        let n = rng.next_u32_inclusive(1, 4);
        assert!((1..=4).contains(&n));
        let mut buf = [0u8; 20];
        rng.fill_bytes(&mut buf);
        assert!(buf.iter().any(|&b| b != 0));
    }
}
