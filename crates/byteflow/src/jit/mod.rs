//! Trace JIT for Byteflow — Cranelift backend, isolated `unsafe`.
//!
//! Compiles int-specialized traces (`LoadImm`, `AddImm`, `SubImm`, `MulImm`,
//! `EqImm`, `LeImm`, `LoadConst`, `Move`, binops, bitwise ops, `Neg`, `Branch`,
//! `Jump`, `Call`, `Return`). Scheduler effects stop recording.

mod call;
mod compiler;
mod dispatch;
mod error;
mod exit;
mod frame;
mod module_local;
mod runtime;
mod trace;

pub use compiler::TraceCompiler;
pub use dispatch::{
    apply_exit_to_vm, force_compile, hot_threshold, run_compiled_trace, run_compiled_trace_ref,
    run_vm_with_jit, run_vm_with_jit_runtime, sync_slots_from_vm, sync_slots_to_vm, try_run_hot,
    try_run_hot_runtime, SyncSlotsResult,
};
pub use error::CompileError;
pub use exit::{
    ExitReason, JitReturn, JIT_BUDGET, JIT_CONTINUE, JIT_DEOPT, JIT_EFFECT, JIT_RETURN, JIT_TRAP,
};
pub use frame::{JitCallRecord, JitEntry, JitFrame, MAX_JIT_CALL_DEPTH};
pub use runtime::JitRuntime;
pub use trace::{
    CompiledTrace, HotCounter, JitContext, TraceCache, TraceKey, TraceSpan, HOT_THRESHOLD,
    MAX_TRACE_LENGTH,
};
