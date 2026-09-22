//! OS entropy without external crates.
//!
//! Uses [`std::collections::hash_map::RandomState`], whose keys are drawn
//! from the platform CSPRNG inside `std`. Adequate for [`crate::CapId`]
//! uniqueness (collision retries remain in the caller).

use core::fmt;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};

/// Entropy fill failed (should be unreachable with `std` RandomState).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntropyError;

impl fmt::Display for EntropyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("entropy fill failed")
    }
}

impl std::error::Error for EntropyError {}

/// Fill `buf` with entropy-derived bytes. Never panics.
pub fn fill_bytes(buf: &mut [u8]) -> Result<(), EntropyError> {
    let mut filled = 0;
    while filled < buf.len() {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_usize(filled);
        let word = hasher.finish().to_ne_bytes();
        let take = (buf.len() - filled).min(word.len());
        buf[filled..filled + take].copy_from_slice(&word[..take]);
        filled += take;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_bytes_varies() {
        let mut a = [0u8; 16];
        let mut b = [0u8; 16];
        let _ = fill_bytes(&mut a);
        let _ = fill_bytes(&mut b);
        // Astronomically unlikely to collide for two fresh RandomState draws.
        assert_ne!(a, b);
    }
}
