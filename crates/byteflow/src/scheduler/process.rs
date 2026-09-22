use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::bytecode::{Cap, CapRights, CapTarget, RestartPolicy, RevocationCell};
use crate::vm::Vm;

use super::mailbox::Mailbox;
use super::oneshot;

/// Identifier of a **flow** — Byteflow's unit of concurrent work.
///
/// Host APIs and the directory key on this type. Inside messages it appears
/// as [`crate::Value::Pid`] (`Message.sender` / `msg_sender`) for **identity**.
/// Bytecode addressing uses [`crate::Value::Cap`] (FlowCap) — a Pid is not a
/// Send/Ask authority token.
///
/// Backed by a single global, wait-free `AtomicU64` counter rather than
/// anything derived from memory addresses: ids must stay unique for the
/// lifetime of the runtime and must **never** be reused, or a stale id in
/// someone's registers could address a *different* later flow (ABA) via the
/// host/`Directory` path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FlowId(pub(crate) u64);

impl FlowId {
    /// Embedder origin for host `Runtime::send`. Never spawned, never finalized.
    pub const HOST: FlowId = FlowId(0);

    pub fn as_u64(self) -> u64 {
        self.0
    }

    #[inline]
    pub fn is_host(self) -> bool {
        self.0 == 0
    }
}

impl std::fmt::Display for FlowId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "flow#{}", self.0)
    }
}

static NEXT_FLOW_ID: AtomicU64 = AtomicU64::new(1);
static FLOW_IDS_EXHAUSTED: AtomicBool = AtomicBool::new(false);

/// Allocate the next id. Tests and host helpers; spawn uses
/// [`try_next_flow_id`] so wrap-around cannot mint [`FlowId::HOST`].
pub fn next_flow_id() -> FlowId {
    match try_next_flow_id() {
        Ok(id) => id,
        Err(_) => FlowId(u64::MAX),
    }
}

/// Fail closed if the counter wrapped onto [`FlowId::HOST`] (`0`).
/// After the first wrap the allocator stays exhausted so later ids cannot
/// collide with live flows (ABA).
pub(crate) fn try_next_flow_id() -> Result<FlowId, super::error::RuntimeError> {
    if FLOW_IDS_EXHAUSTED.load(Ordering::Relaxed) {
        return Err(super::error::RuntimeError::FlowIdExhausted);
    }
    let id = NEXT_FLOW_ID.fetch_add(1, Ordering::Relaxed);
    if id == 0 {
        FLOW_IDS_EXHAUSTED.store(true, Ordering::Relaxed);
        return Err(super::error::RuntimeError::FlowIdExhausted);
    }
    Ok(FlowId(id))
}

/// Live counters for one flow (updated only by the worker currently
/// running it).
#[derive(Debug, Default)]
pub struct FlowMetrics {
    pub instructions: AtomicU64,
    /// Atomic hops sent (`Send` of [`crate::Value::Message`]).
    pub messages_sent: AtomicU64,
    pub messages_received: AtomicU64,
    pub reschedules: AtomicU64,
}

/// A single **flow**: VM state, mailbox, and bookkeeping.
///
/// This is the unit of work moved by the scheduler — pushed onto worker
/// deques, stolen, parked inside a [`Mailbox`] on `Receive`, or held by
/// the timer wheel while sleeping. Flows talk only via **Atomic Hops**
/// ([`crate::Value::Message`] on `Send`).
pub struct Flow {
    pub id: FlowId,
    pub vm: Vm,
    pub mailbox: Arc<Mailbox>,
    pub metrics: Arc<FlowMetrics>,
    pub restart_policy: RestartPolicy,
    /// Completion channel consumed by [`super::handle::FlowHandle::join`].
    pub(crate) completion: oneshot::Sender<FlowOutcome>,
    /// Set by [`Mailbox::park`] when a hop wins the park race.
    pub pending_message: Option<crate::bytecode::Value>,
    /// Destination register of the most recent `Receive` /
    /// `ReceiveTimeout` / selective receive (`ReceiveMatch*`).
    pub last_receive_dest: Option<u8>,
    /// Continuation after a `WAITING_SEND` park is admitted.
    pub(crate) pending_send: Option<PendingSend>,
    pub(crate) supervisor: Option<super::supervisor::SupervisorLink>,
    /// Self-authority (rights + native mask). Derived only via [`Cap::attenuate`].
    pub(crate) authority: Cap,
    pub(crate) cell: Arc<RevocationCell>,
    pub(crate) quota: Arc<super::quota::FlowQuota>,
}

/// What the worker should do after a parked sender's hop is admitted.
#[derive(Debug, Clone, Copy)]
pub(crate) enum PendingSend {
    FireAndForget,
    Ask {
        dest_reg: u8,
        expect_request_id: u64,
        expect_sender: u64,
        timeout: Option<Duration>,
    },
}

/// Terminal outcome of a flow, delivered to whoever holds its
/// [`super::handle::FlowHandle`].
#[derive(Clone, Debug, PartialEq)]
pub enum FlowOutcome {
    Completed(crate::bytecode::Value),
    Failed(String),
}

impl Flow {
    pub fn new(
        id: FlowId,
        vm: Vm,
        mailbox: Arc<Mailbox>,
        restart_policy: RestartPolicy,
        completion: oneshot::Sender<FlowOutcome>,
    ) -> Self {
        let cell = Arc::new(RevocationCell::new());
        let authority = Cap::root(
            CapTarget::Flow(id.as_u64()),
            CapRights::NONE,
            None,
            cell.as_ref(),
        );
        Self {
            id,
            vm,
            mailbox,
            metrics: Arc::new(FlowMetrics::default()),
            restart_policy,
            completion,
            pending_message: None,
            last_receive_dest: None,
            pending_send: None,
            supervisor: None,
            authority,
            cell,
            quota: Arc::new(super::quota::FlowQuota::from_config(
                super::quota::QuotaConfig::default(),
            )),
        }
    }

    pub(crate) fn complete(self, outcome: FlowOutcome) {
        self.completion.send(outcome);
    }
}
