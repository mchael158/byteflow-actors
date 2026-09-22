#![allow(unsafe_code)]

use std::collections::{HashMap, HashSet, VecDeque};

use cranelift_codegen::ir::types;
use cranelift_codegen::ir::{
    AbiParam, Block, Function, InstBuilder, MachMemFlags, Signature, Value as ClifValue,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{Linkage, Module};

use crate::{Chunk, Instruction, Opcode, Value};

use super::error::CompileError;
use super::exit::{JIT_BUDGET, JIT_CONTINUE, JIT_RETURN, JIT_TRAP};
use super::frame::{
    JitEntry, MAX_JIT_CALL_DEPTH, OFF_BUDGET, OFF_CALL_DEPTH, OFF_CALL_STACK, OFF_EXIT_KIND,
    OFF_FUNCTION, OFF_PC, OFF_REGISTER_COUNT, OFF_RETURN_REG,
};
use super::trace::{CompiledTrace, TraceKey, TraceSpan, MAX_TRACE_LENGTH};

/// Maps bytecode register indices to Cranelift SSA values for the current trace.
pub struct RegisterMap {
    values: Vec<Option<ClifValue>>,
}

impl RegisterMap {
    pub fn new(registers: usize) -> Self {
        Self {
            values: vec![None; registers],
        }
    }

    pub fn get(&self, reg: usize) -> Option<ClifValue> {
        self.values.get(reg).copied().flatten()
    }

    pub fn set(&mut self, reg: usize, value: ClifValue) {
        if let Some(slot) = self.values.get_mut(reg) {
            *slot = Some(value);
        }
    }

    pub fn clear(&mut self) {
        for slot in &mut self.values {
            *slot = None;
        }
    }
}

pub struct TraceCompiler<'a> {
    module: &'a mut cranelift_jit::JITModule,
}

impl<'a> TraceCompiler<'a> {
    pub fn new(module: &'a mut cranelift_jit::JITModule) -> Self {
        Self { module }
    }

    pub fn compile_trace(
        &mut self,
        chunk: &Chunk,
        key: TraceKey,
    ) -> Result<CompiledTrace, CompileError> {
        let def = chunk
            .function(key.function)
            .ok_or(CompileError::EmptyTrace {
                function: key.function,
                pc: key.entry_pc,
            })?;
        let region = collect_trace_region(chunk, key)?;
        if region.blocks.is_empty() {
            return Err(CompileError::EmptyTrace {
                function: key.function,
                pc: key.entry_pc,
            });
        }

        let end_pc = region_max_pc(&region, key)?;
        let span = TraceSpan {
            function: key.function,
            range: key.entry_pc..end_pc.saturating_add(1),
        };

        let target = self.module.target_config();
        let pointer_type = target.pointer_type();
        let mut sig = Signature::new(target.default_call_conv);
        sig.params.push(AbiParam::new(pointer_type));

        let func_id = self
            .module
            .declare_function(
                &format!("trace_{}_{}", key.function, key.entry_pc),
                Linkage::Local,
                &sig,
            )
            .map_err(|e| CompileError::Module(e.to_string()))?;

        let mut ctx = cranelift_codegen::Context::new();
        ctx.func = Function::with_name_signature(
            cranelift_codegen::ir::UserFuncName::user(0, func_id.as_u32()),
            sig,
        );

        let mut func_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut func_ctx);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);

        let frame_ptr = builder.block_params(entry)[0];
        let slots_ptr = builder
            .ins()
            .load(pointer_type, MachMemFlags::new(), frame_ptr, 0);
        let budget_ptr = field_ptr(&mut builder, frame_ptr, pointer_type, OFF_BUDGET);
        let call_depth_ptr = field_ptr(&mut builder, frame_ptr, pointer_type, OFF_CALL_DEPTH);
        let call_stack_ptr = {
            let addr = field_ptr(&mut builder, frame_ptr, pointer_type, OFF_CALL_STACK);
            builder
                .ins()
                .load(pointer_type, MachMemFlags::new(), addr, 0)
        };

        let supports_calls = region.blocks.iter().any(|block| {
            block
                .pcs
                .iter()
                .any(|pc| chunk.code[*pc as usize].op == Opcode::Call)
        });

        let trap_block = builder.create_block();
        let return_block = builder.create_block();

        let mut pc_to_block = HashMap::new();
        for block in &region.blocks {
            pc_to_block.insert(block.start_pc, builder.create_block());
        }

        let entry_clif = *pc_to_block
            .get(&key.entry_pc)
            .ok_or(CompileError::EmptyTrace {
                function: key.function,
                pc: key.entry_pc,
            })?;
        builder.ins().jump(entry_clif, &[]);

        for block in &region.blocks {
            let clif_block = pc_to_block[&block.start_pc];
            builder.switch_to_block(clif_block);
            let mut registers = RegisterMap::new(def.num_registers as usize);

            for pc in &block.pcs {
                emit_budget_tick(&mut builder, budget_ptr, frame_ptr, pointer_type, *pc);
                let instr = chunk.code[*pc as usize];
                let mut emit_ctx = EmitContext {
                    builder: &mut builder,
                    chunk,
                    registers: &mut registers,
                    slots_ptr,
                    frame_ptr,
                    pointer_type,
                    trap_block,
                    return_block,
                    pc_to_block: &pc_to_block,
                    call_depth_ptr,
                    call_stack_ptr,
                    supports_calls,
                };
                emit_instruction(&mut emit_ctx, *pc, instr)?;
            }
        }

        builder.switch_to_block(trap_block);
        builder.seal_block(trap_block);
        let trap_pc = region_max_pc(&region, key)?;
        write_exit(&mut builder, frame_ptr, pointer_type, JIT_TRAP, trap_pc, 0);

        builder.switch_to_block(return_block);
        builder.seal_block(return_block);
        let (last_pc, return_reg) = match last_return_in_region(chunk, &region) {
            Some(pair) => pair,
            // Side-exit-only traces never reach this block; metadata is unused.
            None => (key.entry_pc, 0),
        };
        write_exit(
            &mut builder,
            frame_ptr,
            pointer_type,
            JIT_RETURN,
            last_pc,
            return_reg,
        );

        builder.seal_all_blocks();
        builder.finalize(self.module.target_config());

        self.module
            .define_function(func_id, &mut ctx)
            .map_err(|e| CompileError::Module(e.to_string()))?;
        self.module.clear_context(&mut ctx);
        self.module
            .finalize_definitions()
            .map_err(|e| CompileError::Module(e.to_string()))?;

        let code = self.module.get_finalized_function(func_id);
        let entry: JitEntry = unsafe { std::mem::transmute(code) };

        Ok(CompiledTrace { span, entry })
    }
}

struct TraceBlock {
    start_pc: u32,
    pcs: Vec<u32>,
}

struct TraceRegion {
    blocks: Vec<TraceBlock>,
}

fn region_max_pc(region: &TraceRegion, key: TraceKey) -> Result<u32, CompileError> {
    region
        .blocks
        .iter()
        .flat_map(|b| b.pcs.iter())
        .copied()
        .max()
        .ok_or(CompileError::EmptyTrace {
            function: key.function,
            pc: key.entry_pc,
        })
}

fn last_return_in_region(chunk: &Chunk, region: &TraceRegion) -> Option<(u32, u32)> {
    region
        .blocks
        .iter()
        .flat_map(|b| b.pcs.iter().map(|pc| (*pc, chunk.code[*pc as usize])))
        .rev()
        .find(|(_, instr)| instr.op == Opcode::Return)
        .map(|(pc, instr)| (pc, u32::from(instr.a)))
}

fn jump_target(jpc: u32, imm: i32) -> u32 {
    (i64::from(jpc) + 1 + i64::from(imm)) as u32
}

fn branch_target(bpc: u32, imm: i32) -> u32 {
    jump_target(bpc, imm)
}

struct EmitContext<'a, 'b> {
    builder: &'a mut FunctionBuilder<'b>,
    chunk: &'a Chunk,
    registers: &'a mut RegisterMap,
    slots_ptr: ClifValue,
    frame_ptr: ClifValue,
    pointer_type: types::Type,
    trap_block: Block,
    return_block: Block,
    pc_to_block: &'a HashMap<u32, Block>,
    call_depth_ptr: ClifValue,
    call_stack_ptr: ClifValue,
    supports_calls: bool,
}

fn emit_instruction(
    ctx: &mut EmitContext<'_, '_>,
    pc: u32,
    instr: Instruction,
) -> Result<(), CompileError> {
    let builder = &mut *ctx.builder;
    let chunk = ctx.chunk;
    let registers = &mut *ctx.registers;
    let slots_ptr = ctx.slots_ptr;
    let frame_ptr = ctx.frame_ptr;
    let pointer_type = ctx.pointer_type;
    let trap_block = ctx.trap_block;
    let return_block = ctx.return_block;
    let supports_calls = ctx.supports_calls;
    let pc_to_block = ctx.pc_to_block;
    let call_depth_ptr = ctx.call_depth_ptr;
    let call_stack_ptr = ctx.call_stack_ptr;
    match instr.op {
        Opcode::LoadImm => {
            let v = builder.ins().iconst(types::I64, i64::from(instr.imm));
            registers.set(instr.a as usize, v);
        }
        Opcode::LoadConst => {
            let idx = instr.imm as u32;
            let konst = chunk.constant(idx).ok_or(CompileError::UnsupportedOpcode {
                opcode: Opcode::LoadConst,
                pc,
            })?;
            let v = match konst {
                Value::Int(i) => builder.ins().iconst(types::I64, *i),
                Value::Bool(b) => builder.ins().iconst(types::I64, i64::from(*b)),
                _ => {
                    return Err(CompileError::UnsupportedOpcode {
                        opcode: Opcode::LoadConst,
                        pc,
                    });
                }
            };
            registers.set(instr.a as usize, v);
        }
        Opcode::Move => {
            let v = load_reg(builder, registers, slots_ptr, pointer_type, instr.b, pc)?;
            registers.set(instr.a as usize, v);
        }
        Opcode::Add | Opcode::Sub | Opcode::Mul | Opcode::Eq | Opcode::Lt | Opcode::Le => {
            let lhs = load_reg(builder, registers, slots_ptr, pointer_type, instr.b, pc)?;
            let rhs = load_reg(builder, registers, slots_ptr, pointer_type, instr.c, pc)?;
            let result = match instr.op {
                Opcode::Add => builder.ins().iadd(lhs, rhs),
                Opcode::Sub => builder.ins().isub(lhs, rhs),
                Opcode::Mul => builder.ins().imul(lhs, rhs),
                Opcode::Eq => {
                    let cmp = builder.ins().icmp(
                        cranelift_codegen::ir::condcodes::IntCC::Equal,
                        lhs,
                        rhs,
                    );
                    bool_as_i64(builder, cmp)
                }
                Opcode::Lt => {
                    let cmp = builder.ins().icmp(
                        cranelift_codegen::ir::condcodes::IntCC::SignedLessThan,
                        lhs,
                        rhs,
                    );
                    bool_as_i64(builder, cmp)
                }
                Opcode::Le => {
                    let cmp = builder.ins().icmp(
                        cranelift_codegen::ir::condcodes::IntCC::SignedLessThanOrEqual,
                        lhs,
                        rhs,
                    );
                    bool_as_i64(builder, cmp)
                }
                _ => unreachable!(),
            };
            registers.set(instr.a as usize, result);
        }
        Opcode::Div | Opcode::Mod => {
            let lhs = load_reg(builder, registers, slots_ptr, pointer_type, instr.b, pc)?;
            let rhs = load_reg(builder, registers, slots_ptr, pointer_type, instr.c, pc)?;
            flush_registers(builder, registers, slots_ptr, pointer_type);
            let zero = builder.ins().iconst(types::I64, 0);
            let is_zero =
                builder
                    .ins()
                    .icmp(cranelift_codegen::ir::condcodes::IntCC::Equal, rhs, zero);
            let ok = builder.create_block();
            builder.ins().brif(is_zero, trap_block, &[], ok, &[]);
            builder.switch_to_block(ok);
            builder.seal_block(ok);
            let result = if instr.op == Opcode::Div {
                builder.ins().sdiv(lhs, rhs)
            } else {
                builder.ins().srem(lhs, rhs)
            };
            registers.set(instr.a as usize, result);
        }
        Opcode::Neg => {
            let v = load_reg(builder, registers, slots_ptr, pointer_type, instr.b, pc)?;
            let zero = builder.ins().iconst(types::I64, 0);
            registers.set(instr.a as usize, builder.ins().isub(zero, v));
        }
        Opcode::Jump => {
            flush_registers(builder, registers, slots_ptr, pointer_type);
            let target_pc = jump_target(pc, instr.imm);
            branch_to_pc(builder, frame_ptr, pointer_type, pc_to_block, target_pc);
        }
        Opcode::Branch => {
            let cond = load_reg(builder, registers, slots_ptr, pointer_type, instr.a, pc)?;
            flush_registers(builder, registers, slots_ptr, pointer_type);
            let zero = builder.ins().iconst(types::I64, 0);
            let is_falsy =
                builder
                    .ins()
                    .icmp(cranelift_codegen::ir::condcodes::IntCC::Equal, cond, zero);
            let fall_pc = pc + 1;
            let falsy_pc = branch_target(pc, instr.imm);
            match (pc_to_block.get(&fall_pc), pc_to_block.get(&falsy_pc)) {
                (Some(fall), Some(falsy)) => {
                    builder.ins().brif(is_falsy, *falsy, &[], *fall, &[]);
                }
                (Some(fall), None) => {
                    let side = builder.create_block();
                    builder.ins().brif(is_falsy, side, &[], *fall, &[]);
                    builder.switch_to_block(side);
                    builder.seal_block(side);
                    write_exit(builder, frame_ptr, pointer_type, JIT_CONTINUE, falsy_pc, 0);
                }
                (None, Some(falsy)) => {
                    let side = builder.create_block();
                    builder.ins().brif(is_falsy, *falsy, &[], side, &[]);
                    builder.switch_to_block(side);
                    builder.seal_block(side);
                    write_exit(builder, frame_ptr, pointer_type, JIT_CONTINUE, fall_pc, 0);
                }
                (None, None) => {
                    write_exit(builder, frame_ptr, pointer_type, JIT_CONTINUE, falsy_pc, 0);
                }
            }
        }
        Opcode::Return => {
            let v = load_reg(builder, registers, slots_ptr, pointer_type, instr.a, pc)?;
            store_slot(builder, slots_ptr, pointer_type, instr.a, v);
            if supports_calls {
                emit_return(
                    builder,
                    frame_ptr,
                    pointer_type,
                    call_depth_ptr,
                    call_stack_ptr,
                    slots_ptr,
                    registers,
                    instr.a,
                    pc,
                );
            } else {
                write_u32_field(builder, frame_ptr, pointer_type, OFF_PC, pc);
                builder.ins().jump(return_block, &[]);
            }
        }
        Opcode::Nop => {}
        Opcode::Call => {
            emit_call(
                builder,
                chunk,
                registers,
                slots_ptr,
                frame_ptr,
                pointer_type,
                pc_to_block,
                call_depth_ptr,
                call_stack_ptr,
                pc,
                instr,
            )?;
        }
        other => {
            return Err(CompileError::UnsupportedOpcode { opcode: other, pc });
        }
    }
    Ok(())
}

fn branch_to_pc(
    builder: &mut FunctionBuilder<'_>,
    frame_ptr: ClifValue,
    pointer_type: types::Type,
    pc_to_block: &HashMap<u32, Block>,
    target_pc: u32,
) {
    if let Some(block) = pc_to_block.get(&target_pc) {
        builder.ins().jump(*block, &[]);
    } else {
        write_exit(builder, frame_ptr, pointer_type, JIT_CONTINUE, target_pc, 0);
    }
}

fn flush_registers(
    builder: &mut FunctionBuilder<'_>,
    registers: &RegisterMap,
    slots_ptr: ClifValue,
    pointer_type: types::Type,
) {
    for (reg, value) in registers.values.iter().enumerate() {
        if let Some(v) = *value {
            store_slot(builder, slots_ptr, pointer_type, reg as u8, v);
        }
    }
}

fn load_reg(
    builder: &mut FunctionBuilder<'_>,
    registers: &mut RegisterMap,
    slots_ptr: ClifValue,
    pointer_type: types::Type,
    reg: u8,
    _pc: u32,
) -> Result<ClifValue, CompileError> {
    if let Some(v) = registers.get(reg as usize) {
        return Ok(v);
    }
    let offset = builder
        .ins()
        .iconst(pointer_type, i64::from(u32::from(reg) * 8));
    let addr = builder.ins().iadd(slots_ptr, offset);
    let v = builder.ins().load(types::I64, MachMemFlags::new(), addr, 0);
    registers.set(reg as usize, v);
    Ok(v)
}

fn collect_trace_region(chunk: &Chunk, key: TraceKey) -> Result<TraceRegion, CompileError> {
    let mut queue = VecDeque::from([key.entry_pc]);
    let mut seen_starts = HashSet::new();
    let mut blocks = Vec::new();
    let mut total = 0usize;

    while let Some(start) = queue.pop_front() {
        if !seen_starts.insert(start) {
            continue;
        }
        if total >= MAX_TRACE_LENGTH {
            break;
        }

        let mut pcs = Vec::new();
        let mut pc = start;
        while let Some(instr) = chunk.code.get(pc as usize).copied() {
            if is_effect_opcode(instr.op) {
                break;
            }
            if instr.op == Opcode::LoadConst {
                let idx = instr.imm as u32;
                match chunk.constant(idx) {
                    Some(Value::Int(_) | Value::Bool(_)) => {}
                    _ => break,
                }
            }
            pcs.push(pc);
            total += 1;
            if total >= MAX_TRACE_LENGTH {
                break;
            }

            match instr.op {
                Opcode::Return => break,
                Opcode::Jump => {
                    queue.push_back(jump_target(pc, instr.imm));
                    break;
                }
                Opcode::Branch => {
                    queue.push_back(pc + 1);
                    queue.push_back(branch_target(pc, instr.imm));
                    break;
                }
                Opcode::Call => {
                    let callee = instr.imm as u32;
                    if let Some(def) = chunk.function(callee) {
                        queue.push_back(def.entry);
                    }
                    queue.push_back(pc + 1);
                    break;
                }
                _ => pc += 1,
            }
        }

        if !pcs.is_empty() {
            blocks.push(TraceBlock {
                start_pc: start,
                pcs,
            });
        }
    }

    Ok(TraceRegion { blocks })
}

fn is_effect_opcode(op: Opcode) -> bool {
    matches!(
        op,
        Opcode::CallNative
            | Opcode::Spawn
            | Opcode::Yield
            | Opcode::Sleep
            | Opcode::Exit
            | Opcode::SelfPid
            | Opcode::Send
            | Opcode::Receive
            | Opcode::ReceiveTimeout
            | Opcode::ReceiveMatch
            | Opcode::ReceiveMatchImm
            | Opcode::Ask
            | Opcode::AskTimeout
            | Opcode::Monitor
            | Opcode::Demonitor
            | Opcode::Link
            | Opcode::Unlink
            | Opcode::SetTrapExit
            | Opcode::SetRestartPolicy
            | Opcode::Delegate
            | Opcode::FreshRequestId
            | Opcode::ReceiveMatchCorr
            | Opcode::ReceiveMatchCorrImm
            | Opcode::ReceiveMatchKind
            | Opcode::RegisterName
            | Opcode::Whereis
            | Opcode::Trap
            | Opcode::Halt
    )
}

fn field_ptr(
    builder: &mut FunctionBuilder<'_>,
    base: ClifValue,
    pointer_type: types::Type,
    offset: i32,
) -> ClifValue {
    let off = builder.ins().iconst(pointer_type, i64::from(offset));
    builder.ins().iadd(base, off)
}

fn store_slot(
    builder: &mut FunctionBuilder<'_>,
    slots: ClifValue,
    pointer_type: types::Type,
    reg: u8,
    value: ClifValue,
) {
    let offset = builder
        .ins()
        .iconst(pointer_type, i64::from(u32::from(reg) * 8));
    let addr = builder.ins().iadd(slots, offset);
    builder.ins().store(MachMemFlags::new(), value, addr, 0);
}

fn bool_as_i64(builder: &mut FunctionBuilder<'_>, cond: ClifValue) -> ClifValue {
    let one = builder.ins().iconst(types::I64, 1);
    let zero = builder.ins().iconst(types::I64, 0);
    builder.ins().select(cond, one, zero)
}

fn emit_budget_tick(
    builder: &mut FunctionBuilder<'_>,
    budget_ptr: ClifValue,
    frame_ptr: ClifValue,
    pointer_type: types::Type,
    pc: u32,
) {
    let budget = builder
        .ins()
        .load(types::I32, MachMemFlags::new(), budget_ptr, 0);
    let zero = builder.ins().iconst(types::I32, 0);
    let exhausted =
        builder
            .ins()
            .icmp(cranelift_codegen::ir::condcodes::IntCC::Equal, budget, zero);
    let continue_insn = builder.create_block();
    let budget_exit = builder.create_block();
    builder
        .ins()
        .brif(exhausted, budget_exit, &[], continue_insn, &[]);
    builder.switch_to_block(budget_exit);
    builder.seal_block(budget_exit);
    write_exit(builder, frame_ptr, pointer_type, JIT_BUDGET, pc, 0);
    builder.switch_to_block(continue_insn);
    builder.seal_block(continue_insn);
    let one = builder.ins().iconst(types::I32, 1);
    let new_budget = builder.ins().isub(budget, one);
    builder
        .ins()
        .store(MachMemFlags::new(), new_budget, budget_ptr, 0);
}

fn write_u32_field(
    builder: &mut FunctionBuilder<'_>,
    frame_ptr: ClifValue,
    pointer_type: types::Type,
    offset: i32,
    value: u32,
) {
    let addr = field_ptr(builder, frame_ptr, pointer_type, offset);
    let val = builder.ins().iconst(types::I32, i64::from(value));
    builder.ins().store(MachMemFlags::new(), val, addr, 0);
}

fn write_exit(
    builder: &mut FunctionBuilder<'_>,
    frame_ptr: ClifValue,
    pointer_type: types::Type,
    kind: u32,
    pc: u32,
    return_reg: u32,
) {
    write_u32_field(builder, frame_ptr, pointer_type, OFF_EXIT_KIND, kind);
    write_u32_field(builder, frame_ptr, pointer_type, OFF_PC, pc);
    write_u32_field(builder, frame_ptr, pointer_type, OFF_RETURN_REG, return_reg);
    builder.ins().return_(&[]);
}

#[allow(clippy::too_many_arguments)]
fn emit_call(
    builder: &mut FunctionBuilder<'_>,
    chunk: &Chunk,
    registers: &mut RegisterMap,
    slots_ptr: ClifValue,
    frame_ptr: ClifValue,
    pointer_type: types::Type,
    pc_to_block: &HashMap<u32, Block>,
    call_depth_ptr: ClifValue,
    call_stack_ptr: ClifValue,
    pc: u32,
    instr: Instruction,
) -> Result<(), CompileError> {
    let callee = instr.imm as u32;
    let argc = instr.b;
    let dst = instr.a;
    let def = chunk
        .function(callee)
        .ok_or(CompileError::UnsupportedOpcode {
            opcode: Opcode::Call,
            pc,
        })?;
    let callee_entry = def.entry;
    let Some(callee_block) = pc_to_block.get(&callee_entry) else {
        flush_registers(builder, registers, slots_ptr, pointer_type);
        write_exit(builder, frame_ptr, pointer_type, JIT_CONTINUE, pc, 0);
        return Ok(());
    };

    flush_registers(builder, registers, slots_ptr, pointer_type);

    let depth = builder
        .ins()
        .load(types::I32, MachMemFlags::new(), call_depth_ptr, 0);
    let max_depth = builder
        .ins()
        .iconst(types::I32, i64::from(MAX_JIT_CALL_DEPTH as u32));
    let too_deep = builder.ins().icmp(
        cranelift_codegen::ir::condcodes::IntCC::SignedGreaterThanOrEqual,
        depth,
        max_depth,
    );
    let effect_exit = builder.create_block();
    let call_body = builder.create_block();
    builder
        .ins()
        .brif(too_deep, effect_exit, &[], call_body, &[]);
    builder.switch_to_block(effect_exit);
    builder.seal_block(effect_exit);
    write_exit(
        builder,
        frame_ptr,
        pointer_type,
        super::exit::JIT_EFFECT,
        pc,
        0,
    );
    builder.switch_to_block(call_body);
    builder.seal_block(call_body);

    let record_bytes = builder.ins().iconst(
        types::I32,
        i64::from(std::mem::size_of::<super::frame::JitCallRecord>() as u32),
    );
    let record_off = builder.ins().imul(depth, record_bytes);
    let record_off_ptr = builder.ins().uextend(pointer_type, record_off);
    let record_addr = builder.ins().iadd(call_stack_ptr, record_off_ptr);

    let return_pc = builder.ins().iconst(types::I32, i64::from(pc + 1));
    builder
        .ins()
        .store(MachMemFlags::new(), return_pc, record_addr, 0);

    let func_ptr = field_ptr(builder, frame_ptr, pointer_type, OFF_FUNCTION);
    let caller_fn = builder
        .ins()
        .load(types::I32, MachMemFlags::new(), func_ptr, 0);
    let caller_fn_addr = {
        let off = builder.ins().iconst(pointer_type, 4);
        builder.ins().iadd(record_addr, off)
    };
    builder
        .ins()
        .store(MachMemFlags::new(), caller_fn, caller_fn_addr, 0);

    let dest = builder.ins().iconst(types::I32, i64::from(u32::from(dst)));
    let dest_addr = {
        let off = builder.ins().iconst(pointer_type, 8);
        builder.ins().iadd(record_addr, off)
    };
    builder.ins().store(MachMemFlags::new(), dest, dest_addr, 0);

    let reg_ptr = field_ptr(builder, frame_ptr, pointer_type, OFF_REGISTER_COUNT);
    let caller_regs = builder
        .ins()
        .load(types::I32, MachMemFlags::new(), reg_ptr, 0);
    let caller_regs_addr = {
        let off = builder.ins().iconst(pointer_type, 12);
        builder.ins().iadd(record_addr, off)
    };
    builder
        .ins()
        .store(MachMemFlags::new(), caller_regs, caller_regs_addr, 0);

    let one = builder.ins().iconst(types::I32, 1);
    let new_depth = builder.ins().iadd(depth, one);
    builder
        .ins()
        .store(MachMemFlags::new(), new_depth, call_depth_ptr, 0);

    for i in 0..argc {
        let src_reg = u32::from(dst) + u32::from(i);
        let src_offset = builder.ins().iconst(pointer_type, i64::from(src_reg * 8));
        let src_addr = builder.ins().iadd(slots_ptr, src_offset);
        let value = builder
            .ins()
            .load(types::I64, MachMemFlags::new(), src_addr, 0);
        let dst_offset = builder
            .ins()
            .iconst(pointer_type, i64::from(u32::from(i) * 8));
        let dst_addr = builder.ins().iadd(slots_ptr, dst_offset);
        builder.ins().store(MachMemFlags::new(), value, dst_addr, 0);
    }

    let callee_fn = builder.ins().iconst(types::I32, i64::from(callee));
    builder
        .ins()
        .store(MachMemFlags::new(), callee_fn, func_ptr, 0);
    let callee_regs = builder
        .ins()
        .iconst(types::I32, i64::from(def.num_registers));
    builder
        .ins()
        .store(MachMemFlags::new(), callee_regs, reg_ptr, 0);

    registers.clear();
    builder.ins().jump(*callee_block, &[]);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_return(
    builder: &mut FunctionBuilder<'_>,
    frame_ptr: ClifValue,
    pointer_type: types::Type,
    call_depth_ptr: ClifValue,
    call_stack_ptr: ClifValue,
    slots_ptr: ClifValue,
    registers: &mut RegisterMap,
    return_reg: u8,
    pc: u32,
) {
    let depth = builder
        .ins()
        .load(types::I32, MachMemFlags::new(), call_depth_ptr, 0);
    let zero = builder.ins().iconst(types::I32, 0);
    let is_outer = builder
        .ins()
        .icmp(cranelift_codegen::ir::condcodes::IntCC::Equal, depth, zero);
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

    let one = builder.ins().iconst(types::I32, 1);
    let new_depth = builder.ins().isub(depth, one);
    builder
        .ins()
        .store(MachMemFlags::new(), new_depth, call_depth_ptr, 0);

    let record_bytes = builder.ins().iconst(
        types::I32,
        i64::from(std::mem::size_of::<super::frame::JitCallRecord>() as u32),
    );
    let record_off = builder.ins().imul(new_depth, record_bytes);
    let record_off_ptr = builder.ins().uextend(pointer_type, record_off);
    let record_addr = builder.ins().iadd(call_stack_ptr, record_off_ptr);

    let return_pc = builder
        .ins()
        .load(types::I32, MachMemFlags::new(), record_addr, 0);
    let return_fn_addr = {
        let off = builder.ins().iconst(pointer_type, 4);
        builder.ins().iadd(record_addr, off)
    };
    let return_fn = builder
        .ins()
        .load(types::I32, MachMemFlags::new(), return_fn_addr, 0);
    let dest_addr = {
        let off = builder.ins().iconst(pointer_type, 8);
        builder.ins().iadd(record_addr, off)
    };
    let dest_reg = builder
        .ins()
        .load(types::I32, MachMemFlags::new(), dest_addr, 0);
    let caller_regs_addr = {
        let off = builder.ins().iconst(pointer_type, 12);
        builder.ins().iadd(record_addr, off)
    };
    let caller_regs = builder
        .ins()
        .load(types::I32, MachMemFlags::new(), caller_regs_addr, 0);

    let ret_offset = builder
        .ins()
        .iconst(pointer_type, i64::from(u32::from(return_reg) * 8));
    let ret_addr = builder.ins().iadd(slots_ptr, ret_offset);
    let ret_val = builder
        .ins()
        .load(types::I64, MachMemFlags::new(), ret_addr, 0);
    let eight = builder.ins().iconst(types::I32, 8);
    let dest_offset = builder.ins().imul(dest_reg, eight);
    let dest_offset_ptr = builder.ins().uextend(pointer_type, dest_offset);
    let dest_slot = builder.ins().iadd(slots_ptr, dest_offset_ptr);
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

    registers.clear();
    write_dynamic_exit(builder, frame_ptr, pointer_type, JIT_CONTINUE, return_pc, 0);
    let _ = pc;
}

fn write_dynamic_exit(
    builder: &mut FunctionBuilder<'_>,
    frame_ptr: ClifValue,
    pointer_type: types::Type,
    kind: u32,
    pc: ClifValue,
    return_reg: u32,
) {
    write_u32_field(builder, frame_ptr, pointer_type, OFF_EXIT_KIND, kind);
    let pc_ptr = field_ptr(builder, frame_ptr, pointer_type, OFF_PC);
    builder.ins().store(MachMemFlags::new(), pc, pc_ptr, 0);
    write_u32_field(builder, frame_ptr, pointer_type, OFF_RETURN_REG, return_reg);
    builder.ins().return_(&[]);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{bytecode::builder::ChunkBuilder, NativeTable, Opcode, Vm};

    use super::*;
    use crate::jit::dispatch::force_compile;
    use crate::jit::trace::{JitContext, TraceKey};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn compiles_load_imm_add_return() -> TestResult {
        let mut b = ChunkBuilder::new("jit");
        b.begin_function("main", 0, 3);
        b.emit_load_imm(0, 41);
        b.emit_load_imm(1, 1);
        b.emit_binop(Opcode::Add, 2, 0, 1);
        b.emit_return(2);
        let chunk = Arc::new(b.finish());

        let mut ctx = JitContext::new(chunk.clone())?;
        let key = TraceKey {
            function: 0,
            entry_pc: 0,
        };
        let mut compiler = TraceCompiler::new(ctx.module_mut());
        let trace = compiler.compile_trace(&chunk, key)?;
        assert!(trace.span.range.contains(&0));
        Ok(())
    }

    #[test]
    fn compiles_branch_falsy_taken() -> TestResult {
        let mut b = ChunkBuilder::new("jit-branch");
        b.begin_function("main", 0, 1);
        let done = b.new_label();
        let ret = b.new_label();
        b.emit_load_imm(0, 0);
        b.emit_branch(0, done);
        b.emit_load_imm(0, 99);
        b.emit_jump(ret);
        b.bind_label(done);
        b.emit_load_imm(0, 7);
        b.bind_label(ret);
        b.emit_return(0);
        let chunk = Arc::new(b.finish());

        let mut ctx = JitContext::new(chunk.clone())?;
        let key = TraceKey {
            function: 0,
            entry_pc: 0,
        };
        let mut compiler = TraceCompiler::new(ctx.module_mut());
        let trace = compiler.compile_trace(&chunk, key)?;
        let mut slots = vec![0i64; 1];
        let ret =
            super::super::dispatch::run_compiled_trace_ref(&trace, &mut slots, 0, 10_000, 0, 1);
        assert_eq!(slots[0], 7);
        assert!(matches!(
            ret.into_reason(),
            super::super::exit::ExitReason::Return { .. }
        ));
        Ok(())
    }

    #[test]
    fn compiles_unconditional_jump() -> TestResult {
        let mut b = ChunkBuilder::new("jit-jump");
        b.begin_function("main", 0, 1);
        let skip = b.new_label();
        let ret = b.new_label();
        b.emit_load_imm(0, 1);
        b.emit_jump(skip);
        b.emit_load_imm(0, 99);
        b.bind_label(skip);
        b.emit_load_imm(0, 41);
        b.bind_label(ret);
        b.emit_return(0);
        let chunk = Arc::new(b.finish());

        let mut ctx = JitContext::new(chunk.clone())?;
        let key = TraceKey {
            function: 0,
            entry_pc: 0,
        };
        let mut compiler = TraceCompiler::new(ctx.module_mut());
        let trace = compiler.compile_trace(&chunk, key)?;
        let mut slots = vec![0i64; 1];
        let ret =
            super::super::dispatch::run_compiled_trace_ref(&trace, &mut slots, 0, 10_000, 0, 1);
        assert!(matches!(
            ret.into_reason(),
            super::super::exit::ExitReason::Return { return_reg: 0 }
        ));
        assert_eq!(slots[0], 41);
        Ok(())
    }

    #[test]
    fn compiles_intra_chunk_call() -> TestResult {
        let mut b = ChunkBuilder::new("jit-call");
        b.begin_function("double", 0, 1);
        b.emit_load_imm(0, 21);
        b.emit_return(0);
        b.begin_function("main", 1, 1);
        b.emit_load_imm(0, 0);
        b.emit_call(0, 0, 1);
        b.emit_return(0);
        let chunk = Arc::new(b.finish());

        let mut ctx = JitContext::new(chunk.clone())?;
        let key = TraceKey {
            function: 1,
            entry_pc: chunk.functions[1].entry,
        };
        force_compile(&mut ctx, &chunk, key)?;
        let mut vm = Vm::new(chunk, NativeTable::empty(), key.function, &[])?;
        let result = super::super::dispatch::run_vm_with_jit(&mut vm, 10_000, &mut ctx);
        assert!(matches!(
            result,
            crate::VmResult::Complete(crate::Value::Int(21))
        ));
        Ok(())
    }
}
