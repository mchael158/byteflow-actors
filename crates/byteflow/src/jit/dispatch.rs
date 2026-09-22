//! Execute compiled traces and map exits back to VM state.
#![allow(unsafe_code)]

use std::sync::Arc;

use crate::{Chunk, Fault, Value, Vm, VmResult};

use super::compiler::TraceCompiler;
use super::exit::{ExitReason, JitReturn};
use super::frame::{JitCallRecord, JitFrame, MAX_JIT_CALL_DEPTH};
use super::module_local::with_jit_module;
use super::runtime::JitRuntime;
use super::trace::{CompiledTrace, JitContext, TraceKey, HOT_THRESHOLD};

/// Result of copying VM registers into JIT shadow slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncSlotsResult {
    Ok,
    /// A register held a non-int type — interpreter must resume.
    Deopt,
    /// Frame unavailable.
    Unavailable,
}

/// Attempt one native trace at the current `(function, pc)`.
fn try_jit_step(vm: &mut Vm, ctx: &mut JitContext, budget: u32) -> Option<VmResult> {
    if !Arc::ptr_eq(&ctx.chunk, &vm.chunk_arc()) {
        return None;
    }
    let pc = vm.current_pc()?;
    let function = vm.current_function();
    let register_count = vm.current_num_registers()?;

    let mut slots = vec![0i64; slot_capacity(vm)];
    match sync_slots_from_vm(vm, &mut slots) {
        SyncSlotsResult::Ok => {}
        SyncSlotsResult::Deopt | SyncSlotsResult::Unavailable => return None,
    }

    let key = TraceKey {
        function,
        entry_pc: pc as u32,
    };
    let exit = try_run_hot(
        ctx,
        vm.chunk_arc().as_ref(),
        key,
        &mut slots,
        budget,
        register_count,
        None,
    )?;
    match apply_exit_to_vm(vm, &slots, exit) {
        Ok(Some(result)) => Some(result),
        Ok(None) => None,
        Err(fault) => Some(VmResult::Trap(fault)),
    }
}

fn try_jit_step_runtime(
    vm: &mut Vm,
    runtime: &JitRuntime,
    budget: u32,
    metrics: Option<&crate::scheduler::RuntimeMetrics>,
) -> Option<VmResult> {
    if !runtime.matches_chunk(&vm.chunk_arc()) {
        return None;
    }
    let pc = vm.current_pc()?;
    let function = vm.current_function();
    let register_count = vm.current_num_registers()?;

    let mut slots = vec![0i64; slot_capacity(vm)];
    match sync_slots_from_vm(vm, &mut slots) {
        SyncSlotsResult::Ok => {}
        SyncSlotsResult::Deopt => {
            if let Some(m) = metrics {
                crate::scheduler::RuntimeMetrics::inc(&m.jit_deopts);
            }
            return None;
        }
        SyncSlotsResult::Unavailable => return None,
    }

    let key = TraceKey {
        function,
        entry_pc: pc as u32,
    };
    let exit = try_run_hot_runtime(
        runtime,
        vm.chunk_arc().as_ref(),
        key,
        &mut slots,
        budget,
        register_count,
        metrics,
    )?;
    match apply_exit_to_vm(vm, &slots, exit) {
        Ok(Some(result)) => Some(result),
        Ok(None) => None,
        Err(fault) => Some(VmResult::Trap(fault)),
    }
}

/// Run at most `budget` instructions, retrying compiled traces after each
/// interpreter step when a site becomes hot.
pub fn run_vm_with_jit(vm: &mut Vm, budget: u32, ctx: &mut JitContext) -> VmResult {
    run_vm_with_jit_loop(vm, budget, |vm, remaining| try_jit_step(vm, ctx, remaining))
}

/// Runtime path: shared cache, per-thread compilation module.
pub fn run_vm_with_jit_runtime(
    vm: &mut Vm,
    budget: u32,
    runtime: &JitRuntime,
    metrics: Option<&crate::scheduler::RuntimeMetrics>,
) -> VmResult {
    run_vm_with_jit_loop(vm, budget, |vm, remaining| {
        try_jit_step_runtime(vm, runtime, remaining, metrics)
    })
}

fn run_vm_with_jit_loop(
    vm: &mut Vm,
    budget: u32,
    mut try_step: impl FnMut(&mut Vm, u32) -> Option<VmResult>,
) -> VmResult {
    let mut remaining = budget;
    while remaining > 0 {
        if let Some(result) = try_step(vm, remaining) {
            return result;
        }
        match vm.run(1) {
            VmResult::Yield => {
                remaining -= 1;
            }
            other => return other,
        }
    }
    VmResult::Yield
}

fn slot_capacity(vm: &Vm) -> usize {
    let chunk = vm.chunk_arc();
    let max_in_chunk = chunk
        .functions
        .iter()
        .map(|f| f.num_registers as usize)
        .max();
    match (max_in_chunk, vm.current_num_registers()) {
        (Some(max_regs), Some(current)) => max_regs.max(current as usize),
        (Some(max_regs), None) => max_regs,
        (None, Some(current)) => current as usize,
        (None, None) => 0,
    }
}

/// Run a compiled trace against the shadow slot table.
pub fn run_compiled_trace(
    entry: super::frame::JitEntry,
    slots: &mut [i64],
    pc: u32,
    budget: u32,
    function: u32,
    register_count: u32,
) -> JitReturn {
    let mut call_stack = [JitCallRecord {
        return_pc: 0,
        return_function: 0,
        dest_reg: 0,
        caller_register_count: 0,
    }; MAX_JIT_CALL_DEPTH];
    let mut frame = JitFrame {
        slots: slots.as_mut_ptr(),
        register_count,
        pc,
        budget,
        function,
        exit_kind: 0,
        return_reg: 0,
        call_depth: 0,
        call_stack: call_stack.as_mut_ptr(),
    };
    unsafe { (entry)(&mut frame) };
    JitReturn {
        kind: frame.exit_kind,
        pc: frame.pc,
        return_reg: frame.return_reg,
    }
}

/// Run a cached trace by reference.
pub fn run_compiled_trace_ref(
    trace: &CompiledTrace,
    slots: &mut [i64],
    pc: u32,
    budget: u32,
    function: u32,
    register_count: u32,
) -> JitReturn {
    run_compiled_trace(trace.entry, slots, pc, budget, function, register_count)
}

/// Compile `key` if needed and execute once. Returns `None` when compilation
/// fails (caller should fall back to the interpreter).
pub fn try_run_hot(
    ctx: &mut JitContext,
    chunk: &Chunk,
    key: TraceKey,
    slots: &mut [i64],
    budget: u32,
    register_count: u8,
    metrics: Option<&crate::scheduler::RuntimeMetrics>,
) -> Option<ExitReason> {
    if ctx.cache.get(&key).is_none() {
        if !ctx.hot.hit(key, ctx.hot_threshold) {
            if let Some(m) = metrics {
                crate::scheduler::RuntimeMetrics::inc(&m.jit_misses);
            }
            return None;
        }
        let mut compiler = TraceCompiler::new(ctx.module_mut());
        match compiler.compile_trace(chunk, key) {
            Ok(compiled) => {
                ctx.cache.insert(key, compiled);
                if let Some(m) = metrics {
                    crate::scheduler::RuntimeMetrics::inc(&m.jit_compiles);
                }
            }
            Err(_) => {
                if let Some(m) = metrics {
                    crate::scheduler::RuntimeMetrics::inc(&m.jit_compile_failures);
                }
                return None;
            }
        }
    }
    let trace = ctx.cache.get(&key)?;
    if let Some(m) = metrics {
        crate::scheduler::RuntimeMetrics::inc(&m.jit_executions);
    }
    let ret = run_compiled_trace_ref(
        trace,
        slots,
        key.entry_pc,
        budget,
        key.function,
        u32::from(register_count),
    );
    Some(ret.into_reason())
}

/// Shared-runtime variant (no global lock during execution).
pub fn try_run_hot_runtime(
    runtime: &JitRuntime,
    chunk: &Chunk,
    key: TraceKey,
    slots: &mut [i64],
    budget: u32,
    register_count: u8,
    metrics: Option<&crate::scheduler::RuntimeMetrics>,
) -> Option<ExitReason> {
    if let Some(copy) = runtime.get_trace(&key) {
        if let Some(m) = metrics {
            crate::scheduler::RuntimeMetrics::inc(&m.jit_executions);
        }
        let ret = run_compiled_trace(
            copy.entry,
            slots,
            key.entry_pc,
            budget,
            key.function,
            u32::from(register_count),
        );
        return Some(ret.into_reason());
    }

    if !runtime.record_hot_hit(key) {
        if let Some(m) = metrics {
            crate::scheduler::RuntimeMetrics::inc(&m.jit_misses);
        }
        return None;
    }

    // Double-checked compile after hot threshold.
    if runtime.get_trace(&key).is_some() {
        return try_run_hot_runtime(runtime, chunk, key, slots, budget, register_count, metrics);
    }

    let compiled =
        match with_jit_module(|module| TraceCompiler::new(module).compile_trace(chunk, key)) {
            Ok(trace) => trace,
            Err(_) => {
                if let Some(m) = metrics {
                    crate::scheduler::RuntimeMetrics::inc(&m.jit_compile_failures);
                }
                return None;
            }
        };

    runtime.insert_trace(key, compiled);
    if let Some(m) = metrics {
        crate::scheduler::RuntimeMetrics::inc(&m.jit_compiles);
    }

    try_run_hot_runtime(runtime, chunk, key, slots, budget, register_count, metrics)
}

/// Apply a native trace exit to a live [`Vm`], producing an optional early
/// [`VmResult`] when the trace finished the flow.
pub fn apply_exit_to_vm(
    vm: &mut Vm,
    slots: &[i64],
    exit: ExitReason,
) -> Result<Option<VmResult>, Fault> {
    match exit {
        ExitReason::Return { return_reg } => {
            let value = Value::Int(slots[return_reg as usize]);
            vm.set_register(return_reg, value.clone())?;
            let pc = vm
                .current_pc()
                .ok_or(Fault::Invariant("empty frame stack while running"))?;
            vm.set_pc(pc.saturating_add(1));
            vm.return_value(value)
        }
        ExitReason::Trap { pc } => {
            vm.set_pc(pc as usize);
            Ok(Some(VmResult::Trap(Fault::DivideByZero)))
        }
        ExitReason::Budget { pc } => {
            vm.set_pc(pc as usize);
            sync_slots_to_vm(vm, slots)?;
            Ok(None)
        }
        ExitReason::Continue { pc } | ExitReason::Effect { pc } | ExitReason::Deopt { pc } => {
            vm.set_pc(pc as usize);
            if matches!(
                exit,
                ExitReason::Continue { .. } | ExitReason::Effect { .. }
            ) {
                sync_slots_to_vm(vm, slots)?;
            }
            Ok(None)
        }
    }
}

/// Copy int-compatible registers from the VM into JIT shadow slots.
pub fn sync_slots_from_vm(vm: &Vm, slots: &mut [i64]) -> SyncSlotsResult {
    let Some(regs) = vm.top_registers() else {
        return SyncSlotsResult::Unavailable;
    };
    for (slot, value) in slots.iter_mut().zip(regs.iter()) {
        match value {
            Value::Int(i) => *slot = *i,
            Value::Bool(b) => *slot = i64::from(*b),
            Value::Unit => *slot = 0,
            _ => return SyncSlotsResult::Deopt,
        }
    }
    SyncSlotsResult::Ok
}

/// Write JIT shadow slots back into the active VM register file.
pub fn sync_slots_to_vm(vm: &mut Vm, slots: &[i64]) -> Result<(), Fault> {
    let Some(regs) = vm.top_registers_mut() else {
        return Err(Fault::Invariant("empty frame stack while syncing slots"));
    };
    for (reg, slot) in regs.iter_mut().zip(slots.iter()) {
        *reg = Value::Int(*slot);
    }
    Ok(())
}

/// Threshold at which a `(function, pc)` pair becomes a compilation candidate.
pub const fn hot_threshold() -> u32 {
    HOT_THRESHOLD
}

/// Force-compile a trace (tests / benchmarks).
pub fn force_compile(
    ctx: &mut JitContext,
    chunk: &Chunk,
    key: TraceKey,
) -> Result<(), super::error::CompileError> {
    if ctx.cache.get(&key).is_none() {
        let mut compiler = TraceCompiler::new(ctx.module_mut());
        let compiled = compiler.compile_trace(chunk, key)?;
        ctx.cache.insert(key, compiled);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{
        bytecode::builder::ChunkBuilder, FlowOutcome, NativeTable, Opcode, Runtime, Value, Vm,
        VmResult,
    };

    use super::*;
    use crate::jit::trace::{JitContext, TraceKey};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn add_chunk() -> std::sync::Arc<crate::Chunk> {
        let mut b = ChunkBuilder::new("jit-add");
        b.begin_function("main", 0, 3);
        b.emit_load_imm(0, 41);
        b.emit_load_imm(1, 1);
        b.emit_binop(Opcode::Add, 2, 0, 1);
        b.emit_return(2);
        Arc::new(b.finish())
    }

    #[test]
    fn run_vm_with_jit_completes_scalar_add() -> TestResult {
        let chunk = add_chunk();
        let mut ctx = JitContext::new(chunk.clone())?;
        let key = TraceKey {
            function: 0,
            entry_pc: 0,
        };
        force_compile(&mut ctx, &chunk, key)?;

        let mut vm = Vm::new(chunk, NativeTable::empty(), 0, &[])?;
        let result = run_vm_with_jit(&mut vm, 10_000, &mut ctx);
        assert!(matches!(result, VmResult::Complete(Value::Int(42))));
        Ok(())
    }

    #[test]
    fn native_trace_computes_42() -> TestResult {
        let chunk = add_chunk();
        let mut ctx = JitContext::new(chunk.clone())?;
        let key = TraceKey {
            function: 0,
            entry_pc: 0,
        };
        force_compile(&mut ctx, &chunk, key)?;
        let trace = ctx
            .cache
            .get(&key)
            .ok_or("trace missing after force_compile")?;
        let mut slots = vec![0i64; 3];
        let ret = run_compiled_trace_ref(trace, &mut slots, 0, 10_000, 0, 3);
        let exit = ret.into_reason();
        assert!(matches!(exit, ExitReason::Return { return_reg: 2 }));
        assert_eq!(slots[2], 42);
        Ok(())
    }

    #[test]
    fn jit_matches_interpreter_on_scalar_add() -> TestResult {
        let chunk = add_chunk();
        let rt = Runtime::new((*chunk).clone())?;
        let interp = rt.spawn(0, &[])?.join();
        rt.shutdown();
        assert!(matches!(interp, FlowOutcome::Completed(Value::Int(42))));

        let mut ctx = JitContext::new(chunk.clone())?;
        let key = TraceKey {
            function: 0,
            entry_pc: 0,
        };
        let mut slots = vec![0i64; 3];
        force_compile(&mut ctx, &chunk, key)?;
        let trace = ctx
            .cache
            .get(&key)
            .ok_or("trace missing after force_compile")?;
        let exit = run_compiled_trace_ref(trace, &mut slots, 0, 10_000, 0, 3).into_reason();
        assert!(matches!(exit, ExitReason::Return { return_reg: 2 }));
        assert_eq!(slots[2], 42);

        let mut vm = Vm::new(chunk, NativeTable::empty(), 0, &[])?;
        if let Some(result) = apply_exit_to_vm(&mut vm, &slots, exit)? {
            assert!(matches!(result, VmResult::Complete(Value::Int(42))));
        } else {
            return Err("expected Completed(42)".into());
        }
        Ok(())
    }

    #[test]
    fn div_by_zero_returns_trap_exit() -> TestResult {
        let mut b = ChunkBuilder::new("jit-div0");
        b.begin_function("main", 0, 3);
        b.emit_load_imm(0, 1);
        b.emit_load_imm(1, 0);
        b.emit_binop(Opcode::Div, 2, 0, 1);
        b.emit_return(2);
        let chunk = Arc::new(b.finish());

        let mut ctx = JitContext::new(chunk.clone())?;
        let key = TraceKey {
            function: 0,
            entry_pc: 0,
        };
        force_compile(&mut ctx, &chunk, key)?;
        let trace = ctx
            .cache
            .get(&key)
            .ok_or("trace missing after force_compile")?;
        let mut slots = vec![0i64; 3];
        let ret = run_compiled_trace_ref(trace, &mut slots, 0, 10_000, 0, 3);
        assert!(matches!(ret.into_reason(), ExitReason::Trap { .. }));
        Ok(())
    }

    #[test]
    fn sync_slots_deopts_on_float() -> TestResult {
        let chunk = add_chunk();
        let mut vm = Vm::new(chunk, NativeTable::empty(), 0, &[])?;
        vm.set_register(0, Value::Float(1.0))?;
        let mut slots = vec![0i64; 3];
        assert_eq!(sync_slots_from_vm(&vm, &mut slots), SyncSlotsResult::Deopt);
        Ok(())
    }

    #[test]
    fn runtime_path_compiles_and_runs() -> TestResult {
        let chunk = add_chunk();
        let runtime = JitRuntime::new(chunk.clone(), 1);
        let key = TraceKey {
            function: 0,
            entry_pc: 0,
        };
        let mut slots = vec![0i64; 3];
        let exit = try_run_hot_runtime(&runtime, &chunk, key, &mut slots, 10_000, 3, None)
            .ok_or("expected jit execution")?;
        assert!(matches!(exit, ExitReason::Return { return_reg: 2 }));
        assert_eq!(slots[2], 42);
        Ok(())
    }

    #[test]
    fn budget_exit_when_exhausted_mid_trace() -> TestResult {
        let mut b = ChunkBuilder::new("jit-budget");
        b.begin_function("main", 0, 1);
        b.emit_load_imm(0, 1);
        b.emit_load_imm(0, 2);
        b.emit_load_imm(0, 3);
        b.emit_return(0);
        let chunk = Arc::new(b.finish());

        let mut ctx = JitContext::new(chunk.clone())?;
        let key = TraceKey {
            function: 0,
            entry_pc: 0,
        };
        force_compile(&mut ctx, &chunk, key)?;
        let trace = ctx
            .cache
            .get(&key)
            .ok_or("trace missing after force_compile")?;
        let mut slots = vec![0i64; 1];
        let ret = run_compiled_trace_ref(trace, &mut slots, 0, 2, 0, 1);
        assert!(matches!(ret.into_reason(), ExitReason::Budget { pc: 2 }));
        Ok(())
    }

    #[test]
    fn interleaved_jit_compiles_mid_quantum() -> TestResult {
        let mut b = ChunkBuilder::new("jit-interleave");
        b.begin_function("main", 0, 2);
        b.emit_load_imm(0, 0);
        let head = b.new_label();
        b.bind_label(head);
        b.emit_load_imm(1, 1);
        b.emit_binop(Opcode::Add, 0, 0, 1);
        b.emit_load_imm(1, 100);
        b.emit_binop(Opcode::Lt, 1, 0, 1);
        let done = b.new_label();
        b.emit_branch(1, done);
        b.emit_jump(head);
        b.bind_label(done);
        b.emit_return(0);
        let chunk = Arc::new(b.finish());
        let head_pc = chunk.functions[0].entry + 1;

        let mut ctx = JitContext::new(chunk.clone())?;
        ctx.hot_threshold = 2;
        let mut vm = Vm::new(chunk, NativeTable::empty(), 0, &[])?;
        let result = run_vm_with_jit(&mut vm, 10_000, &mut ctx);
        assert!(matches!(result, VmResult::Complete(Value::Int(100))));
        assert!(ctx
            .cache
            .get(&TraceKey {
                function: 0,
                entry_pc: head_pc,
            })
            .is_some());
        Ok(())
    }

    #[test]
    fn reload_chunk_invalidates_jit_cache() -> TestResult {
        let chunk = add_chunk();
        let runtime = JitRuntime::new(chunk.clone(), 1);
        let key = TraceKey {
            function: 0,
            entry_pc: 0,
        };
        let mut slots = vec![0i64; 3];
        let exit = try_run_hot_runtime(&runtime, &chunk, key, &mut slots, 10_000, 3, None)
            .ok_or("expected jit execution")?;
        assert!(matches!(exit, ExitReason::Return { .. }));

        let mut b = ChunkBuilder::new("other");
        b.begin_function("main", 0, 1);
        b.emit_load_imm(0, 1);
        b.emit_return(0);
        runtime.reload(Arc::new(b.finish()));
        assert!(runtime.get_trace(&key).is_none());
        Ok(())
    }
}
