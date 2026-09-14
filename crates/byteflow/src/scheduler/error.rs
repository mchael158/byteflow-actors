//! Infrastructure failures of the scheduler (category C in the error model).
//!
//! # Failure taxonomy (Byteflow)
//!
//! | Kind | Example | Surface |
//! |------|---------|---------|
//! | A — user / API | `spawn` bad function index | `Result<_, SpawnError>` |
//! | B — flow | `HwError`, native fault | `FlowOutcome::Failed` → Supervisor |
//! | C — infrastructure | mutex poison, dead worker | [`RuntimeError`] + fail-closed |
//! | D — invariant | empty frame stack while running | types / `debug_assert` — not `unwrap` |
//!
//! **Panic is a runtime bug, not an error-handling mechanism.** Device faults
//! kill actors; scheduler faults are explicit and fail-closed.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// Count of infrastructure faults observed since flow start (Relaxed).
static FAULTS: AtomicU64 = AtomicU64::new(0);

/// Scheduler / host infrastructure error — not a bytecode flow fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    /// A `Mutex` was poisoned: another thread panicked while holding it.
    /// Shared tables may be inconsistent; callers must not continue using them.
    PoisonedLock(&'static str),
    /// A flow was destroyed before it produced a [`crate::FlowOutcome`], so
    /// the completion value its handle was waiting for will never arrive.
    ///
    /// Reachable when the runtime shuts down while a flow is suspended (in
    /// the timer, in a worker deque, or parked in its mailbox): those flows
    /// are dropped without reaching `finish`. The alternative to reporting
    /// this is worse — a `join()` that blocks the embedder's thread forever
    /// with no way to tell "still running" from "never will".
    Abandoned(&'static str),
    /// The outcome was already collected by an earlier non-consuming
    /// `try_join` / `join_timeout`, so there is nothing left to hand out.
    ///
    /// Caller misuse rather than an infrastructure failure (kind A in the
    /// table above), reported here because it shares `join`'s return type.
    /// It exists so a second poll cannot be answered with `Abandoned`, which
    /// would blame the runtime for destroying a flow that in fact completed
    /// normally and was already observed.
    AlreadyCollected(&'static str),
    /// OS CSPRNG failed while minting a capability.
    EntropyFailed,
    /// CSPRNG produced colliding ids beyond the retry budget (should not happen).
    CapIdCollision,
    /// `finalize_flow` was asked to tear down the reserved host FlowId.
    CannotFinalizeHostFlow,
    /// `next_flow_id` wrapped onto [`crate::FlowId::HOST`] (`0`).
    FlowIdExhausted,
    /// Directory register saw a live id twice (wrap or double-spawn).
    DuplicateFlowId,
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuntimeError::PoisonedLock(where_) => {
                write!(f, "runtime mutex poisoned at {where_}")
            }
            RuntimeError::Abandoned(where_) => {
                write!(
                    f,
                    "{where_}: flow was destroyed before producing an outcome"
                )
            }
            RuntimeError::AlreadyCollected(where_) => {
                write!(f, "{where_}: outcome was already collected")
            }
            RuntimeError::EntropyFailed => {
                write!(f, "capability CSPRNG unavailable")
            }
            RuntimeError::CapIdCollision => {
                write!(f, "could not allocate a unique capability id")
            }
            RuntimeError::CannotFinalizeHostFlow => {
                write!(f, "cannot finalize reserved host flow")
            }
            RuntimeError::FlowIdExhausted => {
                write!(f, "flow id space exhausted (counter wrapped)")
            }
            RuntimeError::DuplicateFlowId => {
                write!(f, "directory already has this flow id")
            }
        }
    }
}

impl std::error::Error for RuntimeError {}

/// User-facing lifecycle / naming errors (category A).
///
/// Distinct from [`RuntimeError`] (infrastructure / poison). Host
/// `monitor` / `link` / `register_name` return this so a dead FlowId or
/// a duplicate name is not reported as a poisoned mutex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleError {
    NoSuchFlow(super::process::FlowId),
    InvalidCapability,
    InvalidMonitor,
    InvalidLink,
    NotOwner,
    AlreadyRegistered,
    AlreadyLinked,
    EmptyName,
    SelfRelation,
    Unavailable,
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchFlow(id) => write!(f, "no live flow {id}"),
            Self::InvalidCapability => write!(f, "unknown or revoked capability"),
            Self::InvalidMonitor => write!(f, "unknown monitor"),
            Self::InvalidLink => write!(f, "unknown link"),
            Self::NotOwner => write!(f, "caller does not own this relation"),
            Self::AlreadyRegistered => write!(f, "registry name already taken"),
            Self::AlreadyLinked => write!(f, "flows are already linked"),
            Self::EmptyName => write!(f, "registry name must be non-empty"),
            Self::SelfRelation => write!(f, "cannot link or monitor a flow to itself"),
            Self::Unavailable => write!(f, "runtime table unavailable (poisoned lock)"),
        }
    }
}

impl std::error::Error for LifecycleError {}

/// Record an infrastructure fault (stderr + counter). Does not panic.
#[cold]
pub fn report_fault(err: RuntimeError) {
    FAULTS.fetch_add(1, Ordering::Relaxed);
    eprintln!("byteflow: {err} — fail-closed");
}

/// How many [`report_fault`] calls have been made (tests / diagnostics).
pub fn fault_count() -> u64 {
    FAULTS.load(Ordering::Relaxed)
}

/// User-facing spawn / load errors (category A in the taxonomy above).
///
/// These used to be `.expect(...)` panics on `Runtime::new` / `spawn` /
/// OS thread creation. Panic is reserved for *bugs*; a bad function index
/// or a refused `thread::spawn` is an embedder-visible failure and must
/// surface as `Result` so the host can recover without taking workers down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnError {
    /// `function` index is outside the runtime chunk's function table.
    BadFunction { index: u32, table_size: u32 },
    /// Chunk failed verification before the runtime could start.
    VerifyFailed(crate::bytecode::VerifyError),
    /// Spawn arguments contained an unknown or unusable capability.
    InvalidCapability,
    /// Scheduler table poisoned (fail closed).
    Unavailable,
    /// Flow-id counter wrapped onto the reserved host id.
    FlowIdExhausted,
    /// Live flow count would exceed [`crate::RuntimeConfig::max_flows`].
    FlowLimit { current: usize, max: u32 },
    /// Bytecode spawn failed the quota / SPAWN-right / attenuation check.
    SpawnDenied(String),
    SpawnRateExceeded,
    ParentCapRevoked,
    MissingSpawnRight,
    /// [`crate::ChildSpec::name`] is already in the runtime registry.
    NameTaken { name: String },
    /// OS refused to create a worker / timer / supervisor thread.
    ThreadSpawnFailed(String),
    /// `Vm::new` failed for a reason other than a bad function index
    /// (mapped from [`crate::Fault`] via `From`).
    VmInit(String),
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpawnError::BadFunction { index, table_size } => {
                write!(
                    f,
                    "spawn: function index {index} out of range (table size {table_size})"
                )
            }
            SpawnError::VerifyFailed(err) => write!(f, "chunk verification failed: {err}"),
            SpawnError::InvalidCapability => {
                write!(f, "spawn: argument capability is unknown or not held")
            }
            SpawnError::Unavailable => write!(f, "spawn: runtime table unavailable"),
            SpawnError::FlowIdExhausted => {
                write!(f, "spawn: flow id space exhausted (counter wrapped)")
            }
            SpawnError::FlowLimit { current, max } => {
                write!(f, "spawn: live flow limit reached ({current}/{max})")
            }
            SpawnError::SpawnDenied(msg) => write!(f, "spawn: {msg}"),
            SpawnError::SpawnRateExceeded => write!(f, "spawn: spawn rate exceeded"),
            SpawnError::ParentCapRevoked => write!(f, "spawn: parent capability revoked"),
            SpawnError::MissingSpawnRight => write!(f, "spawn: parent lacks SPAWN right"),
            SpawnError::NameTaken { name } => {
                write!(f, "spawn: registry name {name:?} already taken")
            }
            SpawnError::ThreadSpawnFailed(msg) => {
                write!(f, "failed to spawn runtime thread: {msg}")
            }
            SpawnError::VmInit(msg) => write!(f, "vm init failed: {msg}"),
        }
    }
}

impl std::error::Error for SpawnError {}

impl From<crate::vm::Fault> for SpawnError {
    fn from(fault: crate::vm::Fault) -> Self {
        match fault {
            crate::vm::Fault::BadFunction { index, table_size } => {
                SpawnError::BadFunction { index, table_size }
            }
            other => SpawnError::VmInit(other.to_string()),
        }
    }
}
