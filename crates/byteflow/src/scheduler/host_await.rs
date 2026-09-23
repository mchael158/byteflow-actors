//! Host ↔ flow async bridge (`Opcode::HostAwait`).
//!
//! The worker parks the flow and hands a [`HostAwaitCompleter`] to the
//! embedder. Completion is a register writeback (same path as `Receive`),
//! not a mailbox hop and not a `CallNative` slot.
//!
//! The bridge itself must return quickly from [`HostAwaitBridge::submit`];
//! heavy work belongs on a host thread / Tokio task outside this crate.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::bytecode::Value;

use super::error::RuntimeError;
use super::finalize::{finalize_flow, resume_or_fail};
use super::monitor::FlowExitReason;
use super::process::{Flow, FlowId, FlowOutcome};
use super::runtime::{wake_workers, Shared};
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
/// completer is dropped unused, the parked flow is failed closed (avoids a
/// permanent wait when the host forgets to settle the await).
///
/// Holds a [`Weak`] to the runtime shared state so a bridge that retains
/// completers cannot keep the runtime alive in a reference cycle
/// (`Shared` → bridge → completer → `Shared`).
pub struct HostAwaitCompleter {
    shared: Weak<Shared>,
    ticket: u64,
    consumed: AtomicBool,
}

/// Errors from [`HostAwaitCompleter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostAwaitError {
    /// Ticket already used, flow finalized, or await cancelled.
    Stale,
    /// Infrastructure failure while completing.
    Runtime(RuntimeError),
}

impl std::fmt::Display for HostAwaitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostAwaitError::Stale => f.write_str("HostAwait completer is stale"),
            HostAwaitError::Runtime(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for HostAwaitError {}

impl HostAwaitCompleter {
    pub(crate) fn new(shared: Arc<Shared>, ticket: u64) -> Self {
        Self {
            shared: Arc::downgrade(&shared),
            ticket,
            consumed: AtomicBool::new(false),
        }
    }

    /// Resume the parked flow with `value` in the await destination register.
    pub fn complete(self, value: Value) -> Result<(), HostAwaitError> {
        self.finish(Ok(value))
    }

    /// Fail the parked flow (`FlowOutcome::Failed`).
    pub fn fail(self, msg: impl Into<String>) -> Result<(), HostAwaitError> {
        self.finish(Err(msg.into()))
    }

    fn finish(self, result: Result<Value, String>) -> Result<(), HostAwaitError> {
        if self.consumed.swap(true, Ordering::SeqCst) {
            return Err(HostAwaitError::Stale);
        }
        settle_parked(&self.shared, self.ticket, result)
    }
}

impl Drop for HostAwaitCompleter {
    fn drop(&mut self) {
        if self.consumed.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = settle_parked(
            &self.shared,
            self.ticket,
            Err("HostAwait completer dropped".into()),
        );
    }
}

fn settle_parked(
    shared: &Weak<Shared>,
    ticket: u64,
    result: Result<Value, String>,
) -> Result<(), HostAwaitError> {
    let Some(shared) = shared.upgrade() else {
        return Err(HostAwaitError::Stale);
    };
    let parked = shared
        .host_awaits
        .take_ticket(ticket)
        .map_err(HostAwaitError::Runtime)?;
    let Some(parked) = parked else {
        return Err(HostAwaitError::Stale);
    };
    match result {
        Ok(value) => {
            let Some(flow) = resume_or_fail(&shared, parked.flow, parked.dest_reg, value) else {
                return Ok(());
            };
            shared.injector.push(flow);
            wake_workers(&shared);
            Ok(())
        }
        Err(msg) => {
            finalize_flow(
                &shared,
                *parked.flow,
                FlowOutcome::Failed(msg),
                FlowExitReason::Fault,
            );
            Ok(())
        }
    }
}

/// Embedder hook: schedule host work for a parked [`Opcode::HostAwait`](crate::Opcode::HostAwait).
///
/// `submit` must return promptly. Spawn a thread / enqueue a task for I/O.
pub trait HostAwaitBridge: Send + Sync + std::fmt::Debug + 'static {
    fn submit(&self, req: HostAwaitRequest, done: HostAwaitCompleter);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostAwaitParkError {
    Duplicate,
    LimitReached { current: usize, max: usize },
}

impl std::fmt::Display for HostAwaitParkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostAwaitParkError::Duplicate => {
                f.write_str("duplicate in-flight HostAwait for this flow")
            }
            HostAwaitParkError::LimitReached { current, max } => {
                write!(f, "HostAwait limit reached ({current}/{max})")
            }
        }
    }
}

pub(crate) struct ParkedHostAwait {
    pub flow: Box<Flow>,
    pub dest_reg: u8,
}

/// Process-wide table of flows parked on `HostAwait`.
pub struct HostAwaitIndex {
    inner: Mutex<HostAwaitInner>,
    max: usize,
    next_ticket: AtomicU64,
}

struct HostAwaitInner {
    by_ticket: HashMap<u64, ParkedHostAwait>,
    by_flow: HashMap<FlowId, u64>,
}

impl HostAwaitIndex {
    /// `max_host_awaits == 0` → unlimited.
    pub fn new(max_host_awaits: u32) -> Self {
        Self {
            inner: Mutex::new(HostAwaitInner {
                by_ticket: HashMap::new(),
                by_flow: HashMap::new(),
            }),
            max: max_host_awaits as usize,
            next_ticket: AtomicU64::new(1),
        }
    }

    #[allow(clippy::type_complexity)]
    pub fn park(
        &self,
        flow: Box<Flow>,
        dest_reg: u8,
    ) -> Result<Result<(u64, FlowId), (HostAwaitParkError, Box<Flow>)>, (RuntimeError, Box<Flow>)>
    {
        let flow_id = flow.id;
        let mut g = match sync_lock::lock(&self.inner, "HostAwaitIndex::park") {
            Ok(g) => g,
            Err(e) => return Err((e, flow)),
        };
        if g.by_flow.contains_key(&flow_id) {
            return Ok(Err((HostAwaitParkError::Duplicate, flow)));
        }
        if self.max != 0 && g.by_ticket.len() >= self.max {
            return Ok(Err((
                HostAwaitParkError::LimitReached {
                    current: g.by_ticket.len(),
                    max: self.max,
                },
                flow,
            )));
        }
        let ticket = self.next_ticket.fetch_add(1, Ordering::Relaxed);
        g.by_flow.insert(flow_id, ticket);
        g.by_ticket.insert(
            ticket,
            ParkedHostAwait {
                flow,
                dest_reg,
            },
        );
        Ok(Ok((ticket, flow_id)))
    }

    pub fn take_ticket(&self, ticket: u64) -> Result<Option<ParkedHostAwait>, RuntimeError> {
        let mut g = sync_lock::lock(&self.inner, "HostAwaitIndex::take_ticket")?;
        let Some(parked) = g.by_ticket.remove(&ticket) else {
            return Ok(None);
        };
        g.by_flow.remove(&parked.flow.id);
        Ok(Some(parked))
    }

    /// Pull a parked await by flow id (kill / link extract).
    pub fn take_flow(&self, id: FlowId) -> Result<Option<Box<Flow>>, RuntimeError> {
        let mut g = sync_lock::lock(&self.inner, "HostAwaitIndex::take_flow")?;
        let Some(ticket) = g.by_flow.remove(&id) else {
            return Ok(None);
        };
        Ok(g.by_ticket.remove(&ticket).map(|p| p.flow))
    }

    /// Drop any ticket mapping for `id` without returning a flow (finalize cleanup).
    pub fn forget_flow(&self, id: FlowId) -> Result<(), RuntimeError> {
        let mut g = sync_lock::lock(&self.inner, "HostAwaitIndex::forget_flow")?;
        if let Some(ticket) = g.by_flow.remove(&id) {
            g.by_ticket.remove(&ticket);
        }
        Ok(())
    }

    /// Take every parked await (runtime shutdown). Late completers become
    /// [`HostAwaitError::Stale`].
    pub(crate) fn drain_all(&self) -> Result<Vec<ParkedHostAwait>, RuntimeError> {
        let mut g = sync_lock::lock(&self.inner, "HostAwaitIndex::drain_all")?;
        g.by_flow.clear();
        Ok(g.by_ticket.drain().map(|(_, parked)| parked).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::builder::ChunkBuilder;
    use crate::bytecode::RestartPolicy;
    use crate::scheduler::mailbox::Mailbox;
    use crate::scheduler::oneshot;
    use crate::scheduler::process::next_flow_id;
    use crate::vm::{NativeTable, Vm};

    fn dummy_flow() -> Result<Box<Flow>, Box<dyn std::error::Error>> {
        let mut b = ChunkBuilder::new("ha");
        b.begin_function("main", 0, 2);
        b.emit_load_imm(0, 0);
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

    #[test]
    fn park_hits_process_limit() -> Result<(), Box<dyn std::error::Error>> {
        let idx = HostAwaitIndex::new(1);
        match idx.park(dummy_flow()?, 0) {
            Ok(Ok(_)) => {}
            Ok(Err((e, _))) => return Err(e.to_string().into()),
            Err((e, _)) => return Err(e.into()),
        }
        match idx.park(dummy_flow()?, 0) {
            Ok(Err((HostAwaitParkError::LimitReached { current: 1, max: 1 }, _))) => Ok(()),
            Ok(Ok(_)) => Err("expected limit".into()),
            Ok(Err((e, _))) => Err(format!("wrong park error: {e}").into()),
            Err((e, _)) => Err(e.into()),
        }
    }

    #[test]
    fn take_ticket_clears_membership() -> Result<(), Box<dyn std::error::Error>> {
        let idx = HostAwaitIndex::new(0);
        let (ticket, id) = match idx.park(dummy_flow()?, 2) {
            Ok(Ok(v)) => v,
            Ok(Err((e, _))) => return Err(e.to_string().into()),
            Err((e, _)) => return Err(e.into()),
        };
        let parked = idx.take_ticket(ticket)?.expect("take");
        assert_eq!(parked.flow.id, id);
        assert_eq!(parked.dest_reg, 2);
        assert!(idx.take_ticket(ticket)?.is_none());
        assert!(idx.take_flow(id)?.is_none());
        Ok(())
    }

    #[test]
    fn park_reuses_slot_after_take() -> Result<(), Box<dyn std::error::Error>> {
        let idx = HostAwaitIndex::new(1);
        let (ticket, _) = match idx.park(dummy_flow()?, 0) {
            Ok(Ok(v)) => v,
            Ok(Err((e, _))) => return Err(e.to_string().into()),
            Err((e, _)) => return Err(e.into()),
        };
        assert!(idx.take_ticket(ticket)?.is_some());
        match idx.park(dummy_flow()?, 0) {
            Ok(Ok(_)) => Ok(()),
            Ok(Err((e, _))) => Err(format!("expected reuse after take, got {e}").into()),
            Err((e, _)) => Err(e.into()),
        }
    }

    #[test]
    fn unlimited_max_allows_many() -> Result<(), Box<dyn std::error::Error>> {
        let idx = HostAwaitIndex::new(0);
        for _ in 0..8 {
            match idx.park(dummy_flow()?, 0) {
                Ok(Ok(_)) => {}
                Ok(Err((e, _))) => return Err(e.to_string().into()),
                Err((e, _)) => return Err(e.into()),
            }
        }
        Ok(())
    }
}

/// Runtime-level HostAwait coverage (bridge wiring, limits, lifecycle).
#[cfg(test)]
mod runtime_tests {
    use super::*;
    use crate::bytecode::Program;
    use crate::scheduler::mailbox::MailboxConfig;
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

        fn wait_for_submissions(&self, n: usize) {
            let mut g = self.state.lock().expect("HoldBridge lock");
            while g.submitted < n {
                g = self.cv.wait(g).expect("HoldBridge wait");
            }
        }

        fn take_all(&self) -> Vec<(FlowId, HostAwaitCompleter)> {
            let mut g = self.state.lock().expect("HoldBridge lock");
            std::mem::take(&mut g.held)
        }

        fn take_one(&self) -> Option<(FlowId, HostAwaitCompleter)> {
            let mut g = self.state.lock().expect("HoldBridge lock");
            if g.held.is_empty() {
                None
            } else {
                Some(g.held.remove(0))
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
                Ok(mut g) => {
                    g.held.push((req.flow, done));
                    g.submitted += 1;
                    self.cv.notify_all();
                }
                Err(_) => {
                    // Fail-closed: never leave the flow parked on a poisoned lock.
                    let _ = done.fail("HoldBridge lock poisoned");
                }
            }
        }
    }

    fn waiter_program() -> (Program, u32) {
        let mut program = Program::new("ha-waiter");
        let waiter = program.function("waiter", 0, |f| {
            let args = f.load_i32(0);
            let out = f.host_await(1, args);
            f.return_(out);
        });
        (program, waiter)
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
    fn host_await_thread_bridge_writeback() -> Result<(), Box<dyn std::error::Error>> {
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
            let out = f.host_await(1, args);
            f.return_(out);
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
        match outcome {
            FlowOutcome::Completed(Value::Int(42)) => Ok(()),
            other => Err(format!("expected 42 writeback, got {other:?}").into()),
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
        bridge.wait_for_submissions(1);
        let second = rt.spawn(waiter, &[])?;
        let second_out = second
            .join_timeout(Duration::from_secs(2))
            .ok_or("second HostAwait did not finish")?;

        for (_, done) in bridge.take_all() {
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
        bridge.wait_for_submissions(1);
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

        let (_, done) = bridge.take_one().ok_or("missing first completer")?;
        done.complete(Value::Int(7))?;
        let first_out = first
            .join_timeout(Duration::from_secs(2))
            .ok_or("first writeback timed out")?;
        assert!(
            matches!(first_out, FlowOutcome::Completed(Value::Int(7))),
            "got {first_out:?}"
        );

        let third = rt.spawn(waiter, &[])?;
        bridge.wait_for_submissions(2);
        let (_, done3) = bridge.take_one().ok_or("missing third completer")?;
        done3.complete(Value::Int(9))?;
        let third_out = third
            .join_timeout(Duration::from_secs(2))
            .ok_or("third writeback timed out")?;
        rt.shutdown();

        match third_out {
            FlowOutcome::Completed(Value::Int(9)) => Ok(()),
            other => Err(format!("expected slot reuse writeback, got {other:?}").into()),
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
        bridge.wait_for_submissions(2);
        for (_, done) in bridge.take_all() {
            let _ = done.complete(Value::Int(1));
        }
        let a_out = a.join_timeout(Duration::from_secs(2)).ok_or("a timed out")?;
        let b_out = b.join_timeout(Duration::from_secs(2)).ok_or("b timed out")?;
        rt.shutdown();

        match (a_out, b_out) {
            (FlowOutcome::Completed(Value::Int(1)), FlowOutcome::Completed(Value::Int(1))) => {
                Ok(())
            }
            other => Err(format!("expected both completed, got {other:?}").into()),
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
        bridge.wait_for_submissions(1);
        let (flow_id, done) = bridge.take_one().ok_or("missing completer")?;
        assert_eq!(flow_id, handle.id());
        rt.kill(handle.id())?;
        let outcome = handle
            .join_timeout(Duration::from_secs(2))
            .ok_or("kill join timed out")?;
        assert!(
            matches!(outcome, FlowOutcome::Failed(_)),
            "kill must fail joiner, got {outcome:?}"
        );
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
        bridge.wait_for_submissions(1);
        // Completers still held by the bridge — shutdown must still settle.
        rt.shutdown();
        let outcome = handle
            .join_timeout(Duration::from_secs(2))
            .ok_or("joiner stuck after shutdown")?;
        // Late host complete is Stale after drain.
        for (_, done) in bridge.take_all() {
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
}
