//! M:N **flows**, mailboxes, timer, supervisor and [`Runtime`].
//!
//! A **flow** is Byteflow's unit of concurrent work (not an OS thread). Flows
//! exchange **Atomic Hops** ([`crate::Value::Message`]) — never bare scalars
//! on bytecode `Send` / `Ask`.
//!
//! # Authority boundary
//!
//! The VM only validates types. This module owns delivery:
//!
//! 1. Resolve [`crate::Value::Cap`] → [`FlowId`] + rights ([`CapRights`])
//! 2. Stamp `Message.sender` and attach `reply_cap` (SEND-only, reused per pair)
//! 3. Push into the target [`Mailbox`] (bounded; anti lost-wakeup under one mutex)
//!
//! [`crate::Value::Pid`] is identity inside hops, not an ambient address.
//! Host [`Runtime::send`] takes [`FlowId`] directly (trusted) and is stamped
//! like a bytecode hop (`sender = 0`, `reply_cap = NONE`).
//!
//! See [`crate::docs::security`] and [`crate::docs::atomic_hop`].

mod capability;
mod directory;
mod error;
mod finalize;
mod flow_limit;
mod handle;
mod link;
mod link_admin;
mod monitor;
mod registry;
#[cfg(feature = "jit")]
mod jit;
mod mailbox;
mod metrics;
pub(crate) mod oneshot;
mod process;
mod quota;
mod runqueue;
mod runtime;
mod spawn;
mod delegate;
mod supervisor;
mod sync_lock;
mod timer;
mod worker;

pub use capability::{CapError, CapRights, Capability};
pub use quota::{FlowQuota, QuotaConfig, QuotaError};
pub use delegate::{exec_delegate, DelegateError};
pub use link_admin::{check_admin, check_link, check_monitor, AdminError, LinkError};
pub use error::{fault_count, report_fault, LifecycleError, RuntimeError, SpawnError};
pub use link::LinkId;
pub use monitor::{DownEvent, FlowExitReason, MonitorRef};
pub use registry::RegistryName;
pub use handle::{FlowHandle, JoinError};
pub use mailbox::{
    Delivery, Mailbox, MailboxBytes, MailboxCapacity, MailboxConfig, MailboxFull,
    MailboxFullReason, MailboxStats, OverflowPolicy, WaitEpoch,
};
pub use metrics::{RuntimeMetrics, RuntimeMetricsSnapshot};
pub use process::{
    next_flow_id, Flow, FlowId, FlowMetrics, FlowOutcome, RestartPolicy,
};
pub use runtime::{
    flow_id_from_u64, Runtime, RuntimeConfig, RuntimeSpawner, SendError, DEFAULT_QUANTUM,
};
#[cfg(feature = "jit")]
pub use runtime::JitConfig;
pub use supervisor::{ChildSpec, RestartStrategy, Supervisor, SupervisorConfig};
