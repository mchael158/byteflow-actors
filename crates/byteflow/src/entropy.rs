//! OS CSPRNG bytes for [`crate::CapId`].
//!
//! [`getrandom::fill`] reads the platform generator (`ProcessPrng` on
//! Windows, `getrandom` / `/dev/urandom` on Linux). A failed read is
//! [`EntropyError`] and keeps the OS error. This module does not substitute
//! a hash.

use core::fmt;

/// The operating system refused or failed the entropy read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntropyError {
    inner: getrandom::Error,
}

impl EntropyError {
    fn new(inner: getrandom::Error) -> Self {
        Self { inner }
    }
}

impl fmt::Display for EntropyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

impl std::error::Error for EntropyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.inner)
    }
}

/// Fill `buf` with OS CSPRNG bytes. An empty buffer succeeds. Never panics.
pub fn fill_bytes(buf: &mut [u8]) -> Result<(), EntropyError> {
    match getrandom::fill(buf) {
        Ok(()) => Ok(()),
        Err(inner) => Err(EntropyError::new(inner)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_fill_succeeds() -> Result<(), EntropyError> {
        let mut buf = [];
        fill_bytes(&mut buf)
    }

    #[test]
    fn fill_bytes_varies() -> Result<(), EntropyError> {
        let mut a = [0u8; 16];
        let mut b = [0u8; 16];
        fill_bytes(&mut a)?;
        fill_bytes(&mut b)?;
        assert_ne!(a, b);
        Ok(())
    }
}
