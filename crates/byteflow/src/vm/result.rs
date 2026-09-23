use std::sync::Arc;
use std::time::Duration;

use crate::bytecode::{CapId, Value};

use super::fault::Fault;

/// What happened at the end of a [`crate::Vm::run`] slice.
///
/// This is the entire interface between `byteflow-vm` and the scheduler: the
/// VM never touches threads, mailboxes or timers directly. It runs bytecode
/// until it either finishes, needs an effect only the scheduler can perform,
/// or exhausts its instruction budget — then hands one of these back and
/// stops. That separation is what lets `byteflow-scheduler` move a suspended
/// `Vm` between worker threads freely: it's just a value sitting in a
/// `Flow`.
#[derive(Debug)]
pub enum VmResult {
    /// The outermost frame returned/exited. The Flow should terminate
    /// with this value delivered to `FlowHandle::join`.
    Complete(Value),
    /// Cooperative yield or instruction-budget exhaustion. Re-enqueue as
    /// `Ready` on any worker; resuming picks up at the saved `pc` with no
    /// register writeback needed.
    Yield,
    /// `Sleep` opcode. Register the Flow on the timer wheel; resume with
    /// a plain `run()` call (no writeback) once it elapses.
    Sleep(Duration),
    /// `Spawn` — create a child flow; parent receives a **Cap**
    /// ([`CapRights::ADDRESSING`](crate::CapRights::ADDRESSING)).
    Spawn {
        function: u32,
        args: Vec<Value>,
        dest_reg: u8,
        requested_rights: crate::bytecode::CapRights,
    },
    /// `SelfPid` — write a **self Cap**
    /// ([`CapRights::ADDRESSING`](crate::CapRights::ADDRESSING)) into `dest_reg`.
    SelfPid { dest_reg: u8 },
    /// `Send` — Atomic Hop to a **capability** target (requires SEND).
    Send { target_cap: CapId, message: Value },
    /// `Receive` / `ReceiveTimeout` / `ReceiveMatch` / `ReceiveMatchImm` /
    /// `ReceiveMatchCorr` / `ReceiveMatchCorrImm` / `ReceiveMatchKind`.
    Receive {
        dest_reg: u8,
        timeout: Option<Duration>,
        match_tag: Option<u16>,
        match_request_id: Option<u64>,
        /// When set, wait for a hop whose `payload.wire_tag()` equals this
        /// BFV0 tag (`0..=8`). Independent of [`Self::Receive::match_tag`].
        match_payload_kind: Option<u8>,
    },
    /// `Ask` / `AskTimeout` — RPC hop to a **capability** target (requires ASK).
    Ask {
        dest_reg: u8,
        target_cap: CapId,
        request: Value,
        timeout: Option<Duration>,
    },
    /// `Monitor ra, rb` — watch the flow addressed by Cap `r[b]`.
    Monitor { dest_reg: u8, target_cap: CapId },
    /// `Demonitor ra` — drop monitor whose ref is `r[a]` (Int).
    Demonitor { monitor_reg: u8 },
    /// `Link ra, rb` — bidirectional link with Cap `r[b]`.
    Link { dest_reg: u8, target_cap: CapId },
    /// `Unlink ra` — drop link whose id is `r[a]` (Int).
    Unlink { link_reg: u8 },
    /// `SetTrapExit ra` — enable/disable link exit trapping from truthy `r[a]`
    /// (including converting `Normal` linked exits into hops).
    SetTrapExit { enabled: bool },
    /// `SetRestartPolicy imm` — update this flow's restart policy (`0..=2`).
    SetRestartPolicy { policy: u8 },
    /// `HostAwait` — park for a host-side async completion into `dest_reg`.
    HostAwait {
        dest_reg: u8,
        op: u32,
        args: Value,
    },
    /// `RegisterName ra` — publish `r[a]` (`Str`) as this flow's name.
    RegisterName { name: Arc<str> },
    /// `Whereis ra, rb` — resolve `r[b]` (`Str`) to a SEND Cap or Unit.
    Whereis { dest_reg: u8, name: Arc<str> },
    /// `Delegate ra, rb` — attenuate Cap `r[b]` into `r[a]`.
    Delegate {
        dest_reg: u8,
        src_cap: CapId,
        want_rights: crate::bytecode::CapRights,
        want_native_cap: Option<CapId>,
    },
    /// A fault occurred; the Flow fails. See [`Fault`].
    Trap(Fault),
}
