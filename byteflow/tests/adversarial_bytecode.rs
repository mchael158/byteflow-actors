//! Hostile bytecode and wire inputs: verify/decode must fail closed, never panic.

use std::sync::Arc;

use byteflow::{
    decode, decode_with, encode, verify, verify_with, CapId, Chunk, Fault, FunctionDef,
    Instruction, NativeTable, Opcode, Program, TrustLevel, Value, VerifyConfig, VerifyError, Vm,
    VmResult, MAX_LIST_LEN, MAX_MAP_LEN,
};

fn minimal_main() -> Chunk {
    let mut p = Program::new("minimal");
    p.function("main", 0, |f| {
        let z = f.load_i32(0);
        f.return_(z);
    });
    p.build()
}

#[test]
fn truncated_modules_never_panic_on_decode() {
    let bytes = encode(&minimal_main());
    for len in 0..bytes.len() {
        let _ = decode(&bytes[..len]);
        let _ = decode_with(&bytes[..len], TrustLevel::Untrusted);
    }
}

#[test]
fn verify_rejects_jump_out_of_range() {
    let chunk = Chunk {
        name: "bad-jump".into(),
        constants: vec![],
        code: vec![
            Instruction::only_imm(Opcode::Jump, 999),
            Instruction::nullary(Opcode::Halt),
        ],
        functions: vec![FunctionDef {
            name: "main".into(),
            entry: 0,
            arity: 0,
            num_registers: 1,
        }],
    };
    match verify(&chunk) {
        Err(VerifyError::JumpOutOfRange { .. }) => {}
        other => panic!("expected JumpOutOfRange, got {other:?}"),
    }
}

#[test]
fn verify_rejects_const_index_out_of_range() {
    let chunk = Chunk {
        name: "bad-const".into(),
        constants: vec![],
        code: vec![Instruction::a_imm(Opcode::LoadConst, 0, 42)],
        functions: vec![FunctionDef {
            name: "main".into(),
            entry: 0,
            arity: 0,
            num_registers: 1,
        }],
    };
    match verify(&chunk) {
        Err(VerifyError::ConstOutOfRange { .. }) => {}
        other => panic!("expected ConstOutOfRange, got {other:?}"),
    }
}

#[test]
fn verify_rejects_call_to_missing_function() {
    let chunk = Chunk {
        name: "bad-call".into(),
        constants: vec![],
        code: vec![
            Instruction::new(Opcode::Call, 0, 0, 0, 99),
            Instruction::nullary(Opcode::Halt),
        ],
        functions: vec![FunctionDef {
            name: "main".into(),
            entry: 0,
            arity: 0,
            num_registers: 1,
        }],
    };
    match verify(&chunk) {
        Err(VerifyError::FunctionOutOfRange { .. }) => {}
        other => panic!("expected FunctionOutOfRange, got {other:?}"),
    }
}

#[test]
fn verify_rejects_arity_above_register_file() {
    let chunk = Chunk {
        name: "bad-arity".into(),
        constants: vec![],
        code: vec![Instruction::nullary(Opcode::Halt)],
        functions: vec![FunctionDef {
            name: "main".into(),
            entry: 0,
            arity: 4,
            num_registers: 2,
        }],
    };
    match verify(&chunk) {
        Err(VerifyError::ArityExceedsRegisters { .. }) => {}
        other => panic!("expected ArityExceedsRegisters, got {other:?}"),
    }
}

#[test]
fn verify_rejects_destination_register_past_frame() {
    let chunk = Chunk {
        name: "bad-reg".into(),
        constants: vec![],
        code: vec![Instruction::new(Opcode::StartChild, 5, 0, 0, 0)],
        functions: vec![FunctionDef {
            name: "main".into(),
            entry: 0,
            arity: 0,
            num_registers: 2,
        }],
    };
    match verify(&chunk) {
        Err(VerifyError::BadImmediate { .. }) => {}
        other => panic!("expected BadImmediate for reg, got {other:?}"),
    }
}

#[test]
fn verify_rejects_untrusted_cap_constant() -> Result<(), String> {
    let cap = CapId::random().map_err(|e| e.to_string())?;
    let mut chunk = minimal_main();
    chunk.constants.push(Value::Cap(cap));
    chunk.code = vec![
        Instruction::a_imm(Opcode::LoadConst, 0, 0),
        Instruction::abc(Opcode::Return, 0, 0, 0),
    ];
    match verify_with(
        &chunk,
        VerifyConfig {
            trust: TrustLevel::Untrusted,
        },
    ) {
        Err(VerifyError::ForbiddenConstant { .. }) => {}
        other => return Err(format!("expected ForbiddenConstant, got {other:?}")),
    }
    Ok(())
}

#[test]
fn verify_rejects_list_pack_beyond_limits() {
    let mut chunk = minimal_main();
    chunk.code = vec![
        Instruction::new(Opcode::ListPack, 0, 0, 0, MAX_LIST_LEN as i32 + 1),
        Instruction::abc(Opcode::Return, 0, 0, 0),
    ];
    match verify(&chunk) {
        Err(VerifyError::BadImmediate { .. }) => {}
        other => panic!("expected BadImmediate for list pack, got {other:?}"),
    }
}

#[test]
fn verify_rejects_map_pack_beyond_limits() {
    let mut chunk = minimal_main();
    chunk.code = vec![
        Instruction::new(Opcode::MapPack, 0, 0, 0, MAX_MAP_LEN as i32 + 1),
        Instruction::abc(Opcode::Return, 0, 0, 0),
    ];
    match verify(&chunk) {
        Err(VerifyError::BadImmediate { .. }) => {}
        other => panic!("expected BadImmediate for map pack, got {other:?}"),
    }
}

#[test]
fn vm_traps_on_unverified_bad_register_without_panicking() -> Result<(), String> {
    let chunk = Arc::new(Chunk {
        name: "run-bad-reg".into(),
        constants: vec![],
        code: vec![
            Instruction::a_imm(Opcode::LoadImm, 0, 1),
            Instruction::abc(Opcode::Move, 5, 0, 0),
            Instruction::abc(Opcode::Return, 5, 0, 0),
        ],
        functions: vec![FunctionDef {
            name: "main".into(),
            entry: 0,
            arity: 0,
            num_registers: 2,
        }],
    });
    let mut vm = Vm::new(chunk, NativeTable::empty(), 0, &[]).map_err(|e| e.to_string())?;
    match vm.run(u32::MAX) {
        VmResult::Trap(Fault::RegisterOutOfRange { .. }) => Ok(()),
        other => Err(format!("expected RegisterOutOfRange trap, got {other:?}")),
    }
}
