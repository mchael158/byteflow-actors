//! Runtime memory accounting.
//!
//! The accounting is intentionally explicit:
//! - a buffer is charged for the bytes it stores (`len`), the same unit a
//!   register `Str` / `Bytes` pays on this budget;
//! - spare capacity the caller allocated before handing the buffer over is
//!   not charged;
//! - replacements release the old charge;
//! - [`Drop`] releases the final charge;
//! - failed reservations never modify the current value.
//!
//! Order for overwrite (S3):
//!
//! ```text
//! reserve(new) → release(old) → install(new)
//! ```
//!
//! Never install before releasing the previous charge. Peak usage during a
//! successful replace is therefore `old + new` until the old charge is
//! released — a same-size swap can fail against a tight ceiling even though
//! the steady-state footprint would fit.
//!
//! # What this does *not* guarantee
//!
//! [`HeapStr`] / [`HeapBytes`] charge the bytes they store. Rust code that
//! builds a `String` / `Vec` before handing it over has already allocated;
//! spare capacity in that buffer is invisible to the budget, the same way a
//! register `Str` pays `str.len()`.
//! [`RuntimeConfig::max_runtime_bytes`](crate::RuntimeConfig) is therefore a
//! runtime-managed accounting ceiling, not a global allocator sandbox.

use core::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Point-in-time view of a budget (read-only observability).
///
/// Concurrent charges may change `used` immediately after this returns —
/// treat it as metrics, not a transactional lock.
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

    /// Return previously charged bytes.
    ///
    /// Underflow is a programming bug. Uses CAS so a violated invariant never
    /// wraps `used` to a huge value in release builds (unlike bare
    /// `fetch_sub`).
    pub fn release(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }

        let mut current = self.used.load(Ordering::Acquire);
        loop {
            assert!(
                current >= bytes,
                "memory accounting underflow: previous={current}, release={bytes}"
            );
            let next = current - bytes;
            match self.used.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }
}

/// UTF-8 heap buffer charged against a [`MemoryBudget`].
///
/// The charge is [`str::len`], the same unit a register `Str` pays. Spare
/// capacity in the `String` is not charged.
///
/// [`Self::new`] / [`Self::replace`] take an already-built `String` (via
/// `Into`); any allocation performed by the caller happens **before** the
/// budget sees it — see the module docs.
pub struct HeapStr {
    budget: Arc<MemoryBudget>,
    value: String,
    charged: usize,
}

impl HeapStr {
    pub fn new(budget: Arc<MemoryBudget>, value: impl Into<String>) -> Result<Self, MemoryError> {
        let value = value.into();
        let charged = value.len();

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

    /// Bytes the `String` allocator reserved. The budget charges [`Self::len`],
    /// not this spare room.
    pub fn capacity(&self) -> usize {
        self.value.capacity()
    }

    /// Replace contents using the S3 overwrite order:
    /// `reserve(new) → release(old) → install(new)`.
    ///
    /// If reservation fails, the previous value and charge remain unchanged.
    pub fn replace(&mut self, value: impl Into<String>) -> Result<(), MemoryError> {
        let next = value.into();
        let next_charge = next.len();

        // S3: reserve the replacement before touching the old state.
        self.budget.try_charge(next_charge)?;

        let old_charge = self.charged;

        // S3: release the previous charge before installing the replacement.
        self.budget.release(old_charge);

        self.value = next;
        self.charged = next_charge;

        Ok(())
    }
}

impl Drop for HeapStr {
    fn drop(&mut self) {
        self.budget.release(self.charged);
    }
}

/// Opaque byte buffer charged against a [`MemoryBudget`].
///
/// The charge is the slice length, the same unit a register `Bytes` pays.
/// Spare capacity in the `Vec` is not charged.
pub struct HeapBytes {
    budget: Arc<MemoryBudget>,
    bytes: Vec<u8>,
    charged: usize,
}

impl HeapBytes {
    pub fn new(budget: Arc<MemoryBudget>, bytes: impl Into<Vec<u8>>) -> Result<Self, MemoryError> {
        let bytes = bytes.into();
        let charged = bytes.len();

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

    /// Bytes the `Vec` allocator reserved. The budget charges [`Self::len`],
    /// not this spare room.
    pub fn capacity(&self) -> usize {
        self.bytes.capacity()
    }

    /// Replace contents using the S3 overwrite order:
    /// `reserve(new) → release(old) → install(new)`.
    ///
    /// If reservation fails, the previous value and charge remain unchanged.
    pub fn replace(&mut self, bytes: impl Into<Vec<u8>>) -> Result<(), MemoryError> {
        let next = bytes.into();
        let next_charge = next.len();

        self.budget.try_charge(next_charge)?;

        let old_charge = self.charged;

        self.budget.release(old_charge);

        self.bytes = next;
        self.charged = next_charge;

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
            assert_eq!(budget.used(), value.len());
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
            assert_eq!(budget.used(), value.len());
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

        assert_eq!(budget.used(), value.len());
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

    #[test]
    fn try_charge_detects_arithmetic_overflow() {
        let budget = MemoryBudget::new(usize::MAX);

        assert!(matches!(budget.try_charge(usize::MAX), Ok(())));
        assert!(matches!(
            budget.try_charge(1),
            Err(MemoryError::ArithmeticOverflow)
        ));
        assert_eq!(budget.used(), usize::MAX);
    }

    #[test]
    fn zero_limit_rejects_non_zero_charge() {
        let budget = MemoryBudget::new(0);

        assert!(matches!(
            budget.try_charge(1),
            Err(MemoryError::LimitExceeded { .. })
        ));
        assert_eq!(budget.used(), 0);
        assert!(budget.try_charge(0).is_ok());
    }

    #[test]
    fn same_len_replace_needs_peak_headroom() {
        // S3 peaks at old+new before releasing old. With the ceiling equal
        // to the live length, a same-size replace must fail even though the
        // steady-state footprint would fit.
        let budget = Arc::new(MemoryBudget::new(600));
        let mut value = match HeapBytes::new(Arc::clone(&budget), vec![0u8; 600]) {
            Ok(v) => v,
            Err(e) => panic!("unexpected: {e}"),
        };
        assert_eq!(budget.used(), 600);

        let err = value.replace(vec![0u8; 600]);
        assert!(
            matches!(err, Err(MemoryError::LimitExceeded { .. })),
            "same-size replace must need peak headroom under S3, got {err:?}"
        );
        assert_eq!(budget.used(), 600);
        assert_eq!(value.len(), 600);
    }

    #[test]
    fn same_len_replace_succeeds_with_peak_room() {
        let budget = Arc::new(MemoryBudget::new(2048));
        let mut value = match HeapBytes::new(Arc::clone(&budget), vec![0u8; 64]) {
            Ok(v) => v,
            Err(e) => panic!("unexpected: {e}"),
        };
        if let Err(e) = value.replace(vec![1u8; 64]) {
            panic!("unexpected: {e}");
        }
        assert_eq!(budget.used(), 64);
        assert_eq!(value.as_slice(), &[1u8; 64]);
    }

    #[test]
    fn spare_capacity_is_not_charged() {
        let budget = Arc::new(MemoryBudget::new(64));
        let mut buf = Vec::with_capacity(10_000);
        buf.extend_from_slice(&[1, 2, 3, 4]);
        let value = match HeapBytes::new(Arc::clone(&budget), buf) {
            Ok(v) => v,
            Err(e) => panic!("unexpected: {e}"),
        };
        assert!(value.capacity() > value.len());
        assert_eq!(budget.used(), 4);
    }

    #[test]
    fn release_zero_is_noop() {
        let budget = MemoryBudget::new(100);
        assert!(budget.try_charge(40).is_ok());
        budget.release(0);
        assert_eq!(budget.used(), 40);
        budget.release(40);
        assert_eq!(budget.used(), 0);
    }
}
