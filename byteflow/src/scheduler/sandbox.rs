//! Sandbox policy surface — fingerprint, counters, and capability attestation.
//!
//! The sandbox is **opt-in** via [`crate::RuntimeConfig::sandbox`]. Default
//! [`crate::RuntimeConfig`] stays permissive (unlimited process-wide caps,
//! generous per-flow quotas). This module does **not** replace
//! [`super::capability::CapTable`], [`super::quota::FlowQuota`], or
//! [`crate::MemoryBudget`]; it exposes host-facing policy identity and
//! observability on top of those enforcement points.
//!
//! Isolation is **in-process**. Native code and the host remain inside the
//! same trust boundary — see `docs/sandbox/limitations.md`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::bytecode::{CapId, CapRights, CapTarget, TrustLevel};

use super::capability::{CapTable, Capability};
use super::error::RuntimeError;
use super::process::FlowId;
use super::runtime::RuntimeConfig;

/// Canonical encoding version mixed into [`policy_fingerprint`].
const POLICY_VERSION: u8 = 1;

/// Stable identity of the security-relevant knobs on a [`RuntimeConfig`].
///
/// The 16 bytes are the leading half of SHA-256 over a fixed-width encoding
/// (see [`policy_fingerprint`]). The same config yields the same bytes on
/// every rustc, OS, and pointer width. It is **not** a cryptographic
/// attestation of process integrity. A value stored before this encoding
/// (std `DefaultHasher`) will not match and must be recomputed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PolicyFingerprint([u8; 16]);

impl PolicyFingerprint {
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(32);
        for b in &self.0 {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
        }
        s
    }
}

impl std::fmt::Display for PolicyFingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// Compute a fingerprint over the security-relevant fields of `config`.
///
/// Encoding (`POLICY_VERSION` = 1), little-endian, fixed widths so a 32-bit
/// host matches a 64-bit host:
///
/// `b"BFQP" | version | trust | root_authority | max_flows u32 | max_ask_waits u32 |
/// max_host_awaits u32 | max_links u32 | max_monitors u32 | max_registry_names u32 |
/// max_runtime_bytes u64 | mailbox capacity u32 | mailbox bytes u64 | overflow u8 |
/// cpu_budget i64 | mem_limit u64 | spawn_burst i64 | spawn_per_sec i64 |
/// send_burst i64 | send_per_sec i64`
///
/// Workers, quantum, JIT, sinks, and bridges are not part of the policy
/// identity. The returned value is SHA-256 of that buffer, truncated to 16
/// bytes.
pub fn policy_fingerprint(config: &RuntimeConfig) -> PolicyFingerprint {
    let digest = crate::digest::sha256(&canonical_policy_bytes(config));
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    PolicyFingerprint(out)
}

fn canonical_policy_bytes(config: &RuntimeConfig) -> Vec<u8> {
    let mut buf = Vec::with_capacity(100);
    buf.extend_from_slice(b"BFQP");
    buf.push(POLICY_VERSION);
    buf.push(trust_tag(config.trust));
    buf.push(root_authority_tag(config.root_authority));
    push_u32(&mut buf, config.max_flows);
    push_u32(&mut buf, config.max_ask_waits);
    push_u32(&mut buf, config.max_host_awaits);
    push_u32(&mut buf, config.max_links);
    push_u32(&mut buf, config.max_monitors);
    push_u32(&mut buf, config.max_registry_names);
    push_u64(&mut buf, as_u64(config.max_runtime_bytes));
    push_u32(&mut buf, config.mailbox.capacity().get_u32());
    push_u64(&mut buf, as_u64(config.mailbox.bytes().get()));
    buf.push(config.mailbox.overflow() as u8);
    let q = &config.quota;
    push_i64(&mut buf, q.cpu_budget);
    push_u64(&mut buf, as_u64(q.mem_limit));
    push_i64(&mut buf, q.spawn_burst);
    push_i64(&mut buf, q.spawn_per_sec);
    push_i64(&mut buf, q.send_burst);
    push_i64(&mut buf, q.send_per_sec);
    buf
}

fn as_u64(n: usize) -> u64 {
    match u64::try_from(n) {
        Ok(v) => v,
        Err(_) => u64::MAX,
    }
}

fn push_u32(buf: &mut Vec<u8>, value: u32) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(buf: &mut Vec<u8>, value: u64) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn push_i64(buf: &mut Vec<u8>, value: i64) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn trust_tag(trust: TrustLevel) -> u8 {
    match trust {
        TrustLevel::Trusted => 0,
        TrustLevel::Restricted => 1,
        TrustLevel::Untrusted => 2,
    }
}

fn root_authority_tag(policy: crate::scheduler::RootAuthorityPolicy) -> u8 {
    match policy {
        crate::scheduler::RootAuthorityPolicy::TrustedHost => 0,
        crate::scheduler::RootAuthorityPolicy::Restricted => 1,
        crate::scheduler::RootAuthorityPolicy::Untrusted => 2,
    }
}

/// Host-visible security counters. Untrusted bytecode cannot write these.
#[derive(Default, Debug)]
pub struct SecurityCounters {
    pub denied_operations: AtomicU64,
    pub quota_exceeded: AtomicU64,
    pub capabilities_created: AtomicU64,
    pub capabilities_revoked: AtomicU64,
    pub invalid_modules: AtomicU64,
    pub native_denied: AtomicU64,
    pub active_untrusted_flows: AtomicU64,
}

impl SecurityCounters {
    #[inline]
    pub fn inc(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Saturating decrement for [`Self::active_untrusted_flows`].
    pub fn dec_active_untrusted(&self) {
        let mut cur = self.active_untrusted_flows.load(Ordering::Relaxed);
        while cur > 0 {
            match self.active_untrusted_flows.compare_exchange_weak(
                cur,
                cur - 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => cur = actual,
            }
        }
    }

    pub fn snapshot(&self) -> SecurityCountersSnapshot {
        SecurityCountersSnapshot {
            denied_operations: self.denied_operations.load(Ordering::Relaxed),
            quota_exceeded: self.quota_exceeded.load(Ordering::Relaxed),
            capabilities_created: self.capabilities_created.load(Ordering::Relaxed),
            capabilities_revoked: self.capabilities_revoked.load(Ordering::Relaxed),
            invalid_modules: self.invalid_modules.load(Ordering::Relaxed),
            native_denied: self.native_denied.load(Ordering::Relaxed),
            active_untrusted_flows: self.active_untrusted_flows.load(Ordering::Relaxed),
        }
    }
}

/// Point-in-time copy of [`SecurityCounters`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SecurityCountersSnapshot {
    pub denied_operations: u64,
    pub quota_exceeded: u64,
    pub capabilities_created: u64,
    pub capabilities_revoked: u64,
    pub invalid_modules: u64,
    pub native_denied: u64,
    pub active_untrusted_flows: u64,
}

impl std::fmt::Display for SecurityCountersSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "denied={} quota={} caps_created={} caps_revoked={} invalid_modules={} native_denied={} untrusted_flows={}",
            self.denied_operations,
            self.quota_exceeded,
            self.capabilities_created,
            self.capabilities_revoked,
            self.invalid_modules,
            self.native_denied,
            self.active_untrusted_flows,
        )
    }
}

/// One live capability as seen by host attestation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityGrantView {
    pub id: CapId,
    pub holder: FlowId,
    pub target: CapTarget,
    pub rights: CapRights,
}

impl CapabilityGrantView {
    pub fn from_entry(id: CapId, cap: &Capability) -> Self {
        Self {
            id,
            holder: cap.holder,
            target: cap.cap.target(),
            rights: cap.cap.rights(),
        }
    }
}

/// Host inspection of what a flow currently holds.
///
/// Not a cryptographic proof of process integrity — only a table snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityAttestation {
    pub flow: FlowId,
    pub trust: TrustLevel,
    pub policy: PolicyFingerprint,
    pub grants: Vec<CapabilityGrantView>,
}

impl CapabilityAttestation {
    pub fn from_table(
        table: &CapTable,
        flow: FlowId,
        trust: TrustLevel,
        policy: PolicyFingerprint,
    ) -> Result<Self, RuntimeError> {
        Ok(Self {
            flow,
            trust,
            policy,
            grants: table.list_held_by(flow)?,
        })
    }
}

/// Compact security event for an optional host audit sink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SecurityEvent {
    CapabilityCreated { holder: u64 },
    CapabilityDelegated { from: u64, to: u64 },
    CapabilityRevoked { flow: u64, removed: usize },
    AccessDenied { reason: &'static str },
    QuotaExceeded { kind: &'static str },
    ModuleRejected { reason: &'static str },
    NativeDenied { index: u32 },
    FlowFinalized { flow: u64 },
}

impl std::fmt::Display for SecurityEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecurityEvent::CapabilityCreated { holder } => {
                write!(f, "cap_created holder={holder}")
            }
            SecurityEvent::CapabilityDelegated { from, to } => {
                write!(f, "cap_delegated from={from} to={to}")
            }
            SecurityEvent::CapabilityRevoked { flow, removed } => {
                write!(f, "cap_revoked flow={flow} removed={removed}")
            }
            SecurityEvent::AccessDenied { reason } => write!(f, "access_denied {reason}"),
            SecurityEvent::QuotaExceeded { kind } => write!(f, "quota_exceeded {kind}"),
            SecurityEvent::ModuleRejected { reason } => write!(f, "module_rejected {reason}"),
            SecurityEvent::NativeDenied { index } => write!(f, "native_denied idx={index}"),
            SecurityEvent::FlowFinalized { flow } => write!(f, "flow_finalized id={flow}"),
        }
    }
}

/// Optional host callback for security events (never receives full CapId tokens).
///
/// The runtime delivers events on a **dedicated audit thread** with a bounded
/// queue. `record` on the host sink may block that
/// thread but must not block scheduler workers. Prefer fast, non-panicking
/// sinks; a full queue drops the newest event (fail-open for observability).
pub trait SecurityAuditSink: Send + Sync + std::fmt::Debug + 'static {
    fn record(&self, event: &SecurityEvent);
}

/// Default capacity for the async audit queue between workers and the sink.
pub const DEFAULT_AUDIT_QUEUE: usize = 1024;

/// Join handle for the audit dispatcher started by [`wrap_audit_sink`].
pub(crate) struct AuditWorkerHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl AuditWorkerHandle {
    pub(crate) fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for AuditWorkerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Debug)]
struct QueuedAuditSink {
    tx: SyncSender<SecurityEvent>,
}

impl SecurityAuditSink for QueuedAuditSink {
    fn record(&self, event: &SecurityEvent) {
        let _ = self.tx.try_send(event.clone());
    }
}

/// Wrap a host [`SecurityAuditSink`] so worker `record` never blocks on it.
///
/// Returns a sink suitable for [`crate::RuntimeConfig::security_audit`] /
/// `Shared`, plus a handle the runtime joins on shutdown (drains remaining
/// events after `stop`).
pub(crate) fn wrap_audit_sink(
    sink: Arc<dyn SecurityAuditSink>,
    capacity: usize,
) -> (Arc<dyn SecurityAuditSink>, AuditWorkerHandle) {
    let capacity = capacity.max(1);
    let (tx, rx) = sync_channel::<SecurityEvent>(capacity);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_worker = Arc::clone(&stop);
    let join = thread::Builder::new()
        .name("byteflow-audit".into())
        .spawn(move || {
            while !stop_worker.load(Ordering::Acquire) {
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(event) => sink.record(&event),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            while let Ok(event) = rx.try_recv() {
                sink.record(&event);
            }
        })
        .ok();
    (
        Arc::new(QueuedAuditSink { tx }),
        AuditWorkerHandle { stop, join },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::TrustLevel;
    use crate::scheduler::mailbox::{MailboxCapacity, MailboxConfig, OverflowPolicy};

    #[test]
    fn sandbox_fingerprint_differs_from_default() {
        let permissive = RuntimeConfig::default();
        let sandboxed = RuntimeConfig::sandbox();
        assert_ne!(
            policy_fingerprint(&permissive),
            policy_fingerprint(&sandboxed)
        );
    }

    #[test]
    fn fingerprint_is_stable_for_identical_config() {
        let a = RuntimeConfig::sandbox();
        let b = RuntimeConfig::sandbox();
        assert_eq!(policy_fingerprint(&a), policy_fingerprint(&b));
    }

    /// Pinned SHA-256 prefixes. A rustc or pointer-width change must not move these.
    #[test]
    fn fingerprint_matches_canonical_sha256_prefix() {
        assert_eq!(
            policy_fingerprint(&RuntimeConfig::default()).to_hex(),
            "2e1be905f47892334e759d3327a05d46"
        );
        assert_eq!(
            policy_fingerprint(&RuntimeConfig::sandbox()).to_hex(),
            "d59bbad5eefb7b525a0cdeaf6708be36"
        );
    }

    #[test]
    fn fingerprint_changes_when_trust_changes() {
        let mut a = RuntimeConfig::sandbox();
        let mut b = RuntimeConfig::sandbox();
        a.trust = TrustLevel::Untrusted;
        b.trust = TrustLevel::Restricted;
        assert_ne!(policy_fingerprint(&a), policy_fingerprint(&b));
    }

    #[test]
    fn fingerprint_changes_when_mailbox_changes() {
        let a = RuntimeConfig::sandbox();
        let mut b = RuntimeConfig::sandbox();
        b.mailbox = MailboxConfig::new(MailboxCapacity::DEFAULT, OverflowPolicy::DropNewest);
        assert_ne!(policy_fingerprint(&a), policy_fingerprint(&b));
    }
}
