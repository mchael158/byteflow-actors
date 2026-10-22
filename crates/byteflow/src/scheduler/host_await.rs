//! Host ↔ flow async bridge (`Opcode::HostAwait`).
//!
//! The worker registers one ticket per operation and keeps running the flow.
//! The destination register receives that ticket as `Int`. Completion is an
//! authenticated host hop ([`crate::TAG_SYS_AWAIT`], `request_id` = ticket),
//! the same stamp and capability reissue as [`crate::Runtime::send`]. Bytecode
//! reads the result with `Receive`. A cap the table does not know fails the
//! flow instead of entering the mailbox.
//!
//! Several tickets may be outstanding for one flow. Finalize, kill, and
//! runtime shutdown cancel every ticket of that flow. A late `complete`
//! finds the ticket gone and returns [`HostAwaitError::Stale`]. The host
//! callback runs after the registry lock is released.
//!
//! The bridge itself must return quickly from [`HostAwaitBridge::submit`];
//! heavy work belongs on a host thread / Tokio task outside this crate.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::bytecode::{Message, Value, TAG_SYS_AWAIT};

use super::deterministic::{replay_label, StagedHostHop};
use super::error::{report_fault, RuntimeError};
use super::finalize::{extract_for_kill, finalize_flow, resume_handoff};
use super::mailbox::{Delivery, MailboxFullReason};
use super::monitor::FlowExitReason;
use super::process::{Flow, FlowId, FlowOutcome};
use super::runtime::Shared;
use super::sync_lock;

/// Opaque host discriminator passed as the `HostAwait` immediate.
pub type HostAwaitOp = u32;

/// Request handed to [`HostAwaitBridge::submit`].
#[derive(Debug, Clone)]
pub struct HostAwaitRequest {
    pub flow: FlowId,
    pub op: HostAwaitOp,
    pub args: Value,
}

/// Host-side async completion channel (one-shot).
///
/// Prefer calling [`Self::complete`] / [`Self::fail`] explicitly. If the
/// completer is dropped unused, the flow is failed closed (avoids a
/// permanent wait when the host forgets to settle the await).
///
/// Holds a [`Weak`] to the runtime shared state so a bridge that retains
/// completers cannot keep the runtime alive in a reference cycle
/// (`Shared` → bridge → completer → `Shared`).
pub struct HostAwaitCompleter {
    shared: Weak<Shared>,
    ticket: u64,
    cancelled: Arc<AtomicBool>,
    consumed: AtomicBool,
}

/// Errors from [`HostAwaitCompleter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostAwaitError {
    /// Ticket already used, flow finalized, or await cancelled.
    Stale,
    /// Infrastructure failure while completing.
    Runtime(RuntimeError),
    /// The completion value carried a capability the runtime would not grant.
    Rejected,
    /// The mailbox refused the completion hop. The flow is failed when it
    /// is still alive, so a later `Receive` does not wait for a hop that
    /// was never stored.
    Mailbox(MailboxFullReason),
    /// The test replay gate rejected this completion's recorded sequence.
    Replay(&'static str),
}

impl std::fmt::Display for HostAwaitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostAwaitError::Stale => f.write_str("HostAwait completer is stale"),
            HostAwaitError::Runtime(e) => write!(f, "{e}"),
            HostAwaitError::Rejected => {
                f.write_str("HostAwait completion refused a capability grant")
            }
            HostAwaitError::Mailbox(reason) => {
                write!(f, "HostAwait mailbox refused the completion ({reason})")
            }
            HostAwaitError::Replay(reason) => {
                write!(f, "host replay rejected the sequence ({reason})")
            }
        }
    }
}

impl std::error::Error for HostAwaitError {}

impl HostAwaitCompleter {
    pub(crate) fn new(shared: Arc<Shared>, ticket: u64, cancelled: Arc<AtomicBool>) -> Self {
        Self {
            shared: Arc::downgrade(&shared),
            ticket,
            cancelled,
            consumed: AtomicBool::new(false),
        }
    }

    /// Ticket id written into the flow's destination register.
    pub fn ticket(&self) -> u64 {
        self.ticket
    }

    /// `true` after finalize, kill, or shutdown cancelled this operation.
    ///
    /// Cooperative: the host observes this or [`HostAwaitBridge::cancel`] and
    /// aborts the I/O it owns. The flag alone does not close a socket.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Deliver `value` as a [`crate::TAG_SYS_AWAIT`] hop to the awaiting flow.
    pub fn complete(self, value: Value) -> Result<(), HostAwaitError> {
        self.finish(Ok(value))
    }

    /// Fail the flow (`FlowOutcome::Failed`).
    pub fn fail(self, msg: impl Into<String>) -> Result<(), HostAwaitError> {
        self.finish(Err(msg.into()))
    }

    fn finish(self, result: Result<Value, String>) -> Result<(), HostAwaitError> {
        if self.consumed.swap(true, Ordering::SeqCst) {
            return Err(HostAwaitError::Stale);
        }
        settle(&self.shared, self.ticket, result)
    }
}

impl Drop for HostAwaitCompleter {
    fn drop(&mut self) {
        if self.consumed.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = settle(
            &self.shared,
            self.ticket,
            Err("HostAwait completer dropped".into()),
        );
    }
}

fn settle(
    shared: &Weak<Shared>,
    ticket: u64,
    result: Result<Value, String>,
) -> Result<(), HostAwaitError> {
    let Some(shared) = shared.upgrade() else {
        return Err(HostAwaitError::Stale);
    };
    let Some(flow_id) = shared
        .host_awaits
        .take_ticket(ticket)
        .map_err(HostAwaitError::Runtime)?
    else {
        return Err(HostAwaitError::Stale);
    };
    match result {
        Ok(value) => {
            let raw = Message::request(ticket, TAG_SYS_AWAIT, value);
            let stamped =
                match super::worker::authenticate_host_outgoing_message(&shared, flow_id, raw) {
                    Ok(message) => Value::Message(message),
                    Err(_) => {
                        fail_live(&shared, flow_id, "HostAwait: capability grant refused");
                        return Err(HostAwaitError::Rejected);
                    }
                };
            let ready = stage_completion(
                &shared,
                ticket,
                StagedHostHop {
                    flow: flow_id,
                    value: stamped,
                },
            )?;
            deliver_ready(&shared, ready)
        }
        Err(msg) => {
            if fail_live(&shared, flow_id, &msg) {
                Ok(())
            } else {
                Err(HostAwaitError::Stale)
            }
        }
    }
}

/// Record `msg` and fail the flow with that text.
///
/// Returns `false` when the flow has already committed an outcome. A flow
/// still on its worker keeps running only until the next result check,
/// which reads this table before it can `return` successfully.
fn fail_live(shared: &Shared, flow_id: FlowId, msg: &str) -> bool {
    match shared.directory.lookup(flow_id) {
        Ok(Some(_)) => {}
        Ok(None) => return false,
        Err(err) => {
            report_fault(err);
            return false;
        }
    }
    match shared.host_await_failures.request(flow_id, msg) {
        Ok(true) => {}
        Ok(false) => return false,
        Err(err) => {
            report_fault(err);
            return false;
        }
    }
    if let Some(flow) = extract_for_kill(shared, flow_id, FlowExitReason::Fault) {
        let text = match shared.host_await_failures.take(flow_id) {
            Ok(Some(text)) => text,
            _ => msg.to_string(),
        };
        finalize_flow(
            shared,
            flow,
            FlowOutcome::Failed(text),
            FlowExitReason::Fault,
        );
    }
    true
}

/// `None` when `flow` was finalized with a pending host failure.
pub(crate) fn park_flow_if_host_failed(shared: &Shared, flow: Box<Flow>) -> Option<Box<Flow>> {
    match shared.host_await_failures.take(flow.id) {
        Ok(Some(msg)) => {
            let _ = shared.kill_signals.take(flow.id);
            finalize_flow(
                shared,
                *flow,
                FlowOutcome::Failed(msg),
                FlowExitReason::Fault,
            );
            None
        }
        Ok(None) => Some(flow),
        Err(err) => {
            report_fault(err);
            finalize_flow(
                shared,
                *flow,
                FlowOutcome::Failed("HostAwait failure table unavailable".into()),
                FlowExitReason::Fault,
            );
            None
        }
    }
}

/// Host text when a kill and a host failure name the same flow.
pub(crate) fn message_or_kill(shared: &Shared, id: FlowId, reason: FlowExitReason) -> String {
    match shared.host_await_failures.take(id) {
        Ok(Some(msg)) => msg,
        Ok(None) => super::finalize::kill_outcome_message(reason),
        Err(err) => {
            report_fault(err);
            super::finalize::kill_outcome_message(reason)
        }
    }
}

fn stage_completion(
    shared: &Shared,
    ticket: u64,
    hop: StagedHostHop,
) -> Result<Vec<StagedHostHop>, HostAwaitError> {
    let prepared = {
        let mut gate = sync_lock::lock(&shared.host_replay, "HostAwait replay")
            .map_err(HostAwaitError::Runtime)?;
        match gate.as_mut() {
            None => Ok(vec![hop]),
            Some(replay) => {
                let flow = hop.flow;
                match replay.stage(ticket, hop) {
                    Ok(()) => Ok(replay.drain_ready()),
                    Err(err) => Err((flow, err)),
                }
            }
        }
    };
    match prepared {
        Ok(ready) => Ok(ready),
        Err((flow, err)) => {
            let _ = fail_live(shared, flow, "host replay rejected the sequence");
            Err(HostAwaitError::Replay(replay_label(err)))
        }
    }
}

/// Deliver every hop already removed from the replay queue.
///
/// One refusal does not drop the rest: those flows are still waiting on
/// `Receive`.
pub(crate) fn deliver_ready(
    shared: &Shared,
    ready: Vec<StagedHostHop>,
) -> Result<(), HostAwaitError> {
    let mut first = None;
    for hop in ready {
        if let Err(err) = deliver_staged(shared, hop) {
            if first.is_none() {
                first = Some(err);
            }
        }
    }
    match first {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Push one already-authenticated completion through [`crate::Runtime::send`]'s
/// mailbox admission. A closed inbox is [`HostAwaitError::Stale`]. A full
/// inbox fails the live flow and returns [`HostAwaitError::Mailbox`].
pub(crate) fn deliver_staged(shared: &Shared, hop: StagedHostHop) -> Result<(), HostAwaitError> {
    let mailbox = match shared.directory.lookup(hop.flow) {
        Ok(Some(mailbox)) => mailbox,
        Ok(None) => return Err(HostAwaitError::Stale),
        Err(err) => return Err(HostAwaitError::Runtime(err)),
    };
    let flow_id = hop.flow;
    match mailbox.push(hop.value.clone()) {
        Ok(Ok(Delivery::Handoff(parked))) => {
            resume_handoff(shared, parked, hop.value);
            Ok(())
        }
        Ok(Ok(Delivery::Queued | Delivery::QueuedDropOldest)) => Ok(()),
        Ok(Ok(Delivery::DroppedNewest)) => {
            let _ = fail_live(shared, flow_id, "HostAwait mailbox dropped the completion");
            Err(HostAwaitError::Mailbox(MailboxFullReason::MessageLimit))
        }
        Ok(Err(full)) if full.reason() == MailboxFullReason::Closed => Err(HostAwaitError::Stale),
        Ok(Err(full)) => {
            let reason = full.reason();
            let _ = fail_live(shared, flow_id, "HostAwait mailbox refused the completion");
            Err(HostAwaitError::Mailbox(reason))
        }
        Err(err) => Err(HostAwaitError::Runtime(err)),
    }
}

/// Embedder hook: schedule host work for [`Opcode::HostAwait`](crate::Opcode::HostAwait).
///
/// `submit` must return promptly. Spawn a thread / enqueue a task for I/O.
pub trait HostAwaitBridge: Send + Sync + std::fmt::Debug + 'static {
    fn submit(&self, req: HostAwaitRequest, done: HostAwaitCompleter);

    /// The flow was finalized or the runtime is shutting down.
    ///
    /// Called without the registry lock. Must return quickly and must not
    /// take a lock the finalizer already holds. Aborting the socket or
    /// future is the host's job; [`HostAwaitCompleter::is_cancelled`] is
    /// already set. The default body does nothing.
    fn cancel(&self, _flow: FlowId, _op: HostAwaitOp, _ticket: u64) {}
}

/// Tell the bridge about tickets removed by finalize, kill, or shutdown.
pub(crate) fn notify_cancelled(shared: &Shared, notices: Vec<CancelledAwait>) {
    let Some(bridge) = shared.host_await_bridge.as_ref() else {
        return;
    };
    for notice in notices {
        bridge.cancel(notice.flow, notice.op, notice.ticket);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostAwaitParkError {
    LimitReached { current: usize, max: usize },
    IdExhausted,
}

impl std::fmt::Display for HostAwaitParkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostAwaitParkError::LimitReached { current, max } => {
                write!(f, "HostAwait limit reached ({current}/{max})")
            }
            HostAwaitParkError::IdExhausted => f.write_str("HostAwait ticket ids exhausted"),
        }
    }
}

pub(crate) struct RegisteredAwait {
    pub id: u64,
    pub cancelled: Arc<AtomicBool>,
}

pub(crate) struct CancelledAwait {
    pub flow: FlowId,
    pub op: HostAwaitOp,
    pub ticket: u64,
}

struct AwaitEntry {
    flow: FlowId,
    op: HostAwaitOp,
    cancelled: Arc<AtomicBool>,
}

/// Host `fail` text observed by the worker before a successful `return`.
///
/// `settled` is the commit bit: once the worker has chosen an outcome, a
/// later `fail` is too late and does not invent a second one.
pub(crate) struct HostAwaitFailures {
    inner: Mutex<HashMap<FlowId, FailSlot>>,
}

#[derive(Default)]
struct FailSlot {
    message: Option<String>,
    settled: bool,
}

impl HostAwaitFailures {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// `Ok(true)` stores `msg` for the worker. `Ok(false)` means the flow
    /// already committed an outcome.
    pub(crate) fn request(&self, id: FlowId, msg: &str) -> Result<bool, RuntimeError> {
        let mut slots = sync_lock::lock(&self.inner, "HostAwaitFailures::request")?;
        let slot = slots.entry(id).or_default();
        if slot.settled {
            return Ok(false);
        }
        if slot.message.is_none() {
            slot.message = Some(msg.to_string());
        }
        Ok(true)
    }

    pub(crate) fn take(&self, id: FlowId) -> Result<Option<String>, RuntimeError> {
        let mut slots = sync_lock::lock(&self.inner, "HostAwaitFailures::take")?;
        let Some(slot) = slots.get_mut(&id) else {
            return Ok(None);
        };
        let message = slot.message.take();
        if message.is_none() && !slot.settled {
            slots.remove(&id);
        }
        Ok(message)
    }

    /// Mark a successful outcome. `Some` is a host failure that won the race
    /// and must replace that success.
    pub(crate) fn commit_success(&self, id: FlowId) -> Result<Option<String>, RuntimeError> {
        let mut slots = sync_lock::lock(&self.inner, "HostAwaitFailures::commit_success")?;
        let slot = slots.entry(id).or_default();
        slot.settled = true;
        Ok(slot.message.take())
    }

    pub(crate) fn seal(&self, id: FlowId) -> Result<(), RuntimeError> {
        let mut slots = sync_lock::lock(&self.inner, "HostAwaitFailures::seal")?;
        let slot = slots.entry(id).or_default();
        slot.settled = true;
        slot.message = None;
        Ok(())
    }

    pub(crate) fn clear(&self, id: FlowId) -> Result<(), RuntimeError> {
        let mut slots = sync_lock::lock(&self.inner, "HostAwaitFailures::clear")?;
        slots.remove(&id);
        Ok(())
    }
}

/// Process-wide table of in-flight `HostAwait` tickets.
///
/// The flow itself stays on a worker or in its mailbox. This table only
/// records the ticket so completion and cancellation compete for one removal.
pub struct HostAwaitIndex {
    inner: Mutex<HostAwaitInner>,
    max: usize,
}

struct HostAwaitInner {
    by_ticket: HashMap<u64, AwaitEntry>,
    by_flow: HashMap<FlowId, Vec<u64>>,
    next_ticket: u64,
}

impl HostAwaitIndex {
    /// `max_host_awaits == 0` → unlimited.
    pub fn new(max_host_awaits: u32) -> Self {
        Self::with_next(max_host_awaits, 1)
    }

    fn with_next(max_host_awaits: u32, next_ticket: u64) -> Self {
        Self {
            inner: Mutex::new(HostAwaitInner {
                by_ticket: HashMap::new(),
                by_flow: HashMap::new(),
                next_ticket,
            }),
            max: max_host_awaits as usize,
        }
    }

    pub fn register(
        &self,
        flow: FlowId,
        op: HostAwaitOp,
    ) -> Result<Result<RegisteredAwait, HostAwaitParkError>, RuntimeError> {
        let mut entries = sync_lock::lock(&self.inner, "HostAwaitIndex::register")?;
        if self.max != 0 && entries.by_ticket.len() >= self.max {
            return Ok(Err(HostAwaitParkError::LimitReached {
                current: entries.by_ticket.len(),
                max: self.max,
            }));
        }
        if entries.next_ticket == 0 || entries.next_ticket >= i64::MAX as u64 {
            return Ok(Err(HostAwaitParkError::IdExhausted));
        }
        let id = entries.next_ticket;
        entries.next_ticket += 1;
        let cancelled = Arc::new(AtomicBool::new(false));
        entries.by_flow.entry(flow).or_default().push(id);
        entries.by_ticket.insert(
            id,
            AwaitEntry {
                flow,
                op,
                cancelled: Arc::clone(&cancelled),
            },
        );
        Ok(Ok(RegisteredAwait { id, cancelled }))
    }

    /// Removes the ticket once. `None` means it was already completed or cancelled.
    pub fn take_ticket(&self, ticket: u64) -> Result<Option<FlowId>, RuntimeError> {
        let mut entries = sync_lock::lock(&self.inner, "HostAwaitIndex::take_ticket")?;
        let Some(entry) = entries.by_ticket.remove(&ticket) else {
            return Ok(None);
        };
        unlink(&mut entries, entry.flow, ticket);
        if entry.cancelled.load(Ordering::Acquire) {
            return Ok(None);
        }
        Ok(Some(entry.flow))
    }

    /// Cancels every ticket of `flow`. The caller notifies the host after
    /// this returns, so the bridge does not run under the registry lock.
    pub fn cancel_flow(&self, flow: FlowId) -> Result<Vec<CancelledAwait>, RuntimeError> {
        let mut entries = sync_lock::lock(&self.inner, "HostAwaitIndex::cancel_flow")?;
        Ok(take_flow_ids(&mut entries, flow))
    }

    /// Cancels every outstanding ticket (runtime shutdown).
    pub(crate) fn cancel_all(&self) -> Result<Vec<CancelledAwait>, RuntimeError> {
        let mut entries = sync_lock::lock(&self.inner, "HostAwaitIndex::cancel_all")?;
        entries.by_flow.clear();
        let mut notices = Vec::with_capacity(entries.by_ticket.len());
        for (ticket, entry) in entries.by_ticket.drain() {
            entry.cancelled.store(true, Ordering::Release);
            notices.push(CancelledAwait {
                flow: entry.flow,
                op: entry.op,
                ticket,
            });
        }
        Ok(notices)
    }

    #[cfg(test)]
    pub fn outstanding(&self) -> Result<usize, RuntimeError> {
        let entries = sync_lock::lock(&self.inner, "HostAwaitIndex::outstanding")?;
        Ok(entries.by_ticket.len())
    }
}

fn unlink(entries: &mut HostAwaitInner, flow: FlowId, ticket: u64) {
    let Some(ids) = entries.by_flow.get_mut(&flow) else {
        return;
    };
    ids.retain(|id| *id != ticket);
    if ids.is_empty() {
        entries.by_flow.remove(&flow);
    }
}

fn take_flow_ids(entries: &mut HostAwaitInner, flow: FlowId) -> Vec<CancelledAwait> {
    let Some(ids) = entries.by_flow.remove(&flow) else {
        return Vec::new();
    };
    let mut notices = Vec::with_capacity(ids.len());
    for ticket in ids {
        if let Some(entry) = entries.by_ticket.remove(&ticket) {
            entry.cancelled.store(true, Ordering::Release);
            notices.push(CancelledAwait {
                flow,
                op: entry.op,
                ticket,
            });
        }
    }
    notices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_multiple_operations_for_one_flow() -> Result<(), Box<dyn std::error::Error>> {
        let index = HostAwaitIndex::new(0);
        let first = registered(&index, FlowId(7), 1)?;
        let second = registered(&index, FlowId(7), 2)?;

        assert_ne!(first.id, second.id);
        assert!(!first.cancelled.load(Ordering::Acquire));
        assert!(!second.cancelled.load(Ordering::Acquire));
        assert_eq!(index.outstanding()?, 2);

        assert_eq!(index.take_ticket(first.id)?, Some(FlowId(7)));
        assert_eq!(index.take_ticket(second.id)?, Some(FlowId(7)));
        assert_eq!(index.outstanding()?, 0);
        Ok(())
    }

    #[test]
    fn cancel_flow_invalidates_all_its_operations() -> Result<(), Box<dyn std::error::Error>> {
        let index = HostAwaitIndex::new(0);
        let left = registered(&index, FlowId(10), 1)?;
        let right = registered(&index, FlowId(10), 2)?;
        let other = registered(&index, FlowId(20), 1)?;

        let notices = index.cancel_flow(FlowId(10))?;
        assert_eq!(notices.len(), 2);
        assert!(left.cancelled.load(Ordering::Acquire));
        assert!(right.cancelled.load(Ordering::Acquire));
        assert!(!other.cancelled.load(Ordering::Acquire));

        assert_eq!(index.take_ticket(left.id)?, None);
        assert_eq!(index.take_ticket(right.id)?, None);
        assert_eq!(index.take_ticket(other.id)?, Some(FlowId(20)));
        assert_eq!(index.outstanding()?, 0);
        Ok(())
    }

    #[test]
    fn completion_is_consumed_only_once() -> Result<(), Box<dyn std::error::Error>> {
        let index = HostAwaitIndex::new(0);
        let registered = registered(&index, FlowId(42), 1)?;

        assert_eq!(index.take_ticket(registered.id)?, Some(FlowId(42)));
        assert_eq!(index.take_ticket(registered.id)?, None);
        Ok(())
    }

    #[test]
    fn shutdown_cancels_all_operations() -> Result<(), Box<dyn std::error::Error>> {
        let index = HostAwaitIndex::new(0);
        let left = registered(&index, FlowId(1), 1)?;
        let right = registered(&index, FlowId(2), 1)?;

        assert_eq!(index.cancel_all()?.len(), 2);
        assert!(left.cancelled.load(Ordering::Acquire));
        assert!(right.cancelled.load(Ordering::Acquire));
        assert_eq!(index.take_ticket(left.id)?, None);
        assert_eq!(index.take_ticket(right.id)?, None);
        assert_eq!(index.outstanding()?, 0);
        Ok(())
    }

    #[test]
    fn register_hits_process_limit() -> Result<(), Box<dyn std::error::Error>> {
        let index = HostAwaitIndex::new(1);
        let _first = registered(&index, FlowId(1), 1)?;
        match index.register(FlowId(2), 1)? {
            Err(HostAwaitParkError::LimitReached { current: 1, max: 1 }) => Ok(()),
            Ok(_) => Err("expected limit".into()),
            Err(err) => Err(format!("wrong park error: {err}").into()),
        }
    }

    #[test]
    fn slot_is_free_after_take() -> Result<(), Box<dyn std::error::Error>> {
        let index = HostAwaitIndex::new(1);
        let first = registered(&index, FlowId(1), 1)?;
        assert!(index.take_ticket(first.id)?.is_some());
        let _second = registered(&index, FlowId(2), 1)?;
        Ok(())
    }

    #[test]
    fn commit_keeps_a_fail_that_arrives_after_the_poll() -> Result<(), RuntimeError> {
        let failures = HostAwaitFailures::new();
        assert!(failures.take(FlowId(1))?.is_none());
        assert!(failures.request(FlowId(1), "host boom")?);
        assert_eq!(
            failures.commit_success(FlowId(1))?.as_deref(),
            Some("host boom")
        );
        Ok(())
    }

    #[test]
    fn request_after_commit_is_too_late() -> Result<(), RuntimeError> {
        let failures = HostAwaitFailures::new();
        assert!(failures.commit_success(FlowId(1))?.is_none());
        assert!(!failures.request(FlowId(1), "late")?);
        Ok(())
    }

    #[test]
    fn ticket_ids_stop_before_i64_max() -> Result<(), Box<dyn std::error::Error>> {
        let index = HostAwaitIndex::with_next(0, i64::MAX as u64);
        match index.register(FlowId(1), 1)? {
            Err(HostAwaitParkError::IdExhausted) => Ok(()),
            Ok(_) => Err("expected exhausted ids".into()),
            Err(err) => Err(format!("wrong park error: {err}").into()),
        }
    }

    #[test]
    fn complete_and_cancel_remove_the_ticket_once() -> Result<(), Box<dyn std::error::Error>> {
        let index = Arc::new(HostAwaitIndex::new(0));
        let registered = registered(&index, FlowId(3), 9)?;
        let ticket = registered.id;
        let taker = Arc::clone(&index);
        let canceller = Arc::clone(&index);
        let took = std::thread::spawn(move || taker.take_ticket(ticket));
        let cancelled = std::thread::spawn(move || canceller.cancel_flow(FlowId(3)));
        let took = match took.join() {
            Ok(result) => result?,
            Err(_) => return Err("take join".into()),
        };
        let cancelled = match cancelled.join() {
            Ok(result) => result?,
            Err(_) => return Err("cancel join".into()),
        };
        let won_take = took.is_some();
        let won_cancel = !cancelled.is_empty();
        assert!(won_take ^ won_cancel, "take={won_take} cancel={won_cancel}");
        assert_eq!(index.outstanding()?, 0);
        if won_cancel {
            assert!(registered.cancelled.load(Ordering::Acquire));
        }
        Ok(())
    }

    fn registered(
        index: &HostAwaitIndex,
        flow: FlowId,
        op: HostAwaitOp,
    ) -> Result<RegisteredAwait, Box<dyn std::error::Error>> {
        match index.register(flow, op)? {
            Ok(registered) => Ok(registered),
            Err(err) => Err(err.to_string().into()),
        }
    }
}

/// Runtime-level HostAwait coverage (bridge wiring, limits, lifecycle).
#[cfg(test)]
mod runtime_tests {
    use super::*;
    use crate::bytecode::{CapId, Message, Program};
    use crate::scheduler::mailbox::{MailboxCapacity, MailboxConfig, OverflowPolicy};
    use crate::scheduler::process::FlowOutcome;
    use crate::scheduler::runtime::{Runtime, RuntimeConfig};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    /// Causal hold: waits until `n` submits arrived (no sleep races).
    struct HoldBridge {
        state: Mutex<HoldState>,
        cv: Condvar,
    }

    struct HoldState {
        held: Vec<(FlowId, HostAwaitCompleter)>,
        submitted: usize,
    }

    impl HoldBridge {
        fn new() -> Self {
            Self {
                state: Mutex::new(HoldState {
                    held: Vec::new(),
                    submitted: 0,
                }),
                cv: Condvar::new(),
            }
        }

        fn wait_for_submissions(&self, n: usize) -> Result<(), &'static str> {
            let mut guard = match self.state.lock() {
                Ok(guard) => guard,
                Err(_) => return Err("HoldBridge lock"),
            };
            while guard.submitted < n {
                guard = match self.cv.wait(guard) {
                    Ok(guard) => guard,
                    Err(_) => return Err("HoldBridge wait"),
                };
            }
            Ok(())
        }

        fn take_all(&self) -> Result<Vec<(FlowId, HostAwaitCompleter)>, &'static str> {
            let mut guard = match self.state.lock() {
                Ok(guard) => guard,
                Err(_) => return Err("HoldBridge lock"),
            };
            Ok(std::mem::take(&mut guard.held))
        }

        fn take_one(&self) -> Result<Option<(FlowId, HostAwaitCompleter)>, &'static str> {
            let mut guard = match self.state.lock() {
                Ok(guard) => guard,
                Err(_) => return Err("HoldBridge lock"),
            };
            if guard.held.is_empty() {
                Ok(None)
            } else {
                Ok(Some(guard.held.remove(0)))
            }
        }
    }

    impl std::fmt::Debug for HoldBridge {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("HoldBridge")
        }
    }

    impl HostAwaitBridge for HoldBridge {
        fn submit(&self, req: HostAwaitRequest, done: HostAwaitCompleter) {
            match self.state.lock() {
                Ok(mut guard) => {
                    guard.held.push((req.flow, done));
                    guard.submitted += 1;
                    self.cv.notify_all();
                }
                Err(_) => {
                    let _ = done.fail("HoldBridge lock poisoned");
                }
            }
        }
    }

    fn waiter_program() -> (Program, u32) {
        let mut program = Program::new("ha-waiter");
        let waiter = program.function("waiter", 0, |f| {
            let args = f.load_i32(0);
            let _ticket = f.host_await(1, args);
            let msg = f.receive();
            f.return_(msg);
        });
        (program, waiter)
    }

    fn payload_int(outcome: &FlowOutcome) -> Option<i64> {
        match outcome {
            FlowOutcome::Completed(Value::Message(msg)) => match msg.payload.as_ref() {
                Value::Int(n) => Some(*n),
                _ => None,
            },
            _ => None,
        }
    }

    #[test]
    fn host_await_without_bridge_fails() -> Result<(), Box<dyn std::error::Error>> {
        let mut program = Program::new("ha-none");
        program.function("main", 0, |f| {
            let args = f.load_i32(1);
            let out = f.host_await(1, args);
            f.return_(out);
        });
        let rt = Runtime::with_config(
            program.build(),
            RuntimeConfig {
                workers: 1,
                quantum: 1_000,
                mailbox: MailboxConfig::DEFAULT,
                host_await: None,
                ..Default::default()
            },
        )?;
        let outcome = rt.spawn(0, &[])?.join();
        rt.shutdown();
        match outcome {
            FlowOutcome::Failed(msg) if msg.contains("no bridge") => Ok(()),
            other => Err(format!("expected no-bridge failure, got {other:?}").into()),
        }
    }

    #[test]
    fn host_await_thread_bridge_delivers_a_hop() -> Result<(), Box<dyn std::error::Error>> {
        #[derive(Debug)]
        struct EchoBridge;

        impl HostAwaitBridge for EchoBridge {
            fn submit(&self, req: HostAwaitRequest, done: HostAwaitCompleter) {
                std::thread::spawn(move || {
                    let value = match (req.op, &req.args) {
                        (1, Value::Int(n)) => Value::Int(n + 1),
                        _ => Value::Int(42),
                    };
                    let _ = done.complete(value);
                });
            }
        }

        let mut program = Program::new("ha-echo");
        program.function("main", 0, |f| {
            let args = f.load_i32(41);
            let _ticket = f.host_await(1, args);
            let msg = f.receive();
            f.return_(msg);
        });
        let rt = Runtime::with_config(
            program.build(),
            RuntimeConfig {
                workers: 1,
                quantum: 1_000,
                mailbox: MailboxConfig::DEFAULT,
                host_await: Some(Arc::new(EchoBridge)),
                ..Default::default()
            },
        )?;
        let outcome = rt
            .spawn(0, &[])?
            .join_timeout(Duration::from_secs(2))
            .ok_or("host_await timed out")?;
        rt.shutdown();
        match &outcome {
            FlowOutcome::Completed(Value::Message(msg))
                if msg.sender == FlowId::HOST.as_u64()
                    && msg.reply_cap.is_none()
                    && msg.tag == TAG_SYS_AWAIT
                    && matches!(msg.payload.as_ref(), Value::Int(42)) =>
            {
                Ok(())
            }
            other => Err(format!("expected await hop 42, got {other:?}").into()),
        }
    }

    #[test]
    fn host_await_limit_rejects_second() -> Result<(), Box<dyn std::error::Error>> {
        let bridge = Arc::new(HoldBridge::new());
        let (program, waiter) = waiter_program();
        let rt = Runtime::with_config(
            program.build(),
            RuntimeConfig {
                workers: 2,
                quantum: 1_000,
                mailbox: MailboxConfig::DEFAULT,
                max_host_awaits: 1,
                host_await: Some(Arc::clone(&bridge) as Arc<dyn HostAwaitBridge>),
                ..Default::default()
            },
        )?;

        let first = rt.spawn(waiter, &[])?;
        bridge.wait_for_submissions(1)?;
        let second = rt.spawn(waiter, &[])?;
        let second_out = second
            .join_timeout(Duration::from_secs(2))
            .ok_or("second HostAwait did not finish")?;

        for (_, done) in bridge.take_all()? {
            let _ = done.complete(Value::Int(0));
        }
        let _ = first.join_timeout(Duration::from_secs(2));
        rt.shutdown();

        match second_out {
            FlowOutcome::Failed(msg) if msg.contains("HostAwait limit") => Ok(()),
            other => Err(format!("expected HostAwait limit failure, got {other:?}").into()),
        }
    }

    #[test]
    fn host_await_slot_reused_after_complete() -> Result<(), Box<dyn std::error::Error>> {
        let bridge = Arc::new(HoldBridge::new());
        let (program, waiter) = waiter_program();
        let rt = Runtime::with_config(
            program.build(),
            RuntimeConfig {
                workers: 2,
                quantum: 1_000,
                mailbox: MailboxConfig::DEFAULT,
                max_host_awaits: 1,
                host_await: Some(Arc::clone(&bridge) as Arc<dyn HostAwaitBridge>),
                ..Default::default()
            },
        )?;

        let first = rt.spawn(waiter, &[])?;
        bridge.wait_for_submissions(1)?;
        let second = rt.spawn(waiter, &[])?;
        let second_out = second
            .join_timeout(Duration::from_secs(2))
            .ok_or("second should fail on limit")?;
        assert!(
            matches!(
                &second_out,
                FlowOutcome::Failed(msg) if msg.contains("HostAwait limit")
            ),
            "got {second_out:?}"
        );

        let (_, done) = bridge.take_one()?.ok_or("missing first completer")?;
        done.complete(Value::Int(7))?;
        let first_out = first
            .join_timeout(Duration::from_secs(2))
            .ok_or("first hop timed out")?;
        assert_eq!(payload_int(&first_out), Some(7), "got {first_out:?}");

        let third = rt.spawn(waiter, &[])?;
        bridge.wait_for_submissions(2)?;
        let (_, done3) = bridge.take_one()?.ok_or("missing third completer")?;
        done3.complete(Value::Int(9))?;
        let third_out = third
            .join_timeout(Duration::from_secs(2))
            .ok_or("third hop timed out")?;
        rt.shutdown();

        match payload_int(&third_out) {
            Some(9) => Ok(()),
            _ => Err(format!("expected slot reuse hop, got {third_out:?}").into()),
        }
    }

    #[test]
    fn host_await_max_zero_allows_concurrent() -> Result<(), Box<dyn std::error::Error>> {
        let bridge = Arc::new(HoldBridge::new());
        let (program, waiter) = waiter_program();
        let rt = Runtime::with_config(
            program.build(),
            RuntimeConfig {
                workers: 2,
                quantum: 1_000,
                mailbox: MailboxConfig::DEFAULT,
                max_host_awaits: 0,
                host_await: Some(Arc::clone(&bridge) as Arc<dyn HostAwaitBridge>),
                ..Default::default()
            },
        )?;

        let a = rt.spawn(waiter, &[])?;
        let b = rt.spawn(waiter, &[])?;
        bridge.wait_for_submissions(2)?;
        for (_, done) in bridge.take_all()? {
            let _ = done.complete(Value::Int(1));
        }
        let a_out = a
            .join_timeout(Duration::from_secs(2))
            .ok_or("a timed out")?;
        let b_out = b
            .join_timeout(Duration::from_secs(2))
            .ok_or("b timed out")?;
        rt.shutdown();

        match (payload_int(&a_out), payload_int(&b_out)) {
            (Some(1), Some(1)) => Ok(()),
            _ => Err(format!("expected both hops, got {a_out:?} {b_out:?}").into()),
        }
    }

    #[test]
    fn two_awaits_on_one_flow_both_deliver() -> Result<(), Box<dyn std::error::Error>> {
        let bridge = Arc::new(HoldBridge::new());
        let mut program = Program::new("ha-two");
        program.function("main", 0, |f| {
            let left = f.load_i32(11);
            let right = f.load_i32(22);
            let _first = f.host_await(1, left);
            let _second = f.host_await(2, right);
            let first = f.receive();
            let _second_msg = f.receive();
            f.return_(first);
        });
        let rt = Runtime::with_config(
            program.build(),
            RuntimeConfig {
                workers: 1,
                quantum: 1_000,
                mailbox: MailboxConfig::DEFAULT,
                host_await: Some(Arc::clone(&bridge) as Arc<dyn HostAwaitBridge>),
                ..Default::default()
            },
        )?;
        let handle = rt.spawn(0, &[])?;
        bridge.wait_for_submissions(2)?;
        let mut held = bridge.take_all()?;
        assert_eq!(held.len(), 2);
        let (_, second) = held.pop().ok_or("second completer")?;
        let (_, first) = held.pop().ok_or("first completer")?;
        assert_ne!(first.ticket(), second.ticket());
        first.complete(Value::Int(11))?;
        second.complete(Value::Int(22))?;
        let outcome = handle
            .join_timeout(Duration::from_secs(2))
            .ok_or("two-await timed out")?;
        rt.shutdown();
        match payload_int(&outcome) {
            Some(11) => Ok(()),
            _ => Err(format!("expected first hop 11, got {outcome:?}").into()),
        }
    }

    #[test]
    fn replay_delivers_completions_in_script_order() -> Result<(), Box<dyn std::error::Error>> {
        let bridge = Arc::new(HoldBridge::new());
        let mut program = Program::new("ha-replay");
        program.function("main", 0, |f| {
            let left = f.load_i32(11);
            let right = f.load_i32(22);
            let _first = f.host_await(1, left);
            let _second = f.host_await(2, right);
            let first = f.receive();
            let _second_msg = f.receive();
            f.return_(first);
        });
        let rt = Runtime::with_config(
            program.build(),
            RuntimeConfig {
                workers: 1,
                quantum: 1_000,
                mailbox: MailboxConfig::DEFAULT,
                host_await: Some(Arc::clone(&bridge) as Arc<dyn HostAwaitBridge>),
                ..Default::default()
            },
        )?;
        rt.enable_host_await_replay()?;
        let handle = rt.spawn(0, &[])?;
        bridge.wait_for_submissions(2)?;
        let mut held = bridge.take_all()?;
        let (_, second) = held.pop().ok_or("second completer")?;
        let (_, first) = held.pop().ok_or("first completer")?;
        rt.bind_host_await_replay(first.ticket(), 1)?;
        rt.bind_host_await_replay(second.ticket(), 0)?;
        first.complete(Value::Int(11))?;
        second.complete(Value::Int(22))?;
        let outcome = handle
            .join_timeout(Duration::from_secs(2))
            .ok_or("replay timed out")?;
        rt.shutdown();
        match payload_int(&outcome) {
            Some(22) => Ok(()),
            _ => Err(format!("expected script order 22 then 11, got {outcome:?}").into()),
        }
    }

    #[test]
    fn kill_parked_host_await_then_complete_is_stale() -> Result<(), Box<dyn std::error::Error>> {
        let bridge = Arc::new(HoldBridge::new());
        let (program, waiter) = waiter_program();
        let rt = Runtime::with_config(
            program.build(),
            RuntimeConfig {
                workers: 1,
                quantum: 1_000,
                mailbox: MailboxConfig::DEFAULT,
                host_await: Some(Arc::clone(&bridge) as Arc<dyn HostAwaitBridge>),
                ..Default::default()
            },
        )?;

        let handle = rt.spawn(waiter, &[])?;
        bridge.wait_for_submissions(1)?;
        let (flow_id, done) = bridge.take_one()?.ok_or("missing completer")?;
        assert_eq!(flow_id, handle.id());
        rt.kill(handle.id())?;
        let outcome = handle
            .join_timeout(Duration::from_secs(2))
            .ok_or("kill join timed out")?;
        assert!(
            matches!(outcome, FlowOutcome::Failed(_)),
            "kill must fail joiner, got {outcome:?}"
        );
        assert!(done.is_cancelled());
        let stale = done.complete(Value::Int(1));
        rt.shutdown();
        match stale {
            Err(HostAwaitError::Stale) => Ok(()),
            other => Err(format!("expected Stale after kill, got {other:?}").into()),
        }
    }

    #[test]
    fn shutdown_with_parked_host_await_does_not_hang() -> Result<(), Box<dyn std::error::Error>> {
        let bridge = Arc::new(HoldBridge::new());
        let (program, waiter) = waiter_program();
        let rt = Runtime::with_config(
            program.build(),
            RuntimeConfig {
                workers: 1,
                quantum: 1_000,
                mailbox: MailboxConfig::DEFAULT,
                host_await: Some(Arc::clone(&bridge) as Arc<dyn HostAwaitBridge>),
                ..Default::default()
            },
        )?;

        let handle = rt.spawn(waiter, &[])?;
        bridge.wait_for_submissions(1)?;
        rt.shutdown();
        let outcome = handle
            .join_timeout(Duration::from_secs(2))
            .ok_or("joiner stuck after shutdown")?;
        for (_, done) in bridge.take_all()? {
            assert!(done.is_cancelled());
            assert!(matches!(
                done.complete(Value::Int(1)),
                Err(HostAwaitError::Stale)
            ));
        }
        match outcome {
            FlowOutcome::Failed(msg) if msg.contains("runtime shutdown") => Ok(()),
            other => Err(format!("expected shutdown failure, got {other:?}").into()),
        }
    }

    #[test]
    fn unknown_cap_in_completion_rejects_and_fails_the_flow(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let bridge = Arc::new(HoldBridge::new());
        let (program, waiter) = waiter_program();
        let rt = Runtime::with_config(
            program.build(),
            RuntimeConfig {
                workers: 1,
                quantum: 1_000,
                mailbox: MailboxConfig::DEFAULT,
                host_await: Some(Arc::clone(&bridge) as Arc<dyn HostAwaitBridge>),
                ..Default::default()
            },
        )?;
        let handle = rt.spawn(waiter, &[])?;
        bridge.wait_for_submissions(1)?;
        let (_, done) = bridge.take_one()?.ok_or("missing completer")?;
        let rejected = done.complete(Value::Cap(CapId::from_raw(1)));
        let outcome = handle
            .join_timeout(Duration::from_secs(2))
            .ok_or("cap rejection timed out")?;
        rt.shutdown();
        match rejected {
            Err(HostAwaitError::Rejected) => {}
            other => return Err(format!("expected Rejected, got {other:?}").into()),
        }
        match outcome {
            FlowOutcome::Failed(msg) if msg.contains("capability") => Ok(()),
            other => Err(format!("expected capability failure, got {other:?}").into()),
        }
    }

    #[test]
    fn fail_during_submit_uses_the_host_message() -> Result<(), Box<dyn std::error::Error>> {
        #[derive(Debug)]
        struct Boom;
        impl HostAwaitBridge for Boom {
            fn submit(&self, _req: HostAwaitRequest, done: HostAwaitCompleter) {
                let _ = done.fail("host boom");
            }
        }
        let mut program = Program::new("ha-boom");
        program.function("main", 0, |f| {
            let args = f.load_i32(1);
            let _ticket = f.host_await(1, args);
            f.return_(args);
        });
        let rt = Runtime::with_config(
            program.build(),
            RuntimeConfig {
                workers: 1,
                quantum: 1_000,
                mailbox: MailboxConfig::DEFAULT,
                host_await: Some(Arc::new(Boom)),
                ..Default::default()
            },
        )?;
        let outcome = rt
            .spawn(0, &[])?
            .join_timeout(Duration::from_secs(2))
            .ok_or("fail join timed out")?;
        rt.shutdown();
        match outcome {
            FlowOutcome::Failed(msg) if msg.contains("host boom") => Ok(()),
            other => Err(format!("expected host boom, got {other:?}").into()),
        }
    }

    #[test]
    fn fail_after_the_flow_commits_is_stale() -> Result<(), Box<dyn std::error::Error>> {
        let bridge = Arc::new(HoldBridge::new());
        let mut program = Program::new("ha-late-fail");
        program.function("main", 0, |f| {
            let args = f.load_i32(1);
            let _ticket = f.host_await(1, args);
            f.return_(args);
        });
        let rt = Runtime::with_config(
            program.build(),
            RuntimeConfig {
                workers: 1,
                quantum: 1_000,
                mailbox: MailboxConfig::DEFAULT,
                host_await: Some(Arc::clone(&bridge) as Arc<dyn HostAwaitBridge>),
                ..Default::default()
            },
        )?;
        let handle = rt.spawn(0, &[])?;
        bridge.wait_for_submissions(1)?;
        let (_, done) = bridge.take_one()?.ok_or("missing completer")?;
        let outcome = handle
            .join_timeout(Duration::from_secs(2))
            .ok_or("commit timed out")?;
        let late = done.fail("too late");
        rt.shutdown();
        match outcome {
            FlowOutcome::Completed(Value::Int(1)) => {}
            other => return Err(format!("expected completed 1, got {other:?}").into()),
        }
        match late {
            Err(HostAwaitError::Stale) => Ok(()),
            other => Err(format!("expected stale fail, got {other:?}").into()),
        }
    }

    #[test]
    fn replay_keeps_delivering_after_a_full_mailbox() -> Result<(), Box<dyn std::error::Error>> {
        let bridge = Arc::new(HoldBridge::new());
        let capacity = MailboxCapacity::new(1).ok_or("capacity")?;
        let mut program = Program::new("ha-replay-full");
        let blocked = program.function("blocked", 0, |f| {
            let args = f.load_i32(1);
            let _ticket = f.host_await(1, args);
            let _ignored = f.receive_match_imm(1);
            f.return_(args);
        });
        let open = program.function("open", 0, |f| {
            let args = f.load_i32(22);
            let _ticket = f.host_await(2, args);
            let msg = f.receive();
            f.return_(msg);
        });
        let gate = program.function("gate", 0, |f| {
            let n = f.load_i32(1);
            f.return_(n);
        });
        let rt = Runtime::with_config(
            program.build(),
            RuntimeConfig {
                workers: 1,
                quantum: 1_000,
                mailbox: MailboxConfig::new(capacity, OverflowPolicy::Reject),
                host_await: Some(Arc::clone(&bridge) as Arc<dyn HostAwaitBridge>),
                ..Default::default()
            },
        )?;
        rt.enable_host_await_replay()?;
        let blocked_handle = rt.spawn(blocked, &[])?;
        let open_handle = rt.spawn(open, &[])?;
        let blocked_id = blocked_handle.id();
        let open_id = open_handle.id();
        bridge.wait_for_submissions(2)?;
        let parked = rt.spawn(gate, &[])?;
        let parked_out = parked
            .join_timeout(Duration::from_secs(2))
            .ok_or("worker did not become free")?;
        match parked_out {
            FlowOutcome::Completed(Value::Int(1)) => {}
            other => return Err(format!("worker did not become free, got {other:?}").into()),
        }
        let mut blocked_done = None;
        let mut open_done = None;
        for (id, done) in bridge.take_all()? {
            if id == blocked_id {
                blocked_done = Some(done);
            } else if id == open_id {
                open_done = Some(done);
            }
        }
        let blocked_done = blocked_done.ok_or("blocked completer")?;
        let open_done = open_done.ok_or("open completer")?;
        rt.send(
            blocked_id,
            Value::Message(Message::request(9, 2, Value::Int(0))),
        )?;
        rt.bind_host_await_replay(open_done.ticket(), 1)?;
        rt.bind_host_await_replay(blocked_done.ticket(), 0)?;
        open_done.complete(Value::Int(22))?;
        let blocked_result = blocked_done.complete(Value::Int(7));
        let open_out = open_handle
            .join_timeout(Duration::from_secs(2))
            .ok_or("open hop timed out")?;
        let _ = blocked_handle.join_timeout(Duration::from_secs(2));
        rt.shutdown();
        match blocked_result {
            Err(HostAwaitError::Mailbox(crate::scheduler::MailboxFullReason::MessageLimit)) => {}
            other => return Err(format!("expected a full mailbox, got {other:?}").into()),
        }
        match payload_int(&open_out) {
            Some(22) => Ok(()),
            _ => Err(format!("expected the later hop to arrive, got {open_out:?}").into()),
        }
    }
}
