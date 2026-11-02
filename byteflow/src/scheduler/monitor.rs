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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::error::{LifecycleError, RuntimeError};
use super::process::FlowId;
use super::sync_lock;

/// Opaque monitor reference, echoed on the `DOWN` hop as `request_id`.
///
/// The inner id is runtime-minted (`pub(crate)`): bytecode only ever sees
/// it as `Value::Int` after `Monitor`, never as a host-constructed token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MonitorRef(pub(crate) u64);

/// Bytecode stores [`MonitorRef`] as [`crate::Value::Int`]; never mint past this.
const MAX_MONITOR_ID: u64 = i64::MAX as u64;

static NEXT_MONITOR: AtomicU64 = AtomicU64::new(1);
static MONITOR_IDS_EXHAUSTED: AtomicBool = AtomicBool::new(false);

fn try_next_monitor_id() -> Result<MonitorRef, RuntimeError> {
    if MONITOR_IDS_EXHAUSTED.load(Ordering::Relaxed) {
        return Err(RuntimeError::MonitorIdExhausted);
    }
    let id = NEXT_MONITOR.fetch_add(1, Ordering::Relaxed);
    if id == 0 || id > MAX_MONITOR_ID {
        MONITOR_IDS_EXHAUSTED.store(true, Ordering::Relaxed);
        return Err(RuntimeError::MonitorIdExhausted);
    }
    Ok(MonitorRef(id))
}

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
    /// Host-wide runtime teardown (parked HostAwait drain, etc.).
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
///
/// `by_owner` / `by_target` are adjacency indexes so owner-exit and
/// `DOWN` delivery are O(degree), not a full-table scan.
pub struct MonitorTable {
    monitors: HashMap<MonitorRef, MonitorEntry>,
    by_owner: HashMap<FlowId, Vec<MonitorRef>>,
    by_target: HashMap<FlowId, Vec<MonitorRef>>,
}

impl MonitorTable {
    pub fn new() -> Self {
        Self {
            monitors: HashMap::new(),
            by_owner: HashMap::new(),
            by_target: HashMap::new(),
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.monitors.len()
    }

    fn index_add(map: &mut HashMap<FlowId, Vec<MonitorRef>>, flow: FlowId, id: MonitorRef) {
        map.entry(flow).or_default().push(id);
    }

    fn index_remove(map: &mut HashMap<FlowId, Vec<MonitorRef>>, flow: FlowId, id: MonitorRef) {
        if let Some(ids) = map.get_mut(&flow) {
            ids.retain(|existing| *existing != id);
            if ids.is_empty() {
                map.remove(&flow);
            }
        }
    }

    fn insert(&mut self, monitor: MonitorRef, owner: FlowId, target: FlowId) {
        self.monitors
            .insert(monitor, MonitorEntry { owner, target });
        Self::index_add(&mut self.by_owner, owner, monitor);
        Self::index_add(&mut self.by_target, target, monitor);
    }

    fn take(&mut self, monitor: MonitorRef) -> Option<MonitorEntry> {
        let entry = self.monitors.remove(&monitor)?;
        Self::index_remove(&mut self.by_owner, entry.owner, monitor);
        Self::index_remove(&mut self.by_target, entry.target, monitor);
        Some(entry)
    }

    #[cfg(test)]
    pub fn create(&mut self, owner: FlowId, target: FlowId) -> Result<MonitorRef, RuntimeError> {
        let monitor = try_next_monitor_id()?;
        self.insert(monitor, owner, target);
        Ok(monitor)
    }

    /// Remove `monitor` only if `owner` still owns it.
    pub fn remove_owned(
        &mut self,
        owner: FlowId,
        monitor: MonitorRef,
    ) -> Result<(), LifecycleError> {
        match self.monitors.get(&monitor) {
            Some(entry) if entry.owner == owner => {
                let _ = self.take(monitor);
                Ok(())
            }
            Some(_) => Err(LifecycleError::NotOwner),
            None => Err(LifecycleError::InvalidMonitor),
        }
    }

    /// Drop every monitor whose **owner** is `flow` (owner exited).
    pub fn remove_owned_by(&mut self, owner: FlowId) {
        let Some(ids) = self.by_owner.remove(&owner) else {
            return;
        };
        for id in ids {
            if let Some(entry) = self.monitors.remove(&id) {
                Self::index_remove(&mut self.by_target, entry.target, id);
            }
        }
    }

    /// Collect `DOWN` events for monitors watching `target`, then drop them.
    pub fn notify_target_exit(&mut self, target: FlowId, reason: FlowExitReason) -> Vec<DownEvent> {
        let Some(ids) = self.by_target.remove(&target) else {
            return Vec::new();
        };
        let mut events = Vec::with_capacity(ids.len());
        for monitor in ids {
            let Some(entry) = self.monitors.remove(&monitor) else {
                continue;
            };
            Self::index_remove(&mut self.by_owner, entry.owner, monitor);
            events.push(DownEvent {
                monitor,
                owner: entry.owner,
                target,
                reason,
            });
        }
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
///
/// Process-wide monitor quota lives here (`table.len()` under the same lock).
pub struct MonitorStore {
    inner: std::sync::Mutex<MonitorTable>,
    max_monitors: u32,
}

impl MonitorStore {
    /// `max_monitors == 0` → unlimited.
    pub fn new(max_monitors: u32) -> Self {
        Self {
            inner: std::sync::Mutex::new(MonitorTable::new()),
            max_monitors,
        }
    }

    pub fn create(
        &self,
        owner: FlowId,
        target: FlowId,
    ) -> Result<Result<MonitorRef, LifecycleError>, RuntimeError> {
        let mut table = sync_lock::lock(&self.inner, "MonitorStore::create")?;
        if self.max_monitors != 0 && table.len() >= self.max_monitors as usize {
            return Ok(Err(LifecycleError::MonitorLimitReached {
                limit: self.max_monitors,
            }));
        }
        let monitor = try_next_monitor_id()?;
        table.insert(monitor, owner, target);
        Ok(Ok(monitor))
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
        Self::new(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::process::next_flow_id;

    #[test]
    fn notify_removes_and_sorts() -> Result<(), RuntimeError> {
        let mut table = MonitorTable::new();
        let owner = next_flow_id();
        let target = next_flow_id();
        let a = table.create(owner, target)?;
        let b = table.create(owner, target)?;
        let events = table.notify_target_exit(target, FlowExitReason::Fault);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].monitor, a);
        assert_eq!(events[1].monitor, b);
        assert!(table
            .notify_target_exit(target, FlowExitReason::Fault)
            .is_empty());
        Ok(())
    }

    #[test]
    fn remove_owned_rejects_other_flow() -> Result<(), RuntimeError> {
        let mut table = MonitorTable::new();
        let owner = next_flow_id();
        let other = next_flow_id();
        let target = next_flow_id();
        let mon = table.create(owner, target)?;
        assert_eq!(
            table.remove_owned(other, mon),
            Err(LifecycleError::NotOwner)
        );
        assert!(table.remove_owned(owner, mon).is_ok());
        Ok(())
    }

    #[test]
    fn monitor_limit_is_process_wide_and_released() -> Result<(), Box<dyn std::error::Error>> {
        let store = MonitorStore::new(1);
        let owner = next_flow_id();
        let t1 = next_flow_id();
        let t2 = next_flow_id();
        let first = store.create(owner, t1)??;
        let second = store.create(owner, t2)?;
        assert!(matches!(
            second,
            Err(LifecycleError::MonitorLimitReached { limit: 1 })
        ));
        store.remove_owned(owner, first)??;
        match store.create(owner, t2)? {
            Ok(_) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}
