//! Interpreter / JIT differential. Each program runs on `Vm::run` and on
//! `run_vm_with_jit`. A compiled opcode must be present in a successful
//! trace. List, map, record, and scheduler effects stop the trace; the
//! observable `VmResult` still has to match.

use std::sync::Arc;

use crate::bytecode::builder::ChunkBuilder;
use crate::jit::dispatch::{force_compile, run_vm_with_jit};
use crate::jit::error::CompileError;
use crate::jit::trace::{JitContext, TraceKey};
use crate::{Fault, Instruction, NativeTable, Opcode, Value, Vm, VmResult};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Opcodes `emit_instruction` lowers on an integer trace.
const INTEGER_TRACE: &[&str] = &[
    "LoadImm",
    "AddImm",
    "SubImm",
    "MulImm",
    "LtImm",
    "EqImm",
    "LeImm",
    "LoadConst",
    "Move",
    "Nop",
    "Add",
    "Sub",
    "Mul",
    "Div",
    "Mod",
    "Neg",
    "Eq",
    "Lt",
    "Le",
    "BitAnd",
    "BitOr",
    "BitXor",
    "BitNot",
    "Shl",
    "Shr",
    "Jump",
    "Branch",
    "Call",
    "Return",
];

/// Opcodes a float trace lowers. `LoadImm` and the bitwise ops are refused.
const FLOAT_TRACE: &[&str] = &[
    "LoadConst",
    "Move",
    "Add",
    "Sub",
    "Mul",
    "Div",
    "Neg",
    "Eq",
    "Lt",
    "Le",
    "Jump",
    "Branch",
    "Return",
    "Nop",
];

#[test]
fn integer_trace_opcodes_match_the_interpreter() -> TestResult {
    let cases: Vec<(&str, Arc<crate::Chunk>, u32)> = vec![
        ("LoadImm", arc(load_imm()), 0),
        ("AddImm", arc(add_imm()), 0),
        ("SubImm", arc(sub_imm()), 0),
        ("MulImm", arc(mul_imm()), 0),
        ("LtImm", arc(lt_imm()), 0),
        ("EqImm", arc(eq_imm()), 0),
        ("LeImm", arc(le_imm()), 0),
        ("LoadConst", arc(load_const()), 0),
        ("Move", arc(mov()), 0),
        ("Nop", arc(nop()), 0),
        ("Add", arc(binop(Opcode::Add, 20, 22)), 0),
        ("Sub", arc(binop(Opcode::Sub, 50, 8)), 0),
        ("Mul", arc(binop(Opcode::Mul, 6, 7)), 0),
        ("Div", arc(binop(Opcode::Div, 20, 3)), 0),
        ("Mod", arc(binop(Opcode::Mod, 20, 3)), 0),
        ("Neg", arc(neg()), 0),
        ("Eq", arc(binop(Opcode::Eq, 4, 4)), 0),
        ("Lt", arc(binop(Opcode::Lt, 1, 2)), 0),
        ("Le", arc(binop(Opcode::Le, 2, 2)), 0),
        ("BitAnd", arc(bit(Opcode::BitAnd, 0b1100, 0b1010)), 0),
        ("BitOr", arc(bit(Opcode::BitOr, 0b1100, 0b1010)), 0),
        ("BitXor", arc(bit(Opcode::BitXor, 0b1100, 0b1010)), 0),
        ("BitNot", arc(bit_not()), 0),
        ("Shl", arc(bit(Opcode::Shl, 1, 3)), 0),
        ("Shr", arc(bit(Opcode::Shr, -16, 2)), 0),
        ("Jump", arc(jump()), 0),
        ("Branch", arc(branch()), 0),
        ("Call", arc(call()), 1),
        ("Return", arc(ret_selects_register()), 0),
    ];
    if cases.len() != INTEGER_TRACE.len() {
        return Err(format!(
            "integer trace lists {} names and {} programs",
            INTEGER_TRACE.len(),
            cases.len()
        )
        .into());
    }
    for (name, listed) in INTEGER_TRACE.iter().zip(cases.iter().map(|(n, _, _)| n)) {
        if name != listed {
            return Err(format!("opcode list {name} != program {listed}").into());
        }
    }
    for (name, chunk, function) in cases {
        match_paths(name, &chunk, function, false)?;
        let key = TraceKey {
            function,
            entry_pc: chunk.functions[function as usize].entry,
            floats: false,
        };
        if !trace_contains(&chunk, key, name)? {
            return Err(format!("{name} was not inside the compiled integer trace").into());
        }
    }
    Ok(())
}

/// Both successors of `Branch` are effects, so neither is inside the trace.
/// A truthy register must fall through. An unconditional exit to the branch
/// target would return the other list.
#[test]
fn branch_with_both_successors_outside_the_trace_matches() -> TestResult {
    for (name, cond) in [("truthy", 1i32), ("falsy", 0)] {
        let chunk = Arc::new(branch_both_sides_are_effects(cond));
        match_paths(name, &chunk, 0, false)?;
    }
    Ok(())
}

#[test]
fn shifts_match_across_the_mask_boundary() -> TestResult {
    let amounts = [0i64, 1, 31, 32, 63, 64, 65, 127, -1, i64::MIN, i64::MAX];
    let values = [0i64, 1, -1, 31, i64::MIN, i64::MAX];
    for op in [Opcode::Shl, Opcode::Shr] {
        for &value in &values {
            for &amount in &amounts {
                let chunk = Arc::new(shift(op, value, amount));
                match_paths("shift", &chunk, 0, false)?;
            }
        }
    }
    Ok(())
}

#[test]
fn div_and_mod_by_zero_trap_on_both_paths() -> TestResult {
    for op in [Opcode::Div, Opcode::Mod] {
        let chunk = Arc::new(binop(op, 1, 0));
        let left = interpret(&chunk, 0)?;
        let right = jit(&chunk, 0, false)?;
        match (&left, &right) {
            (VmResult::Trap(Fault::DivideByZero), VmResult::Trap(Fault::DivideByZero)) => {}
            _ => {
                return Err(format!("{op:?} zero: interpreter {left:?} jit {right:?}").into());
            }
        }
    }
    Ok(())
}

#[test]
fn float_trace_opcodes_match_the_interpreter() -> TestResult {
    let cases = [
        ("LoadConst", float_const()),
        ("Move", float_move()),
        ("Add", float_binop(Opcode::Add)),
        ("Sub", float_binop(Opcode::Sub)),
        ("Mul", float_binop(Opcode::Mul)),
        ("Div", float_binop(Opcode::Div)),
        ("Neg", float_neg()),
        ("Eq", float_binop(Opcode::Eq)),
        ("Lt", float_binop(Opcode::Lt)),
        ("Le", float_binop(Opcode::Le)),
        ("Jump", float_jump()),
        ("Branch", float_branch()),
        ("Return", float_return()),
        ("Nop", float_nop()),
    ];
    if cases.len() != FLOAT_TRACE.len() {
        return Err("float trace list and programs differ".into());
    }
    for (name, chunk) in cases {
        let chunk = Arc::new(chunk);
        let mut plain = Vm::new(Arc::clone(&chunk), NativeTable::empty(), 0, &[])?;
        let left = plain.run(100);
        let mut ctx = JitContext::new(Arc::clone(&chunk))?;
        ctx.hot_threshold = 1;
        let mut vm = Vm::new(Arc::clone(&chunk), NativeTable::empty(), 0, &[])?;
        let right = run_vm_with_jit(&mut vm, 100, &mut ctx);
        if !same_result(&left, &right) {
            return Err(format!("{name}: interpreter {left:?} jit {right:?}").into());
        }
        let Some(pc) = opcode_pc(&chunk, name) else {
            return Err(format!("{name} is missing from its program").into());
        };
        if !ctx.cache.float_trace_covers(pc) {
            return Err(format!("{name} at pc {pc} was not inside a float trace").into());
        }
    }
    let inherited = Arc::new(float_branch_inherited());
    let mut plain = Vm::new(Arc::clone(&inherited), NativeTable::empty(), 0, &[])?;
    let left = plain.run(100);
    let mut ctx = JitContext::new(Arc::clone(&inherited))?;
    ctx.hot_threshold = 1;
    let mut vm = Vm::new(Arc::clone(&inherited), NativeTable::empty(), 0, &[])?;
    let right = run_vm_with_jit(&mut vm, 100, &mut ctx);
    if !same_result(&left, &right) {
        return Err(format!("inherited float branch: interpreter {left:?} jit {right:?}").into());
    }
    Ok(())
}

#[test]
fn collections_and_effects_match_after_the_trace_stops() -> TestResult {
    for (name, chunk) in [
        ("list", list_of_ints()),
        ("map", map_of_ints()),
        ("record", record_in_list()),
        ("str", const_value(Value::str("héllo"))),
        ("bytes", const_value(Value::bytes([0u8, 255]))),
        ("yield", yield_only()),
    ] {
        let chunk = Arc::new(chunk);
        observe(name, &chunk)?;
    }
    let pack = Arc::new(record_in_list());
    let mut ctx = JitContext::new(Arc::clone(&pack))?;
    force_compile(
        &mut ctx,
        &pack,
        TraceKey {
            function: 0,
            entry_pc: 0,
            floats: false,
        },
    )?;
    let trace = ctx
        .cache
        .get(&TraceKey {
            function: 0,
            entry_pc: 0,
            floats: false,
        })
        .ok_or("record trace missing")?;
    let pack_pc = pack
        .code
        .iter()
        .position(|instr| instr.op == Opcode::RecordPack)
        .ok_or("missing RecordPack")?;
    if trace.span.range.contains(&(pack_pc as u32)) {
        return Err("RecordPack was compiled into the trace".into());
    }
    Ok(())
}

#[test]
fn a_trace_that_starts_on_an_effect_is_empty() -> TestResult {
    for chunk in [
        list_pack_first(),
        map_pack_first(),
        record_pack_first(),
        yield_only(),
    ] {
        let chunk = Arc::new(chunk);
        let mut ctx = JitContext::new(Arc::clone(&chunk))?;
        let err = force_compile(
            &mut ctx,
            &chunk,
            TraceKey {
                function: 0,
                entry_pc: 0,
                floats: false,
            },
        );
        if !matches!(err, Err(CompileError::EmptyTrace { .. })) {
            return Err(format!("expected an empty trace, got {err:?}").into());
        }
        observe("effect-first", &chunk)?;
    }
    Ok(())
}

#[test]
fn list_get_and_len_match_after_the_trace_stops() -> TestResult {
    deopts("list-get", list_get_at(0), Opcode::ListGet)?;
    deopts("list-get-tail", list_get_at(1), Opcode::ListGet)?;
    deopts("list-get-oob", list_get_at(5), Opcode::ListGet)?;
    deopts("list-get-neg", list_get_at(-1), Opcode::ListGet)?;
    deopts("list-len", list_len_of_two(), Opcode::ListLen)?;
    deopts(
        "list-len-type",
        len_of_int(Opcode::ListLen),
        Opcode::ListLen,
    )?;
    Ok(())
}

/// Run both paths. An empty trace is a miss: the interpreter executes the
/// effect. Any other compile failure is a broken program.
fn observe(name: &str, chunk: &Arc<crate::Chunk>) -> TestResult {
    let left = interpret(chunk, 0)?;
    let mut ctx = JitContext::new(Arc::clone(chunk))?;
    match force_compile(
        &mut ctx,
        chunk,
        TraceKey {
            function: 0,
            entry_pc: 0,
            floats: false,
        },
    ) {
        Ok(()) => {}
        Err(CompileError::EmptyTrace { .. }) => {}
        Err(err) => return Err(format!("{name}: {err}").into()),
    }
    let mut vm = Vm::new(Arc::clone(chunk), NativeTable::empty(), 0, &[])?;
    let right = run_vm_with_jit(&mut vm, 10_000, &mut ctx);
    if same_result(&left, &right) {
        Ok(())
    } else {
        Err(format!("{name}: interpreter {left:?} jit {right:?}").into())
    }
}

/// The named opcode stops the trace. Both paths still return the same
/// `VmResult`, and a compiled prefix does not contain that instruction.
fn deopts(name: &str, chunk: crate::Chunk, op: Opcode) -> TestResult {
    let chunk = Arc::new(chunk);
    observe(name, &chunk)?;
    let key = TraceKey {
        function: 0,
        entry_pc: 0,
        floats: false,
    };
    let mut ctx = JitContext::new(Arc::clone(&chunk))?;
    match force_compile(&mut ctx, &chunk, key) {
        Ok(()) => {}
        Err(CompileError::EmptyTrace { .. }) => return Ok(()),
        Err(err) => return Err(format!("{name}: {err}").into()),
    }
    let Some(pc) = chunk.code.iter().position(|instr| instr.op == op) else {
        return Err(format!("{name}: missing {op:?}").into());
    };
    let inside = ctx
        .cache
        .get(&key)
        .is_some_and(|trace| trace.span.range.contains(&(pc as u32)));
    if inside {
        Err(format!("{name}: {op:?} was compiled into the trace").into())
    } else {
        Ok(())
    }
}

fn match_paths(name: &str, chunk: &Arc<crate::Chunk>, function: u32, floats: bool) -> TestResult {
    let left = interpret(chunk, function)?;
    let right = jit(chunk, function, floats)?;
    if same_result(&left, &right) {
        Ok(())
    } else {
        Err(format!("{name}: interpreter {left:?} jit {right:?}").into())
    }
}

fn interpret(chunk: &Arc<crate::Chunk>, function: u32) -> Result<VmResult, Fault> {
    let mut vm = Vm::new(Arc::clone(chunk), NativeTable::empty(), function, &[])?;
    Ok(vm.run(10_000))
}

fn jit(
    chunk: &Arc<crate::Chunk>,
    function: u32,
    floats: bool,
) -> Result<VmResult, Box<dyn std::error::Error>> {
    let mut ctx = JitContext::new(Arc::clone(chunk))?;
    let entry = chunk.functions[function as usize].entry;
    force_compile(
        &mut ctx,
        chunk,
        TraceKey {
            function,
            entry_pc: entry,
            floats,
        },
    )?;
    let mut vm = Vm::new(Arc::clone(chunk), NativeTable::empty(), function, &[])?;
    Ok(run_vm_with_jit(&mut vm, 10_000, &mut ctx))
}

fn trace_contains(
    chunk: &crate::Chunk,
    key: TraceKey,
    name: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    let Some(pc) = opcode_pc(chunk, name) else {
        return Ok(false);
    };
    let mut ctx = JitContext::new(Arc::new(chunk.clone()))?;
    force_compile(&mut ctx, chunk, key)?;
    Ok(ctx
        .cache
        .get(&key)
        .is_some_and(|trace| trace.span.range.contains(&pc)))
}

fn opcode_pc(chunk: &crate::Chunk, name: &str) -> Option<u32> {
    chunk
        .code
        .iter()
        .enumerate()
        .rev()
        .find(|(_, instr)| format!("{:?}", instr.op) == name)
        .map(|(pc, _)| pc as u32)
}

fn same_result(left: &VmResult, right: &VmResult) -> bool {
    match (left, right) {
        (VmResult::Complete(a), VmResult::Complete(b)) => same_value(a, b),
        (VmResult::Trap(a), VmResult::Trap(b)) => a == b,
        (VmResult::Yield, VmResult::Yield) => true,
        _ => false,
    }
}

fn same_value(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
        (Value::List(a), Value::List(b)) | (Value::Record(a), Value::Record(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| same_value(x, y))
        }
        (Value::Map(a), Value::Map(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b.iter())
                    .all(|((ak, av), (bk, bv))| ak == bk && same_value(av, bv))
        }
        _ => left == right,
    }
}

fn arc(chunk: crate::Chunk) -> Arc<crate::Chunk> {
    Arc::new(chunk)
}

fn main(name: &str, regs: u8, body: impl FnOnce(&mut ChunkBuilder)) -> crate::Chunk {
    let mut b = ChunkBuilder::new(name);
    b.begin_function("main", 0, regs);
    body(&mut b);
    b.finish()
}

fn load_imm() -> crate::Chunk {
    main("imm", 1, |b| {
        b.emit_load_imm(0, 41);
        b.emit_return(0);
    })
}

fn add_imm() -> crate::Chunk {
    main("addimm", 1, |b| {
        b.emit_load_imm(0, 40);
        b.emit_add_imm(0, 0, 2);
        b.emit_return(0);
    })
}

fn sub_imm() -> crate::Chunk {
    main("subimm", 1, |b| {
        b.emit_load_imm(0, 10);
        b.emit_sub_imm(0, 0, 3);
        b.emit_return(0);
    })
}

fn mul_imm() -> crate::Chunk {
    main("mulimm", 1, |b| {
        b.emit_load_imm(0, 6);
        b.emit_mul_imm(0, 0, 7);
        b.emit_return(0);
    })
}

fn lt_imm() -> crate::Chunk {
    main("ltimm", 2, |b| {
        b.emit_load_imm(0, 1);
        b.emit_lt_imm(1, 0, 1);
        b.emit_return(1);
    })
}

fn eq_imm() -> crate::Chunk {
    main("eqimm", 2, |b| {
        b.emit_load_imm(0, 9);
        b.emit_eq_imm(1, 0, 9);
        b.emit_return(1);
    })
}

fn le_imm() -> crate::Chunk {
    main("leimm", 2, |b| {
        b.emit_load_imm(0, 3);
        b.emit_le_imm(1, 0, 3);
        b.emit_return(1);
    })
}

fn load_const() -> crate::Chunk {
    let mut b = ChunkBuilder::new("const");
    let k = b.const_(Value::Int(i64::MAX));
    b.begin_function("main", 0, 1);
    b.emit_load_const(0, k);
    b.emit_return(0);
    b.finish()
}

fn mov() -> crate::Chunk {
    main("mov", 2, |b| {
        b.emit_load_imm(0, 9);
        b.emit_move(1, 0);
        b.emit_return(1);
    })
}

fn nop() -> crate::Chunk {
    let mut chunk = load_imm();
    chunk.code.insert(1, Instruction::nullary(Opcode::Nop));
    chunk
}

fn binop(op: Opcode, lhs: i32, rhs: i32) -> crate::Chunk {
    main("binop", 3, |b| {
        b.emit_load_imm(0, lhs);
        b.emit_load_imm(1, rhs);
        b.emit_binop(op, 2, 0, 1);
        b.emit_return(2);
    })
}

fn neg() -> crate::Chunk {
    let mut b = ChunkBuilder::new("neg");
    let k = b.const_(Value::Int(i64::MIN));
    b.begin_function("main", 0, 2);
    b.emit_load_const(0, k);
    b.emit_neg(1, 0);
    b.emit_return(1);
    b.finish()
}

fn bit(op: Opcode, lhs: i32, rhs: i32) -> crate::Chunk {
    main("bit", 3, |b| {
        b.emit_load_imm(0, lhs);
        b.emit_load_imm(1, rhs);
        b.emit_bit_binop(op, 2, 0, 1);
        b.emit_return(2);
    })
}

fn bit_not() -> crate::Chunk {
    main("not", 2, |b| {
        b.emit_load_imm(0, 0);
        b.emit_bit_not(1, 0);
        b.emit_return(1);
    })
}

fn shift(op: Opcode, value: i64, amount: i64) -> crate::Chunk {
    let mut b = ChunkBuilder::new("shift");
    let left = b.const_(Value::Int(value));
    let right = b.const_(Value::Int(amount));
    b.begin_function("main", 0, 3);
    b.emit_load_const(0, left);
    b.emit_load_const(1, right);
    b.emit_bit_binop(op, 2, 0, 1);
    b.emit_return(2);
    b.finish()
}

fn branch_both_sides_are_effects(cond: i32) -> crate::Chunk {
    main("branch-exit", 4, |b| {
        let taken = b.new_label();
        b.emit_load_imm(0, cond);
        b.emit_load_imm(2, 4);
        b.emit_branch(0, taken);
        b.emit_list_pack(1, 0, 0);
        b.emit_return(1);
        b.bind_label(taken);
        b.emit_list_pack(1, 2, 1);
        b.emit_return(1);
    })
}

fn jump() -> crate::Chunk {
    main("jump", 1, |b| {
        let done = b.new_label();
        b.emit_load_imm(0, 1);
        b.emit_jump(done);
        b.emit_load_imm(0, 2);
        b.bind_label(done);
        b.emit_return(0);
    })
}

fn branch() -> crate::Chunk {
    main("branch", 2, |b| {
        let taken = b.new_label();
        b.emit_load_imm(0, 0);
        b.emit_branch(0, taken);
        b.emit_load_imm(1, 2);
        b.emit_return(1);
        b.bind_label(taken);
        b.emit_load_imm(1, 9);
        b.emit_return(1);
    })
}

fn call() -> crate::Chunk {
    let mut b = ChunkBuilder::new("call");
    let child = b.begin_function("child", 0, 1);
    b.emit_load_imm(0, 11);
    b.emit_return(0);
    b.begin_function("main", 0, 1);
    b.emit_call(0, child, 0);
    b.emit_return(0);
    b.finish()
}

fn ret_selects_register() -> crate::Chunk {
    main("ret", 2, |b| {
        b.emit_load_imm(0, 3);
        b.emit_load_imm(1, 4);
        b.emit_return(1);
    })
}

fn float_pair(name: &str, op: impl FnOnce(&mut ChunkBuilder, u32, u32)) -> crate::Chunk {
    let mut b = ChunkBuilder::new(name);
    let left = b.const_(Value::Float(1.5));
    let right = b.const_(Value::Float(0.5));
    b.begin_function("main", 0, 3);
    b.emit_load_const(0, left);
    b.emit_load_const(1, right);
    op(&mut b, left, right);
    b.finish()
}

fn float_const() -> crate::Chunk {
    let mut b = ChunkBuilder::new("fconst");
    let prime = b.const_(Value::Float(1.0));
    let nan = b.const_(Value::Float(f64::from_bits(0x7ff8_0000_0000_0001)));
    b.begin_function("main", 0, 2);
    b.emit_load_const(1, prime);
    b.emit_load_const(0, nan);
    b.emit_return(0);
    b.finish()
}

fn float_move() -> crate::Chunk {
    float_pair("fmov", |b, _, _| {
        b.emit_move(2, 0);
        b.emit_return(2);
    })
}

fn float_binop(op: Opcode) -> crate::Chunk {
    float_pair("fbin", |b, _, _| {
        b.emit_binop(op, 2, 0, 1);
        b.emit_return(2);
    })
}

fn float_neg() -> crate::Chunk {
    let mut b = ChunkBuilder::new("fneg");
    let zero = b.const_(Value::Float(-0.0));
    b.begin_function("main", 0, 2);
    b.emit_load_const(0, zero);
    b.emit_neg(1, 0);
    b.emit_return(1);
    b.finish()
}

fn float_jump() -> crate::Chunk {
    float_pair("fjump", |b, _, _| {
        let done = b.new_label();
        b.emit_jump(done);
        b.emit_load_const(0, 1);
        b.bind_label(done);
        b.emit_return(0);
    })
}

fn float_branch() -> crate::Chunk {
    let mut b = ChunkBuilder::new("fbranch");
    let prime = b.const_(Value::Float(1.0));
    let zero = b.const_(Value::Float(0.0));
    let one = b.const_(Value::Float(1.0));
    b.begin_function("main", 0, 3);
    b.emit_load_const(2, prime);
    b.emit_load_const(0, zero);
    let taken = b.new_label();
    b.emit_branch(0, taken);
    b.emit_load_const(1, one);
    b.emit_return(1);
    b.bind_label(taken);
    b.emit_load_const(1, zero);
    b.emit_return(1);
    b.finish()
}

/// `0.0` is already in `r0` when the trace starts, so the tag is inherited.
fn float_branch_inherited() -> crate::Chunk {
    let mut b = ChunkBuilder::new("fbranch-in");
    let zero = b.const_(Value::Float(0.0));
    let one = b.const_(Value::Float(1.0));
    b.begin_function("main", 0, 2);
    b.emit_load_const(0, zero);
    let taken = b.new_label();
    b.emit_branch(0, taken);
    b.emit_load_const(1, one);
    b.emit_return(1);
    b.bind_label(taken);
    b.emit_load_const(1, zero);
    b.emit_return(1);
    b.finish()
}

fn float_return() -> crate::Chunk {
    float_pair("fret", |b, _, _| {
        b.emit_return(1);
    })
}

fn float_nop() -> crate::Chunk {
    let mut chunk = float_const();
    chunk.code.insert(1, Instruction::nullary(Opcode::Nop));
    chunk
}

fn list_of_ints() -> crate::Chunk {
    main("list", 4, |b| {
        b.emit_load_imm(0, 1);
        b.emit_load_imm(1, 2);
        b.emit_list_pack(2, 0, 2);
        b.emit_return(2);
    })
}

fn map_of_ints() -> crate::Chunk {
    main("map", 4, |b| {
        b.emit_load_imm(0, 1);
        b.emit_load_imm(1, 2);
        b.emit_map_pack(2, 0, 1);
        b.emit_return(2);
    })
}

fn record_in_list() -> crate::Chunk {
    main("nested", 5, |b| {
        b.emit_load_imm(0, 1);
        b.emit_load_imm(1, 2);
        b.emit_record_pack(2, 0, 2);
        b.emit_list_pack(3, 2, 1);
        b.emit_return(3);
    })
}

fn const_value(value: Value) -> crate::Chunk {
    let mut b = ChunkBuilder::new("val");
    let k = b.const_(value);
    b.begin_function("main", 0, 1);
    b.emit_load_const(0, k);
    b.emit_return(0);
    b.finish()
}

fn yield_only() -> crate::Chunk {
    main("yield", 1, |b| {
        b.emit_yield();
        b.emit_return(0);
    })
}

fn list_get_at(index: i32) -> crate::Chunk {
    main("lget", 4, |b| {
        b.emit_load_imm(0, 10);
        b.emit_load_imm(1, 20);
        b.emit_list_pack(2, 0, 2);
        b.emit_list_get(3, 2, index);
        b.emit_return(3);
    })
}

fn list_len_of_two() -> crate::Chunk {
    main("llen", 4, |b| {
        b.emit_load_imm(0, 10);
        b.emit_load_imm(1, 20);
        b.emit_list_pack(2, 0, 2);
        b.emit_list_len(3, 2);
        b.emit_return(3);
    })
}

fn len_of_int(op: Opcode) -> crate::Chunk {
    main("len-ty", 2, |b| {
        b.emit_load_imm(0, 1);
        match op {
            Opcode::ListLen => b.emit_list_len(1, 0),
            Opcode::MapLen => b.emit_map_len(1, 0),
            Opcode::RecordLen => b.emit_record_len(1, 0),
            _ => b.emit_list_len(1, 0),
        }
        b.emit_return(1);
    })
}

fn list_pack_first() -> crate::Chunk {
    main("list0", 1, |b| {
        b.emit_list_pack(0, 0, 0);
        b.emit_return(0);
    })
}

fn map_pack_first() -> crate::Chunk {
    main("map0", 1, |b| {
        b.emit_map_pack(0, 0, 0);
        b.emit_return(0);
    })
}

fn record_pack_first() -> crate::Chunk {
    main("rec0", 1, |b| {
        b.emit_record_pack(0, 0, 0);
        b.emit_return(0);
    })
}
