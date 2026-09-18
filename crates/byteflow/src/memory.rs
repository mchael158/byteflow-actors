//! Runtime memory accounting.
//!
//! The accounting is intentionally explicit:
//! - allocations are charged when capacity is acquired;
//! - replacements release the old allocation;
//! - [`Drop`] releases the final allocation;
//! - failed reservations never modify the current value.
//!
//! Order for overwrite (S3):
//!
//! ```text
//! reserve(new) → liberar old → instalar new
//! ```
//!
//! Never install before releasing the previous charge.

use core::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Hard ceiling for a [`MemoryBudget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryLimit {
    pub limit: usize,
}

/// Point-in-time view of a budget (read-only observability).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemorySnapshot {
    pub used: usize,
    pub limit: usize,
}

/// Fail-closed memory reservation error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryError {
    LimitExceeded {
        requested: usize,
        used: usize,
        limit: usize,
    },
    ArithmeticOverflow,
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LimitExceeded {
                requested,
                used,
                limit,
            } => write!(
                f,
                "runtime memory limit exceeded: requested={requested}, used={used}, limit={limit}"
            ),
            Self::ArithmeticOverflow => {
                write!(f, "runtime memory accounting overflow")
            }
        }
    }
}

impl std::error::Error for MemoryError {}

/// Process- or scope-wide byte budget with atomic charge/release.
///
/// Used as the runtime-wide ceiling ([`crate::RuntimeConfig::max_runtime_bytes`])
/// and as the backing budget for [`HeapStr`] / [`HeapBytes`].
pub struct MemoryBudget {
    used: AtomicUsize,
    limit: usize,
}

impl MemoryBudget {
    pub fn new(limit: usize) -> Self {
        Self {
            used: AtomicUsize::new(0),
            limit,
        }
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    pub fn snapshot(&self) -> MemorySnapshot {
        MemorySnapshot {
            used: self.used(),
            limit: self.limit(),
        }
    }

    /// Reserve `bytes` against the ceiling. Does not modify state on failure.
    pub fn try_charge(&self, bytes: usize) -> Result<(), MemoryError> {
        if bytes == 0 {
            return Ok(());
        }

        let mut current = self.used.load(Ordering::Acquire);

        loop {
            let next = current
                .checked_add(bytes)
                .ok_or(MemoryError::ArithmeticOverflow)?;

            if next > self.limit {
                return Err(MemoryError::LimitExceeded {
                    requested: bytes,
                    used: current,
                    limit: self.limit,
                });
            }

            match self.used.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => current = actual,
            }
        }
    }

    /// Return previously charged bytes. Underflow is a programming bug.
    pub fn release(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }

        let previous = self.used.fetch_sub(bytes, Ordering::AcqRel);

        debug_assert!(
            previous >= bytes,
            "memory accounting underflow: previous={previous}, release={bytes}"
        );
    }
}

/// UTF-8 heap buffer charged against a [`MemoryBudget`].
///
/// Charge is based on [`String::capacity`], not just `len`, so growth of the
/// backing allocation is visible to the budget.
pub struct HeapStr {
    budget: Arc<MemoryBudget>,
    value: String,
    charged: usize,
}

impl HeapStr {
    pub fn new(budget: Arc<MemoryBudget>, value: impl Into<String>) -> Result<Self, MemoryError> {
        let value = value.into();
        let charged = value.capacity();

        budget.try_charge(charged)?;

        Ok(Self {
            budget,
            value,
            charged,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.value
    }

    pub fn len(&self) -> usize {
        self.value.len()
    }

    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.value.capacity()
    }

    /// Replace contents: reserve new, install, release old (S3).
    /// On failure the previous value and charge are unchanged.
    pub fn replace(&mut self, value: impl Into<String>) -> Result<(), MemoryError> {
        let next = value.into();
        let next_charge = next.capacity();

        self.budget.try_charge(next_charge)?;

        let old_charge = self.charged;

        self.value = next;
        self.charged = next_charge;

        self.budget.release(old_charge);

        Ok(())
    }
}

impl Drop for HeapStr {
    fn drop(&mut self) {
        self.budget.release(self.charged);
    }
}

/// Opaque byte buffer charged against a [`MemoryBudget`].
pub struct HeapBytes {
    budget: Arc<MemoryBudget>,
    bytes: Vec<u8>,
    charged: usize,
}

impl HeapBytes {
    pub fn new(
        budget: Arc<MemoryBudget>,
        bytes: impl Into<Vec<u8>>,
    ) -> Result<Self, MemoryError> {
        let bytes = bytes.into();
        let charged = bytes.capacity();

        budget.try_charge(charged)?;

        Ok(Self {
            budget,
            bytes,
            charged,
        })
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.bytes.capacity()
    }

    /// Replace contents: reserve new, install, release old (S3).
    pub fn replace(&mut self, bytes: impl Into<Vec<u8>>) -> Result<(), MemoryError> {
        let next = bytes.into();
        let next_charge = next.capacity();

        self.budget.try_charge(next_charge)?;

        let old_charge = self.charged;

        self.bytes = next;
        self.charged = next_charge;

        self.budget.release(old_charge);

        Ok(())
    }
}

impl Drop for HeapBytes {
    fn drop(&mut self) {
        self.budget.release(self.charged);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn string_drop_releases_memory() {
        let budget = Arc::new(MemoryBudget::new(1024));

        {
            let value = match HeapStr::new(Arc::clone(&budget), "hello") {
                Ok(v) => v,
                Err(e) => panic!("unexpected: {e}"),
            };
            assert_eq!(budget.used(), value.capacity());
        }

        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn bytes_drop_releases_memory() {
        let budget = Arc::new(MemoryBudget::new(1024));

        {
            let value = match HeapBytes::new(Arc::clone(&budget), vec![1; 128]) {
                Ok(v) => v,
                Err(e) => panic!("unexpected: {e}"),
            };
            assert_eq!(budget.used(), value.capacity());
        }

        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn replacement_does_not_leak() {
        let budget = Arc::new(MemoryBudget::new(4096));

        let mut value = match HeapBytes::new(Arc::clone(&budget), vec![0; 64]) {
            Ok(v) => v,
            Err(e) => panic!("unexpected: {e}"),
        };

        let first = budget.used();

        if let Err(e) = value.replace(vec![0; 512]) {
            panic!("unexpected: {e}");
        }

        assert_eq!(budget.used(), value.capacity());
        assert_ne!(first, budget.used());

        drop(value);

        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn failed_replacement_keeps_old_value() {
        let budget = Arc::new(MemoryBudget::new(128));

        let mut value = match HeapBytes::new(Arc::clone(&budget), vec![0; 64]) {
            Ok(v) => v,
            Err(e) => panic!("unexpected: {e}"),
        };

        let old = value.as_slice().to_vec();
        let old_used = budget.used();

        let result = value.replace(vec![0; 128]);

        assert!(result.is_err());
        assert_eq!(value.as_slice(), old.as_slice());
        assert_eq!(budget.used(), old_used);
    }

    #[test]
    fn try_charge_fails_closed_without_mutating() {
        let budget = MemoryBudget::new(10);
        assert!(budget.try_charge(10).is_ok());
        assert_eq!(budget.used(), 10);
        assert!(matches!(
            budget.try_charge(1),
            Err(MemoryError::LimitExceeded { .. })
        ));
        assert_eq!(budget.used(), 10);
    }
}
