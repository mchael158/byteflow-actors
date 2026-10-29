use std::sync::atomic::{AtomicU64, Ordering};

/// Runtime-wide counters (design notes §26). Every field is a plain
/// `AtomicU64` bumped with `Relaxed` ordering: these are monitoring
/// counters, not synchronization primitives, so we don't pay for anything
/// stronger than "eventually visible to a metrics scrape."
#[derive(Default)]
pub struct RuntimeMetrics {
    pub processes_spawned: AtomicU64,
    pub processes_completed: AtomicU64,
    pub processes_failed: AtomicU64,
    pub messages_sent: AtomicU64,
    pub steals: AtomicU64,
    pub reschedules: AtomicU64,
    #[cfg(feature = "jit")]
    pub jit_compiles: AtomicU64,
    #[cfg(feature = "jit")]
    pub jit_compile_failures: AtomicU64,
    #[cfg(feature = "jit")]
    pub jit_executions: AtomicU64,
    #[cfg(feature = "jit")]
    pub jit_misses: AtomicU64,
    #[cfg(feature = "jit")]
    pub jit_deopts: AtomicU64,
}

impl RuntimeMetrics {
    #[inline]
    pub fn inc(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> RuntimeMetricsSnapshot {
        RuntimeMetricsSnapshot {
            processes_spawned: self.processes_spawned.load(Ordering::Relaxed),
            processes_completed: self.processes_completed.load(Ordering::Relaxed),
            processes_failed: self.processes_failed.load(Ordering::Relaxed),
            messages_sent: self.messages_sent.load(Ordering::Relaxed),
            steals: self.steals.load(Ordering::Relaxed),
            reschedules: self.reschedules.load(Ordering::Relaxed),
            #[cfg(feature = "jit")]
            jit_compiles: self.jit_compiles.load(Ordering::Relaxed),
            #[cfg(feature = "jit")]
            jit_compile_failures: self.jit_compile_failures.load(Ordering::Relaxed),
            #[cfg(feature = "jit")]
            jit_executions: self.jit_executions.load(Ordering::Relaxed),
            #[cfg(feature = "jit")]
            jit_misses: self.jit_misses.load(Ordering::Relaxed),
            #[cfg(feature = "jit")]
            jit_deopts: self.jit_deopts.load(Ordering::Relaxed),
        }
    }
}

/// Point-in-time, non-atomic copy of [`RuntimeMetrics`] suitable for
/// printing or exporting.
#[derive(Debug, Clone, Copy, Default)]
pub struct RuntimeMetricsSnapshot {
    pub processes_spawned: u64,
    pub processes_completed: u64,
    pub processes_failed: u64,
    pub messages_sent: u64,
    pub steals: u64,
    pub reschedules: u64,
    #[cfg(feature = "jit")]
    pub jit_compiles: u64,
    #[cfg(feature = "jit")]
    pub jit_compile_failures: u64,
    #[cfg(feature = "jit")]
    pub jit_executions: u64,
    #[cfg(feature = "jit")]
    pub jit_misses: u64,
    #[cfg(feature = "jit")]
    pub jit_deopts: u64,
}

impl std::fmt::Display for RuntimeMetricsSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "spawned={} completed={} failed={} messages={} steals={} reschedules={}",
            self.processes_spawned,
            self.processes_completed,
            self.processes_failed,
            self.messages_sent,
            self.steals,
            self.reschedules,
        )?;
        #[cfg(feature = "jit")]
        write!(
            f,
            " jit_compiles={} jit_failures={} jit_exec={} jit_miss={} jit_deopt={}",
            self.jit_compiles,
            self.jit_compile_failures,
            self.jit_executions,
            self.jit_misses,
            self.jit_deopts,
        )?;
        Ok(())
    }
}

/// Point-in-time runtime view for dashboards and debug tooling.
#[derive(Debug, Clone)]
pub struct RuntimeSnapshot {
    pub metrics: RuntimeMetricsSnapshot,
    pub live_flows: usize,
    pub flow_limit_live: usize,
    pub max_flows: u32,
    pub distribution_pending: Option<usize>,
    pub memory: crate::MemorySnapshot,
    pub workers: usize,
}

impl std::fmt::Display for RuntimeSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} live_flows={} flow_limit={}/{} bridge_pending={:?} memory={}/{} workers={}",
            self.metrics,
            self.live_flows,
            self.flow_limit_live,
            self.max_flows,
            self.distribution_pending,
            self.memory.used,
            self.memory.limit,
            self.workers,
        )
    }
}
