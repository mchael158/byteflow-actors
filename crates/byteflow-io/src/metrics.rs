//! Lightweight counters for host queue and HostAwait bridge observability.

use std::sync::atomic::{AtomicU64, Ordering};

/// Process-local counters for enqueue / settle outcomes.
///
/// All updates use relaxed atomics: these are diagnostic gauges, not
/// synchronization points.
#[derive(Debug, Default)]
pub struct IoMetrics {
    submitted: AtomicU64,
    rejected_full: AtomicU64,
    rejected_closed: AtomicU64,
    completed_ok: AtomicU64,
    completed_err: AtomicU64,
}

/// Immutable snapshot of [`IoMetrics`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IoMetricsSnapshot {
    /// Jobs / commands successfully accepted by a bounded queue.
    pub submitted: u64,
    /// Rejections because the bounded queue was full.
    pub rejected_full: u64,
    /// Rejections because the queue / bridge was shut down.
    pub rejected_closed: u64,
    /// HostAwait jobs settled with `Ok`.
    pub completed_ok: u64,
    /// HostAwait jobs settled with `Err` (includes driver faults; excludes
    /// pure cancel/stale settles that never ran the driver).
    pub completed_err: u64,
}

impl IoMetrics {
    /// Capture a point-in-time view of all counters.
    pub fn snapshot(&self) -> IoMetricsSnapshot {
        IoMetricsSnapshot {
            submitted: self.submitted.load(Ordering::Relaxed),
            rejected_full: self.rejected_full.load(Ordering::Relaxed),
            rejected_closed: self.rejected_closed.load(Ordering::Relaxed),
            completed_ok: self.completed_ok.load(Ordering::Relaxed),
            completed_err: self.completed_err.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn record_submitted(&self) {
        self.submitted.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_full(&self) {
        self.rejected_full.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_closed(&self) {
        self.rejected_closed.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_ok(&self) {
        self.completed_ok.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_err(&self) {
        self.completed_err.fetch_add(1, Ordering::Relaxed);
    }
}
