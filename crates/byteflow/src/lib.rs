//! Byteflow — embeddable **flow** runtime (package **`byteflow-actors`**).
//!
//! Not a language, not Tokio, not a JVM. You assemble register bytecode in
//! host Rust ([`Program`] / [`Fn`]), spawn many lightweight **flows** on an M:N
//! scheduler, and they talk through mailboxes with a strict hop protocol.
//!
//! Dependents write `use byteflow::...` (crate name) while crates.io lists
//! the package as [`byteflow-actors`](https://crates.io/crates/byteflow-actors).
//!
//! # What you get
//!
//! | Piece | Role |
//! |-------|------|
//! | [`Program`] / [`Fn`] / [`Opcode`] | Assemble `.bf` programs in Rust (no source language) |
//! | [`verify`] | Static gate for untrusted chunks — mandatory, not advisory |
//! | [`Vm`] / [`VmResult`] | Per-flow register interpreter; effects hand off to the scheduler |
//! | [`Runtime`] | Worker pool + timer; spawn / join / host [`Runtime::send`] |
//! | [`FlowHandle`] | Collect an outcome: blocking [`FlowHandle::join`] or a bounded form |
//! | [`Value::Message`] | **Atomic Hop** envelope — the only value allowed on `Send` / `Ask` |
//! | [`Value::Cap`] | **FlowCap** address for bytecode delivery (`Send` / `Ask` targets) |
//! | [`Cap`] / [`CapRights`] / [`NativeMask`] | Holder + rights; [`Cap::attenuate`] is the only grant path |
//! | [`QuotaConfig`] / [`FlowQuota`] | Per-flow CPU / heap / spawn-send budgets |
//! | [`Supervisor`] | OTP strategies (`OneForOne` / `OneForAll` / `RestForOne`) |
//! | [`std_native_table`] | `print`, `now_ms`, `make_msg` (3-arg), `msg_*`, `msg_reply_cap` |
//!
//! # Atomic Hop (messaging contract)
//!
//! Every bytecode `Send` / `Ask` carries exactly one [`Message`]:
//!
//! ```text
//! Message { sender, reply_cap, request_id, tag, payload }
//! ```
//!
//! - Bare `Int` / `Pid` / `Str` on `Send` → trap / [`SendError::NotAHop`]
//! - Scheduler **stamps** `sender` (authenticated origin) and attaches
//!   `reply_cap` (stable SEND-only Cap back to the caller, reused per pair)
//! - Reply with [`std_native_table`]'s `msg_reply_cap` — **not** `msg_sender`
//!   (`Pid` is identity, not an address)
//!
//! Also: selective receive (`ReceiveMatch` / `ReceiveMatchCorr`),
//! `Ask` / `AskTimeout` for correlated RPC, [`Fn::fresh_request_id`].
//!
//! # FlowCap (addressing)
//!
//! | Value | Use |
//! |-------|-----|
//! | [`Value::Cap`] | Target of `Send` / `Ask`; from `SelfPid`, `Spawn`, or `reply_cap` |
//! | [`Value::Pid`] | Identity inside a delivered hop (`msg_sender`) |
//!
//! Host [`Runtime::send`] still takes [`FlowId`] (trusted embedder path)
//! and is stamped `sender = 0` / `reply_cap = NONE`.
//!
//! Named discovery: [`Fn::register_name`] / [`Fn::whereis`] — `whereis`
//! returns a SEND Cap for the caller, never a raw FlowId.
//!
//! Caps resolve only for the **holder** with sufficient rights. Derive a
//! weaker grant with [`Fn::delegate`] / [`Cap::attenuate`] — never by
//! copying a `CapId`. Host spawn is `ROOT`; bytecode [`Fn::spawn_confined`]
//! starts at `NONE`.
//!
//! # Values (ABI v5)
//!
//! `Unit | Bool | Int | Float | Pid | Message | Cap | Str | Bytes`
//!
//! `CapId` is a 128-bit CSPRNG token. `Message.payload` is a nested
//! [`Value`]. `Str` / `Bytes` are `Arc`-backed for cheap register/mailbox
//! clones. They are **not** Atomic Hops by themselves.
//!
//! # Quick start — scalar
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use byteflow::{Program, FlowOutcome, Runtime, Value};
//!
//! let mut program = Program::new("demo");
//! program.function("main", 0, |f| {
//!     let a = f.load_i32(41);
//!     let b = f.load_i32(1);
//!     let sum = f.add(a, b);
//!     f.return_(sum);
//! });
//!
//! let rt = Runtime::new(program.build())?;
//! let outcome = rt.spawn(0, &[])?.join();
//! rt.shutdown();
//! assert!(matches!(outcome, FlowOutcome::Completed(Value::Int(42))));
//! # Ok(())
//! # }
//! ```
//!
//! # Quick start — Atomic Hop (ping-pong)
//!
//! Assemble with [`Program`] / [`Fn`], then run on [`Runtime`]:
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use byteflow::{FlowOutcome, Program, Runtime, Value, std_native_table};
//!
//! const TAG_PING: i32 = 10;
//! const TAG_PONG: i32 = 11;
//!
//! let mut program = Program::new("ping-pong");
//! let pong = program.function("pong", 0, |f| {
//!     let msg = f.receive();
//!     let payload = f.hop_payload(msg);
//!     f.add_imm(payload, 1);
//!     f.send_reply(msg, TAG_PONG, payload);
//!     f.exit(payload);
//! });
//! program.function("main", 0, |f| {
//!     let child = f.spawn(pong, 0);
//!     let payload = f.load_i32(1);
//!     let req = f.hop_fresh(TAG_PING, payload);
//!     let rid = f.hop_request_id(req);
//!     f.send(child, req);
//!     let reply = f.receive_match_corr_imm(TAG_PONG as u16, rid);
//!     let out = f.hop_payload(reply);
//!     f.return_(out);
//! });
//!
//! let rt = Runtime::with_natives(program.build(), std_native_table())?;
//! let Some(main) = rt.function_index("main") else {
//!     return Err("missing main".into());
//! };
//! let outcome = rt.spawn(main, &[])?.join();
//! rt.shutdown();
//! assert!(matches!(outcome, FlowOutcome::Completed(Value::Int(2))));
//! # Ok(())
//! # }
//! ```
//!
//! Built-in helpers for tests: [`samples::ping_pong`], [`samples::ask_reply`],
//! [`samples::selective_receive`], forged-sender security regressions.
//!
//! # Collecting a result
//!
//! [`FlowHandle::join`] blocks, which suits a `main` with nothing else to
//! do. Anything holding a deadline picks its own bound instead:
//!
//! | Call | Waits | While the flow is still running |
//! |------|-------|---------------------------------|
//! | [`FlowHandle::try_join`] | never | `None` |
//! | [`FlowHandle::join_timeout`] / [`FlowHandle::join_deadline`] | up to the bound | `None` |
//! | [`FlowHandle::join`] | unbounded | (blocks) |
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use byteflow::{Program, FlowOutcome, Runtime, Value};
//! use std::time::Duration;
//!
//! let mut program = Program::new("slow");
//! program.function("main", 0, |f| {
//!     let ms = f.load_i32(300);
//!     f.sleep(ms);
//!     let out = f.load_i32(7);
//!     f.return_(out);
//! });
//!
//! let rt = Runtime::new(program.build())?;
//! let handle = rt.spawn(0, &[])?;
//!
//! // Neither of these consumes the handle or the outcome.
//! assert!(handle.try_join().is_none());
//! assert!(handle.join_timeout(Duration::from_millis(10)).is_none());
//!
//! let outcome = handle.join_timeout(Duration::from_secs(10));
//! rt.shutdown();
//! assert!(matches!(outcome, Some(FlowOutcome::Completed(Value::Int(7)))));
//! # Ok(())
//! # }
//! ```
//!
//! A flow destroyed before it produced an outcome — [`Runtime::shutdown`]
//! does not drain suspended flows — wakes its joiner with a failure instead
//! of leaving it parked forever. See [`docs::error_model`].
//!
//! # Design guides (rendered on docs.rs)
//!
//! - [`docs::atomic_hop`] — hop protocol, Cap addressing, natives table
//! - [`docs::beam_mapping`] — BEAM / OTP mental model → Byteflow equivalents
//! - [`docs::lifecycle`] — monitors, links, registry, `WAITING_SEND`
//! - [`docs::mailbox`] — bounded inbox, overflow, lost-wakeup
//! - [`docs::security`] — threat model, invariants S1–S7, Phase 3 (0.9.2+)
//! - [`docs::error_model`] — fail-closed errors (no `unwrap`), bounded joins
//! - [`docs::vm_safety`] — trust boundary: `verify` vs per-step `Fault`
//!
//! # What this is *not*
//!
//! - Not a replacement for Tokio / async Rust (no `.await` IO loop)
//! - Not a distributed cluster runtime (single process, in-memory mailboxes)
//! - Not a full object-capability OS (no distributed revocation / Cap persistence)
//!
//! Host owns I/O and policy. Byteflow owns cheap concurrency and hop delivery.
#![deny(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod bytecode;
pub mod entropy;
pub mod log;
pub mod memory;
pub mod natives;
pub mod output;
pub(crate) mod prng;
pub mod samples;
pub mod scheduler;
pub mod vm;

#[cfg(feature = "jit")]
#[cfg_attr(docsrs, doc(cfg(feature = "jit")))]
pub mod jit;

/// Long-form design notes shipped inside the crate (also under `docs/` on GitHub).
///
/// These modules exist so [docs.rs](https://docs.rs/byteflow-actors) shows the
/// same guides as the repository, not only API rustdoc.
pub mod docs {
    /// Atomic Hop: Message-only Send, FlowCap addressing, Ask, selective receive.
    #[doc = include_str!("../docs/atomic-hop.md")]
    pub mod atomic_hop {}

    /// BEAM / OTP concepts mapped to Byteflow flows, Caps, and Atomic Hop.
    #[doc = include_str!("../docs/beam-mapping.md")]
    pub mod beam_mapping {}

    /// Flow lifecycle: monitors, links, registry, WAITING_SEND.
    #[doc = include_str!("../docs/lifecycle.md")]
    pub mod lifecycle {}

    /// Security model: authenticated sender, FlowCap, invariants S1–S7.
    #[doc = include_str!("../docs/security.md")]
    pub mod security {}

    /// Fail-closed error taxonomy and mutex policy.
    #[doc = include_str!("../docs/error-model.md")]
    pub mod error_model {}

    /// Trust boundary: what `verify` settles statically vs what the `Vm`
    /// checks per step, and why a fault never becomes a panic.
    #[doc = include_str!("../docs/vm-safety.md")]
    pub mod vm_safety {}

    /// Bounded mailbox: capacity contract, overflow, anti lost-wakeup.
    #[doc = include_str!("../docs/mailbox.md")]
    pub mod mailbox {}

    /// Property / stress tests (in-house PRNG) for mailbox, decode/verify, Caps.
    #[doc = include_str!("../docs/properties.md")]
    pub mod properties {}
}

pub use bytecode::{
    asm_macros, decode, decode_with, disassemble, encode, verify, verify_with, Cap, CapId,
    CapIdError, CapRights, CapTarget, Chunk, Fn, ConstantKind, FormatError, FuncId, FunctionDef,
    Instruction, Label, Message, NativeIdx, NativeMask, Opcode, Program, Reg, RegWindow,
    RevocationCell, TrustLevel, Value, VerifyConfig, VerifyError, ABI_VERSION, MAGIC,
    TAG_SYS_DOWN, TAG_SYS_EXIT,
};
pub use output::{NullSink, OutputSink, StdoutSink};
pub use memory::{
    HeapBytes, HeapStr, MemoryBudget, MemoryError, MemoryLimit, MemorySnapshot,
};
pub use natives::{std_native, std_native_map, std_native_table, std_native_table_with, std_natives};
pub use scheduler::{
    fault_count, next_flow_id, flow_id_from_u64, report_fault, CapError, Capability,
    ChildSpec, Delivery, DownEvent, FlowExitReason, FlowQuota, LifecycleError, LinkId, Mailbox, MailboxBytes,
    MailboxCapacity, MailboxConfig, MailboxFull, MailboxFullReason, MailboxStats,
    MonitorRef, OverflowPolicy, QuotaConfig, QuotaError, RegistryName, WaitEpoch, Flow, FlowHandle, FlowId,
    FlowMetrics, FlowOutcome, JoinError, RestartPolicy, RestartStrategy, Runtime, RuntimeConfig,
    RuntimeError, RuntimeMetrics, RuntimeMetricsSnapshot, RuntimeSpawner, SendError,
    SpawnError, Supervisor, SupervisorConfig, DEFAULT_QUANTUM, check_admin, check_link,
    check_monitor, exec_delegate, AdminError, DelegateError, LinkError,
};
#[cfg(feature = "jit")]
pub use scheduler::JitConfig;
pub use vm::{
    expect_arg, expect_bool, expect_int, expect_message, expect_u64, check_native_call,
    check_native_gate,
    Fault, NativeCallError, NativeFn, NativeGate, NativeResult, NativeTable, NativeTableBuilder,
    NativeTableError, Vm, VmResult, MAX_CALL_DEPTH,
};

#[cfg(feature = "jit")]
pub use jit::{
    apply_exit_to_vm, force_compile, hot_threshold, run_compiled_trace, run_compiled_trace_ref,
    run_vm_with_jit, run_vm_with_jit_runtime, sync_slots_from_vm, try_run_hot, try_run_hot_runtime,
    CompileError, CompiledTrace, ExitReason, HotCounter, JitContext, JitEntry, JitFrame, JitReturn,
    JitRuntime, SyncSlotsResult, TraceCache, TraceCompiler, TraceKey, TraceSpan, HOT_THRESHOLD,
    MAX_TRACE_LENGTH, JIT_BUDGET, JIT_CONTINUE, JIT_DEOPT, JIT_EFFECT, JIT_RETURN, JIT_TRAP,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn std_native_map_has_stable_indices() {
        let map = std_native_map();
        assert_eq!(map.get("print"), Some(&0));
        assert_eq!(map.get("now_ms"), Some(&1));
        assert_eq!(map.get("make_msg"), Some(&2));
        assert_eq!(map.get("msg_payload"), Some(&6));
        assert_eq!(map.get("msg_reply_cap"), Some(&7));
    }

    #[test]
    fn std_native_chunk_runs() -> Result<(), Box<dyn std::error::Error>> {
        let mut program = Program::new("std-natives-demo");
        program.function("main", 0, |f| {
            let n = f.load_i32(42);
            f.native1_on(n, 0);
            let ms = f.call_native0(1);
            f.return_(ms);
        });

        let chunk = program.build();
        verify(&chunk)?;

        let rt = Runtime::with_natives(chunk, std_native_table())?;
        let outcome = rt.spawn(0, &[])?.join();
        rt.shutdown();

        match outcome {
            FlowOutcome::Completed(Value::Int(ms)) => {
                assert!(ms >= 0);
                Ok(())
            }
            other => Err(format!("expected Completed(Value::Int(_)), got {other:?}").into()),
        }
    }
}
