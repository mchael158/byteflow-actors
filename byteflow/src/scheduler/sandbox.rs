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

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::bytecode::{CapId, CapRights, CapTarget, TrustLevel};

use super::capability::{CapTable, Capability};
use super::error::RuntimeError;
use super::process::FlowId;
use super::quota::QuotaConfig;
use super::runtime::RuntimeConfig;

/// Domain separator for [`PolicyFingerprint`] (not a secret).
const POLICY_DOMAIN: u64 = 0xBF51_5341_4E44_5031; // "BFQSANDP1"

/// Stable identity of the security-relevant knobs on a [`RuntimeConfig`].
///
/// Useful for audit trails ("which policy admitted this run?"). It is **not**
/// a cryptographic attestation of process integrity.
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
pub fn policy_fingerprint(config: &RuntimeConfig) -> PolicyFingerprint {
    let mut hasher = DefaultHasher::new();
    POLICY_DOMAIN.hash(&mut hasher);
    trust_tag(config.trust).hash(&mut hasher);
    root_authority_tag(config.root_authority).hash(&mut hasher);
    config.max_flows.hash(&mut hasher);
    config.max_ask_waits.hash(&mut hasher);
    config.max_host_awaits.hash(&mut hasher);
    config.max_links.hash(&mut hasher);
    config.max_monitors.hash(&mut hasher);
    config.max_registry_names.hash(&mut hasher);
    config.max_runtime_bytes.hash(&mut hasher);
    config.mailbox.capacity().get().hash(&mut hasher);
    config.mailbox.bytes().get().hash(&mut hasher);
    (config.mailbox.overflow() as u8).hash(&mut hasher);
    hash_quota(&mut hasher, &config.quota);
    let lo = hasher.finish();
    // Second lane so the digest is 128-bit without a crypto dependency.
    let mut hasher2 = DefaultHasher::new();
    POLICY_DOMAIN.hash(&mut hasher2);
    lo.hash(&mut hasher2);
    hash_quota(&mut hasher2, &config.quota);
    config.max_runtime_bytes.hash(&mut hasher2);
    let hi = hasher2.finish();
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&lo.to_le_bytes());
    out[8..].copy_from_slice(&hi.to_le_bytes());
    PolicyFingerprint(out)
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

fn hash_quota(hasher: &mut DefaultHasher, q: &QuotaConfig) {
    q.cpu_budget.hash(hasher);
    q.mem_limit.hash(hasher);
    q.spawn_burst.hash(hasher);
    q.spawn_per_sec.hash(hasher);
    q.send_burst.hash(hasher);
    q.send_per_sec.hash(hasher);
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
pub trait SecurityAuditSink: Send + Sync + std::fmt::Debug + 'static {
    fn record(&self, event: &SecurityEvent);
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
