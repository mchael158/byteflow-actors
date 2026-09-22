//! Per-flow inbox: bounded hop queue + parked waiter (anti lost-wakeup).
//!
//! # Memory contract
//!
//! Unbounded growth without a logical limit is not a capacity API — it is
//! an OOM path when many flows share few workers. Every mailbox is constructed
//! with a [`MailboxConfig`]: a validated [`MailboxCapacity`], a [`MailboxBytes`]
//! budget, and an [`OverflowPolicy`]. Logical bound ≠ physical allocation;
//! the queue grows geometrically up to the limit (see [`queue`]).
//!
//! Both bounds are load-bearing. A hop count alone stopped being a memory
//! bound once hops could carry `Str` / `Bytes`: at the default capacity,
//! 256 hops is ~12 KiB of scalars or ~256 MiB of 1 MiB blobs. Whichever
//! bound is reached first refuses the hop, and [`MailboxFull::reason`] says
//! which one it was.
//!
//! There is **no** `Block` policy. Blocking an OS worker on a full inbox
//! would stall every other flow on that thread. Overflow is Reject /
//! DropNewest / DropOldest. Scheduler-level [`Mailbox::park_sender`]
//! (`WAITING_SEND`) parks the **sender flow** in this mailbox and wakes
//! **one** waiter per freed slot (no wake storm).
//!
//! # Wake
//!
//! `park` and `push` share one mutex (lost-wakeup invariant — keep this
//! comment and the race diagram). A hop that does not match a selective
//! waiter is queued (if the bound allows) and the waiter stays parked.
//! Overflow that **drops** a hop never produces [`Delivery::Handoff`].
//!
//! See `docs/mailbox.md`.

mod capacity;
mod metrics;
mod policy;
mod queue;

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::bytecode::Value;

use super::error::RuntimeError;
use super::process::{Flow, FlowId};
use super::sync_lock;

pub use capacity::{MailboxBytes, MailboxCapacity};
pub use metrics::MailboxStats;
pub use policy::{MailboxConfig, OverflowPolicy};

use queue::{EnqueueEffect, MailboxQueue};

/// Selective wait criterion installed while a flow is parked in its mailbox.
///
/// Used by classic `Receive`, `ReceiveMatch`, and `Ask`. Matching always
/// **skips** (never drops) non-matching hops already in the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitFilter {
    /// `Receive`: consume the oldest value regardless of contents.
    Any,
    /// `ReceiveMatch`: oldest `Message` whose application `tag` matches.
    Tag(u16),
    /// `ReceiveMatchKind`: oldest `Message` whose `payload.wire_tag()` matches.
    PayloadKind(u8),
    /// `Ask`: reply belonging to one specific request.
    ///
    /// `expect_request_id` is the RPC correlation key.
    /// `expect_sender`, when present, additionally constrains reply origin.
    /// Ask uses `Some(resolved_target_FlowId)` so a hop with the same
    /// `request_id` from an unrelated flow cannot complete the RPC.
    /// Compare against FlowId, never CapId (S2 after FlowCap).
    Correlation {
        expect_request_id: u64,
        expect_sender: Option<u64>,
    },
    /// Fire-and-forget correlated receive: tag + request_id, any sender.
    TaggedCorrelation { tag: u16, expect_request_id: u64 },
}

impl WaitFilter {
    #[inline]
    pub(crate) fn matches(&self, value: &Value) -> bool {
        match *self {
            Self::Any => true,
            Self::Tag(expected_tag) => match value.as_message() {
                Some(m) => m.tag == expected_tag,
                None => false,
            },
            Self::PayloadKind(kind) => match value.as_message() {
                Some(m) => m.payload.wire_tag() == kind,
                None => false,
            },
            Self::Correlation {
                expect_request_id,
                expect_sender,
            } => match value.as_message() {
                Some(m) => {
                    let id_ok = m.request_id == expect_request_id;
                    let sender_ok = match expect_sender {
                        Some(s) => m.sender == s,
                        None => true,
                    };
                    id_ok && sender_ok
                }
                None => false,
            },
            Self::TaggedCorrelation {
                tag,
                expect_request_id,
            } => match value.as_message() {
                Some(m) => m.tag == tag && m.request_id == expect_request_id,
                None => false,
            },
        }
    }
}

/// A flow's inbox, plus (when the owning flow is blocked on
/// `Receive` / `ReceiveMatch` / `Ask` with nothing to read) the parked flow itself.
///
/// # Why the flow lives *inside* its own mailbox while waiting
///
/// Internally, a flow is reachable through its [`FlowId`](crate::FlowId), which
/// resolves (via the runtime's flow directory) to this `Mailbox`. Bytecode
/// does **not** address by Pid anymore (FlowCap): `Send` / `Ask` resolve a
/// Cap to a FlowId first, then look up here. Host [`crate::Runtime::send`]
/// still uses FlowId directly (trusted).
///
/// Storing the blocked `Box<Flow>` directly in `MailboxInner::parked`, behind
/// the same mutex that guards the message queue, turns "deliver a message and
/// wake the receiver if it was waiting" into a single critical section — which
/// is what actually prevents the classic lost-wakeup race:
///
/// ```text
/// racing without a shared lock:
///   receiver: queue.pop() -> None
///   sender:   queue.push(msg); wake(receiver)   // receiver isn't parked yet!
///   receiver: park()                            // ...and now sleeps forever
///
/// with both steps under one mutex (what this type does):
///   receiver: lock; queue.pop() -> None; store self in `parked`; unlock
///   sender:   lock; parked.take() -> Some(receiver); unlock; wake(receiver)
/// ```
/// Because "check the queue" and "become parked" happen atomically with
/// respect to "push and check for a parked receiver", there is no window
/// where a message can be pushed without either landing in the queue for a
/// later `Receive` or immediately waking an already-parked one.
///
/// Wake only happens when a hop is **accepted and matches** the waiter.
/// A flow that is already runnable (message queued, nobody parked) does
/// not generate extra scheduler work — no wake storm on every `Send`.
///
/// # Selective wait (`ReceiveMatch` / `Ask`)
///
/// When parked with a selective (non-`Any`) filter, only a hop that
/// satisfies the filter wakes the flow. Other hops are appended to the
/// queue (subject to the bound) and the waiter stays parked (FIFO skip,
/// never drop matching semantics).
///
/// # Bound
///
/// `push` may return [`MailboxFull`] when the policy is Reject and the
/// logical capacity is already occupied **and** nobody matching is parked.
/// A parked waiter that matches the hop still takes a **handoff** — that
/// hop never occupies a queue slot.
pub struct Mailbox {
    inner: Mutex<MailboxInner>,
    config: MailboxConfig,
}

struct MailboxInner {
    queue: MailboxQueue,
    parked: Option<Box<Flow>>,
    /// Active while `parked` is `Some`. Ignored when nobody is waiting.
    parked_filter: WaitFilter,
    /// Bumped on every park install, so a deadline armed for an earlier
    /// wait can be told apart from the current one. See [`WaitEpoch`].
    wait_epoch: u64,
    stats: MailboxStats,
    /// Bytecode senders waiting for a free slot (`WAITING_SEND`).
    /// Woken one-at-a-time from [`Mailbox::admit_waiting_sender`].
    waiting_senders: VecDeque<WaitingSender>,
    /// Set by [`Mailbox::close`] during finalize so a late `park_sender`
    /// cannot land after `drain_waiting_senders` and leak the sender.
    closed: bool,
}

struct WaitingSender {
    flow: Box<Flow>,
    message: Value,
}

/// Outcome of pushing a message.
///
/// `Queued*` means the hop (or a replacement under DropOldest) lives in
/// the inbox for a later `Receive`. [`Handoff`](Delivery::Handoff) means a parked flow was
/// waiting for **this** hop — the caller (`worker::deliver` /
/// [`crate::Runtime::send`]) must `resume_with` and re-enqueue the flow.
/// Dropped variants never wake a waiter.
pub enum Delivery {
    Queued,
    QueuedDropOldest,
    DroppedNewest,
    Handoff(Box<Flow>),
}

/// Which of a mailbox's two bounds refused a hop.
///
/// Reported so an operator can tell "this flow is not draining its inbox"
/// ([`Self::MessageLimit`]) from "this flow is being sent payloads too
/// large for its budget" ([`Self::ByteLimit`]) without instrumenting the
/// sender. Those have different fixes: raise capacity / speed up the
/// receiver versus raise the byte budget / shrink the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailboxFullReason {
    /// [`MailboxCapacity`] hops are already queued.
    MessageLimit,
    /// Accepting the hop would exceed the [`MailboxBytes`] budget. Also
    /// reported when a single hop is larger than the entire budget, which
    /// no eviction policy can make room for.
    ByteLimit,
    /// Inbox was closed by finalize — the flow is exiting. Delivery must
    /// treat this like “target gone”, not like a capacity refusal.
    Closed,
}

impl std::fmt::Display for MailboxFullReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MailboxFullReason::MessageLimit => write!(f, "hop count limit"),
            MailboxFullReason::ByteLimit => write!(f, "byte budget"),
            MailboxFullReason::Closed => write!(f, "mailbox closed"),
        }
    }
}

/// Inbox at one of its logical bounds under [`OverflowPolicy::Reject`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailboxFull {
    reason: MailboxFullReason,
}

impl MailboxFull {
    #[inline]
    pub const fn reason(self) -> MailboxFullReason {
        self.reason
    }
}

impl std::fmt::Display for MailboxFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "mailbox full ({})", self.reason)
    }
}

impl std::error::Error for MailboxFull {}

/// Identifies **one specific park** of a flow in its mailbox.
///
/// # Why a timeout needs a token
///
/// Cancellation of a `ReceiveTimeout` deadline is lazy: the timer thread
/// does not remove its entry when a hop wakes the receiver early, it just
/// finds nobody parked when it eventually fires. That reasoning only holds
/// if the flow never parks *again* before the old deadline — and in a
/// receive loop it always does:
///
/// ```text
///   t=0    ReceiveTimeout(r5, 100ms)  -> park A, timer(100ms) armed
///   t=20   hop arrives                -> handoff, park A over, flow runs
///   t=30   Receive(r7)                -> park B  (no timeout)
///   t=100  timer for park A fires     -> takes park B!
///                                        writes Unit into r5, not r7
/// ```
///
/// That is a spurious wake *and* a write to the previous wait's register.
/// So [`Mailbox::park`] returns the epoch of the park it installed, the
/// timer carries it, and [`Mailbox::take_parked_at`] only hands the flow
/// over while that epoch is still current.
///
/// There is no public constructor: an epoch can only come from parking,
/// so a timeout cannot present one for a wait that never happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WaitEpoch(u64);

impl WaitEpoch {
    /// Raw counter value, for logs and metrics only. Never compare epochs
    /// from two different mailboxes: the counter is per-inbox.
    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for WaitEpoch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "wait#{}", self.0)
    }
}

impl Mailbox {
    pub fn new() -> Self {
        Self::with_config(MailboxConfig::DEFAULT)
    }

    pub fn with_config(config: MailboxConfig) -> Self {
        Mailbox {
            inner: Mutex::new(MailboxInner {
                queue: MailboxQueue::new(config.capacity().get(), config.bytes().get()),
                parked: None,
                parked_filter: WaitFilter::Any,
                wait_epoch: 0,
                stats: MailboxStats::default(),
                waiting_senders: VecDeque::new(),
                closed: false,
            }),
            config,
        }
    }

    #[inline]
    pub fn config(&self) -> MailboxConfig {
        self.config
    }

    /// Snapshot of enqueue/dequeue/drop counters plus current occupancy
    /// (taken under the mailbox lock, so the depth and the counters
    /// describe the same instant).
    pub fn stats(&self) -> Result<MailboxStats, RuntimeError> {
        let inner = sync_lock::lock(&self.inner, "Mailbox::stats")?;
        let mut stats = inner.stats;
        stats.queued_messages = inner.queue.len();
        stats.queued_bytes = inner.queue.bytes();
        Ok(stats)
    }

    /// Push `value` under the mailbox config.
    ///
    /// 1. If a matching waiter is parked → [`Delivery::Handoff`] (does not
    ///    consume a queue slot).
    /// 2. Else enqueue / overflow according to [`OverflowPolicy`].
    /// 3. [`MailboxFull`] only for Reject when the queue is already at one
    ///    of its logical bounds (see [`MailboxFullReason`]) — plus the one
    ///    case no policy can absorb: a hop larger than the whole byte
    ///    budget.
    ///
    /// Mutex poison → [`RuntimeError`] (fail-closed).
    pub fn push(&self, value: Value) -> Result<Result<Delivery, MailboxFull>, RuntimeError> {
        let mut inner = sync_lock::lock(&self.inner, "Mailbox::push")?;
        if inner.closed {
            return Ok(Err(MailboxFull {
                reason: MailboxFullReason::Closed,
            }));
        }
        if let Some(flow) = inner.parked.take() {
            if inner.parked_filter.matches(&value) {
                inner.parked_filter = WaitFilter::Any;
                inner.stats.dequeued = inner.stats.dequeued.saturating_add(1);
                inner.stats.enqueued = inner.stats.enqueued.saturating_add(1);
                return Ok(Ok(Delivery::Handoff(flow)));
            }
            inner.parked = Some(flow);
            return Ok(enqueue_locked(&mut inner, value, self.config.overflow()));
        }
        Ok(enqueue_locked(&mut inner, value, self.config.overflow()))
    }

    /// Non-blocking pop of the front hop (classic `Receive`).
    pub fn try_pop(&self) -> Result<Option<Value>, RuntimeError> {
        self.try_pop_filter(WaitFilter::Any)
    }

    /// Non-blocking selective pop by application `tag` (`ReceiveMatch`).
    pub fn try_pop_match(&self, tag: u16) -> Result<Option<Value>, RuntimeError> {
        self.try_pop_filter(WaitFilter::Tag(tag))
    }

    /// Non-blocking pop under an arbitrary [`WaitFilter`] (FIFO skip).
    pub(crate) fn try_pop_filter(&self, filter: WaitFilter) -> Result<Option<Value>, RuntimeError> {
        let mut inner = sync_lock::lock(&self.inner, "Mailbox::try_pop_filter")?;
        let got = inner.queue.take(filter);
        if got.is_some() {
            inner.stats.dequeued = inner.stats.dequeued.saturating_add(1);
        }
        Ok(got)
    }

    /// Atomically re-check the queue and, if still empty, store `flow`
    /// as parked (classic `Receive` — any hop wakes).
    ///
    /// Outer `Result` is infrastructure (mutex poison). Inner `Result` is
    /// the lost-wakeup race: `Err(flow)` means a message arrived between
    /// the worker's `try_pop` and this call — resume immediately with the
    /// stashed pending message rather than parking forever.
    ///
    /// `Ok(Ok(epoch))` identifies the park that was installed. A caller
    /// arming a `ReceiveTimeout` deadline must carry that [`WaitEpoch`] to
    /// [`Mailbox::take_parked_at`], or a late deadline will wake whatever
    /// wait happens to be current instead.
    pub fn park(&self, flow: Box<Flow>) -> Result<Result<WaitEpoch, Box<Flow>>, RuntimeError> {
        self.park_filter(flow, WaitFilter::Any)
            .map_err(|(e, _flow)| e)
    }

    /// Like [`park`](Self::park), but only a hop with `Message.tag == tag`
    /// ends the wait. Non-matching hops already in the queue are left
    /// untouched (FIFO skip).
    pub fn park_match(
        &self,
        flow: Box<Flow>,
        tag: u16,
    ) -> Result<Result<WaitEpoch, Box<Flow>>, RuntimeError> {
        self.park_filter(flow, WaitFilter::Tag(tag))
            .map_err(|(e, _flow)| e)
    }

    /// Park under an arbitrary filter. Re-checks the queue under the same
    /// mutex before installing the waiter (anti lost-wakeup).
    pub(crate) fn park_filter(
        &self,
        flow: Box<Flow>,
        filter: WaitFilter,
    ) -> Result<Result<WaitEpoch, Box<Flow>>, (RuntimeError, Box<Flow>)> {
        let mut inner = match sync_lock::lock(&self.inner, "Mailbox::park_filter") {
            Ok(g) => g,
            Err(e) => return Err((e, flow)),
        };
        if let Some(value) = inner.queue.take(filter) {
            inner.stats.dequeued = inner.stats.dequeued.saturating_add(1);
            drop(inner);
            return Ok(Err(with_pending(flow, value)));
        }
        debug_assert!(
            inner.parked.is_none(),
            "park_filter would overwrite a parked Flow"
        );
        // Wrapping, not saturating: a saturated counter would make every
        // later epoch compare equal, silently restoring the stale-deadline
        // bug this exists to prevent. Reuse needs 2^64 parks on one inbox.
        inner.wait_epoch = inner.wait_epoch.wrapping_add(1);
        let epoch = WaitEpoch(inner.wait_epoch);
        inner.parked_filter = filter;
        inner.parked = Some(flow);
        Ok(Ok(epoch))
    }

    /// Whether `value` can occupy this inbox when the queue is empty.
    /// A hop larger than the byte budget must never enter `WAITING_SEND`.
    #[inline]
    pub(crate) fn hop_can_ever_fit(&self, value: &Value) -> bool {
        value.memory_size() <= self.config.bytes().get()
    }

    /// Take the parked flow back out **only if** `epoch` is still the
    /// current wait, used by the timer when a `ReceiveTimeout` deadline
    /// fires.
    ///
    /// `None` means the deadline lost the race and must do nothing: either
    /// a hop already woke that wait, or the flow has since parked on a
    /// *different* `Receive` that this deadline does not own (see
    /// [`WaitEpoch`]).
    ///
    /// The selective filter is reset only when the flow is actually taken.
    /// Clearing it on a stale call would downgrade a live `ReceiveMatch` /
    /// `Ask` waiter to "any hop wakes me".
    pub fn take_parked_at(&self, epoch: WaitEpoch) -> Result<Option<Box<Flow>>, RuntimeError> {
        let mut inner = sync_lock::lock(&self.inner, "Mailbox::take_parked_at")?;
        if inner.wait_epoch != epoch.0 {
            return Ok(None);
        }
        match inner.parked.take() {
            Some(flow) => {
                inner.parked_filter = WaitFilter::Any;
                Ok(Some(flow))
            }
            None => Ok(None),
        }
    }

    /// Take the current parked receiver regardless of epoch (lifecycle kill).
    pub(crate) fn take_parked(&self) -> Result<Option<Box<Flow>>, RuntimeError> {
        let mut inner = sync_lock::lock(&self.inner, "Mailbox::take_parked")?;
        inner.parked_filter = WaitFilter::Any;
        Ok(inner.parked.take())
    }

    /// Enqueue a hop even when the inbox is at a bound (system `DOWN` /
    /// `TAG_SYS_EXIT` from `trap_exit` or orphaned Ask).
    pub(crate) fn push_system(&self, value: Value) -> Result<Delivery, RuntimeError> {
        let mut inner = sync_lock::lock(&self.inner, "Mailbox::push_system")?;
        // System lifecycle hops may still target a live owner; if that owner
        // is already closed, drop the hop (owner is exiting / gone).
        if inner.closed {
            return Ok(Delivery::Queued);
        }
        if let Some(flow) = inner.parked.take() {
            if inner.parked_filter.matches(&value) {
                inner.parked_filter = WaitFilter::Any;
                inner.stats.dequeued = inner.stats.dequeued.saturating_add(1);
                inner.stats.enqueued = inner.stats.enqueued.saturating_add(1);
                return Ok(Delivery::Handoff(flow));
            }
            inner.parked = Some(flow);
        }
        inner.queue.force_push(value);
        inner.stats.enqueued = inner.stats.enqueued.saturating_add(1);
        Ok(Delivery::Queued)
    }

    /// Close the inbox for new `WAITING_SEND` parks, then drain waiters.
    ///
    /// Called from `finalize_flow` **before** unregistering the directory
    /// so a sender that lost the Full/park race cannot park on a dead inbox.
    pub(crate) fn close(&self) -> Result<Vec<Flow>, RuntimeError> {
        let mut inner = sync_lock::lock(&self.inner, "Mailbox::close")?;
        inner.closed = true;
        Ok(inner.waiting_senders.drain(..).map(|w| *w.flow).collect())
    }

    /// Park a bytecode sender whose hop was refused (`WAITING_SEND`).
    ///
    /// Never drops `flow`: a closed or poisoned mailbox returns it in
    /// [`ParkSender::Closed`] so the worker can finalize.
    pub(crate) fn park_sender(&self, flow: Box<Flow>, message: Value) -> ParkSender {
        if !self.hop_can_ever_fit(&message) {
            return ParkSender::Undeliverable(flow);
        }
        match sync_lock::lock(&self.inner, "Mailbox::park_sender") {
            Ok(inner) if inner.closed => ParkSender::Closed(flow),
            Ok(mut inner) => {
                if inner.waiting_senders.len() >= self.config.capacity().get() {
                    return ParkSender::Closed(flow);
                }
                inner
                    .waiting_senders
                    .push_back(WaitingSender { flow, message });
                ParkSender::Parked
            }
            Err(e) => {
                super::error::report_fault(e);
                ParkSender::Closed(flow)
            }
        }
    }

    /// After a pop frees a slot, admit **one** waiting sender (no wake storm).
    pub(crate) fn admit_waiting_sender(&self) -> Result<AdmitSender, RuntimeError> {
        let mut inner = sync_lock::lock(&self.inner, "Mailbox::admit_waiting_sender")?;
        if inner.closed {
            return Ok(AdmitSender::Idle);
        }
        let Some(waiter) = inner.waiting_senders.pop_front() else {
            return Ok(AdmitSender::Idle);
        };
        if !inner.queue.can_ever_fit(waiter.message.memory_size()) {
            return Ok(AdmitSender::Undeliverable(waiter.flow));
        }
        match enqueue_locked(&mut inner, waiter.message.clone(), OverflowPolicy::Reject) {
            Ok(_) => Ok(AdmitSender::Woken(waiter.flow)),
            Err(_) => {
                inner.waiting_senders.push_front(waiter);
                Ok(AdmitSender::Idle)
            }
        }
    }

    /// Pull one parked sender out (link-kill of a flow blocked on `WAITING_SEND`).
    pub(crate) fn take_waiting_sender(
        &self,
        sender: FlowId,
    ) -> Result<Option<Box<Flow>>, RuntimeError> {
        let mut inner = sync_lock::lock(&self.inner, "Mailbox::take_waiting_sender")?;
        if let Some(pos) = inner
            .waiting_senders
            .iter()
            .position(|w| w.flow.id == sender)
        {
            return Ok(inner.waiting_senders.remove(pos).map(|w| w.flow));
        }
        Ok(None)
    }
}

/// Outcome of [`Mailbox::park_sender`].
pub(crate) enum ParkSender {
    Parked,
    /// Inbox already closed (target finalizing), waiter cap reached, or lock poisoned.
    Closed(Box<Flow>),
    /// Hop is larger than the inbox byte budget — parking would never unblock.
    Undeliverable(Box<Flow>),
}

/// Outcome of [`Mailbox::admit_waiting_sender`].
pub(crate) enum AdmitSender {
    Woken(Box<Flow>),
    Idle,
    /// Head waiter can never fit an empty inbox (oversized hop).
    Undeliverable(Box<Flow>),
}

impl Default for Mailbox {
    fn default() -> Self {
        Self::new()
    }
}

fn enqueue_locked(
    inner: &mut MailboxInner,
    value: Value,
    policy: OverflowPolicy,
) -> Result<Delivery, MailboxFull> {
    match inner.queue.enqueue(value, policy) {
        Ok(EnqueueEffect::Enqueued) => {
            inner.stats.enqueued = inner.stats.enqueued.saturating_add(1);
            Ok(Delivery::Queued)
        }
        Ok(EnqueueEffect::DroppedOldest) => {
            inner.stats.dropped_oldest = inner.stats.dropped_oldest.saturating_add(1);
            inner.stats.enqueued = inner.stats.enqueued.saturating_add(1);
            Ok(Delivery::QueuedDropOldest)
        }
        Ok(EnqueueEffect::DroppedNewest) => {
            inner.stats.dropped_newest = inner.stats.dropped_newest.saturating_add(1);
            Ok(Delivery::DroppedNewest)
        }
        Err(reason) => {
            inner.stats.rejected = inner.stats.rejected.saturating_add(1);
            if reason == MailboxFullReason::ByteLimit {
                inner.stats.rejected_byte_limit = inner.stats.rejected_byte_limit.saturating_add(1);
            }
            Err(MailboxFull { reason })
        }
    }
}

/// Stashes a message that arrived just as we were about to park, so the
/// worker loop can resume the flow with it on the very next step
/// without re-entering the mailbox. See [`Mailbox::park`].
fn with_pending(mut flow: Box<Flow>, value: Value) -> Box<Flow> {
    flow.pending_message = Some(value);
    flow
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::builder::ChunkBuilder;
    use crate::bytecode::Message;
    use crate::bytecode::RestartPolicy;
    use crate::scheduler::oneshot;
    use crate::scheduler::process::next_flow_id;
    use crate::vm::{NativeTable, Vm};
    use std::sync::Arc;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn dummy_flow() -> Result<Box<Flow>, Box<dyn std::error::Error>> {
        let mut b = ChunkBuilder::new("mb");
        b.begin_function("main", 0, 1);
        b.emit_return(0);
        let chunk = b.finish();
        let vm = Vm::new(Arc::new(chunk), NativeTable::empty(), 0, &[])?;
        let (tx, _rx) = oneshot::channel();
        Ok(Box::new(Flow::new(
            next_flow_id(),
            vm,
            Arc::new(Mailbox::new()),
            RestartPolicy::Never,
            tx,
        )))
    }

    fn hop_payload_int(msg: &Message) -> i64 {
        msg.payload.as_int().unwrap_or(0)
    }

    fn hop(sender: u64, request_id: u64, tag: u16, payload: impl Into<Value>) -> Value {
        Value::Message(Message::new(sender, request_id, tag, payload))
    }

    fn msg(tag: u16, payload: impl Into<Value>) -> Value {
        hop(1, 1, tag, payload)
    }

    fn tiny_reject(n: u32) -> Result<Mailbox, Box<dyn std::error::Error>> {
        let cap = MailboxCapacity::new(n).ok_or("invalid mailbox capacity")?;
        Ok(Mailbox::with_config(MailboxConfig::new(
            cap,
            OverflowPolicy::Reject,
        )))
    }

    fn hop_msg(value: &Value) -> Result<&Message, Box<dyn std::error::Error>> {
        value
            .as_message()
            .ok_or_else(|| "expected Message hop".into())
    }

    #[test]
    fn try_pop_match_skips_non_matching_fifo() -> TestResult {
        let mb = Mailbox::new();
        mb.push(msg(9, 1))??;
        mb.push(msg(1, 42))??;
        mb.push(msg(9, 2))??;
        let got = mb.try_pop_match(1)?.ok_or("match")?;
        assert_eq!(hop_payload_int(hop_msg(&got)?), 42);
        assert_eq!(hop_msg(&mb.try_pop()?.ok_or("first leftover")?)?.tag, 9);
        assert_eq!(
            hop_payload_int(hop_msg(&mb.try_pop()?.ok_or("second leftover")?)?),
            2
        );
        Ok(())
    }

    #[test]
    fn push_while_park_match_queues_junk_keeps_waiter() -> TestResult {
        let mb = Mailbox::new();
        let flow = dummy_flow()?;
        assert!(mb.park_match(flow, 1)?.is_ok());
        assert!(matches!(mb.push(msg(9, 0))??, Delivery::Queued));
        assert!(matches!(mb.push(msg(1, 7))??, Delivery::Handoff(_)));
        assert_eq!(hop_msg(&mb.try_pop()?.ok_or("queued junk")?)?.tag, 9);
        Ok(())
    }

    #[test]
    fn ask_does_not_consume_reply_for_another_request() -> TestResult {
        let mb = Mailbox::new();
        mb.push(hop(10, 2, 2, 99))??;
        mb.push(hop(10, 1, 2, 42))??;
        let filter = WaitFilter::Correlation {
            expect_request_id: 1,
            expect_sender: Some(10),
        };
        let got = mb.try_pop_filter(filter)?.ok_or("id=1")?;
        assert_eq!(hop_payload_int(hop_msg(&got)?), 42);
        let left = mb.try_pop()?.ok_or("leftover")?;
        assert_eq!(hop_msg(&left)?.request_id, 2);
        Ok(())
    }

    #[test]
    fn tagged_correlation_skips_wrong_id_and_tag() -> TestResult {
        let mb = Mailbox::new();
        mb.push(hop(10, 1, 1, 7))??;
        mb.push(hop(10, 2, 2, 9))??;
        mb.push(hop(10, 1, 2, 42))??;
        assert!(mb
            .try_pop_filter(WaitFilter::TaggedCorrelation {
                tag: 99,
                expect_request_id: 1,
            })?
            .is_none());
        let got = mb
            .try_pop_filter(WaitFilter::TaggedCorrelation {
                tag: 2,
                expect_request_id: 1,
            })?
            .ok_or("tag=2 id=1")?;
        assert_eq!(hop_payload_int(hop_msg(&got)?), 42);
        Ok(())
    }

    #[test]
    fn ask_requires_reply_from_target() -> TestResult {
        let mb = Mailbox::new();
        let flow = dummy_flow()?;
        let filter = WaitFilter::Correlation {
            expect_request_id: 1,
            expect_sender: Some(10),
        };
        assert!(mb.park_filter(flow, filter).map_err(|(e, _)| e)?.is_ok());
        assert!(matches!(mb.push(hop(99, 1, 2, 0))??, Delivery::Queued));
        assert!(matches!(mb.push(hop(10, 1, 2, 42))??, Delivery::Handoff(_)));
        assert_eq!(
            hop_msg(&mb.try_pop()?.ok_or("non-matching queued")?)?.sender,
            99
        );
        Ok(())
    }

    #[test]
    fn reject_when_full_without_waiter() -> TestResult {
        let mb = tiny_reject(1)?;
        assert!(matches!(mb.push(msg(1, 1))??, Delivery::Queued));
        // Bind the error: `Err(MailboxFull)` would introduce a *variable*
        // named MailboxFull now that the type carries a reason, matching
        // every error and asserting nothing.
        match mb.push(msg(1, 2))? {
            Err(full) => assert_eq!(full.reason(), MailboxFullReason::MessageLimit),
            Ok(_) => return Err("expected the hop count bound to refuse".into()),
        }
        let s = mb.stats()?;
        assert_eq!(s.enqueued, 1);
        assert_eq!(s.rejected, 1);
        assert_eq!(s.rejected_byte_limit, 0);
        assert_eq!(s.queued_messages, 1);
        Ok(())
    }

    /// Park `flow`, requiring that it actually parked, and hand back the
    /// epoch. Collapses the two-level `Result` the tests do not care about.
    fn park_now(mb: &Mailbox, flow: Box<Flow>) -> Result<WaitEpoch, Box<dyn std::error::Error>> {
        match mb.park(flow)? {
            Ok(epoch) => Ok(epoch),
            Err(_) => Err("an empty mailbox should have parked the flow".into()),
        }
    }

    fn handoff(mb: &Mailbox, value: Value) -> Result<Box<Flow>, Box<dyn std::error::Error>> {
        match mb.push(value)?? {
            Delivery::Handoff(flow) => Ok(flow),
            other => Err(format!("expected a handoff, got {}", delivery_name(&other)).into()),
        }
    }

    fn delivery_name(d: &Delivery) -> &'static str {
        match d {
            Delivery::Queued => "Queued",
            Delivery::QueuedDropOldest => "QueuedDropOldest",
            Delivery::DroppedNewest => "DroppedNewest",
            Delivery::Handoff(_) => "Handoff",
        }
    }

    #[test]
    fn a_stale_deadline_cannot_steal_a_later_wait() -> TestResult {
        let mb = Mailbox::new();
        let first = park_now(&mb, dummy_flow()?)?;
        // A hop beats the deadline: the handoff ends *this* wait.
        let woken = handoff(&mb, msg(1, 1))?;
        // The same flow parks again on a fresh `Receive`.
        let second = park_now(&mb, woken)?;
        assert_ne!(first, second, "each park must get its own epoch");

        // The deadline armed for the first wait fires late. Before the
        // epoch check it took this second wait, resumed the flow, and
        // wrote Unit into the *first* wait's register.
        assert!(mb.take_parked_at(first)?.is_none());
        // The live wait is untouched, so its own deadline still works.
        assert!(mb.take_parked_at(second)?.is_some());
        Ok(())
    }

    #[test]
    fn a_stale_deadline_does_not_downgrade_a_selective_waiter() -> TestResult {
        let mb = Mailbox::new();
        let first = park_now(&mb, dummy_flow()?)?;
        let woken = handoff(&mb, msg(1, 1))?;
        // Second wait is selective: only tag 7 may wake it.
        let second = match mb.park_match(woken, 7)? {
            Ok(epoch) => epoch,
            Err(_) => return Err("empty mailbox should have parked the flow".into()),
        };
        assert_ne!(first, second);

        assert!(mb.take_parked_at(first)?.is_none());
        // The filter must survive the stale call: a tag-9 hop is queued,
        // not handed off.
        assert!(matches!(mb.push(msg(9, 0))??, Delivery::Queued));
        // ...and tag 7 still wakes it.
        assert!(matches!(mb.push(msg(7, 0))??, Delivery::Handoff(_)));
        Ok(())
    }

    #[test]
    fn a_deadline_for_a_wait_that_a_hop_ended_does_nothing() -> TestResult {
        let mb = Mailbox::new();
        let epoch = park_now(&mb, dummy_flow()?)?;
        let _woken = handoff(&mb, msg(1, 1))?;
        // Nobody is parked now; the deadline must be a no-op rather than
        // reporting a flow it does not have.
        assert!(mb.take_parked_at(epoch)?.is_none());
        Ok(())
    }

    #[test]
    fn byte_budget_refuses_before_the_hop_count_and_says_so() -> TestResult {
        // 64 hop slots but a 1 KiB budget: blobs exhaust bytes first.
        let cap = MailboxCapacity::new(64).ok_or("cap")?;
        let budget = MailboxBytes::new(MailboxBytes::MIN).ok_or("bytes")?;
        let mb = Mailbox::with_config(
            MailboxConfig::new(cap, OverflowPolicy::Reject).with_bytes(budget),
        );
        assert!(matches!(
            mb.push(Value::bytes(vec![0u8; 900]))??,
            Delivery::Queued
        ));
        match mb.push(Value::bytes(vec![0u8; 900]))? {
            Err(full) => assert_eq!(full.reason(), MailboxFullReason::ByteLimit),
            Ok(_) => return Err("expected the byte budget to refuse".into()),
        }
        let s = mb.stats()?;
        assert_eq!(s.queued_messages, 1);
        assert!(s.queued_bytes >= 900);
        assert_eq!(s.rejected, 1);
        assert_eq!(s.rejected_byte_limit, 1);
        Ok(())
    }

    #[test]
    fn draining_a_hop_frees_its_byte_charge() -> TestResult {
        let cap = MailboxCapacity::new(64).ok_or("cap")?;
        let budget = MailboxBytes::new(MailboxBytes::MIN).ok_or("bytes")?;
        let mb = Mailbox::with_config(
            MailboxConfig::new(cap, OverflowPolicy::Reject).with_bytes(budget),
        );
        mb.push(Value::bytes(vec![0u8; 900]))??;
        mb.try_pop()?.ok_or("queued blob")?;
        assert_eq!(mb.stats()?.queued_bytes, 0);
        // A receiver that keeps up must not be permanently throttled by a
        // charge that was never refunded.
        assert!(matches!(
            mb.push(Value::bytes(vec![0u8; 900]))??,
            Delivery::Queued
        ));
        Ok(())
    }

    #[test]
    fn matching_handoff_does_not_count_as_full() -> TestResult {
        let mb = tiny_reject(1)?;
        mb.push(msg(9, 0))??;
        let flow = dummy_flow()?;
        assert!(mb.park_match(flow, 1)?.is_ok());
        assert!(matches!(mb.push(msg(1, 7))??, Delivery::Handoff(_)));
        assert_eq!(hop_msg(&mb.try_pop()?.ok_or("queued")?)?.tag, 9);
        Ok(())
    }

    #[test]
    fn waiting_send_admits_one_after_pop() -> TestResult {
        let mb = tiny_reject(1)?;
        assert!(matches!(mb.push(msg(1, 1))??, Delivery::Queued));
        assert!(matches!(
            mb.park_sender(dummy_flow()?, msg(1, 2)),
            ParkSender::Parked
        ));
        assert!(mb.try_pop()?.is_some());
        match mb.admit_waiting_sender()? {
            AdmitSender::Woken(woken) => drop(woken),
            AdmitSender::Idle => return Err("expected admitted sender, got Idle".into()),
            AdmitSender::Undeliverable(_) => {
                return Err("expected admitted sender, got Undeliverable".into())
            }
        }
        assert_eq!(
            hop_payload_int(hop_msg(&mb.try_pop()?.ok_or("second hop")?)?),
            2
        );
        assert!(matches!(mb.admit_waiting_sender()?, AdmitSender::Idle));
        Ok(())
    }

    #[test]
    fn close_rejects_late_park_sender() -> TestResult {
        let mb = tiny_reject(1)?;
        let leftover = mb.close()?;
        assert!(leftover.is_empty());
        match mb.park_sender(dummy_flow()?, msg(1, 1)) {
            ParkSender::Closed(_) => {}
            ParkSender::Parked => return Err("closed mailbox must not park a sender".into()),
            ParkSender::Undeliverable(_) => {
                return Err("closed mailbox should report Closed, not Undeliverable".into())
            }
        }
        Ok(())
    }

    #[test]
    fn close_rejects_late_push() -> TestResult {
        let mb = tiny_reject(4)?;
        let _ = mb.close()?;
        match mb.push(msg(1, 1))? {
            Err(full) if full.reason() == MailboxFullReason::Closed => Ok(()),
            Err(full) => Err(format!("expected Closed, got {}", full.reason()).into()),
            Ok(_) => Err("closed mailbox must refuse push".into()),
        }
    }

    #[test]
    fn oversized_hop_is_undeliverable_not_parked() -> TestResult {
        let cap = MailboxCapacity::new(8).ok_or("cap")?;
        let budget = MailboxBytes::new(MailboxBytes::MIN).ok_or("bytes")?;
        let mb = Mailbox::with_config(
            MailboxConfig::new(cap, OverflowPolicy::Reject).with_bytes(budget),
        );
        let huge = Value::bytes(vec![0u8; MailboxBytes::MIN + 64]);
        match mb.park_sender(dummy_flow()?, huge.clone()) {
            ParkSender::Undeliverable(_) => {}
            ParkSender::Parked => return Err("oversized hop must not park".into()),
            ParkSender::Closed(_) => return Err("oversized hop must be Undeliverable".into()),
        }
        mb.push(msg(1, 1))??;
        mb.try_pop()?.ok_or("drain")?;
        // A hop that can never fit must not sit at the head of waiting_senders.
        match mb.admit_waiting_sender()? {
            AdmitSender::Idle => {}
            AdmitSender::Woken(_) => {
                return Err("expected Idle after refusing oversized hop, got Woken".into())
            }
            AdmitSender::Undeliverable(_) => {
                return Err("expected Idle after refusing oversized hop, got Undeliverable".into())
            }
        }
        let _ = huge;
        Ok(())
    }

    #[test]
    fn drop_oldest_still_wakes_on_match() -> TestResult {
        let cap = MailboxCapacity::new(1).ok_or("cap")?;
        let mb = Mailbox::with_config(MailboxConfig::new(cap, OverflowPolicy::DropOldest));
        let flow = dummy_flow()?;
        assert!(mb.park_match(flow, 1)?.is_ok());
        mb.push(msg(9, 1))??;
        assert!(matches!(mb.push(msg(1, 2))??, Delivery::Handoff(_)));
        Ok(())
    }
}
