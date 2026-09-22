//! Single choke-point for flow termination.
//!
//! Workers call [`finalize_flow`] instead of scattering revoke / DOWN /
//! link / registry / supervisor cleanup. The VM only produces an outcome;
//! this module owns the lifecycle transition.
//!
//! Finalization is **iterative** (a work list): linked peers and stranded
//! `WAITING_SEND` senders are queued rather than recursed, so a wide link
//! graph cannot blow the worker stack.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Mutex;

use crate::bytecode::{Message, Value};
use crate::log;

use super::error::{report_fault, RuntimeError};
use super::mailbox::Delivery;
use super::metrics::RuntimeMetrics;
use super::monitor::{DownEvent, FlowExitReason};
use super::process::{Flow, FlowId, FlowOutcome};
use super::runtime::{wake_workers, Shared};
use super::sync_lock;

/// Per-flow BEAM-style `trap_exit` flag (default off).
///
/// When set on a flow, **that** flow receives a [`crate::TAG_SYS_EXIT`] hop
/// instead of being killed when a linked peer exits (including `Normal`).
pub struct TrapExitFlags {
    inner: Mutex<HashSet<FlowId>>,
}

impl TrapExitFlags {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashSet::new()),
        }
    }

    pub fn set(&self, id: FlowId, enabled: bool) -> Result<(), RuntimeError> {
        let mut set = sync_lock::lock(&self.inner, "TrapExitFlags::set")?;
        if enabled {
            set.insert(id);
        } else {
            set.remove(&id);
        }
        Ok(())
    }

    pub fn is_enabled(&self, id: FlowId) -> Result<bool, RuntimeError> {
        Ok(sync_lock::lock(&self.inner, "TrapExitFlags::is_enabled")?.contains(&id))
    }

    pub fn clear(&self, id: FlowId) -> Result<(), RuntimeError> {
        sync_lock::lock(&self.inner, "TrapExitFlags::clear")?.remove(&id);
        Ok(())
    }
}

impl Default for TrapExitFlags {
    fn default() -> Self {
        Self::new()
    }
}

/// Kill / linked-exit signal consumed at the start of a worker quantum.
pub struct KillSignals {
    inner: Mutex<HashMap<FlowId, FlowExitReason>>,
}

impl KillSignals {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn set(&self, id: FlowId, reason: FlowExitReason) -> Result<(), RuntimeError> {
        sync_lock::lock(&self.inner, "KillSignals::set")?.insert(id, reason);
        Ok(())
    }

    pub fn take(&self, id: FlowId) -> Result<Option<FlowExitReason>, RuntimeError> {
        Ok(sync_lock::lock(&self.inner, "KillSignals::take")?.remove(&id))
    }
}

/// sender FlowId → target FlowId whose mailbox holds a `WAITING_SEND`.
pub struct WaitingSendIndex {
    inner: Mutex<HashMap<FlowId, FlowId>>,
}

impl WaitingSendIndex {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn insert(&self, sender: FlowId, target: FlowId) -> Result<(), RuntimeError> {
        sync_lock::lock(&self.inner, "WaitingSendIndex::insert")?.insert(sender, target);
        Ok(())
    }

    pub fn remove(&self, sender: FlowId) -> Result<Option<FlowId>, RuntimeError> {
        Ok(sync_lock::lock(&self.inner, "WaitingSendIndex::remove")?.remove(&sender))
    }

    pub fn get(&self, sender: FlowId) -> Result<Option<FlowId>, RuntimeError> {
        Ok(sync_lock::lock(&self.inner, "WaitingSendIndex::get")?
            .get(&sender)
            .copied())
    }
}

/// Askers parked on their own mailbox waiting for a reply from `target`.
///
/// When the target exits, [`take_waiters_of`] lets finalize resume those
/// waiters with [`crate::TAG_SYS_EXIT`] instead of leaving them parked forever.
///
/// At most one in-flight `Ask` per asker (`request_id` still pending). The
/// process-wide table size is capped by [`AskWaitIndex::new`]'s `max`
/// (`0` = unlimited), mirroring [`crate::RuntimeConfig::max_ask_waits`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AskInsertError {
    /// The asker already has an in-flight `Ask`.
    Duplicate,
    /// Process-wide outstanding-Ask budget exhausted.
    LimitReached { current: usize, max: usize },
}

impl std::fmt::Display for AskInsertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AskInsertError::Duplicate => f.write_str("duplicate in-flight Ask request_id"),
            AskInsertError::LimitReached { current, max } => {
                write!(f, "Ask wait limit reached ({current}/{max})")
            }
        }
    }
}

pub struct AskWaitIndex {
    inner: Mutex<AskWaitInner>,
    /// `0` = unlimited.
    max: usize,
}

struct AskWaitInner {
    by_asker: HashMap<FlowId, FlowId>,
    by_target: HashMap<FlowId, HashSet<FlowId>>,
    request_ids: HashMap<FlowId, u64>,
}

impl AskWaitIndex {
    /// `max_ask_waits == 0` → unlimited outstanding waits.
    pub fn new(max_ask_waits: u32) -> Self {
        Self {
            inner: Mutex::new(AskWaitInner {
                by_asker: HashMap::new(),
                by_target: HashMap::new(),
                request_ids: HashMap::new(),
            }),
            max: max_ask_waits as usize,
        }
    }

    pub fn insert(
        &self,
        asker: FlowId,
        target: FlowId,
        request_id: u64,
    ) -> Result<Result<(), AskInsertError>, RuntimeError> {
        let mut g = sync_lock::lock(&self.inner, "AskWaitIndex::insert")?;
        if g.request_ids.contains_key(&asker) {
            return Ok(Err(AskInsertError::Duplicate));
        }
        if self.max != 0 && g.by_asker.len() >= self.max {
            return Ok(Err(AskInsertError::LimitReached {
                current: g.by_asker.len(),
                max: self.max,
            }));
        }
        g.request_ids.insert(asker, request_id);
        g.by_asker.insert(asker, target);
        g.by_target.entry(target).or_default().insert(asker);
        Ok(Ok(()))
    }

    pub fn remove_asker(&self, asker: FlowId) -> Result<Option<FlowId>, RuntimeError> {
        let mut g = sync_lock::lock(&self.inner, "AskWaitIndex::remove_asker")?;
        g.request_ids.remove(&asker);
        let Some(target) = g.by_asker.remove(&asker) else {
            return Ok(None);
        };
        if let Some(set) = g.by_target.get_mut(&target) {
            set.remove(&asker);
            if set.is_empty() {
                g.by_target.remove(&target);
            }
        }
        Ok(Some(target))
    }

    /// Target this asker is registered against, if any.
    ///
    /// Used by `park_ask` after `park_filter` to close the race where
    /// [`take_waiters_of`] removes the asker while they are still between
    /// `insert` and park (`take_parked` was still `None`).
    pub fn target_of(&self, asker: FlowId) -> Result<Option<FlowId>, RuntimeError> {
        let g = sync_lock::lock(&self.inner, "AskWaitIndex::target_of")?;
        Ok(g.by_asker.get(&asker).copied())
    }

    pub fn take_waiters_of(&self, target: FlowId) -> Result<Vec<FlowId>, RuntimeError> {
        let mut g = sync_lock::lock(&self.inner, "AskWaitIndex::take_waiters_of")?;
        let Some(set) = g.by_target.remove(&target) else {
            return Ok(Vec::new());
        };
        for asker in &set {
            g.by_asker.remove(asker);
            g.request_ids.remove(asker);
        }
        Ok(set.into_iter().collect())
    }
}

struct PendingExit {
    flow: Flow,
    outcome: FlowOutcome,
    reason: FlowExitReason,
}

/// Unregister, notify monitors, propagate links, sweep registry, complete join.
pub(crate) fn finalize_flow(
    shared: &Shared,
    flow: Flow,
    outcome: FlowOutcome,
    reason: FlowExitReason,
) {
    let mut work = vec![PendingExit {
        flow,
        outcome,
        reason,
    }];
    while let Some(pending) = work.pop() {
        finalize_one(shared, pending, &mut work);
    }
}

/// Write `value` into `dest_reg` after a scheduler effect / handoff.
///
/// On [`crate::vm::Fault`] (register OOB, quota exceeded, …) the flow is
/// finalized as failed — callers must not keep running or re-enqueue it.
/// Returns the flow only when the write succeeded.
pub(crate) fn resume_or_fail(
    shared: &Shared,
    mut flow: Box<Flow>,
    dest_reg: u8,
    value: Value,
) -> Option<Box<Flow>> {
    match flow.vm.resume_with(dest_reg, value) {
        Ok(()) => Some(flow),
        Err(fault) => {
            finalize_flow(
                shared,
                *flow,
                FlowOutcome::Failed(fault.to_string()),
                FlowExitReason::Fault,
            );
            None
        }
    }
}

fn finalize_one(shared: &Shared, mut pending: PendingExit, work: &mut Vec<PendingExit>) {
    let id = pending.flow.id;
    let reason = pending.reason;

    // Host FlowId is never spawned; refuse so a bug cannot revoke host Caps.
    if id.is_host() {
        report_fault(RuntimeError::CannotFinalizeHostFlow);
        return;
    }

    log::info(format!(
        "finalize flow#{id} reason={reason} outcome={:?}",
        pending.outcome
    ));

    if let Err(e) = shared.kill_signals.take(id) {
        report_fault(e);
    }
    if let Err(e) = shared.trap_exits.clear(id) {
        report_fault(e);
    }
    if let Err(e) = shared.waiting_send_at.remove(id) {
        report_fault(e);
    }
    if let Err(e) = shared.ask_waits.remove_asker(id) {
        report_fault(e);
    }

    if let Ok(Some(mailbox)) = shared.directory.lookup(id) {
        match mailbox.close() {
            Ok(senders) => {
                for sender in senders {
                    if let Err(e) = shared.waiting_send_at.remove(sender.id) {
                        report_fault(e);
                    }
                    work.push(PendingExit {
                        flow: sender,
                        outcome: FlowOutcome::Failed(format!("send target {id} exited")),
                        reason: FlowExitReason::Fault,
                    });
                }
            }
            Err(e) => report_fault(e),
        }
    }

    wake_orphaned_asks(shared, id, reason);

    if let Err(e) = shared.caps.revoke_flow(id) {
        report_fault(e);
    }
    match shared.quotas.remove(id) {
        Ok(Some(q)) => {
            let used = q.mem_used();
            if used > 0 {
                shared.memory.release(used);
            }
        }
        Ok(None) => {}
        Err(e) => report_fault(e),
    }
    if let Err(e) = shared.directory.unregister(id) {
        report_fault(e);
    }
    // Paired with `flow_limit.try_reserve` in `spawn_on`.
    shared.flow_limit.release();
    if let Err(e) = shared.monitors.remove_owned_by(id) {
        report_fault(e);
    }
    if let Err(e) = shared.registry.unregister_flow(id) {
        report_fault(e);
    }

    let downs = match shared.monitors.notify_target_exit(id, reason) {
        Ok(events) => events,
        Err(e) => {
            report_fault(e);
            Vec::new()
        }
    };
    for event in downs {
        deliver_down(shared, event);
    }

    let peers = match shared.links.remove_links_of(id) {
        Ok(list) => list,
        Err(e) => {
            report_fault(e);
            Vec::new()
        }
    };
    for (_, peer) in peers {
        let trapping = match shared.trap_exits.is_enabled(peer) {
            Ok(v) => v,
            Err(e) => {
                report_fault(e);
                false
            }
        };
        if trapping {
            // BEAM: every exit signal becomes {'EXIT', From, Reason}.
            deliver_exit(shared, peer, id, reason);
        } else if reason.is_abnormal() {
            collect_link_exit(shared, peer, work);
        }
    }

    if matches!(pending.outcome, FlowOutcome::Completed(_)) {
        RuntimeMetrics::inc(&shared.metrics.processes_completed);
    } else {
        RuntimeMetrics::inc(&shared.metrics.processes_failed);
    }
    if let Some(link) = pending.flow.supervisor.take() {
        link.notify(id, pending.outcome.clone(), pending.flow.restart_policy);
    }
    pending.flow.complete(pending.outcome);
}

pub(crate) fn deliver_down(shared: &Shared, event: DownEvent) {
    let hop = Value::Message(Message::down(
        event.monitor.as_u64(),
        event.target.as_u64(),
        event.reason.as_u64(),
    ));
    deliver_system_hop(shared, event.owner, hop);
}

/// Linked-exit notice for a peer with `trap_exit` enabled.
fn deliver_exit(shared: &Shared, owner: FlowId, dead: FlowId, reason: FlowExitReason) {
    let hop = Value::Message(Message::linked_exit(dead.as_u64(), reason.as_u64()));
    deliver_system_hop(shared, owner, hop);
}

fn deliver_system_hop(shared: &Shared, owner: FlowId, hop: Value) {
    let mailbox = match shared.directory.lookup(owner) {
        Ok(Some(m)) => m,
        Ok(None) => return,
        Err(e) => {
            report_fault(e);
            return;
        }
    };
    match mailbox.push_system(hop.clone()) {
        Ok(Delivery::Handoff(parked)) => {
            let _ = shared.ask_waits.remove_asker(parked.id);
            match parked.last_receive_dest {
                Some(dest) => {
                    let Some(parked) = resume_or_fail(shared, parked, dest, hop) else {
                        return;
                    };
                    parked
                        .metrics
                        .messages_received
                        .fetch_add(1, Ordering::Relaxed);
                    shared.injector.push(parked);
                    wake_workers(shared);
                }
                None => {
                    finalize_flow(
                        shared,
                        *parked,
                        FlowOutcome::Failed("handoff missing dest register".into()),
                        FlowExitReason::Fault,
                    );
                }
            }
        }
        Ok(_) => {}
        Err(e) => report_fault(e),
    }
}

fn wake_orphaned_asks(shared: &Shared, target: FlowId, reason: FlowExitReason) {
    let waiters = match shared.ask_waits.take_waiters_of(target) {
        Ok(w) => w,
        Err(e) => {
            report_fault(e);
            return;
        }
    };
    if waiters.is_empty() {
        return;
    }
    let hop = Value::Message(Message::linked_exit(target.as_u64(), reason.as_u64()));
    for asker in waiters {
        let mailbox = match shared.directory.lookup(asker) {
            Ok(Some(m)) => m,
            Ok(None) => continue,
            Err(e) => {
                report_fault(e);
                continue;
            }
        };
        match mailbox.take_parked() {
            Ok(Some(flow)) => match flow.last_receive_dest {
                Some(dest) => {
                    let Some(flow) = resume_or_fail(shared, flow, dest, hop.clone()) else {
                        continue;
                    };
                    flow.metrics
                        .messages_received
                        .fetch_add(1, Ordering::Relaxed);
                    shared.injector.push(flow);
                    wake_workers(shared);
                }
                None => {
                    finalize_flow(
                        shared,
                        *flow,
                        FlowOutcome::Failed("handoff missing dest register".into()),
                        FlowExitReason::Fault,
                    );
                }
            },
            // Not parked yet: `park_ask` inserts into the index *before*
            // `park_filter`. That asker revalidates via `target_of` after
            // parking and self-wakes with `TAG_SYS_EXIT` if membership is gone.
            Ok(None) => {}
            Err(e) => report_fault(e),
        }
    }
}

/// Set a kill signal and pull the flow out if it is parked (receive or
/// `WAITING_SEND`). A running flow stays on its worker and dies at the
/// next quantum (cooperative preemption).
pub(crate) fn extract_for_kill(
    shared: &Shared,
    id: FlowId,
    reason: FlowExitReason,
) -> Option<Flow> {
    let mailbox = match shared.directory.lookup(id) {
        Ok(Some(m)) => m,
        Ok(None) => return None,
        Err(e) => {
            report_fault(e);
            return None;
        }
    };
    if let Err(e) = shared.kill_signals.set(id, reason) {
        report_fault(e);
        return None;
    }

    match mailbox.take_parked() {
        Ok(Some(parked)) => return Some(*parked),
        Ok(None) => {}
        Err(e) => report_fault(e),
    }

    let target = match shared.waiting_send_at.get(id) {
        Ok(Some(t)) => t,
        Ok(None) => return None,
        Err(e) => {
            report_fault(e);
            return None;
        }
    };
    let mb = match shared.directory.lookup(target) {
        Ok(Some(m)) => m,
        Ok(None) => return None,
        Err(e) => {
            report_fault(e);
            return None;
        }
    };
    match mb.take_waiting_sender(id) {
        Ok(Some(sender)) => {
            if let Err(e) = shared.waiting_send_at.remove(id) {
                report_fault(e);
            }
            Some(*sender)
        }
        Ok(None) => None,
        Err(e) => {
            report_fault(e);
            None
        }
    }
}

/// Host / supervisor abort: finalize immediately when parked, otherwise
/// the next worker quantum consumes the signal.
pub(crate) fn request_kill(shared: &Shared, id: FlowId, reason: FlowExitReason) {
    if let Some(flow) = extract_for_kill(shared, id, reason) {
        finalize_flow(
            shared,
            flow,
            FlowOutcome::Failed(format!("killed ({reason})")),
            reason,
        );
    }
}

fn collect_link_exit(shared: &Shared, peer: FlowId, work: &mut Vec<PendingExit>) {
    if let Some(flow) = extract_for_kill(shared, peer, FlowExitReason::Link) {
        work.push(PendingExit {
            flow,
            outcome: FlowOutcome::Failed("linked exit (link)".into()),
            reason: FlowExitReason::Link,
        });
    }
}

#[cfg(test)]
mod ask_wait_tests {
    use super::*;

    #[test]
    fn take_waiters_clears_membership_before_park() -> Result<(), RuntimeError> {
        // Simulates the insert→park race: finalize's take_waiters_of runs
        // while the asker is indexed but not yet parked. park_ask must see
        // target_of == None and self-wake.
        let idx = AskWaitIndex::new(0);
        let asker = FlowId(7);
        let target = FlowId(9);
        assert!(idx.insert(asker, target, 1)?.is_ok());
        assert_eq!(idx.target_of(asker)?, Some(target));

        let waiters = idx.take_waiters_of(target)?;
        assert_eq!(waiters, vec![asker]);
        assert_eq!(idx.target_of(asker)?, None);
        assert!(idx.take_waiters_of(target)?.is_empty());
        Ok(())
    }

    #[test]
    fn target_of_tracks_insert_and_remove() -> Result<(), RuntimeError> {
        let idx = AskWaitIndex::new(0);
        let asker = FlowId(3);
        let target = FlowId(5);
        assert_eq!(idx.target_of(asker)?, None);
        assert!(idx.insert(asker, target, 42)?.is_ok());
        assert_eq!(idx.target_of(asker)?, Some(target));
        assert_eq!(idx.remove_asker(asker)?, Some(target));
        assert_eq!(idx.target_of(asker)?, None);
        Ok(())
    }

    #[test]
    fn insert_rejects_when_at_process_limit() -> Result<(), RuntimeError> {
        let idx = AskWaitIndex::new(1);
        let a = FlowId(1);
        let b = FlowId(2);
        let target = FlowId(9);
        assert!(idx.insert(a, target, 1)?.is_ok());
        assert!(matches!(
            idx.insert(b, target, 2)?,
            Err(AskInsertError::LimitReached { current: 1, max: 1 })
        ));
        assert_eq!(idx.remove_asker(a)?, Some(target));
        assert!(idx.insert(b, target, 2)?.is_ok());
        Ok(())
    }
}
