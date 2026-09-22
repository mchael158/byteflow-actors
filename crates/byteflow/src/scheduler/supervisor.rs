use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::bytecode::{RestartPolicy, Value};

use super::error::{report_fault, SpawnError};
use super::handle::FlowHandle;
use super::monitor::FlowExitReason;
use super::process::{FlowId, FlowOutcome};
use super::runtime::RuntimeSpawner;
use super::sync_lock;

/// How many restarts OTP-style supervisors allow inside a sliding window
/// before giving up (design notes §15). Three-in-five-seconds is the
/// classic default: enough to absorb a flaky child, tight enough that a
/// crash loop cannot spin the runtime forever.
const DEFAULT_MAX_RESTARTS: u32 = 3;
const DEFAULT_MAX_PERIOD: Duration = Duration::from_secs(5);

/// A child the supervisor should start (and possibly restart).
///
/// `function` is an index into the runtime's chunk — the same number
/// [`super::runtime::Runtime::spawn`] takes. Args are cloned on every
/// restart so a child always comes back with the original call.
///
/// Non-empty [`Self::name`] is registered as `register_name` → an addressing
/// Cap ([`CapRights::ADDRESSING`]) for that incarnation (swept on exit,
/// re-bound on restart).
#[derive(Clone, Debug)]
pub struct ChildSpec {
    pub name: String,
    pub function: u32,
    pub args: Vec<Value>,
    pub restart: RestartPolicy,
}

impl ChildSpec {
    pub fn new(name: impl Into<String>, function: u32) -> Self {
        ChildSpec {
            name: name.into(),
            function,
            args: Vec::new(),
            restart: RestartPolicy::OnFailure,
        }
    }

    pub fn args(mut self, args: Vec<Value>) -> Self {
        self.args = args;
        self
    }

    pub fn restart(mut self, restart: RestartPolicy) -> Self {
        self.restart = restart;
        self
    }
}

/// Which siblings die (and later come back) when one child exits.
///
/// Sibling abort is **cooperative**: parked children are taken out of the
/// mailbox immediately; a child mid-quantum dies at the next budget edge
/// (same contract as [`super::runtime::Runtime::kill`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartStrategy {
    /// Only the child that exited is considered for restart.
    OneForOne,
    /// Every sibling is shut down, then the whole set is started again
    /// in original start order.
    OneForAll,
    /// Children started *after* the failed one are shut down, then that
    /// suffix (failed + later) is started again in start order.
    RestForOne,
}

/// Tunables for [`Supervisor::with_config`].
#[derive(Clone, Debug)]
pub struct SupervisorConfig {
    /// Restarts allowed inside [`Self::max_period`]. The initial start does
    /// not count; only respawns do. Hitting this cap sets
    /// [`Supervisor::intensity_exceeded`] and further restarts are refused.
    pub max_restarts: u32,
    pub max_period: Duration,
    pub strategy: RestartStrategy,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        SupervisorConfig {
            max_restarts: DEFAULT_MAX_RESTARTS,
            max_period: DEFAULT_MAX_PERIOD,
            strategy: RestartStrategy::OneForOne,
        }
    }
}

struct ChildExit {
    id: FlowId,
    outcome: FlowOutcome,
    /// Policy from the dying flow (`SetRestartPolicy` / spawn default).
    restart: RestartPolicy,
}

struct LiveChild {
    spec: ChildSpec,
    /// Set when this incarnation is being torn down by a cascade so its
    /// exit does not start another strategy wave.
    expected_shutdown: bool,
}

struct ChildTable {
    by_id: HashMap<FlowId, LiveChild>,
    order: Vec<FlowId>,
}

impl ChildTable {
    fn new() -> Self {
        Self {
            by_id: HashMap::new(),
            order: Vec::new(),
        }
    }

    fn insert(&mut self, id: FlowId, child: LiveChild) {
        self.order.push(id);
        self.by_id.insert(id, child);
    }

    fn remove(&mut self, id: FlowId) -> Option<LiveChild> {
        self.order.retain(|x| *x != id);
        self.by_id.remove(&id)
    }

    fn len(&self) -> usize {
        self.by_id.len()
    }
}

/// In-flight one-for-all / rest-for-one: wait for sibling kills, then respawn.
struct Cascade {
    waiting: HashSet<FlowId>,
    specs: Vec<ChildSpec>,
}

struct Inner {
    spawner: RuntimeSpawner,
    config: SupervisorConfig,
    events: Mutex<VecDeque<ChildExit>>,
    cvar: Condvar,
    children: Mutex<ChildTable>,
    cascade: Mutex<Option<Cascade>>,
    restart_times: Mutex<VecDeque<Instant>>,
    intensity_exceeded: AtomicBool,
    shutdown: AtomicBool,
}

/// Cheap, `Clone` handle the worker uses to hand a terminal outcome back
/// without taking a lock on the supervisor's child table (the drive loop
/// is the only writer of that table).
#[derive(Clone)]
pub(crate) struct SupervisorLink {
    inner: Arc<Inner>,
}

impl SupervisorLink {
    pub(crate) fn notify(&self, id: FlowId, outcome: FlowOutcome, restart: RestartPolicy) {
        match sync_lock::lock(&self.inner.events, "SupervisorLink::notify") {
            Ok(mut events) => {
                events.push_back(ChildExit {
                    id,
                    outcome,
                    restart,
                });
                self.inner.cvar.notify_one();
            }
            Err(e) => report_fault(e),
        }
    }
}

/// Host-side child restarter (design notes §15-16).
///
/// A `Supervisor` is **not** a bytecode Flow. It is a dedicated OS
/// thread plus a table of [`ChildSpec`]s. When a supervised flow
/// becomes [`FlowOutcome::Failed`] (or completes, under
/// [`RestartPolicy::Always`]), the worker delivers the
/// [`FlowOutcome`] here instead of letting the fault take anything
/// else down. The supervisor then consults the child's
/// [`RestartPolicy`] and, if intensity allows, respawns it under a
/// fresh [`FlowId`] — Pids are never reused (see
/// [`super::process::FlowId`]).
///
/// Constructed from a [`RuntimeSpawner`] so it does not have to own the
/// runtime's worker `JoinHandle`s.
pub struct Supervisor {
    inner: Arc<Inner>,
    thread: Option<JoinHandle<()>>,
}

impl Supervisor {
    pub fn new(spawner: RuntimeSpawner) -> Result<Self, SpawnError> {
        Self::with_config(spawner, SupervisorConfig::default())
    }

    /// Start the dedicated supervisor OS thread. Thread-spawn failure is
    /// [`SpawnError::ThreadSpawnFailed`] — same category-A surface as
    /// [`super::runtime::Runtime::new`], not a panic.
    pub fn with_config(
        spawner: RuntimeSpawner,
        config: SupervisorConfig,
    ) -> Result<Self, SpawnError> {
        let inner = Arc::new(Inner {
            spawner,
            config,
            events: Mutex::new(VecDeque::new()),
            cvar: Condvar::new(),
            children: Mutex::new(ChildTable::new()),
            cascade: Mutex::new(None),
            restart_times: Mutex::new(VecDeque::new()),
            intensity_exceeded: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
        });
        let drive_inner = inner.clone();
        let thread = std::thread::Builder::new()
            .name("byteflow-supervisor".into())
            .spawn(move || drive(drive_inner))
            .map_err(|e| SpawnError::ThreadSpawnFailed(e.to_string()))?;
        Ok(Supervisor {
            inner,
            thread: Some(thread),
        })
    }

    /// Spawn `spec` and start supervising it. The returned handle is for
    /// this incarnation only — a restart allocates a new Pid and a new
    /// completion channel.
    pub fn start_child(&self, spec: ChildSpec) -> Result<FlowHandle, SpawnError> {
        spawn_child(&self.inner, spec)
    }

    pub fn live_children(&self) -> usize {
        match sync_lock::lock(&self.inner.children, "Supervisor::live_children") {
            Ok(g) => g.len(),
            Err(e) => {
                report_fault(e);
                0
            }
        }
    }

    /// `true` once more than [`SupervisorConfig::max_restarts`] respawns
    /// landed inside the intensity window. Remaining children keep
    /// running; we just stop bringing them back (no safe abort of a
    /// mid-quantum flow).
    pub fn intensity_exceeded(&self) -> bool {
        self.inner.intensity_exceeded.load(Ordering::Acquire)
    }

    /// Stop the drive thread. Does not terminate live children — they
    /// belong to the runtime, not to us.
    pub fn shutdown(mut self) {
        self.inner.shutdown.store(true, Ordering::Release);
        self.inner.cvar.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn spawn_child(inner: &Arc<Inner>, spec: ChildSpec) -> Result<FlowHandle, SpawnError> {
    let link = SupervisorLink {
        inner: inner.clone(),
    };
    // Hold the table across spawn so a child that faults in its first
    // quantum cannot notify us before its row exists (the drive loop
    // takes this same lock in `handle_exit`, so the event waits).
    let mut children = match sync_lock::lock(&inner.children, "spawn_child") {
        Ok(c) => c,
        Err(e) => {
            report_fault(e);
            return Err(SpawnError::VmInit("supervisor child table poisoned".into()));
        }
    };
    let handle = inner
        .spawner
        .spawn_linked(spec.function, &spec.args, spec.restart, link)?;
    if !spec.name.is_empty() {
        if let Err(e) = register_child_name(inner, handle.id(), &spec.name) {
            inner
                .spawner
                .request_kill(handle.id(), FlowExitReason::Supervisor);
            return Err(e);
        }
    }
    children.insert(
        handle.id(),
        LiveChild {
            spec,
            expected_shutdown: false,
        },
    );
    Ok(handle)
}

fn register_child_name(inner: &Inner, id: FlowId, name: &str) -> Result<(), SpawnError> {
    let cap = inner
        .spawner
        .shared
        .caps
        .mint(id, id, super::capability::CapRights::ADDRESSING)
        .map_err(|e| {
            report_fault(e);
            SpawnError::VmInit("cap mint failed (poisoned lock)".into())
        })?;
    match inner
        .spawner
        .shared
        .registry
        .register(super::registry::RegistryName::from(name), cap, id)
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(super::error::LifecycleError::AlreadyRegistered)) => Err(SpawnError::NameTaken {
            name: name.to_string(),
        }),
        Ok(Err(e)) => Err(SpawnError::VmInit(e.to_string())),
        Err(e) => {
            report_fault(e);
            Err(SpawnError::VmInit(
                "registry register failed (poisoned lock)".into(),
            ))
        }
    }
}

fn should_restart(policy: RestartPolicy, outcome: &FlowOutcome) -> bool {
    match policy {
        RestartPolicy::Always => true,
        RestartPolicy::OnFailure => matches!(outcome, FlowOutcome::Failed(_)),
        RestartPolicy::Never => false,
    }
}

fn intensity_hit(inner: &Inner) -> bool {
    let now = Instant::now();
    let mut times = match sync_lock::lock(&inner.restart_times, "intensity_hit") {
        Ok(t) => t,
        Err(e) => {
            report_fault(e);
            return true;
        }
    };
    times.push_back(now);
    let window_start = match now.checked_sub(inner.config.max_period) {
        Some(t) => t,
        None => now,
    };
    loop {
        match times.front() {
            Some(t) if *t < window_start => {
                times.pop_front();
            }
            _ => break,
        }
    }
    if times.len() as u32 > inner.config.max_restarts {
        inner.intensity_exceeded.store(true, Ordering::Release);
        true
    } else {
        false
    }
}

fn drive(inner: Arc<Inner>) {
    loop {
        if inner.shutdown.load(Ordering::Acquire) {
            return;
        }
        let exit = {
            let mut events = match sync_lock::lock(&inner.events, "supervisor::drive") {
                Ok(e) => e,
                Err(e) => {
                    report_fault(e);
                    return;
                }
            };
            loop {
                if inner.shutdown.load(Ordering::Acquire) {
                    return;
                }
                if let Some(exit) = events.pop_front() {
                    break exit;
                }
                match sync_lock::wait_timeout(
                    &inner.cvar,
                    events,
                    Duration::from_millis(100),
                    "supervisor::wait",
                ) {
                    Ok((guard, _)) => events = guard,
                    Err(e) => {
                        report_fault(e);
                        return;
                    }
                }
            }
        };
        handle_exit(&inner, exit);
    }
}

fn handle_exit(inner: &Arc<Inner>, exit: ChildExit) {
    let (spec, expected, failed_idx, later) = {
        let mut children = match sync_lock::lock(&inner.children, "handle_exit") {
            Ok(c) => c,
            Err(e) => {
                report_fault(e);
                return;
            }
        };
        let failed_idx = children.order.iter().position(|id| *id == exit.id);
        let live = match children.remove(exit.id) {
            Some(live) => live,
            None => return,
        };
        let later = match (inner.config.strategy, failed_idx) {
            (RestartStrategy::OneForAll, _) => children.order.clone(),
            (RestartStrategy::RestForOne, Some(i)) => children.order[i..].to_vec(),
            _ => Vec::new(),
        };
        (live.spec, live.expected_shutdown, failed_idx, later)
    };

    if expected {
        on_cascade_progress(inner, exit.id);
        return;
    }

    if !should_restart(exit.restart, &exit.outcome) {
        return;
    }
    if inner.intensity_exceeded.load(Ordering::Acquire) || intensity_hit(inner) {
        return;
    }

    match inner.config.strategy {
        RestartStrategy::OneForOne => {
            let _ = spawn_child(inner, spec);
        }
        RestartStrategy::OneForAll | RestartStrategy::RestForOne => {
            start_cascade(inner, spec, failed_idx, later);
        }
    }
}

fn start_cascade(
    inner: &Arc<Inner>,
    failed: ChildSpec,
    failed_idx: Option<usize>,
    later: Vec<FlowId>,
) {
    let specs = {
        let mut children = match sync_lock::lock(&inner.children, "start_cascade") {
            Ok(c) => c,
            Err(e) => {
                report_fault(e);
                return;
            }
        };
        let later_specs: Vec<ChildSpec> = later
            .iter()
            .filter_map(|id| {
                children.by_id.get_mut(id).map(|c| {
                    c.expected_shutdown = true;
                    c.spec.clone()
                })
            })
            .collect();
        match inner.config.strategy {
            RestartStrategy::OneForAll => {
                let mut specs = later_specs;
                specs.insert(failed_idx.unwrap_or(specs.len()), failed);
                specs
            }
            RestartStrategy::RestForOne => {
                let mut specs = vec![failed];
                specs.extend(later_specs);
                specs
            }
            RestartStrategy::OneForOne => vec![failed],
        }
    };

    let waiting: HashSet<FlowId> = later.iter().copied().collect();
    match sync_lock::lock(&inner.cascade, "start_cascade.cascade") {
        Ok(mut slot) => {
            *slot = Some(Cascade {
                waiting: waiting.clone(),
                specs,
            })
        }
        Err(e) => {
            report_fault(e);
            return;
        }
    }

    for id in &later {
        inner.spawner.request_kill(*id, FlowExitReason::Supervisor);
    }

    let remaining = match sync_lock::lock(&inner.cascade, "start_cascade.prune") {
        Ok(mut slot) => {
            if let Some(c) = slot.as_mut() {
                c.waiting
                    .retain(|id| matches!(inner.spawner.shared.directory.lookup(*id), Ok(Some(_))));
            }
            slot.as_ref().map(|c| c.waiting.is_empty()).unwrap_or(true)
        }
        Err(e) => {
            report_fault(e);
            return;
        }
    };
    if remaining || waiting.is_empty() {
        finish_cascade(inner);
    }
}

fn on_cascade_progress(inner: &Arc<Inner>, id: FlowId) {
    let ready = match sync_lock::lock(&inner.cascade, "on_cascade_progress") {
        Ok(mut slot) => {
            let Some(c) = slot.as_mut() else {
                return;
            };
            c.waiting.remove(&id);
            c.waiting.is_empty()
        }
        Err(e) => {
            report_fault(e);
            return;
        }
    };
    if ready {
        finish_cascade(inner);
    }
}

fn finish_cascade(inner: &Arc<Inner>) {
    let specs = match sync_lock::lock(&inner.cascade, "finish_cascade") {
        Ok(mut slot) => match slot.take() {
            Some(c) => c.specs,
            None => return,
        },
        Err(e) => {
            report_fault(e);
            return;
        }
    };
    for spec in specs {
        if inner.intensity_exceeded.load(Ordering::Acquire) {
            return;
        }
        let _ = spawn_child(inner, spec);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    use crate::bytecode::{builder::ChunkBuilder, Chunk, Value};
    use crate::scheduler::runtime::{Runtime, RuntimeConfig};

    fn trap_chunk() -> Chunk {
        let mut b = ChunkBuilder::new("trap");
        b.begin_function("boom", 0, 1);
        b.emit_trap(1);
        b.finish()
    }

    /// Sets `RestartPolicy::Never` then traps — must not be restarted.
    fn never_then_trap_chunk() -> Chunk {
        let mut b = ChunkBuilder::new("never-trap");
        b.begin_function("boom", 0, 1);
        b.emit_set_restart_policy(RestartPolicy::Never.as_u8());
        b.emit_trap(1);
        b.finish()
    }

    fn ok_chunk() -> Chunk {
        let mut b = ChunkBuilder::new("ok");
        b.begin_function("main", 0, 1);
        b.emit_load_imm(0, 7);
        b.emit_return(0);
        b.finish()
    }

    fn tiny_runtime(chunk: Chunk) -> Result<Runtime, crate::scheduler::SpawnError> {
        Runtime::with_config(
            chunk,
            RuntimeConfig {
                workers: 1,
                quantum: 1_000,
                mailbox: super::super::mailbox::MailboxConfig::DEFAULT,
                ..Default::default()
            },
        )
    }

    fn wait_until(mut pred: impl FnMut() -> bool) {
        let start = Instant::now();
        while !pred() {
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "supervisor test timed out"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn on_failure_does_not_restart_a_clean_exit() -> Result<(), Box<dyn std::error::Error>> {
        let rt = tiny_runtime(ok_chunk())?;
        let sup = Supervisor::new(rt.spawner())?;
        let outcome = sup
            .start_child(ChildSpec::new("main", 0).restart(RestartPolicy::OnFailure))?
            .join();
        wait_until(|| sup.live_children() == 0);
        let spawned = rt.metrics().processes_spawned;
        sup.shutdown();
        rt.shutdown();
        assert!(matches!(outcome, FlowOutcome::Completed(_)));
        assert_eq!(spawned, 1);
        Ok(())
    }

    #[test]
    fn on_failure_restarts_until_intensity() -> Result<(), Box<dyn std::error::Error>> {
        let rt = tiny_runtime(trap_chunk())?;
        let sup = Supervisor::with_config(
            rt.spawner(),
            SupervisorConfig {
                max_restarts: 2,
                max_period: Duration::from_secs(5),
                strategy: RestartStrategy::OneForOne,
            },
        )?;
        let _first =
            sup.start_child(ChildSpec::new("boom", 0).restart(RestartPolicy::OnFailure))?;
        wait_until(|| sup.intensity_exceeded() && rt.metrics().processes_failed >= 3);
        let spawned = rt.metrics().processes_spawned;
        let failed = rt.metrics().processes_failed;
        sup.shutdown();
        rt.shutdown();
        // initial start + 2 restarts, then intensity refuses the 3rd restart
        assert_eq!(spawned, 3);
        assert_eq!(failed, 3);
        Ok(())
    }

    #[test]
    fn set_restart_policy_never_skips_restart() -> Result<(), Box<dyn std::error::Error>> {
        let rt = tiny_runtime(never_then_trap_chunk())?;
        let sup = Supervisor::with_config(
            rt.spawner(),
            SupervisorConfig {
                max_restarts: 5,
                max_period: Duration::from_secs(5),
                strategy: RestartStrategy::OneForOne,
            },
        )?;
        // ChildSpec says OnFailure, but bytecode SetRestartPolicy(Never)
        // must win at exit.
        let _ = sup.start_child(ChildSpec::new("boom", 0).restart(RestartPolicy::OnFailure))?;
        wait_until(|| rt.metrics().processes_failed >= 1 && sup.live_children() == 0);
        let spawned = rt.metrics().processes_spawned;
        let failed = rt.metrics().processes_failed;
        std::thread::sleep(Duration::from_millis(50));
        let spawned_after = rt.metrics().processes_spawned;
        sup.shutdown();
        rt.shutdown();
        assert_eq!(failed, 1);
        assert_eq!(spawned, 1);
        assert_eq!(spawned_after, 1, "Never must not restart");
        Ok(())
    }

    #[test]
    fn always_restarts_a_clean_exit_until_intensity() -> Result<(), Box<dyn std::error::Error>> {
        let rt = tiny_runtime(ok_chunk())?;
        let sup = Supervisor::with_config(
            rt.spawner(),
            SupervisorConfig {
                max_restarts: 2,
                max_period: Duration::from_secs(5),
                strategy: RestartStrategy::OneForOne,
            },
        )?;
        let _ = sup.start_child(ChildSpec::new("main", 0).restart(RestartPolicy::Always))?;
        wait_until(|| sup.intensity_exceeded() && rt.metrics().processes_completed >= 3);
        let spawned = rt.metrics().processes_spawned;
        sup.shutdown();
        rt.shutdown();
        assert_eq!(spawned, 3);
        Ok(())
    }

    fn wait_and_trap_chunk() -> Chunk {
        let mut b = ChunkBuilder::new("sup-mix");
        b.begin_function("wait", 0, 1);
        b.emit_receive(0);
        b.emit_return(0);
        b.begin_function("boom", 0, 1);
        b.emit_trap(1);
        b.begin_function("delayed", 0, 1);
        b.emit_load_imm(0, 80);
        b.emit_sleep(0);
        b.emit_trap(1);
        b.finish()
    }

    #[test]
    fn one_for_one_does_not_kill_sibling() -> Result<(), Box<dyn std::error::Error>> {
        let rt = tiny_runtime(wait_and_trap_chunk())?;
        let sup = Supervisor::new(rt.spawner())?;
        let parked = sup.start_child(ChildSpec::new("wait", 0))?;
        let _boom = sup.start_child(ChildSpec::new("boom", 1).restart(RestartPolicy::OnFailure))?;
        wait_until(|| rt.metrics().processes_failed >= 1);
        assert!(
            parked.try_join().is_none(),
            "one-for-one must leave the parked sibling"
        );
        sup.shutdown();
        rt.shutdown();
        Ok(())
    }

    #[test]
    fn one_for_all_kills_parked_sibling() -> Result<(), Box<dyn std::error::Error>> {
        let rt = tiny_runtime(wait_and_trap_chunk())?;
        let sup = Supervisor::with_config(
            rt.spawner(),
            SupervisorConfig {
                max_restarts: 8,
                max_period: Duration::from_secs(5),
                strategy: RestartStrategy::OneForAll,
            },
        )?;
        let parked = sup.start_child(ChildSpec::new("wait", 0))?;
        let _boom = sup.start_child(ChildSpec::new("boom", 1).restart(RestartPolicy::OnFailure))?;
        wait_until(|| parked.try_join().is_some());
        let outcome = parked.join();
        sup.shutdown();
        rt.shutdown();
        assert!(
            matches!(outcome, FlowOutcome::Failed(_)),
            "one-for-all must kill the parked sibling, got {outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn rest_for_one_kills_only_later_children() -> Result<(), Box<dyn std::error::Error>> {
        let rt = tiny_runtime(wait_and_trap_chunk())?;
        let sup = Supervisor::with_config(
            rt.spawner(),
            SupervisorConfig {
                max_restarts: 8,
                max_period: Duration::from_secs(5),
                strategy: RestartStrategy::RestForOne,
            },
        )?;
        let earlier = sup.start_child(ChildSpec::new("keep", 0))?;
        let _boom =
            sup.start_child(ChildSpec::new("delayed", 2).restart(RestartPolicy::OnFailure))?;
        let later = sup.start_child(ChildSpec::new("tail", 0))?;
        wait_until(|| later.try_join().is_some());
        assert!(
            earlier.try_join().is_none(),
            "rest-for-one must not kill children started before the failure"
        );
        let later_outcome = later.join();
        sup.shutdown();
        rt.shutdown();
        assert!(
            matches!(later_outcome, FlowOutcome::Failed(_)),
            "rest-for-one must kill children started after the failure, got {later_outcome:?}"
        );
        Ok(())
    }

    #[test]
    fn child_name_registers_and_rejects_duplicate() -> Result<(), Box<dyn std::error::Error>> {
        let rt = tiny_runtime(wait_and_trap_chunk())?;
        let sup = Supervisor::new(rt.spawner())?;
        let _parked = sup.start_child(ChildSpec::new("svc", 0))?;
        wait_until(|| matches!(rt.whereis("svc"), Ok(Some(_))));
        assert!(rt.whereis("svc")?.is_some());
        let dup = sup.start_child(ChildSpec::new("svc", 1));
        sup.shutdown();
        rt.shutdown();
        match dup {
            Err(SpawnError::NameTaken { name }) if name == "svc" => {}
            Ok(_) => return Err("expected NameTaken, got Ok(handle)".into()),
            Err(e) => return Err(format!("expected NameTaken, got Err({e})").into()),
        }
        Ok(())
    }

    #[test]
    fn policy_table() {
        let ok = FlowOutcome::Completed(Value::Unit);
        let fail = FlowOutcome::Failed("boom".into());
        assert!(should_restart(RestartPolicy::Always, &ok));
        assert!(should_restart(RestartPolicy::Always, &fail));
        assert!(!should_restart(RestartPolicy::OnFailure, &ok));
        assert!(should_restart(RestartPolicy::OnFailure, &fail));
        assert!(!should_restart(RestartPolicy::Never, &ok));
        assert!(!should_restart(RestartPolicy::Never, &fail));
    }
}
