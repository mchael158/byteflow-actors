//! Monitor table: one-way lifecycle watches (`A ──monitor──> B`).
//!
//! A monitor is a **runtime relation**, not a field on [`crate::Message`].
//! When the target exits, [`MonitorTable::notify_target_exit`] yields
//! [`DownEvent`]s that [`super::finalize`] delivers as Atomic Hop
//! [`crate::TAG_SYS_DOWN`] messages.
//!
//! Relations are keyed by [`FlowId`] (identity). Bytecode addresses the
//! target through a Cap; the worker resolves Cap → FlowId before calling
//! here. Caps are revoked on exit; FlowIds are never reused.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use super::error::{LifecycleError, RuntimeError};
use super::process::FlowId;
use super::sync_lock;

/// Opaque monitor reference, echoed on the `DOWN` hop as `request_id`.
///
/// The inner id is runtime-minted (`pub(crate)`): bytecode only ever sees
/// it as `Value::Int` after `Monitor`, never as a host-constructed token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MonitorRef(pub(crate) u64);

static NEXT_MONITOR: AtomicU64 = AtomicU64::new(1);

impl MonitorRef {
    #[inline]
    pub fn as_u64(self) -> u64 {
        self.0
    }

    #[inline]
    pub(crate) fn from_u64(raw: u64) -> Self {
        Self(raw)
    }
}

impl std::fmt::Display for MonitorRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "monitor#{}", self.0)
    }
}

/// Why a flow left the directory. Encoded as `Message.payload` on system hops.
///
/// Named [`FlowExitReason`] so it does not collide with the JIT
/// `ExitReason` (trace-compiler control flow).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FlowExitReason {
    Normal = 0,
    /// Reserved: host-wide runtime teardown (not produced today).
    Shutdown = 1,
    Killed = 2,
    Fault = 3,
    /// Reserved: overflow-kill policy (today overflow is `MailboxFull` / park).
    MailboxOverflow = 4,
    Supervisor = 5,
    Link = 6,
}

impl FlowExitReason {
    pub fn from_u64(raw: u64) -> Option<Self> {
        Some(match raw {
            0 => Self::Normal,
            1 => Self::Shutdown,
            2 => Self::Killed,
            3 => Self::Fault,
            4 => Self::MailboxOverflow,
            5 => Self::Supervisor,
            6 => Self::Link,
            _ => return None,
        })
    }

    #[inline]
    pub fn as_u64(self) -> u64 {
        self as u64
    }

    /// Abnormal exits propagate through links (BEAM: `normal` does not).
    ///
    /// `Shutdown` / `Killed` / `Supervisor` are host-policy reasons;
    /// they still count as abnormal so a supervisor abort is not
    /// silently swallowed by links.
    #[inline]
    pub fn is_abnormal(self) -> bool {
        !matches!(self, Self::Normal)
    }
}

impl std::fmt::Display for FlowExitReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Normal => "normal",
            Self::Shutdown => "shutdown",
            Self::Killed => "killed",
            Self::Fault => "fault",
            Self::MailboxOverflow => "mailbox-overflow",
            Self::Supervisor => "supervisor",
            Self::Link => "link",
        };
        f.write_str(name)
    }
}

/// One monitor firing after the target left the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownEvent {
    pub monitor: MonitorRef,
    pub owner: FlowId,
    pub target: FlowId,
    pub reason: FlowExitReason,
}

#[derive(Debug, Clone, Copy)]
struct MonitorEntry {
    owner: FlowId,
    target: FlowId,
}

/// `MonitorRef → { owner, target }`. Lives on [`super::runtime::Shared`].
pub struct MonitorTable {
    monitors: HashMap<MonitorRef, MonitorEntry>,
}

impl MonitorTable {
    pub fn new() -> Self {
        Self {
            monitors: HashMap::new(),
        }
    }

    pub fn create(&mut self, owner: FlowId, target: FlowId) -> MonitorRef {
        let monitor = MonitorRef(NEXT_MONITOR.fetch_add(1, Ordering::Relaxed));
        self.monitors
            .insert(monitor, MonitorEntry { owner, target });
        monitor
    }

    /// Remove `monitor` only if `owner` still owns it.
    pub fn remove_owned(
        &mut self,
        owner: FlowId,
        monitor: MonitorRef,
    ) -> Result<(), LifecycleError> {
        match self.monitors.get(&monitor) {
            Some(entry) if entry.owner == owner => {
                self.monitors.remove(&monitor);
                Ok(())
            }
            Some(_) => Err(LifecycleError::NotOwner),
            None => Err(LifecycleError::InvalidMonitor),
        }
    }

    /// Drop every monitor whose **owner** is `flow` (owner exited).
    pub fn remove_owned_by(&mut self, owner: FlowId) {
        self.monitors.retain(|_, entry| entry.owner != owner);
    }

    /// Collect `DOWN` events for monitors watching `target`, then drop them.
    pub fn notify_target_exit(&mut self, target: FlowId, reason: FlowExitReason) -> Vec<DownEvent> {
        let mut events = Vec::new();
        self.monitors.retain(|monitor, entry| {
            if entry.target != target {
                return true;
            }
            events.push(DownEvent {
                monitor: *monitor,
                owner: entry.owner,
                target,
                reason,
            });
            false
        });
        events.sort_unstable_by_key(|event| event.monitor.0);
        events
    }
}

impl Default for MonitorTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Fail-closed wrapper around [`MonitorTable`].
pub struct MonitorStore {
    inner: std::sync::Mutex<MonitorTable>,
}

impl MonitorStore {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(MonitorTable::new()),
        }
    }

    pub fn create(&self, owner: FlowId, target: FlowId) -> Result<MonitorRef, RuntimeError> {
        Ok(sync_lock::lock(&self.inner, "MonitorStore::create")?.create(owner, target))
    }

    pub fn remove_owned(
        &self,
        owner: FlowId,
        monitor: MonitorRef,
    ) -> Result<Result<(), LifecycleError>, RuntimeError> {
        Ok(
            sync_lock::lock(&self.inner, "MonitorStore::remove_owned")?
                .remove_owned(owner, monitor),
        )
    }

    pub fn remove_owned_by(&self, owner: FlowId) -> Result<(), RuntimeError> {
        sync_lock::lock(&self.inner, "MonitorStore::remove_owned_by")?.remove_owned_by(owner);
        Ok(())
    }

    pub fn notify_target_exit(
        &self,
        target: FlowId,
        reason: FlowExitReason,
    ) -> Result<Vec<DownEvent>, RuntimeError> {
        Ok(
            sync_lock::lock(&self.inner, "MonitorStore::notify_target_exit")?
                .notify_target_exit(target, reason),
        )
    }
}

impl Default for MonitorStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::process::next_flow_id;

    #[test]
    fn notify_removes_and_sorts() {
        let mut table = MonitorTable::new();
        let owner = next_flow_id();
        let target = next_flow_id();
        let a = table.create(owner, target);
        let b = table.create(owner, target);
        let events = table.notify_target_exit(target, FlowExitReason::Fault);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].monitor, a);
        assert_eq!(events[1].monitor, b);
        assert!(table
            .notify_target_exit(target, FlowExitReason::Fault)
            .is_empty());
    }

    #[test]
    fn remove_owned_rejects_other_flow() {
        let mut table = MonitorTable::new();
        let owner = next_flow_id();
        let other = next_flow_id();
        let target = next_flow_id();
        let mon = table.create(owner, target);
        assert_eq!(
            table.remove_owned(other, mon),
            Err(LifecycleError::NotOwner)
        );
        assert!(table.remove_owned(owner, mon).is_ok());
    }
}
