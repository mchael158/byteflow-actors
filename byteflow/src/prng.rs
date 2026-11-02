//! Deterministic PRNG for in-crate property-style tests (no external crates).
//!
//! Used by integration tests (`tests/properties_*`) and CapTable stress
//! suites. Not for CapId entropy — see [`crate::entropy`].

/// Odd multiplier from Vigna's xorshift64*. It is applied to the returned
/// word only; folding it into the state would collapse the period.
const STAR: u64 = 0x2545_F491_4F6C_DD1D;

/// Stand-in for a zero seed. The all-zero state is a fixed point of the
/// shifts, so it is the one seed this generator cannot run.
const NONZERO_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// xorshift64* — enough for stress loops; not for CapId entropy.
#[derive(Clone, Debug)]
pub struct XorShift64(u64);

impl XorShift64 {
    pub fn new(seed: u64) -> Self {
        XorShift64(if seed == 0 { NONZERO_SEED } else { seed })
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(STAR)
    }

    /// High half of [`Self::next_u64`]. The multiply already mixes the low
    /// bits; narrowing still keeps the half that moved farthest.
    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    pub fn next_i64(&mut self) -> i64 {
        self.next_u64() as i64
    }

    pub fn next_i32(&mut self) -> i32 {
        self.next_u32() as i32
    }

    /// Inclusive `lo..=hi` when `lo <= hi`. When `lo > hi`, returns `lo`.
    ///
    /// Draws that would fall in the incomplete residue class are discarded,
    /// so each value in the span is equally likely.
    pub fn next_u32_inclusive(&mut self, lo: u32, hi: u32) -> u32 {
        if lo >= hi {
            return lo;
        }
        let span = u64::from(hi - lo) + 1;
        let limit = span * (u64::MAX / span);
        loop {
            let draw = self.next_u64();
            if draw < limit {
                return lo + (draw % span) as u32;
            }
        }
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
    fn seeds_that_differ_by_one_diverge() {
        let mut even = XorShift64::new(2);
        let mut odd = XorShift64::new(3);
        assert_ne!(even.next_u64(), odd.next_u64());
    }

    #[test]
    fn first_word_matches_xorshift64_star() {
        // seed 1 → state 0x2000001, times the star constant.
        let mut rng = XorShift64::new(1);
        assert_eq!(rng.next_u64(), 0x47e4_ce4b_896c_dd1d);
    }

    #[test]
    fn width_helpers_and_fill_are_wired() {
        let mut rng = XorShift64::new(0x71E5_7001);
        let _ = rng.next_u32();
        let _ = rng.next_i64();
        let _ = rng.next_i32();
        assert_eq!(rng.next_u32_inclusive(5, 5), 5);
        assert_eq!(rng.next_u32_inclusive(4, 1), 4);
        for _ in 0..64 {
            let n = rng.next_u32_inclusive(1, 4);
            assert!((1..=4).contains(&n));
        }
        let mut buf = [0u8; 20];
        rng.fill_bytes(&mut buf);
        assert!(buf.iter().any(|&b| b != 0));
    }
}
