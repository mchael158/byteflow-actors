//! Trace JIT for Byteflow — Cranelift backend, isolated `unsafe`.
//!
//! Compiles int traces (`LoadImm`, `AddImm`, `SubImm`, `MulImm`, `LtImm`,
//! `EqImm`, `LeImm`, `LoadConst`, `Move`, `Nop`, arithmetic, `Div`, `Mod`,
//! bitwise ops including `Shl`/`Shr`, `Neg`, `Branch`, `Jump`, `Call`,
//! `Return`). A float trace covers `LoadConst` of `Float`, `Move`, `Add`,
//! `Sub`, `Mul`, `Div`, `Eq`, `Lt`, `Le`, `Neg`, and the same control
//! opcodes. Scheduler effects, lists, maps, and records stop recording.

mod call;
mod compiler;

mod dispatch;
mod error;
mod exit;
mod frame;
mod module_local;
#[cfg(test)]
mod parity;
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
