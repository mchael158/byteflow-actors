//! In-trace `Opcode::Call` for a straight-line scalar leaf.
//!
//! The slot file is one array. A leaf call spills the caller window, runs
//! the callee in that same array, then restores the caller and writes the
//! result into the destination register. The `Call` itself does not return
//! to the interpreter.
//!
//! Anything that is not a short leaf (branch, nested effect, heap constant)
//! still exits at the `Call` so the interpreter pushes a real frame. Budget
//! exits inside the leaf are not emitted: the call site charges the leaf
//! length up front and refuses to enter when the slice cannot finish it.

use std::collections::HashMap;

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{self, I32, I64};
use cranelift_codegen::ir::{Block, BlockArg, InstBuilder, MachMemFlags, Value as ClifValue};
use cranelift_frontend::FunctionBuilder;

use crate::{Chunk, Instruction, Opcode, Value};

use super::compiler::{
    field_ptr, flush_registers, inherited_bit, read_reg_bit, set_dynamic_reg_bit, set_reg_bit,
    set_reg_bit_value, write_dynamic_exit, write_exit, write_u32_field, RegisterMap,
};
use super::exit::{JIT_CONTINUE, JIT_RETURN};
use super::frame::{
    JitCallRecord, MAX_JIT_CALL_DEPTH, OFF_AS_BOOL, OFF_AS_UNIT, OFF_BUDGET, OFF_ENTRY_BOOL,
    OFF_ENTRY_UNIT, OFF_FUNCTION, OFF_REGISTER_COUNT, OFF_SPILL, OFF_SPILL_STRIDE, OFF_WRITTEN,
    REC_AS_BOOL, REC_AS_UNIT, REC_CALLER_REGS, REC_DEST, REC_RETURN_FN, REC_RETURN_PC, REC_WRITTEN,
};

const MAX_LEAF_INSNS: usize = 32;

pub(super) struct LeafBody {
    pub entry: u32,
    pub num_registers: u8,
    pub arity: u8,
    pub pcs: Vec<u32>,
}

pub(super) fn leaf_body(chunk: &Chunk, function: u32) -> Option<LeafBody> {
    let def = chunk.function(function)?;
    let end = next_function_entry(chunk, def.entry);
    let mut pc = def.entry;
    let mut pcs = Vec::new();
    loop {
        if pcs.len() >= MAX_LEAF_INSNS {
            return None;
        }
        if pc >= end {
            return None;
        }
        let instr = chunk.code.get(pc as usize)?;
        pcs.push(pc);
        match instr.op {
            Opcode::Return => {
                return Some(LeafBody {
                    entry: def.entry,
                    num_registers: def.num_registers,
                    arity: def.arity,
                    pcs,
                });
            }
            Opcode::LoadImm
            | Opcode::Move
            | Opcode::AddImm
            | Opcode::Add
            | Opcode::Sub
            | Opcode::Mul
            | Opcode::Div
            | Opcode::Mod
            | Opcode::Neg
            | Opcode::Eq
            | Opcode::Lt
            | Opcode::Le
            | Opcode::Nop => pc += 1,
            Opcode::LoadConst => match chunk.constant(instr.imm as u32) {
                Some(Value::Int(_) | Value::Bool(_)) => pc += 1,
                _ => return None,
            },
            _ => return None,
        }
    }
}

fn next_function_entry(chunk: &Chunk, entry: u32) -> u32 {
    let mut end = u32::MAX;
    for def in &chunk.functions {
        if def.entry > entry && def.entry < end {
            end = def.entry;
        }
    }
    end
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_call(
    builder: &mut FunctionBuilder<'_>,
    chunk: &Chunk,
    registers: &mut RegisterMap,
    slots_ptr: ClifValue,
    frame_ptr: ClifValue,
    pointer_type: types::Type,
    pc_to_block: &HashMap<u32, Block>,
    call_depth_ptr: ClifValue,
    call_stack_ptr: ClifValue,
    trap_block: Block,
    pc: u32,
    instr: Instruction,
) -> Result<(), super::error::CompileError> {
    flush_registers(builder, registers, slots_ptr, frame_ptr, pointer_type);
    let Some(leaf) = leaf_body(chunk, instr.imm as u32) else {
        write_exit(builder, frame_ptr, pointer_type, JIT_CONTINUE, pc, 0);
        return Ok(());
    };
    if instr.b != leaf.arity {
        write_exit(builder, frame_ptr, pointer_type, JIT_CONTINUE, pc, 0);
        return Ok(());
    }
    let Some(callee) = pc_to_block.get(&leaf.entry).copied() else {
        write_exit(builder, frame_ptr, pointer_type, JIT_CONTINUE, pc, 0);
        return Ok(());
    };

    let budget_ptr = field_ptr(builder, frame_ptr, pointer_type, OFF_BUDGET);
    let budget = builder.ins().load(I32, MachMemFlags::new(), budget_ptr, 0);
    let need = builder.ins().iconst(I32, leaf.pcs.len() as i64);
    let short = builder.ins().icmp(IntCC::UnsignedLessThan, budget, need);
    let proceed = builder.create_block();
    let leave = builder.create_block();
    builder.ins().brif(short, leave, &[], proceed, &[]);
    builder.switch_to_block(leave);
    builder.seal_block(leave);
    write_exit(builder, frame_ptr, pointer_type, JIT_CONTINUE, pc, 0);
    builder.switch_to_block(proceed);
    builder.seal_block(proceed);

    let new_budget = builder.ins().isub(budget, need);
    builder
        .ins()
        .store(MachMemFlags::new(), new_budget, budget_ptr, 0);

    let depth = builder
        .ins()
        .load(I32, MachMemFlags::new(), call_depth_ptr, 0);
    let max_depth = builder.ins().iconst(I32, MAX_JIT_CALL_DEPTH as i64);
    let overflow = builder
        .ins()
        .icmp(IntCC::UnsignedGreaterThanOrEqual, depth, max_depth);
    let enter = builder.create_block();
    builder.ins().brif(overflow, trap_block, &[], enter, &[]);
    builder.switch_to_block(enter);
    builder.seal_block(enter);

    let spill_ptr = {
        let addr = field_ptr(builder, frame_ptr, pointer_type, OFF_SPILL);
        builder
            .ins()
            .load(pointer_type, MachMemFlags::new(), addr, 0)
    };
    let stride = {
        let addr = field_ptr(builder, frame_ptr, pointer_type, OFF_SPILL_STRIDE);
        builder.ins().load(I32, MachMemFlags::new(), addr, 0)
    };
    let caller_regs_addr = field_ptr(builder, frame_ptr, pointer_type, OFF_REGISTER_COUNT);
    let caller_regs = builder
        .ins()
        .load(I32, MachMemFlags::new(), caller_regs_addr, 0);
    let window = spill_window(builder, pointer_type, spill_ptr, stride, depth);
    emit_copy_window(builder, pointer_type, slots_ptr, window, caller_regs);

    let record = record_addr(builder, pointer_type, call_stack_ptr, depth);
    let return_pc = builder.ins().iconst(I32, i64::from(pc.saturating_add(1)));
    store_u32(builder, pointer_type, record, REC_RETURN_PC, return_pc);
    let function_addr = field_ptr(builder, frame_ptr, pointer_type, OFF_FUNCTION);
    let function = builder
        .ins()
        .load(I32, MachMemFlags::new(), function_addr, 0);
    store_u32(builder, pointer_type, record, REC_RETURN_FN, function);
    let dest = builder.ins().iconst(I32, i64::from(u32::from(instr.a)));
    store_u32(builder, pointer_type, record, REC_DEST, dest);
    store_u32(builder, pointer_type, record, REC_CALLER_REGS, caller_regs);
    copy_tag_words(
        builder,
        pointer_type,
        frame_ptr,
        OFF_WRITTEN,
        record,
        REC_WRITTEN,
    );
    copy_tag_words(
        builder,
        pointer_type,
        frame_ptr,
        OFF_AS_BOOL,
        record,
        REC_AS_BOOL,
    );
    copy_tag_words(
        builder,
        pointer_type,
        frame_ptr,
        OFF_AS_UNIT,
        record,
        REC_AS_UNIT,
    );

    let mut arg_tags = Vec::with_capacity(usize::from(instr.b));
    for i in 0..instr.b {
        let Some(src) = instr.a.checked_add(i) else {
            break;
        };
        arg_tags.push((
            inherited_bit(
                builder,
                frame_ptr,
                pointer_type,
                src,
                OFF_AS_BOOL,
                OFF_ENTRY_BOOL,
            ),
            inherited_bit(
                builder,
                frame_ptr,
                pointer_type,
                src,
                OFF_AS_UNIT,
                OFF_ENTRY_UNIT,
            ),
        ));
    }

    clear_tag_words(builder, frame_ptr, pointer_type);
    zero_slots(builder, pointer_type, slots_ptr, leaf.num_registers);
    copy_args_from_spill(builder, pointer_type, window, slots_ptr, instr.a, instr.b);
    // A fresh interpreter frame is `Unit`. An unread leaf register must stay
    // that way; `zero_slots` only writes the integer 0.
    for reg in 0..leaf.num_registers {
        set_reg_bit(builder, frame_ptr, pointer_type, OFF_WRITTEN, reg, true);
        set_reg_bit(builder, frame_ptr, pointer_type, OFF_AS_BOOL, reg, false);
        set_reg_bit(builder, frame_ptr, pointer_type, OFF_AS_UNIT, reg, true);
    }
    for (reg, (as_bool, as_unit)) in arg_tags.into_iter().enumerate() {
        let Ok(reg) = u8::try_from(reg) else {
            break;
        };
        if reg >= leaf.num_registers {
            break;
        }
        set_reg_bit_value(builder, frame_ptr, pointer_type, OFF_AS_BOOL, reg, as_bool);
        set_reg_bit_value(builder, frame_ptr, pointer_type, OFF_AS_UNIT, reg, as_unit);
    }

    write_u32_field(
        builder,
        frame_ptr,
        pointer_type,
        OFF_REGISTER_COUNT,
        u32::from(leaf.num_registers),
    );
    write_u32_field(
        builder,
        frame_ptr,
        pointer_type,
        OFF_FUNCTION,
        instr.imm as u32,
    );
    let one = builder.ins().iconst(I32, 1);
    let next_depth = builder.ins().iadd(depth, one);
    builder
        .ins()
        .store(MachMemFlags::new(), next_depth, call_depth_ptr, 0);

    registers.clear();
    builder.ins().jump(callee, &[]);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_return(
    builder: &mut FunctionBuilder<'_>,
    frame_ptr: ClifValue,
    pointer_type: types::Type,
    call_depth_ptr: ClifValue,
    call_stack_ptr: ClifValue,
    slots_ptr: ClifValue,
    registers: &mut RegisterMap,
    continuations: &HashMap<u32, Block>,
    return_reg: u8,
    pc: u32,
) {
    let depth = builder
        .ins()
        .load(I32, MachMemFlags::new(), call_depth_ptr, 0);
    let zero = builder.ins().iconst(I32, 0);
    let is_outer = builder.ins().icmp(IntCC::Equal, depth, zero);
    let outer = builder.create_block();
    let inner = builder.create_block();
    builder.ins().brif(is_outer, outer, &[], inner, &[]);
    builder.switch_to_block(outer);
    builder.seal_block(outer);
    write_exit(
        builder,
        frame_ptr,
        pointer_type,
        JIT_RETURN,
        pc,
        u32::from(return_reg),
    );
    builder.switch_to_block(inner);
    builder.seal_block(inner);

    let ret_addr = slot_addr(builder, pointer_type, slots_ptr, u32::from(return_reg));
    let ret_val = builder.ins().load(I64, MachMemFlags::new(), ret_addr, 0);
    let is_bool = read_reg_bit(builder, frame_ptr, pointer_type, OFF_AS_BOOL, return_reg);
    let is_unit = read_reg_bit(builder, frame_ptr, pointer_type, OFF_AS_UNIT, return_reg);

    let one = builder.ins().iconst(I32, 1);
    let new_depth = builder.ins().isub(depth, one);
    builder
        .ins()
        .store(MachMemFlags::new(), new_depth, call_depth_ptr, 0);

    let record = record_addr(builder, pointer_type, call_stack_ptr, new_depth);
    let return_pc = load_u32(builder, pointer_type, record, REC_RETURN_PC);
    let return_fn = load_u32(builder, pointer_type, record, REC_RETURN_FN);
    let dest_reg = load_u32(builder, pointer_type, record, REC_DEST);
    let caller_regs = load_u32(builder, pointer_type, record, REC_CALLER_REGS);

    let spill_ptr = {
        let addr = field_ptr(builder, frame_ptr, pointer_type, OFF_SPILL);
        builder
            .ins()
            .load(pointer_type, MachMemFlags::new(), addr, 0)
    };
    let stride = {
        let addr = field_ptr(builder, frame_ptr, pointer_type, OFF_SPILL_STRIDE);
        builder.ins().load(I32, MachMemFlags::new(), addr, 0)
    };
    let window = spill_window(builder, pointer_type, spill_ptr, stride, new_depth);
    emit_copy_window(builder, pointer_type, window, slots_ptr, caller_regs);

    let eight = builder.ins().iconst(pointer_type, 8);
    let dest_index = builder.ins().uextend(pointer_type, dest_reg);
    let dest_off = builder.ins().imul(dest_index, eight);
    let dest_slot = builder.ins().iadd(slots_ptr, dest_off);
    builder
        .ins()
        .store(MachMemFlags::new(), ret_val, dest_slot, 0);

    let func_ptr = field_ptr(builder, frame_ptr, pointer_type, OFF_FUNCTION);
    builder
        .ins()
        .store(MachMemFlags::new(), return_fn, func_ptr, 0);
    let reg_ptr = field_ptr(builder, frame_ptr, pointer_type, OFF_REGISTER_COUNT);
    builder
        .ins()
        .store(MachMemFlags::new(), caller_regs, reg_ptr, 0);

    copy_tag_words_from(
        builder,
        pointer_type,
        record,
        REC_WRITTEN,
        frame_ptr,
        OFF_WRITTEN,
    );
    copy_tag_words_from(
        builder,
        pointer_type,
        record,
        REC_AS_BOOL,
        frame_ptr,
        OFF_AS_BOOL,
    );
    copy_tag_words_from(
        builder,
        pointer_type,
        record,
        REC_AS_UNIT,
        frame_ptr,
        OFF_AS_UNIT,
    );

    // `dest_reg` is the register stored at the call site (`instr.a`), which
    // may be any of r0..=r255. Update that bit after the caller tags return.
    mark_known_dests(builder, frame_ptr, pointer_type, dest_reg, is_bool, is_unit);

    registers.clear();
    let mut open = builder.create_block();
    builder.ins().jump(open, &[]);
    for (cont_pc, cont_block) in continuations {
        builder.switch_to_block(open);
        builder.seal_block(open);
        let next = builder.create_block();
        let expect = builder.ins().iconst(I32, i64::from(*cont_pc));
        let eq = builder.ins().icmp(IntCC::Equal, return_pc, expect);
        builder.ins().brif(eq, *cont_block, &[], next, &[]);
        open = next;
    }
    builder.switch_to_block(open);
    builder.seal_block(open);
    write_dynamic_exit(builder, frame_ptr, pointer_type, JIT_CONTINUE, return_pc, 0);
    let _ = pc;
}

fn mark_known_dests(
    builder: &mut FunctionBuilder<'_>,
    frame_ptr: ClifValue,
    pointer_type: types::Type,
    dest_reg: ClifValue,
    is_bool: ClifValue,
    is_unit: ClifValue,
) {
    let one = builder.ins().iconst(I64, 1);
    set_dynamic_reg_bit(builder, frame_ptr, pointer_type, OFF_WRITTEN, dest_reg, one);
    set_dynamic_reg_bit(
        builder,
        frame_ptr,
        pointer_type,
        OFF_AS_BOOL,
        dest_reg,
        is_bool,
    );
    set_dynamic_reg_bit(
        builder,
        frame_ptr,
        pointer_type,
        OFF_AS_UNIT,
        dest_reg,
        is_unit,
    );
}

fn spill_window(
    builder: &mut FunctionBuilder<'_>,
    pointer_type: types::Type,
    spill_ptr: ClifValue,
    stride: ClifValue,
    depth: ClifValue,
) -> ClifValue {
    let depth_p = builder.ins().uextend(pointer_type, depth);
    let stride_p = builder.ins().uextend(pointer_type, stride);
    let index = builder.ins().imul(depth_p, stride_p);
    let eight = builder.ins().iconst(pointer_type, 8);
    let bytes = builder.ins().imul(index, eight);
    builder.ins().iadd(spill_ptr, bytes)
}

fn record_addr(
    builder: &mut FunctionBuilder<'_>,
    pointer_type: types::Type,
    call_stack_ptr: ClifValue,
    depth: ClifValue,
) -> ClifValue {
    let size = builder
        .ins()
        .iconst(pointer_type, std::mem::size_of::<JitCallRecord>() as i64);
    let depth_p = builder.ins().uextend(pointer_type, depth);
    let bytes = builder.ins().imul(depth_p, size);
    builder.ins().iadd(call_stack_ptr, bytes)
}

fn slot_addr(
    builder: &mut FunctionBuilder<'_>,
    pointer_type: types::Type,
    slots_ptr: ClifValue,
    index: u32,
) -> ClifValue {
    let off = builder.ins().iconst(pointer_type, i64::from(index) * 8);
    builder.ins().iadd(slots_ptr, off)
}

fn store_u32(
    builder: &mut FunctionBuilder<'_>,
    pointer_type: types::Type,
    base: ClifValue,
    offset: i32,
    value: ClifValue,
) {
    let addr = field_ptr(builder, base, pointer_type, offset);
    builder.ins().store(MachMemFlags::new(), value, addr, 0);
}

fn load_u32(
    builder: &mut FunctionBuilder<'_>,
    pointer_type: types::Type,
    base: ClifValue,
    offset: i32,
) -> ClifValue {
    let addr = field_ptr(builder, base, pointer_type, offset);
    builder.ins().load(I32, MachMemFlags::new(), addr, 0)
}

fn copy_tag_words(
    builder: &mut FunctionBuilder<'_>,
    pointer_type: types::Type,
    frame_ptr: ClifValue,
    frame_base: i32,
    record: ClifValue,
    record_base: i32,
) {
    copy_tag_words_from(
        builder,
        pointer_type,
        frame_ptr,
        frame_base,
        record,
        record_base,
    );
}

fn copy_tag_words_from(
    builder: &mut FunctionBuilder<'_>,
    pointer_type: types::Type,
    src_base_ptr: ClifValue,
    src_off: i32,
    dst_base_ptr: ClifValue,
    dst_off: i32,
) {
    for word in 0..4 {
        let src = field_ptr(builder, src_base_ptr, pointer_type, src_off + word * 8);
        let dst = field_ptr(builder, dst_base_ptr, pointer_type, dst_off + word * 8);
        let value = builder.ins().load(I64, MachMemFlags::new(), src, 0);
        builder.ins().store(MachMemFlags::new(), value, dst, 0);
    }
}

fn clear_tag_words(
    builder: &mut FunctionBuilder<'_>,
    frame_ptr: ClifValue,
    pointer_type: types::Type,
) {
    let zero = builder.ins().iconst(I64, 0);
    for base in [OFF_WRITTEN, OFF_AS_BOOL, OFF_AS_UNIT] {
        for word in 0..4 {
            let addr = field_ptr(builder, frame_ptr, pointer_type, base + word * 8);
            builder.ins().store(MachMemFlags::new(), zero, addr, 0);
        }
    }
}

fn zero_slots(
    builder: &mut FunctionBuilder<'_>,
    pointer_type: types::Type,
    slots_ptr: ClifValue,
    count: u8,
) {
    let zero = builder.ins().iconst(I64, 0);
    for index in 0..count {
        let addr = slot_addr(builder, pointer_type, slots_ptr, u32::from(index));
        builder.ins().store(MachMemFlags::new(), zero, addr, 0);
    }
}

fn copy_args_from_spill(
    builder: &mut FunctionBuilder<'_>,
    pointer_type: types::Type,
    window: ClifValue,
    slots_ptr: ClifValue,
    base: u8,
    argc: u8,
) {
    for index in 0..argc {
        let src = slot_addr(
            builder,
            pointer_type,
            window,
            u32::from(base.saturating_add(index)),
        );
        let dst = slot_addr(builder, pointer_type, slots_ptr, u32::from(index));
        let value = builder.ins().load(I64, MachMemFlags::new(), src, 0);
        builder.ins().store(MachMemFlags::new(), value, dst, 0);
    }
}

fn emit_copy_window(
    builder: &mut FunctionBuilder<'_>,
    pointer_type: types::Type,
    src: ClifValue,
    dst: ClifValue,
    count: ClifValue,
) {
    let header = builder.create_block();
    builder.append_block_param(header, I32);
    let body = builder.create_block();
    let done = builder.create_block();
    let zero = builder.ins().iconst(I32, 0);
    builder.ins().jump(header, &[BlockArg::Value(zero)]);
    builder.switch_to_block(header);
    let index = builder.block_params(header)[0];
    let finished = builder
        .ins()
        .icmp(IntCC::UnsignedGreaterThanOrEqual, index, count);
    builder.ins().brif(finished, done, &[], body, &[]);
    builder.switch_to_block(body);
    builder.seal_block(body);
    let idx_p = builder.ins().uextend(pointer_type, index);
    let eight = builder.ins().iconst(pointer_type, 8);
    let off = builder.ins().imul(idx_p, eight);
    let src_addr = builder.ins().iadd(src, off);
    let dst_addr = builder.ins().iadd(dst, off);
    let value = builder.ins().load(I64, MachMemFlags::new(), src_addr, 0);
    builder.ins().store(MachMemFlags::new(), value, dst_addr, 0);
    let one = builder.ins().iconst(I32, 1);
    let next = builder.ins().iadd(index, one);
    builder.ins().jump(header, &[BlockArg::Value(next)]);
    builder.seal_block(header);
    builder.switch_to_block(done);
}
