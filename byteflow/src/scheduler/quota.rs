//! Resource quotas — Phase 3.
//!
//! Three independent budgets per flow:
//! - `cpu_budget` — remaining reductions/instructions before the flow is
//!   stopped pending an ADMIN top-up. Distinct from the scheduler *quantum*
//!   (fairness / preemption, not an absolute consumption cap).
//! - `mem_used` / `mem_limit` — bytes currently charged to this flow's heap.
//! - `spawn_bucket` / `send_bucket` — token buckets drained on SPAWN / SEND,
//!   refilled by time. Covers the spawn-bomb and mailbox-flood DoS the
//!   threat model already admits.
//!
//! All three fail *closed*: exhausting a budget aborts the instruction
//! (error / trap), never degrades silently into unlimited behaviour.

use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::error::RuntimeError;
use super::sync_lock;

/// Tunables copied onto every newly spawned flow.
#[derive(Clone, Copy, Debug)]
pub struct QuotaConfig {
    pub cpu_budget: i64,
    pub mem_limit: usize,
    pub spawn_burst: i64,
    pub spawn_per_sec: i64,
    pub send_burst: i64,
    pub send_per_sec: i64,
}

impl QuotaConfig {
    /// Current defaults: generous so samples and tests keep passing.
    pub fn permissive() -> Self {
        Self {
            cpu_budget: 1_000_000_000,
            mem_limit: 64 * 1024 * 1024,
            spawn_burst: 10_000,
            spawn_per_sec: 10_000,
            send_burst: 50_000,
            send_per_sec: 50_000,
        }
    }

    /// Starting point for untrusted modules. Tune under real load before
    /// using as a production default. Pair with
    /// [`crate::RuntimeConfig::sandbox`] for process-wide `max_flows` /
    /// `max_ask_waits` ceilings.
    pub fn sandbox() -> Self {
        Self {
            cpu_budget: 50_000,
            mem_limit: 16 * 1024 * 1024,
            spawn_burst: 32,
            spawn_per_sec: 32,
            send_burst: 256,
            send_per_sec: 64,
        }
    }
}

impl Default for QuotaConfig {
    fn default() -> Self {
        Self::permissive()
    }
}

pub struct TokenBucket {
    capacity: i64,
    tokens: AtomicI64,
    refill_per_sec: i64,
    last_refill: Mutex<Instant>,
}

impl TokenBucket {
    pub fn new(capacity: i64, refill_per_sec: i64) -> Self {
        Self {
            capacity,
            tokens: AtomicI64::new(capacity),
            refill_per_sec,
            last_refill: Mutex::new(Instant::now()),
        }
    }

    /// Refill or fail closed on lock poison (no tokens granted).
    fn refill(&self) -> Result<(), RuntimeError> {
        let mut last = sync_lock::lock(&self.last_refill, "TokenBucket::refill")?;
        let elapsed = last.elapsed();
        if elapsed >= Duration::from_millis(50) {
            let add = tokens_for_elapsed(elapsed, self.refill_per_sec);
            if add > 0 {
                let cur = self.tokens.load(Ordering::Relaxed);
                let new = cur.saturating_add(add).min(self.capacity);
                self.tokens.store(new, Ordering::Relaxed);
                *last = Instant::now();
            }
        }
        Ok(())
    }

    /// True and consume one token iff one was available.
    pub fn try_consume(&self) -> bool {
        if self.refill().is_err() {
            return false;
        }
        loop {
            let cur = self.tokens.load(Ordering::Relaxed);
            if cur <= 0 {
                return false;
            }
            if self
                .tokens
                .compare_exchange_weak(cur, cur - 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
        }
    }

    /// ADMIN credit. May exceed capacity until the next refill caps it.
    ///
    /// A credit that does not fit stays at `i64::MAX`. Wrapping would make
    /// the bucket look empty and refuse every later spawn or send.
    pub fn credit(&self, extra: i64) {
        if extra <= 0 {
            return;
        }
        loop {
            let cur = self.tokens.load(Ordering::Relaxed);
            let new = cur.saturating_add(extra);
            if self
                .tokens
                .compare_exchange_weak(cur, new, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }
    }
}

/// How many tokens `per_sec` earns over `elapsed`, without a float cast.
///
/// `as i64` on an out-of-range float is a saturating conversion, and the
/// old `cur + add` then overflowed `i64` (panic in debug, wrap in release).
fn tokens_for_elapsed(elapsed: Duration, per_sec: i64) -> i64 {
    if per_sec <= 0 {
        return 0;
    }
    let add = (per_sec as u128).saturating_mul(elapsed.as_nanos()) / 1_000_000_000;
    match i64::try_from(add) {
        Ok(n) => n,
        Err(_) => i64::MAX,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaError {
    CpuExhausted,
    MemoryExhausted { requested: usize, limit: usize },
    SpawnRateExceeded,
    SendRateExceeded,
}

impl std::fmt::Display for QuotaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QuotaError::CpuExhausted => f.write_str("flow CPU budget exhausted"),
            QuotaError::MemoryExhausted { requested, limit } => {
                write!(
                    f,
                    "flow memory exhausted (requested {requested}, limit {limit})"
                )
            }
            QuotaError::SpawnRateExceeded => f.write_str("spawn rate exceeded"),
            QuotaError::SendRateExceeded => f.write_str("send rate exceeded"),
        }
    }
}

impl std::error::Error for QuotaError {}

pub struct FlowQuota {
    cpu_budget: AtomicI64,
    mem_limit: AtomicUsize,
    mem_used: AtomicUsize,
    spawn_bucket: TokenBucket,
    send_bucket: TokenBucket,
}

impl FlowQuota {
    pub fn from_config(cfg: QuotaConfig) -> Self {
        Self::new(
            cfg.cpu_budget,
            cfg.mem_limit,
            cfg.spawn_burst,
            cfg.spawn_per_sec,
            cfg.send_burst,
            cfg.send_per_sec,
        )
    }

    pub fn new(
        cpu_budget: i64,
        mem_limit: usize,
        spawn_burst: i64,
        spawn_per_sec: i64,
        send_burst: i64,
        send_per_sec: i64,
    ) -> Self {
        Self {
            cpu_budget: AtomicI64::new(cpu_budget),
            mem_limit: AtomicUsize::new(mem_limit),
            mem_used: AtomicUsize::new(0),
            spawn_bucket: TokenBucket::new(spawn_burst, spawn_per_sec),
            send_bucket: TokenBucket::new(send_burst, send_per_sec),
        }
    }

    pub fn remaining_cpu(&self) -> i64 {
        self.cpu_budget.load(Ordering::Relaxed).max(0)
    }

    /// Call once per executed instruction (or every N, in a batch — the
    /// batch only thickens trap latency, it does not change the limit).
    pub fn charge_cpu(&self, cost: i64) -> Result<(), QuotaError> {
        let remaining = self.cpu_budget.fetch_sub(cost, Ordering::Relaxed) - cost;
        if remaining < 0 {
            self.cpu_budget.fetch_add(cost, Ordering::Relaxed);
            return Err(QuotaError::CpuExhausted);
        }
        Ok(())
    }

    pub fn alloc(&self, bytes: usize) -> Result<(), QuotaError> {
        loop {
            let cur = self.mem_used.load(Ordering::Relaxed);
            let limit = self.mem_limit.load(Ordering::Relaxed);
            // `saturating_add` would record a smaller charge than `bytes`
            // when the sum overflows, and then accept it if the limit is
            // `usize::MAX`. Fail closed: the bytes were not accounted for.
            let Some(next) = cur.checked_add(bytes) else {
                return Err(QuotaError::MemoryExhausted {
                    requested: bytes,
                    limit,
                });
            };
            if next > limit {
                return Err(QuotaError::MemoryExhausted {
                    requested: bytes,
                    limit,
                });
            }
            if self
                .mem_used
                .compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(());
            }
        }
    }

    pub fn free(&self, bytes: usize) {
        loop {
            let cur = self.mem_used.load(Ordering::Relaxed);
            let next = cur.saturating_sub(bytes);
            if self
                .mem_used
                .compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }
    }

    pub fn check_spawn(&self) -> Result<(), QuotaError> {
        if self.spawn_bucket.try_consume() {
            Ok(())
        } else {
            Err(QuotaError::SpawnRateExceeded)
        }
    }

    pub fn check_send(&self) -> Result<(), QuotaError> {
        if self.send_bucket.try_consume() {
            Ok(())
        } else {
            Err(QuotaError::SendRateExceeded)
        }
    }

    /// Reachable only after `check_admin` against `CapTarget::Scheduler`.
    ///
    /// A non-positive credit is ignored. A credit that does not fit stops at
    /// `i64::MAX`: wrapping would make `remaining_cpu` look exhausted and
    /// kill the flow the admin just tried to extend.
    pub fn top_up_cpu(&self, extra: i64) {
        if extra <= 0 {
            return;
        }
        loop {
            let cur = self.cpu_budget.load(Ordering::Relaxed);
            let new = cur.saturating_add(extra);
            if self
                .cpu_budget
                .compare_exchange_weak(cur, new, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }
    }

    /// Raise the heap ceiling. A sum that does not fit stops at `usize::MAX`
    /// instead of wrapping the limit down to a handful of bytes.
    pub fn top_up_mem(&self, extra: usize) {
        loop {
            let cur = self.mem_limit.load(Ordering::Relaxed);
            let new = cur.saturating_add(extra);
            if self
                .mem_limit
                .compare_exchange_weak(cur, new, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }
    }

    pub fn top_up_send(&self, extra: i64) {
        self.send_bucket.credit(extra);
    }

    pub fn mem_used(&self) -> usize {
        self.mem_used.load(Ordering::Relaxed)
    }
}

/// Live-flow quota directory so ADMIN top-up can find a running flow.
pub struct QuotaTable {
    inner: Mutex<std::collections::HashMap<u64, Arc<FlowQuota>>>,
}

impl QuotaTable {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn insert(
        &self,
        id: super::process::FlowId,
        quota: Arc<FlowQuota>,
    ) -> Result<(), RuntimeError> {
        sync_lock::lock(&self.inner, "QuotaTable::insert")?.insert(id.as_u64(), quota);
        Ok(())
    }

    pub fn get(&self, id: super::process::FlowId) -> Result<Option<Arc<FlowQuota>>, RuntimeError> {
        Ok(sync_lock::lock(&self.inner, "QuotaTable::get")?
            .get(&id.as_u64())
            .cloned())
    }

    pub fn remove(
        &self,
        id: super::process::FlowId,
    ) -> Result<Option<Arc<FlowQuota>>, RuntimeError> {
        Ok(sync_lock::lock(&self.inner, "QuotaTable::remove")?.remove(&id.as_u64()))
    }
}

impl Default for QuotaTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_budget_fails_closed() {
        let q = FlowQuota::new(10, 1024, 1, 1, 1, 1);
        assert!(q.charge_cpu(7).is_ok());
        assert!(q.charge_cpu(4).is_err());
        assert!(q.charge_cpu(3).is_ok());
    }

    #[test]
    fn memory_never_exceeds_limit() {
        let q = FlowQuota::new(1000, 100, 1, 1, 1, 1);
        assert!(q.alloc(60).is_ok());
        assert!(q.alloc(60).is_err());
        q.free(60);
        assert!(q.alloc(60).is_ok());
    }

    #[test]
    fn alloc_rejects_a_charge_that_would_overflow_the_counter() {
        let q = FlowQuota::new(1000, usize::MAX, 1, 1, 1, 1);
        assert!(q.alloc(usize::MAX - 10).is_ok());
        assert!(q.alloc(20).is_err());
        assert_eq!(q.mem_used(), usize::MAX - 10);
    }

    #[test]
    fn sandbox_is_tighter_than_permissive() {
        let p = QuotaConfig::permissive();
        let s = QuotaConfig::sandbox();
        assert!(s.cpu_budget < p.cpu_budget);
        assert!(s.mem_limit < p.mem_limit);
        assert!(s.send_burst < p.send_burst);
        assert_eq!(QuotaConfig::default().cpu_budget, p.cpu_budget);
    }

    #[test]
    fn top_up_does_not_wrap_a_budget_that_is_already_at_the_top() {
        let q = FlowQuota::new(i64::MAX, usize::MAX, 1, 1, 1, 1);
        q.top_up_cpu(5);
        q.top_up_cpu(-5);
        q.top_up_mem(1);
        assert_eq!(q.remaining_cpu(), i64::MAX);
        assert!(q.alloc(1).is_ok());
    }

    #[test]
    fn top_up_mem_raises_limit() {
        let q = FlowQuota::new(1000, 100, 1, 1, 1, 1);
        assert!(q.alloc(60).is_ok());
        assert!(q.alloc(60).is_err());
        q.top_up_mem(50);
        assert!(q.alloc(60).is_ok());
    }

    #[test]
    fn credit_at_the_top_of_i64_does_not_wrap_the_bucket_negative() {
        let bucket = TokenBucket::new(i64::MAX, 0);
        bucket.credit(8);
        assert!(
            bucket.try_consume(),
            "a wrapped bucket would refuse the token"
        );
    }

    #[test]
    fn tokens_for_a_long_interval_saturate_instead_of_overflowing() {
        let add = tokens_for_elapsed(Duration::from_secs(u64::MAX), i64::MAX);
        assert_eq!(add, i64::MAX);
        assert_eq!(tokens_for_elapsed(Duration::from_millis(50), 32), 1);
        assert_eq!(tokens_for_elapsed(Duration::from_millis(50), -1), 0);
    }

    #[test]
    fn spawn_bucket_does_not_over_issue() {
        let q = FlowQuota::new(1000, 100, 3, 0, 1, 1);
        assert!(q.check_spawn().is_ok());
        assert!(q.check_spawn().is_ok());
        assert!(q.check_spawn().is_ok());
        assert!(q.check_spawn().is_err());
    }
}
