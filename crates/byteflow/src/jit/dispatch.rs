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

/// Tags of the active frame at the moment a trace is entered.
///
/// The public slot runners pass [`EntryTags::INT`]: every inherited `Move`
/// writes an `Int`. The VM runners fill the real `Bool` and `Unit` bits.
#[derive(Clone, Copy)]
struct EntryTags {
    as_bool: [u64; 4],
    as_unit: [u64; 4],
}

impl EntryTags {
    const INT: Self = Self {
        as_bool: [0; 4],
        as_unit: [0; 4],
    };

    fn from_vm(vm: &Vm) -> Self {
        let mut tags = Self::INT;
        let Some(regs) = vm.top_registers() else {
            return tags;
        };
        for (index, value) in regs.iter().enumerate() {
            if index >= 256 {
                break;
            }
            let bit = 1u64 << (index % 64);
            let word = index / 64;
            match value {
                Value::Bool(_) => tags.as_bool[word] |= bit,
                Value::Unit => tags.as_unit[word] |= bit,
                _ => {}
            }
        }
        tags
    }
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
    let exit = try_run_hot_tagged(
        ctx,
        vm.chunk_arc().as_ref(),
        key,
        &mut HotCall {
            slots: &mut slots,
            budget,
            register_count,
            metrics: None,
            tags: EntryTags::from_vm(vm),
        },
    )?;
    match apply_exit_tagged(vm, &slots, exit) {
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
    let exit = try_run_hot_runtime_tagged(
        runtime,
        vm.chunk_arc().as_ref(),
        key,
        &mut HotCall {
            slots: &mut slots,
            budget,
            register_count,
            metrics,
            tags: EntryTags::from_vm(vm),
        },
    )?;
    match apply_exit_tagged(vm, &slots, exit) {
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
///
/// Inherited `Move`s are written back as `Int`. The VM entry path records
/// the real `Bool` and `Unit` bits before calling the trace.
pub fn run_compiled_trace(
    entry: super::frame::JitEntry,
    slots: &mut [i64],
    pc: u32,
    budget: u32,
    function: u32,
    register_count: u32,
) -> JitReturn {
    run_compiled_trace_with_tags(
        entry,
        slots,
        pc,
        budget,
        function,
        register_count,
        EntryTags::INT,
    )
}

fn run_compiled_trace_with_tags(
    entry: super::frame::JitEntry,
    slots: &mut [i64],
    pc: u32,
    budget: u32,
    function: u32,
    register_count: u32,
    tags: EntryTags,
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
        written: [0; 4],
        as_bool: [0; 4],
        as_unit: [0; 4],
        entry_as_bool: tags.as_bool,
        entry_as_unit: tags.as_unit,
    };
    unsafe { (entry)(&mut frame) };
    JitReturn {
        kind: frame.exit_kind,
        pc: frame.pc,
        return_reg: frame.return_reg,
        written: frame.written,
        as_bool: frame.as_bool,
        as_unit: frame.as_unit,
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

struct HotCall<'a> {
    slots: &'a mut [i64],
    budget: u32,
    register_count: u8,
    metrics: Option<&'a crate::scheduler::RuntimeMetrics>,
    tags: EntryTags,
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
    try_run_hot_tagged(
        ctx,
        chunk,
        key,
        &mut HotCall {
            slots,
            budget,
            register_count,
            metrics,
            tags: EntryTags::INT,
        },
    )
    .map(JitReturn::into_reason)
}

fn try_run_hot_tagged(
    ctx: &mut JitContext,
    chunk: &Chunk,
    key: TraceKey,
    call: &mut HotCall<'_>,
) -> Option<JitReturn> {
    if ctx.cache.get(&key).is_none() {
        if !ctx.hot.hit(key, ctx.hot_threshold) {
            if let Some(m) = call.metrics {
                crate::scheduler::RuntimeMetrics::inc(&m.jit_misses);
            }
            return None;
        }
        let mut compiler = TraceCompiler::new(ctx.module_mut());
        match compiler.compile_trace(chunk, key) {
            Ok(compiled) => {
                ctx.cache.insert(key, compiled);
                if let Some(m) = call.metrics {
                    crate::scheduler::RuntimeMetrics::inc(&m.jit_compiles);
                }
            }
            Err(_) => {
                if let Some(m) = call.metrics {
                    crate::scheduler::RuntimeMetrics::inc(&m.jit_compile_failures);
                }
                return None;
            }
        }
    }
    let trace = ctx.cache.get(&key)?;
    if let Some(m) = call.metrics {
        crate::scheduler::RuntimeMetrics::inc(&m.jit_executions);
    }
    Some(run_compiled_trace_with_tags(
        trace.entry,
        call.slots,
        key.entry_pc,
        call.budget,
        key.function,
        u32::from(call.register_count),
        call.tags,
    ))
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
    try_run_hot_runtime_tagged(
        runtime,
        chunk,
        key,
        &mut HotCall {
            slots,
            budget,
            register_count,
            metrics,
            tags: EntryTags::INT,
        },
    )
    .map(JitReturn::into_reason)
}

fn try_run_hot_runtime_tagged(
    runtime: &JitRuntime,
    chunk: &Chunk,
    key: TraceKey,
    call: &mut HotCall<'_>,
) -> Option<JitReturn> {
    if let Some(copy) = runtime.get_trace(&key) {
        if let Some(m) = call.metrics {
            crate::scheduler::RuntimeMetrics::inc(&m.jit_executions);
        }
        return Some(run_compiled_trace_with_tags(
            copy.entry,
            call.slots,
            key.entry_pc,
            call.budget,
            key.function,
            u32::from(call.register_count),
            call.tags,
        ));
    }

    if !runtime.record_hot_hit(key) {
        if let Some(m) = call.metrics {
            crate::scheduler::RuntimeMetrics::inc(&m.jit_misses);
        }
        return None;
    }

    // Double-checked compile after hot threshold.
    if runtime.get_trace(&key).is_some() {
        return try_run_hot_runtime_tagged(runtime, chunk, key, call);
    }

    let compiled =
        match with_jit_module(|module| TraceCompiler::new(module).compile_trace(chunk, key)) {
            Ok(trace) => trace,
            Err(_) => {
                if let Some(m) = call.metrics {
                    crate::scheduler::RuntimeMetrics::inc(&m.jit_compile_failures);
                }
                return None;
            }
        };

    runtime.insert_trace(key, compiled);
    if let Some(m) = call.metrics {
        crate::scheduler::RuntimeMetrics::inc(&m.jit_compiles);
    }

    try_run_hot_runtime_tagged(runtime, chunk, key, call)
}

fn tag_bit(words: &[u64; 4], reg: usize) -> bool {
    if reg >= 256 {
        return false;
    }
    words[reg / 64] & (1u64 << (reg % 64)) != 0
}

/// Write back only registers the trace assigned. Untouched `Unit` / `Bool`
/// values stay as they were. A `Move` of either tag is written back as that
/// tag. Comparison results come back as `Bool`.
fn sync_slots_tagged(vm: &mut Vm, slots: &[i64], ret: &JitReturn) -> Result<(), Fault> {
    let Some(regs) = vm.top_registers_mut() else {
        return Err(Fault::Invariant("empty frame stack while syncing slots"));
    };
    for (index, reg) in regs.iter_mut().enumerate() {
        if !tag_bit(&ret.written, index) {
            continue;
        }
        let Some(slot) = slots.get(index).copied() else {
            continue;
        };
        *reg = if tag_bit(&ret.as_unit, index) {
            Value::Unit
        } else if tag_bit(&ret.as_bool, index) {
            Value::Bool(slot != 0)
        } else {
            Value::Int(slot)
        };
    }
    Ok(())
}

fn apply_exit_tagged(
    vm: &mut Vm,
    slots: &[i64],
    ret: JitReturn,
) -> Result<Option<VmResult>, Fault> {
    match ret.kind {
        super::exit::JIT_RETURN => {
            sync_slots_tagged(vm, slots, &ret)?;
            let return_reg = ret.return_reg as u8;
            let value = match vm
                .top_registers()
                .and_then(|regs| regs.get(usize::from(return_reg)))
            {
                Some(v) => v.clone(),
                None => {
                    let frame_size = match vm.current_num_registers() {
                        Some(n) => n,
                        None => 0,
                    };
                    return Err(Fault::RegisterOutOfRange {
                        reg: return_reg,
                        frame_size,
                    });
                }
            };
            let pc = vm
                .current_pc()
                .ok_or(Fault::Invariant("empty frame stack while running"))?;
            vm.set_pc(pc.saturating_add(1));
            vm.return_value(value)
        }
        super::exit::JIT_TRAP => {
            vm.set_pc(ret.pc as usize);
            Ok(Some(VmResult::Trap(Fault::DivideByZero)))
        }
        super::exit::JIT_BUDGET => {
            vm.set_pc(ret.pc as usize);
            sync_slots_tagged(vm, slots, &ret)?;
            Ok(None)
        }
        super::exit::JIT_CONTINUE | super::exit::JIT_EFFECT => {
            vm.set_pc(ret.pc as usize);
            sync_slots_tagged(vm, slots, &ret)?;
            Ok(None)
        }
        _ => {
            vm.set_pc(ret.pc as usize);
            Ok(None)
        }
    }
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

    /// Budget expiry used to resume past instructions whose results existed
    /// only in SSA, and writeback turned every untouched `Unit` into `Int(0)`.
    #[test]
    fn budget_exit_publishes_assigned_regs_and_keeps_unit() -> TestResult {
        let mut b = ChunkBuilder::new("jit-budget");
        b.begin_function("main", 0, 2);
        b.emit_load_imm(0, 41);
        b.emit_load_imm(1, 1);
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
        let mut slots = vec![0i64; 2];
        let ret = run_compiled_trace_ref(trace, &mut slots, 0, 1, 0, 2);
        assert!(matches!(ret.into_reason(), ExitReason::Budget { pc: 1 }));
        let mut vm = Vm::new(chunk, NativeTable::empty(), 0, &[])?;
        apply_exit_tagged(&mut vm, &slots, ret)?;
        let regs = vm.top_registers().ok_or("no frame")?;
        assert!(matches!(regs.first(), Some(&Value::Int(41))));
        assert!(matches!(regs.get(1), Some(&Value::Unit)));
        Ok(())
    }

    #[test]
    fn jit_call_does_not_clobber_the_caller_frame() -> TestResult {
        let mut b = ChunkBuilder::new("jit-call-frames");
        b.begin_function("callee", 0, 3);
        b.emit_load_imm(2, 99);
        b.emit_load_imm(0, 1);
        b.emit_return(0);
        let main = b.begin_function("main", 0, 3);
        b.emit_load_imm(2, 7);
        b.emit_call(0, 0, 0);
        b.emit_return(2);
        let chunk = Arc::new(b.finish());
        let mut ctx = JitContext::new(chunk.clone())?;
        let entry = chunk.functions[main as usize].entry;
        force_compile(
            &mut ctx,
            &chunk,
            TraceKey {
                function: main,
                entry_pc: entry,
            },
        )?;
        let mut vm = Vm::new(chunk, NativeTable::empty(), main, &[])?;
        let result = run_vm_with_jit(&mut vm, 10_000, &mut ctx);
        assert!(matches!(result, VmResult::Complete(Value::Int(7))));
        Ok(())
    }

    #[test]
    fn move_keeps_bool_unit_and_a_later_int() -> TestResult {
        let mut b = ChunkBuilder::new("jit-move-tags");
        b.begin_function("cmp", 0, 4);
        b.emit_load_imm(0, 1);
        b.emit_load_imm(1, 1);
        b.emit_binop(Opcode::Eq, 2, 0, 1);
        b.emit_move(3, 2);
        b.emit_return(3);
        let moved_bool = b.begin_function("moved-bool", 1, 2);
        b.emit_move(1, 0);
        b.emit_return(1);
        let moved_false = b.begin_function("moved-false", 1, 2);
        b.emit_move(1, 0);
        b.emit_return(1);
        let moved_unit = b.begin_function("moved-unit", 0, 2);
        b.emit_move(1, 0);
        b.emit_return(1);
        let overwritten = b.begin_function("overwritten", 1, 2);
        b.emit_load_imm(0, 5);
        let cont = b.new_label();
        b.emit_jump(cont);
        b.bind_label(cont);
        b.emit_move(1, 0);
        b.emit_return(1);
        let chunk = Arc::new(b.finish());
        let mut ctx = JitContext::new(chunk.clone())?;

        let run = |ctx: &mut JitContext, function: u32, args: &[Value]| -> TestResult {
            let entry = chunk.functions[function as usize].entry;
            force_compile(
                ctx,
                &chunk,
                TraceKey {
                    function,
                    entry_pc: entry,
                },
            )?;
            let mut vm = Vm::new(chunk.clone(), NativeTable::empty(), function, args)?;
            let result = run_vm_with_jit(&mut vm, 10_000, ctx);
            match (function, result) {
                (_, VmResult::Complete(Value::Bool(true))) if function == 0 => Ok(()),
                (_, VmResult::Complete(Value::Bool(true))) if function == moved_bool => Ok(()),
                (_, VmResult::Complete(Value::Bool(false))) if function == moved_false => Ok(()),
                (_, VmResult::Complete(Value::Unit)) if function == moved_unit => Ok(()),
                (_, VmResult::Complete(Value::Int(5))) if function == overwritten => Ok(()),
                (_, other) => Err(format!("function {function} finished as {other:?}").into()),
            }
        };

        run(&mut ctx, 0, &[])?;
        run(&mut ctx, moved_bool, &[Value::Bool(true)])?;
        run(&mut ctx, moved_false, &[Value::Bool(false)])?;
        run(&mut ctx, moved_unit, &[])?;
        run(&mut ctx, overwritten, &[Value::Bool(true)])?;
        Ok(())
    }
}
