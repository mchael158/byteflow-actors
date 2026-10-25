//! Execute compiled traces and map exits back to VM state.
#![allow(unsafe_code)]

use std::sync::Arc;

use crate::{Chunk, Fault, Opcode, Value, Vm, VmResult};

use super::compiler::TraceCompiler;
use super::exit::{ExitReason, JitReturn, JIT_BUDGET, JIT_DEOPT};
use super::frame::{JitCallRecord, JitFrame, MAX_JIT_CALL_DEPTH};
use super::module_local::with_jit_module;
use super::runtime::JitRuntime;
use super::trace::{CompiledTrace, JitContext, TraceKey, HOT_THRESHOLD};

/// Result of copying VM registers into JIT shadow slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncSlotsResult {
    Ok,
    /// Every register is `Float`, `Bool`, or `Unit`, and at least one is
    /// `Float`. Slots hold IEEE bits.
    Float,
    /// A register held a type this specialization cannot guard.
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
    as_float: [u64; 4],
}

impl EntryTags {
    const INT: Self = Self {
        as_bool: [0; 4],
        as_unit: [0; 4],
        as_float: [0; 4],
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
                Value::Float(_) => tags.as_float[word] |= bit,
                _ => {}
            }
        }
        tags
    }
}

/// Attempt one native trace at the current `(function, pc)`.
fn try_jit_step(vm: &mut Vm, ctx: &mut JitContext, budget: u32) -> JitProbe {
    if !Arc::ptr_eq(&ctx.chunk, &vm.chunk_arc()) {
        return JitProbe::Miss;
    }
    let Some(pc) = vm.current_pc() else {
        return JitProbe::Miss;
    };
    let function = vm.current_function();
    let Some(register_count) = vm.current_num_registers() else {
        return JitProbe::Miss;
    };

    let mut slots = vec![0i64; slot_capacity(vm)];
    let floats = match sync_slots_from_vm(vm, &mut slots) {
        SyncSlotsResult::Ok => false,
        SyncSlotsResult::Float => true,
        SyncSlotsResult::Deopt | SyncSlotsResult::Unavailable => return JitProbe::Miss,
    };

    let key = TraceKey {
        function,
        entry_pc: pc as u32,
        floats,
    };
    let Some((exit, spent)) = try_run_hot_tagged(
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
    ) else {
        return JitProbe::Miss;
    };
    finish_probe(vm, &slots, exit, spent)
}

fn try_jit_step_runtime(
    vm: &mut Vm,
    runtime: &JitRuntime,
    budget: u32,
    metrics: Option<&crate::scheduler::RuntimeMetrics>,
) -> JitProbe {
    if !runtime.matches_chunk(&vm.chunk_arc()) {
        return JitProbe::Miss;
    }
    let Some(pc) = vm.current_pc() else {
        return JitProbe::Miss;
    };
    let function = vm.current_function();
    let Some(register_count) = vm.current_num_registers() else {
        return JitProbe::Miss;
    };

    let mut slots = vec![0i64; slot_capacity(vm)];
    let floats = match sync_slots_from_vm(vm, &mut slots) {
        SyncSlotsResult::Ok => false,
        SyncSlotsResult::Float => true,
        SyncSlotsResult::Deopt => {
            if let Some(m) = metrics {
                crate::scheduler::RuntimeMetrics::inc(&m.jit_deopts);
            }
            return JitProbe::Miss;
        }
        SyncSlotsResult::Unavailable => return JitProbe::Miss,
    };

    let key = TraceKey {
        function,
        entry_pc: pc as u32,
        floats,
    };
    let Some((exit, spent)) = try_run_hot_runtime_tagged(
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
    ) else {
        return JitProbe::Miss;
    };
    finish_probe(vm, &slots, exit, spent)
}

/// What one probe of the current `pc` did.
enum JitProbe {
    /// No compiled trace ran. The interpreter executes this instruction.
    Miss,
    /// A trace ran `spent` instructions. `stop` ends the quantum. `None`
    /// means the interpreter still has to execute the instruction now at `pc`
    /// (an effect the trace refused, or the instruction after a side exit).
    Hit { spent: u32, stop: Option<VmResult> },
}

fn finish_probe(vm: &mut Vm, slots: &[i64], exit: JitReturn, spent: u32) -> JitProbe {
    let kind = exit.kind;
    match apply_exit_tagged(vm, slots, exit) {
        Ok(Some(result)) => JitProbe::Hit {
            spent,
            stop: Some(result),
        },
        Ok(None) if kind == JIT_BUDGET => JitProbe::Hit {
            spent,
            stop: Some(VmResult::Yield),
        },
        Ok(None) if kind == JIT_DEOPT => JitProbe::Miss,
        Ok(None) => JitProbe::Hit { spent, stop: None },
        Err(fault) => JitProbe::Hit {
            spent,
            stop: Some(VmResult::Trap(fault)),
        },
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
    mut try_step: impl FnMut(&mut Vm, u32) -> JitProbe,
) -> VmResult {
    let mut remaining = budget;
    while remaining > 0 {
        match try_step(vm, remaining) {
            JitProbe::Hit { spent, stop } => {
                let spent = spent.min(remaining);
                vm.record_instructions(u64::from(spent));
                remaining -= spent;
                if let Some(result) = stop {
                    return result;
                }
                if remaining == 0 {
                    break;
                }
            }
            JitProbe::Miss => {}
        }
        // `vm.run(1)` returns `Yield` both when its one-instruction slice
        // ends and when the bytecode itself executes `Yield`. Only the
        // opcode hands the flow back to the scheduler; the slice end stays
        // inside this quantum.
        let pc_before = vm.current_pc();
        match vm.run(1) {
            VmResult::Yield if explicit_yield(vm, pc_before) => return VmResult::Yield,
            VmResult::Yield => {
                remaining -= 1;
            }
            other => return other,
        }
    }
    VmResult::Yield
}

/// `true` when the instruction just interpreted was `Opcode::Yield`.
///
/// `fetch` advances the pc before the opcode runs, so the executed word is
/// the one at `pc_before`, not the pc the VM holds now.
fn explicit_yield(vm: &Vm, pc_before: Option<usize>) -> bool {
    let Some(pc) = pc_before else {
        return false;
    };
    vm.chunk_arc()
        .code
        .get(pc)
        .is_some_and(|instr| instr.op == Opcode::Yield)
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
    .0
}

fn run_compiled_trace_with_tags(
    entry: super::frame::JitEntry,
    slots: &mut [i64],
    pc: u32,
    budget: u32,
    function: u32,
    register_count: u32,
    tags: EntryTags,
) -> (JitReturn, u32) {
    let mut call_stack = [JitCallRecord {
        return_pc: 0,
        return_function: 0,
        dest_reg: 0,
        caller_register_count: 0,
        written: [0; 4],
        as_bool: [0; 4],
        as_unit: [0; 4],
    }; MAX_JIT_CALL_DEPTH];
    let stride = slots.len();
    let mut spill = vec![0i64; stride.saturating_mul(MAX_JIT_CALL_DEPTH)];
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
        spill: spill.as_mut_ptr(),
        spill_stride: stride as u32,
        written: [0; 4],
        as_bool: [0; 4],
        as_unit: [0; 4],
        entry_as_bool: tags.as_bool,
        entry_as_unit: tags.as_unit,
        as_float: [0; 4],
        entry_as_float: tags.as_float,
    };
    unsafe { (entry)(&mut frame) };
    let spent = budget.saturating_sub(frame.budget);
    (
        JitReturn {
            kind: frame.exit_kind,
            pc: frame.pc,
            return_reg: frame.return_reg,
            written: frame.written,
            as_bool: frame.as_bool,
            as_unit: frame.as_unit,
            as_float: frame.as_float,
        },
        spent,
    )
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
    .map(|(ret, _)| ret.into_reason())
}

fn try_run_hot_tagged(
    ctx: &mut JitContext,
    chunk: &Chunk,
    key: TraceKey,
    call: &mut HotCall<'_>,
) -> Option<(JitReturn, u32)> {
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
    .map(|(ret, _)| ret.into_reason())
}

fn try_run_hot_runtime_tagged(
    runtime: &JitRuntime,
    chunk: &Chunk,
    key: TraceKey,
    call: &mut HotCall<'_>,
) -> Option<(JitReturn, u32)> {
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
        } else if tag_bit(&ret.as_float, index) {
            Value::Float(f64::from_bits(slot as u64))
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
            let Some(bits) = slots.get(usize::from(return_reg)).copied() else {
                return Err(Fault::RegisterOutOfRange {
                    reg: return_reg,
                    frame_size: u8::try_from(slots.len()).unwrap_or(u8::MAX),
                });
            };
            let value = Value::Int(bits);
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
    let mut saw_float = false;
    let mut saw_int = false;
    for value in regs.iter().take(slots.len()) {
        match value {
            Value::Int(_) => saw_int = true,
            Value::Float(_) => saw_float = true,
            Value::Bool(_) | Value::Unit => {}
            _ => return SyncSlotsResult::Deopt,
        }
    }
    if saw_int && saw_float {
        return SyncSlotsResult::Deopt;
    }
    for (slot, value) in slots.iter_mut().zip(regs.iter()) {
        *slot = match value {
            Value::Int(i) => *i,
            Value::Bool(b) => i64::from(*b),
            Value::Unit => 0,
            Value::Float(n) => n.to_bits() as i64,
            _ => 0,
        };
    }
    if saw_float {
        SyncSlotsResult::Float
    } else {
        SyncSlotsResult::Ok
    }
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

            floats: false,
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

            floats: false,
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
    fn mul_imm_eq_imm_and_shifts_match_the_interpreter() -> TestResult {
        let mut b = ChunkBuilder::new("jit-bits");
        b.begin_function("main", 0, 4);
        b.emit_load_imm(0, 6);
        b.emit_mul_imm(1, 0, 7);
        b.emit_load_imm(2, 1);
        b.emit_bit_binop(Opcode::Shl, 3, 1, 2);
        b.emit_bit_binop(Opcode::Shr, 1, 3, 2);
        b.emit_return(1);
        let chunk = Arc::new(b.finish());
        let mut interpreted = Vm::new(chunk.clone(), NativeTable::empty(), 0, &[])?;
        assert!(matches!(
            interpreted.run(20),
            VmResult::Complete(Value::Int(42))
        ));
        let mut ctx = JitContext::new(chunk.clone())?;
        let key = TraceKey {
            function: 0,
            entry_pc: 0,
            floats: false,
        };
        force_compile(&mut ctx, &chunk, key)?;
        let mut vm = Vm::new(chunk, NativeTable::empty(), 0, &[])?;
        let result = run_vm_with_jit(&mut vm, 20, &mut ctx);
        assert!(matches!(result, VmResult::Complete(Value::Int(42))));

        let mut b = ChunkBuilder::new("jit-eq-imm");
        b.begin_function("main", 0, 2);
        b.emit_load_imm(0, 42);
        b.emit_eq_imm(1, 0, 42);
        b.emit_le_imm(0, 0, 41);
        b.emit_return(1);
        let chunk = Arc::new(b.finish());
        let mut interpreted = Vm::new(chunk.clone(), NativeTable::empty(), 0, &[])?;
        assert!(matches!(
            interpreted.run(20),
            VmResult::Complete(Value::Bool(true))
        ));
        let mut ctx = JitContext::new(chunk.clone())?;
        force_compile(&mut ctx, &chunk, key)?;
        let mut vm = Vm::new(chunk, NativeTable::empty(), 0, &[])?;
        let result = run_vm_with_jit(&mut vm, 20, &mut ctx);
        assert!(matches!(result, VmResult::Complete(Value::Bool(true))));
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

            floats: false,
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

            floats: false,
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
    fn div_and_mod_of_i64_min_wrap_instead_of_aborting() -> TestResult {
        let mut b = ChunkBuilder::new("jit-div-overflow");
        b.begin_function("div", 0, 3);
        let min = b.const_(Value::Int(i64::MIN));
        let neg = b.const_(Value::Int(-1));
        b.emit_load_const(0, min);
        b.emit_load_const(1, neg);
        b.emit_binop(Opcode::Div, 2, 0, 1);
        b.emit_return(2);
        let rem = b.begin_function("rem", 0, 3);
        let min = b.const_(Value::Int(i64::MIN));
        let neg = b.const_(Value::Int(-1));
        b.emit_load_const(0, min);
        b.emit_load_const(1, neg);
        b.emit_binop(Opcode::Mod, 2, 0, 1);
        b.emit_return(2);
        let chunk = Arc::new(b.finish());
        let mut ctx = JitContext::new(chunk.clone())?;

        for (function, expect) in [(0, i64::MIN), (rem, 0)] {
            let entry = chunk.functions[function as usize].entry;
            force_compile(
                &mut ctx,
                &chunk,
                TraceKey {
                    function,
                    entry_pc: entry,

                    floats: false,
                },
            )?;
            let mut vm = Vm::new(chunk.clone(), NativeTable::empty(), function, &[])?;
            let result = run_vm_with_jit(&mut vm, 10_000, &mut ctx);
            match result {
                VmResult::Complete(Value::Int(n)) if n == expect => {}
                other => {
                    return Err(format!("function {function} finished as {other:?}").into());
                }
            }
        }
        Ok(())
    }

    #[test]
    fn sync_slots_accepts_a_float_register() -> TestResult {
        let chunk = add_chunk();
        let mut vm = Vm::new(chunk, NativeTable::empty(), 0, &[])?;
        vm.set_register(0, Value::Float(1.0))?;
        let mut slots = vec![0i64; 3];
        assert_eq!(sync_slots_from_vm(&vm, &mut slots), SyncSlotsResult::Float);
        assert_eq!(f64::from_bits(slots[0] as u64), 1.0);
        Ok(())
    }

    #[test]
    fn float_trace_matches_the_interpreter() -> TestResult {
        let mut b = ChunkBuilder::new("jit-float");
        let zero = b.const_(Value::Float(0.0));
        let one = b.const_(Value::Float(1.0));
        let limit = b.const_(Value::Float(4.0));
        b.begin_function("main", 0, 4);
        b.emit_load_const(0, zero);
        b.emit_load_const(1, one);
        b.emit_load_const(2, limit);
        let head = b.new_label();
        let done = b.new_label();
        b.bind_label(head);
        b.emit_binop(Opcode::Add, 0, 0, 1);
        b.emit_binop(Opcode::Lt, 3, 0, 2);
        b.emit_branch(3, done);
        b.emit_jump(head);
        b.bind_label(done);
        b.emit_return(0);
        let chunk = Arc::new(b.finish());
        let mut plain = Vm::new(Arc::clone(&chunk), NativeTable::empty(), 0, &[])?;
        let interpreted = plain.run(100);
        let mut ctx = JitContext::new(Arc::clone(&chunk))?;
        ctx.hot_threshold = 1;
        let mut vm = Vm::new(chunk, NativeTable::empty(), 0, &[])?;
        let compiled = run_vm_with_jit(&mut vm, 100, &mut ctx);
        if !ctx.cache.has_float_trace() {
            return Err("float add loop was not compiled".into());
        }
        match (interpreted, compiled) {
            (VmResult::Complete(Value::Float(a)), VmResult::Complete(Value::Float(b)))
                if a == b && a == 4.0 =>
            {
                Ok(())
            }
            (left, right) => Err(format!("float trace {right:?} interpreter {left:?}").into()),
        }
    }

    fn run_float_chunk(
        chunk: Arc<Chunk>,
        budget: u32,
    ) -> Result<(VmResult, VmResult, JitContext), Box<dyn std::error::Error>> {
        let mut plain = Vm::new(Arc::clone(&chunk), NativeTable::empty(), 0, &[])?;
        let interpreted = plain.run(budget);
        let mut ctx = JitContext::new(Arc::clone(&chunk))?;
        ctx.hot_threshold = 1;
        let mut vm = Vm::new(chunk, NativeTable::empty(), 0, &[])?;
        let compiled = run_vm_with_jit(&mut vm, budget, &mut ctx);
        Ok((interpreted, compiled, ctx))
    }

    #[test]
    fn float_trace_edges_match_the_interpreter() -> TestResult {
        let mut nan_lt = ChunkBuilder::new("jit-nan-lt");
        let nan = nan_lt.const_(Value::Float(f64::NAN));
        let zero = nan_lt.const_(Value::Float(0.0));
        nan_lt.begin_function("main", 0, 3);
        nan_lt.emit_load_const(0, nan);
        nan_lt.emit_load_const(1, zero);
        let head = nan_lt.new_label();
        nan_lt.bind_label(head);
        nan_lt.emit_binop(Opcode::Lt, 2, 0, 1);
        nan_lt.emit_return(2);
        let nan_chunk = Arc::new(nan_lt.finish());
        let (interpreted, compiled, ctx) = run_float_chunk(Arc::clone(&nan_chunk), 20)?;
        if !ctx.cache.has_float_trace() {
            return Err("NaN comparison was not compiled".into());
        }
        match (interpreted, compiled) {
            (VmResult::Complete(Value::Bool(false)), VmResult::Complete(Value::Bool(false))) => {}
            (left, right) => {
                return Err(format!("nan lt {right:?} interpreter {left:?}").into());
            }
        }

        let mut inf_add = ChunkBuilder::new("jit-inf-add");
        let pos = inf_add.const_(Value::Float(f64::INFINITY));
        let neg = inf_add.const_(Value::Float(f64::NEG_INFINITY));
        inf_add.begin_function("main", 0, 3);
        inf_add.emit_load_const(0, pos);
        inf_add.emit_load_const(1, neg);
        let head = inf_add.new_label();
        inf_add.bind_label(head);
        inf_add.emit_binop(Opcode::Add, 2, 0, 1);
        inf_add.emit_return(2);
        let inf_chunk = Arc::new(inf_add.finish());
        let (interpreted, compiled, ctx) = run_float_chunk(Arc::clone(&inf_chunk), 20)?;
        if !ctx.cache.has_float_trace() {
            return Err("infinity add was not compiled".into());
        }
        match (interpreted, compiled) {
            (VmResult::Complete(Value::Float(a)), VmResult::Complete(Value::Float(b)))
                if a.is_nan() && b.is_nan() => {}
            (left, right) => {
                return Err(format!("inf add {right:?} interpreter {left:?}").into());
            }
        }

        let mut zeros = ChunkBuilder::new("jit-signed-zero");
        let neg_zero = zeros.const_(Value::Float(-0.0));
        zeros.begin_function("main", 0, 1);
        zeros.emit_load_const(0, neg_zero);
        let head = zeros.new_label();
        zeros.bind_label(head);
        zeros.emit_add_imm(0, 0, 0);
        zeros.emit_return(0);
        let zero_chunk = Arc::new(zeros.finish());
        let (interpreted, compiled, ctx) = run_float_chunk(Arc::clone(&zero_chunk), 20)?;
        if !ctx.cache.has_float_trace() {
            return Err("signed-zero add was not compiled".into());
        }
        match (interpreted, compiled) {
            (VmResult::Complete(Value::Float(a)), VmResult::Complete(Value::Float(b)))
                if a.to_bits() == 0.0f64.to_bits() && b.to_bits() == 0.0f64.to_bits() => {}
            (left, right) => {
                return Err(format!("signed zero {right:?} interpreter {left:?}").into());
            }
        }

        let mut countdown = ChunkBuilder::new("jit-sub-imm");
        let start = countdown.const_(Value::Float(4.0));
        countdown.begin_function("main", 0, 2);
        countdown.emit_load_const(0, start);
        let head = countdown.new_label();
        let body = countdown.new_label();
        let done = countdown.new_label();
        countdown.bind_label(head);
        countdown.emit_lt_imm(1, 0, 1);
        countdown.emit_branch(1, body);
        countdown.emit_jump(done);
        countdown.bind_label(body);
        countdown.emit_sub_imm(0, 0, 1);
        countdown.emit_jump(head);
        countdown.bind_label(done);
        countdown.emit_return(0);
        let sub_chunk = Arc::new(countdown.finish());
        let (interpreted, compiled, ctx) = run_float_chunk(Arc::clone(&sub_chunk), 40)?;
        if !ctx.cache.has_float_trace() {
            return Err("float SubImm loop was not compiled".into());
        }
        match (interpreted, compiled) {
            (VmResult::Complete(Value::Float(a)), VmResult::Complete(Value::Float(b)))
                if a == 0.0 && b == 0.0 =>
            {
                Ok(())
            }
            (left, right) => Err(format!("sub imm {right:?} interpreter {left:?}").into()),
        }
    }

    #[test]
    fn runtime_path_compiles_and_runs() -> TestResult {
        let chunk = add_chunk();
        let runtime = JitRuntime::new(chunk.clone(), 1);
        let key = TraceKey {
            function: 0,
            entry_pc: 0,

            floats: false,
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

            floats: false,
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
    fn explicit_yield_stops_the_quantum() -> TestResult {
        let mut b = ChunkBuilder::new("jit-yield");
        b.begin_function("main", 0, 1);
        b.emit_load_imm(0, 1);
        b.emit_yield();
        b.emit_load_imm(0, 2);
        b.emit_return(0);
        let chunk = Arc::new(b.finish());

        let mut ctx = JitContext::new(Arc::clone(&chunk))?;
        ctx.hot_threshold = u32::MAX;
        let mut vm = Vm::new(Arc::clone(&chunk), NativeTable::empty(), 0, &[])?;
        let first = run_vm_with_jit(&mut vm, 100, &mut ctx);
        if !matches!(first, VmResult::Yield) {
            return Err(format!("yield was swallowed, got {first:?}").into());
        }
        match vm.top_registers().and_then(|regs| regs.first().cloned()) {
            Some(Value::Int(1)) => {}
            other => return Err(format!("register after yield is {other:?}").into()),
        }
        let second = run_vm_with_jit(&mut vm, 100, &mut ctx);
        if !matches!(second, VmResult::Complete(Value::Int(2))) {
            return Err(format!("resume after yield finished as {second:?}").into());
        }

        force_compile(
            &mut ctx,
            &chunk,
            TraceKey {
                function: 0,
                entry_pc: 0,
                floats: false,
            },
        )?;
        let mut compiled = Vm::new(chunk, NativeTable::empty(), 0, &[])?;
        let stopped = run_vm_with_jit(&mut compiled, 100, &mut ctx);
        if !matches!(stopped, VmResult::Yield) {
            return Err(format!("compiled prefix ran through yield, got {stopped:?}").into());
        }
        match compiled
            .top_registers()
            .and_then(|regs| regs.first().cloned())
        {
            Some(Value::Int(1)) => Ok(()),
            other => Err(format!("compiled prefix left {other:?}").into()),
        }
    }

    #[test]
    fn budget_end_still_runs_the_rest_of_the_quantum() -> TestResult {
        let mut b = ChunkBuilder::new("jit-budget-step");
        b.begin_function("main", 0, 1);
        b.emit_load_imm(0, 1);
        b.emit_load_imm(0, 2);
        b.emit_load_imm(0, 3);
        b.emit_return(0);
        let chunk = Arc::new(b.finish());
        let mut ctx = JitContext::new(Arc::clone(&chunk))?;
        ctx.hot_threshold = u32::MAX;
        let mut vm = Vm::new(chunk, NativeTable::empty(), 0, &[])?;
        let result = run_vm_with_jit(&mut vm, 2, &mut ctx);
        if !matches!(result, VmResult::Yield) {
            return Err(format!("budget 2 finished as {result:?}").into());
        }
        if vm.instructions_executed() != 2 {
            return Err(format!("ran {} instructions", vm.instructions_executed()).into());
        }
        match vm.top_registers().and_then(|regs| regs.first().cloned()) {
            Some(Value::Int(2)) => Ok(()),
            other => Err(format!("expected the second load, got {other:?}").into()),
        }
    }

    #[test]
    fn compiled_trace_counts_the_instructions_it_ran() -> TestResult {
        let mut b = ChunkBuilder::new("jit-count");
        b.begin_function("main", 0, 1);
        b.emit_load_imm(0, 1);
        b.emit_load_imm(0, 2);
        b.emit_load_imm(0, 3);
        b.emit_return(0);
        let chunk = Arc::new(b.finish());
        let mut ctx = JitContext::new(chunk.clone())?;
        force_compile(
            &mut ctx,
            &chunk,
            TraceKey {
                function: 0,
                entry_pc: 0,
                floats: false,
            },
        )?;
        let mut vm = Vm::new(chunk, NativeTable::empty(), 0, &[])?;
        let result = run_vm_with_jit(&mut vm, 10, &mut ctx);
        assert!(matches!(result, VmResult::Complete(Value::Int(3))));
        assert_eq!(vm.instructions_executed(), 4);
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

                floats: false,
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

            floats: false,
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

            floats: false,
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

                floats: false,
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

                    floats: false,
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

    #[test]
    fn leaf_call_stays_in_the_trace() -> TestResult {
        let mut b = ChunkBuilder::new("jit-call");
        let add1 = b.begin_function("add1", 1, 2);
        b.emit_load_imm(1, 1);
        b.emit_binop(Opcode::Add, 0, 0, 1);
        b.emit_return(0);
        let main = b.begin_function("main", 0, 1);
        b.emit_load_imm(0, 41);
        b.emit_call(0, add1, 1);
        b.emit_return(0);
        let chunk = Arc::new(b.finish());
        let mut ctx = JitContext::new(chunk.clone())?;
        let entry = chunk.functions[main as usize].entry;
        let key = TraceKey {
            function: main,
            entry_pc: entry,

            floats: false,
        };
        force_compile(&mut ctx, &chunk, key)?;
        let trace = ctx
            .cache
            .get(&key)
            .ok_or("trace missing after force_compile")?;
        let mut slots = vec![0i64; 2];
        let exit = run_compiled_trace_ref(trace, &mut slots, entry, 10_000, main, 1).into_reason();
        match exit {
            ExitReason::Return { return_reg: 0 } => {}
            other => return Err(format!("call left the trace: {other:?}").into()),
        }
        if slots[0] != 42 {
            return Err(format!("leaf call produced {}", slots[0]).into());
        }
        Ok(())
    }

    #[test]
    fn leaf_call_keeps_bool_unit_and_a_high_dest() -> TestResult {
        let mut b = ChunkBuilder::new("jit-call-tags");
        let is_true = b.begin_function("is-true", 0, 2);
        b.emit_load_imm(0, 1);
        b.emit_load_imm(1, 1);
        b.emit_binop(Opcode::Eq, 0, 0, 1);
        b.emit_return(0);
        let blank = b.begin_function("blank", 0, 1);
        b.emit_return(0);
        let moved = b.begin_function("moved", 1, 2);
        b.emit_move(1, 0);
        b.emit_return(1);
        let add1 = b.begin_function("add1", 1, 2);
        b.emit_load_imm(1, 1);
        b.emit_binop(Opcode::Add, 0, 0, 1);
        b.emit_return(0);

        let main_bool = b.begin_function("main-bool", 0, 1);
        b.emit_call(0, is_true, 0);
        b.emit_return(0);
        let main_unit = b.begin_function("main-unit", 0, 1);
        b.emit_load_imm(0, 9);
        b.emit_call(0, blank, 0);
        b.emit_return(0);
        let main_moved = b.begin_function("main-moved", 1, 1);
        b.emit_call(0, moved, 1);
        b.emit_return(0);
        let main_high = b.begin_function("main-high", 0, 17);
        b.emit_load_imm(16, 41);
        b.emit_call(16, add1, 1);
        b.emit_return(16);
        let chunk = Arc::new(b.finish());
        let mut ctx = JitContext::new(chunk.clone())?;

        let run = |ctx: &mut JitContext,
                   function: u32,
                   args: &[Value]|
         -> Result<VmResult, Box<dyn std::error::Error>> {
            let entry = chunk.functions[function as usize].entry;
            force_compile(
                ctx,
                &chunk,
                TraceKey {
                    function,
                    entry_pc: entry,

                    floats: false,
                },
            )?;
            let mut vm = Vm::new(chunk.clone(), NativeTable::empty(), function, args)?;
            Ok(run_vm_with_jit(&mut vm, 10_000, ctx))
        };

        match run(&mut ctx, main_bool, &[])? {
            VmResult::Complete(Value::Bool(true)) => {}
            other => return Err(format!("bool leaf finished as {other:?}").into()),
        }
        match run(&mut ctx, main_unit, &[])? {
            VmResult::Complete(Value::Unit) => {}
            other => return Err(format!("unread leaf finished as {other:?}").into()),
        }
        match run(&mut ctx, main_moved, &[Value::Bool(false)])? {
            VmResult::Complete(Value::Bool(false)) => {}
            other => return Err(format!("bool arg finished as {other:?}").into()),
        }
        match run(&mut ctx, main_high, &[])? {
            VmResult::Complete(Value::Int(42)) => {}
            other => return Err(format!("r16 dest finished as {other:?}").into()),
        }
        Ok(())
    }
}
