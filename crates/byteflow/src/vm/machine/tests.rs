use super::*;
use crate::bytecode::builder::ChunkBuilder;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// `Spawn a=255` reads its arguments from `a+1`, so the first index
/// already leaves the register space. This used to panic in debug and —
/// far worse — wrap around to `r0` in release, silently spawning with the
/// wrong argument.
#[test]
fn spawn_from_the_last_register_traps_instead_of_wrapping() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 0, 2);
    b.emit_spawn(255, 0, 1);
    b.emit_return(0);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Trap(Fault::RegisterIndexOverflow {
            base: 255,
            offset: 1,
        }) => Ok(()),
        other => Err(format!("expected RegisterIndexOverflow, got {other:?}").into()),
    }
}

/// `Vm::new` is public and reachable without `verify`, and its panic
/// landed on the caller's thread — the embedder's on `Runtime::spawn`, or
/// a worker's on bytecode `Spawn`, outside the `catch_unwind`.
#[test]
fn entering_a_function_with_too_few_registers_faults() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 3, 1);
    b.emit_return(0);
    let args = [Value::Int(1), Value::Int(2), Value::Int(3)];
    match Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &args) {
        Err(Fault::RegisterOutOfRange {
            reg: 1,
            frame_size: 1,
        }) => Ok(()),
        Err(e) => Err(format!("unexpected fault: {e}").into()),
        Ok(_) => Err("three arguments cannot be loaded into one register".into()),
    }
}

#[test]
fn call_with_the_wrong_argc_traps_instead_of_padding() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    let callee = b.begin_function("callee", 0, 1);
    b.emit_load_imm(0, 1);
    b.emit_return(0);
    let main = b.begin_function("main", 0, 2);
    b.emit_load_imm(0, 7);
    b.emit_call(0, callee, 1);
    b.emit_return(0);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), main, &[])?;
    match vm.run(20) {
        VmResult::Trap(Fault::BadArity {
            passed: 1,
            expected: 0,
        }) => Ok(()),
        other => Err(format!("expected BadArity, got {other:?}").into()),
    }
}

#[test]
fn calling_a_function_with_too_few_registers_traps() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    let callee = b.begin_function("callee", 3, 1);
    b.emit_return(0);
    let main = b.begin_function("main", 0, 4);
    b.emit_load_imm(0, 7);
    b.emit_load_imm(1, 8);
    b.emit_load_imm(2, 9);
    b.emit_call(0, callee, 3);
    b.emit_return(0);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), main, &[])?;
    match vm.run(50) {
        VmResult::Trap(Fault::RegisterOutOfRange {
            reg: 1,
            frame_size: 1,
        }) => Ok(()),
        other => Err(format!("expected RegisterOutOfRange, got {other:?}").into()),
    }
}

/// The boundary case that must keep working: gathering right up to the
/// last register is legal, it is only going *past* it that faults.
#[test]
fn gathering_up_to_the_last_register_still_works() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    let callee = b.begin_function("callee", 1, 1);
    b.emit_return(0);
    let main = b.begin_function("main", 0, 255);
    b.emit_load_imm(254, 5);
    // Argument at r254 — the highest index a 255-register frame has.
    b.emit_call(254, callee, 1);
    b.emit_return(254);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), main, &[])?;
    match vm.run(100) {
        VmResult::Complete(_) => Ok(()),
        other => Err(format!("expected completion, got {other:?}").into()),
    }
}

#[test]
fn neg_i64_min_wrapping() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 0, 1);
    let k = b.const_(Value::Int(i64::MIN));
    b.emit_load_const(0, k);
    b.emit_neg(0, 0);
    b.emit_return(0);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Complete(Value::Int(v)) if v == i64::MIN => Ok(()),
        other => Err(format!("expected Complete(Int(i64::MIN)), got {other:?}").into()),
    }
}

#[test]
fn mod_non_int_type_mismatch() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 0, 3);
    b.emit_load_imm(0, 10);
    let kf = b.const_(Value::Float(3.0));
    b.emit_load_const(1, kf);
    b.emit_binop(Opcode::Mod, 2, 0, 1);
    b.emit_return(2);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(20) {
        VmResult::Trap(Fault::TypeMismatch {
            expected: "int",
            got: "float",
        }) => Ok(()),
        other => Err(format!("expected TypeMismatch, got {other:?}").into()),
    }
}

#[test]
fn receive_timeout_negative_traps() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 0, 2);
    b.emit_load_imm(1, -1);
    b.emit_receive_timeout(0, 1);
    b.emit_return(0);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Trap(Fault::TypeMismatch {
            expected: "non-negative int",
            ..
        }) => Ok(()),
        other => Err(format!("expected TypeMismatch, got {other:?}").into()),
    }
}

#[test]
fn ask_timeout_negative_traps() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 0, 4);
    b.emit_load_imm(3, -5);
    // Cap/message regs are garbage; millis is checked first.
    b.emit_ask_timeout(0, 1, 2, 3);
    b.emit_return(0);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Trap(Fault::TypeMismatch {
            expected: "non-negative int",
            ..
        }) => Ok(()),
        other => Err(format!("expected TypeMismatch, got {other:?}").into()),
    }
}

#[test]
fn jump_out_of_range_traps() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 0, 1);
    b.emit_load_imm(0, 0);
    b.emit_return(0);
    let mut chunk = b.finish();
    let code_len = chunk.code.len();
    chunk.code[0] = Instruction::only_imm(Opcode::Jump, 100);
    let mut vm = Vm::new(Arc::new(chunk), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Trap(Fault::BadJump {
            target: 101,
            code_len: len,
        }) if len == code_len => Ok(()),
        other => Err(format!("expected BadJump(101), got {other:?}").into()),
    }
}

#[test]
fn overwrite_releases_previous_heap_charge() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    let k1 = b.const_(Value::Str("a".repeat(100).into()));
    let k2 = b.const_(Value::Str("b".repeat(100).into()));
    b.begin_function("main", 0, 1);
    b.emit_load_const(0, k1);
    b.emit_load_const(0, k2);
    b.emit_return(0);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    // Limit 150: first 100 fits; without release the second 100 would fail.
    let quota = Arc::new(crate::scheduler::FlowQuota::from_config(
        crate::QuotaConfig {
            mem_limit: 150,
            ..crate::QuotaConfig::permissive()
        },
    ));
    let memory = Arc::new(crate::MemoryBudget::new(10_000));
    vm.set_memory_budget(Arc::clone(&memory));
    vm.set_quota(Arc::clone(&quota))?;
    match vm.run(20) {
        VmResult::Complete(Value::Str(s)) if s.len() == 100 => {
            assert_eq!(quota.mem_used(), 100);
            assert_eq!(memory.used(), 100);
            Ok(())
        }
        other => Err(format!("expected Complete(100-byte Str), got {other:?}").into()),
    }
}

#[test]
fn return_releases_callee_heap_before_caller_writeback() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    let k = b.const_(Value::Str("x".repeat(200).into()));
    let callee = b.begin_function("callee", 0, 1);
    b.emit_load_const(0, k);
    b.emit_return(0);
    let main = b.begin_function("main", 0, 1);
    b.emit_call(0, callee, 0);
    b.emit_return(0);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), main, &[])?;
    // Old bug: callee charged 200 and return writeback charged again without
    // release → QuotaExceeded at 250. Correct path releases then recharges.
    let quota = Arc::new(crate::scheduler::FlowQuota::new(10_000, 250, 1, 1, 1, 1));
    vm.set_quota(Arc::clone(&quota))?;
    match vm.run(50) {
        VmResult::Complete(Value::Str(s)) if s.len() == 200 => {
            assert_eq!(quota.mem_used(), 200);
            Ok(())
        }
        other => Err(format!("expected Complete(200-byte Str), got {other:?}").into()),
    }
}

#[test]
fn set_reg_out_of_range_does_not_charge() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 0, 1);
    b.emit_return(0);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    let quota = Arc::new(crate::scheduler::FlowQuota::from_config(
        crate::QuotaConfig {
            mem_limit: 10_000,
            ..crate::QuotaConfig::permissive()
        },
    ));
    vm.set_quota(Arc::clone(&quota))?;
    let big = Value::Str("z".repeat(500).into());
    match vm.set_register(255, big) {
        Err(Fault::RegisterOutOfRange {
            reg: 255,
            frame_size: 1,
        }) => {
            assert_eq!(quota.mem_used(), 0);
            Ok(())
        }
        other => Err(format!("expected RegisterOutOfRange, got {other:?}").into()),
    }
}

#[test]
fn set_quota_is_transactional() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 0, 2);
    b.emit_return(0);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    // Two 100-byte Strs already in registers; limit 150 must refuse the
    // whole attach without leaving a partial charge on the quota object.
    vm.set_register(0, Value::Str("a".repeat(100).into()))?;
    vm.set_register(1, Value::Str("b".repeat(100).into()))?;
    let quota = Arc::new(crate::scheduler::FlowQuota::from_config(
        crate::QuotaConfig {
            mem_limit: 150,
            ..crate::QuotaConfig::permissive()
        },
    ));
    match vm.set_quota(Arc::clone(&quota)) {
        Err(Fault::QuotaExceeded(_)) => {
            assert_eq!(
                quota.mem_used(),
                0,
                "failed set_quota must not leave a partial charge"
            );
            Ok(())
        }
        other => Err(format!("expected QuotaExceeded, got {other:?}").into()),
    }
}

#[test]
fn message_payload_counts_against_the_heap_quota() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 0, 1);
    b.emit_return(0);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    let quota = Arc::new(FlowQuota::new(10_000, 64, 1, 1, 1, 1));
    vm.set_quota(Arc::clone(&quota))?;
    let hop = Value::Message(crate::bytecode::Message::new(
        0,
        1,
        1,
        Value::str("x".repeat(200)),
    ));
    match vm.set_register(0, hop) {
        Err(Fault::QuotaExceeded(_)) => {
            assert_eq!(quota.mem_used(), 0);
            Ok(())
        }
        other => Err(format!("expected QuotaExceeded, got {other:?}").into()),
    }
}

#[test]
fn overwriting_a_message_releases_the_payload_charge() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 0, 1);
    b.emit_return(0);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    let quota = Arc::new(FlowQuota::new(10_000, 10_000, 1, 1, 1, 1));
    vm.set_quota(Arc::clone(&quota))?;
    vm.set_register(
        0,
        Value::Message(crate::bytecode::Message::new(
            0,
            1,
            1,
            Value::str("x".repeat(200)),
        )),
    )?;
    assert_eq!(quota.mem_used(), 200);
    vm.set_register(0, Value::Int(1))?;
    assert_eq!(quota.mem_used(), 0);
    Ok(())
}

#[test]
fn a_message_past_the_decoder_ceiling_is_not_stored() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 0, 1);
    b.emit_return(0);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    let quota = Arc::new(FlowQuota::new(10_000, 10_000, 1, 1, 1, 1));
    vm.set_quota(Arc::clone(&quota))?;
    let mut value = Value::str("x".repeat(200));
    for _ in 0..17 {
        value = Value::Message(crate::bytecode::Message::new(0, 1, 1, value));
    }
    match vm.set_register(0, value) {
        Err(Fault::QuotaExceeded(_)) => {
            assert_eq!(quota.mem_used(), 0);
            Ok(())
        }
        other => Err(format!("expected QuotaExceeded, got {other:?}").into()),
    }
}

#[test]
fn in_flight_hop_does_not_recount_the_held_payload() {
    let hop = Value::Message(crate::bytecode::Message::new(
        0,
        1,
        1,
        Value::str("x".repeat(200)),
    ));
    let inflight = in_flight_hop_charge(&hop);
    assert_eq!(inflight + 200, hop.memory_size());
    assert!(inflight < hop.memory_size());
}

#[test]
fn sleep_rejects_a_bool() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    let flag = b.const_(Value::Bool(true));
    b.begin_function("main", 0, 1);
    b.emit_load_const(0, flag);
    b.emit_sleep(0);
    b.emit_return(0);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Trap(Fault::TypeMismatch {
            expected: "int",
            got: "bool",
        }) => Ok(()),
        other => Err(format!("expected TypeMismatch(bool), got {other:?}").into()),
    }
}

#[test]
fn tag_and_request_id_reject_a_bool() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    let flag = b.const_(Value::Bool(false));
    b.begin_function("main", 0, 3);
    b.emit_load_const(0, flag);
    b.emit_receive_match(1, 0);
    b.emit_return(0);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Trap(Fault::TypeMismatch {
            expected: "int",
            got: "bool",
        }) => {}
        other => return Err(format!("tag bool: {other:?}").into()),
    }
    let mut b = ChunkBuilder::new("t");
    let flag = b.const_(Value::Bool(true));
    b.begin_function("main", 0, 3);
    b.emit_load_imm(0, 1);
    b.emit_load_const(1, flag);
    b.emit_receive_match_corr(2, 0, 1);
    b.emit_return(0);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Trap(Fault::TypeMismatch {
            expected: "int",
            got: "bool",
        }) => Ok(()),
        other => Err(format!("request_id bool: {other:?}").into()),
    }
}

#[test]
fn sleep_accepts_zero_and_rejects_a_float() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 0, 1);
    b.emit_load_imm(0, 0);
    b.emit_sleep(0);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Sleep(d) if d.is_zero() => {}
        other => return Err(format!("expected Sleep(0), got {other:?}").into()),
    }
    let mut b = ChunkBuilder::new("t");
    let n = b.const_(Value::Float(1.0));
    b.begin_function("main", 0, 1);
    b.emit_load_const(0, n);
    b.emit_sleep(0);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Trap(Fault::TypeMismatch {
            expected: "int",
            got: "float",
        }) => Ok(()),
        other => Err(format!("expected float rejection, got {other:?}").into()),
    }
}

#[test]
fn dict_str_stays_charged_after_the_register_is_cleared() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    let text = b.const_(Value::Str("x".repeat(100).into()));
    b.begin_function("main", 0, 3);
    b.emit_load_const(0, text);
    b.emit_load_imm(1, 7);
    b.emit_dict_put(2, 1, 0);
    b.emit_load_imm(0, 0);
    b.emit_halt();
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    let quota = Arc::new(FlowQuota::new(10_000, 500, 1, 1, 1, 1));
    vm.set_quota(Arc::clone(&quota))?;
    match vm.run(20) {
        VmResult::Complete(_) => {
            assert_eq!(quota.mem_used(), 100);
            Ok(())
        }
        other => Err(format!("expected Complete, got {other:?}").into()),
    }
}

#[test]
fn dict_bytes_round_trip_charges_the_buffer() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    let blob = b.const_(Value::bytes(vec![1u8; 60]));
    b.begin_function("main", 0, 3);
    b.emit_load_const(0, blob);
    b.emit_load_imm(1, 4);
    b.emit_dict_put(2, 1, 0);
    b.emit_load_imm(0, 0);
    b.emit_dict_get(0, 1);
    b.emit_halt();
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    let quota = Arc::new(FlowQuota::new(10_000, 500, 1, 1, 1, 1));
    vm.set_quota(Arc::clone(&quota))?;
    match vm.run(30) {
        VmResult::Complete(Value::Bytes(buf)) if buf.len() == 60 => {
            // Register and dictionary each hold the buffer.
            assert_eq!(quota.mem_used(), 120);
            Ok(())
        }
        other => Err(format!("expected bytes, got {other:?}").into()),
    }
}

#[test]
fn dict_put_rejects_a_second_copy_that_does_not_fit() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    let text = b.const_(Value::Str("x".repeat(100).into()));
    b.begin_function("main", 0, 3);
    b.emit_load_const(0, text);
    b.emit_load_imm(1, 1);
    b.emit_dict_put(2, 1, 0);
    b.emit_halt();
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    let quota = Arc::new(FlowQuota::new(10_000, 150, 1, 1, 1, 1));
    vm.set_quota(Arc::clone(&quota))?;
    match vm.run(20) {
        VmResult::Trap(Fault::QuotaExceeded(_)) => {
            assert_eq!(quota.mem_used(), 100);
            Ok(())
        }
        other => Err(format!("expected QuotaExceeded, got {other:?}").into()),
    }
}

#[test]
fn dict_replace_that_cannot_park_the_old_string_keeps_it() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    let first = b.const_(Value::Str("a".repeat(100).into()));
    let second = b.const_(Value::Str("b".repeat(80).into()));
    b.begin_function("main", 0, 4);
    b.emit_load_const(0, first);
    b.emit_load_imm(1, 1);
    b.emit_dict_put(2, 1, 0);
    b.emit_load_imm(0, 0);
    b.emit_load_const(0, second);
    b.emit_dict_put(3, 1, 0);
    b.emit_halt();
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    // 200 admits the original string in both the register and the
    // dictionary. Returning that string to r3 while r0 and the
    // dictionary both hold the 80-byte replacement needs 260.
    let quota = Arc::new(FlowQuota::new(10_000, 200, 1, 1, 1, 1));
    vm.set_quota(Arc::clone(&quota))?;
    match vm.run(30) {
        VmResult::Trap(Fault::QuotaExceeded(_)) => {
            // r0 still holds 80 and the dictionary still holds 100.
            assert_eq!(quota.mem_used(), 180);
            Ok(())
        }
        other => Err(format!("expected QuotaExceeded, got {other:?}").into()),
    }
}

#[test]
fn dict_erase_moves_the_charge_back_to_the_register() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    let text = b.const_(Value::Str("y".repeat(80).into()));
    b.begin_function("main", 0, 4);
    b.emit_load_const(0, text);
    b.emit_load_imm(1, 3);
    b.emit_dict_put(2, 1, 0);
    b.emit_load_imm(0, 0);
    b.emit_dict_erase(0, 1);
    b.emit_halt();
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    let quota = Arc::new(FlowQuota::new(10_000, 1_000, 1, 1, 1, 1));
    vm.set_quota(Arc::clone(&quota))?;
    match vm.run(30) {
        VmResult::Complete(Value::Str(s)) if s.len() == 80 => {
            assert_eq!(quota.mem_used(), 80);
            Ok(())
        }
        other => Err(format!("expected the erased string, got {other:?}").into()),
    }
}

#[test]
fn dict_rejects_a_message_and_an_empty_string_is_free() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    let hop = b.const_(Value::Message(crate::bytecode::Message::new(0, 1, 1, 1i64)));
    b.begin_function("main", 0, 3);
    b.emit_load_const(0, hop);
    b.emit_load_imm(1, 1);
    b.emit_dict_put(2, 1, 0);
    b.emit_halt();
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Trap(Fault::TypeMismatch {
            expected: "dict value (scalar, str, or bytes)",
            got: "message",
        }) => {}
        other => return Err(format!("message dict: {other:?}").into()),
    }
    let mut b = ChunkBuilder::new("t");
    let empty = b.const_(Value::Str("".into()));
    b.begin_function("main", 0, 3);
    b.emit_load_const(0, empty);
    b.emit_load_imm(1, 1);
    b.emit_dict_put(2, 1, 0);
    b.emit_halt();
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    let quota = Arc::new(FlowQuota::new(10_000, 64, 1, 1, 1, 1));
    vm.set_quota(Arc::clone(&quota))?;
    match vm.run(20) {
        VmResult::Complete(_) => {
            assert_eq!(quota.mem_used(), 0);
            Ok(())
        }
        other => Err(format!("empty str: {other:?}").into()),
    }
}

#[test]
fn dict_replace_releases_the_previous_string() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    let first = b.const_(Value::Str("a".repeat(100).into()));
    let second = b.const_(Value::Str("b".repeat(40).into()));
    b.begin_function("main", 0, 4);
    b.emit_load_const(0, first);
    b.emit_load_imm(1, 1);
    b.emit_dict_put(2, 1, 0);
    b.emit_load_imm(0, 0);
    b.emit_load_const(0, second);
    b.emit_dict_put(2, 1, 0);
    b.emit_load_imm(2, 0);
    b.emit_halt();
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    let quota = Arc::new(FlowQuota::new(10_000, 1_000, 1, 1, 1, 1));
    vm.set_quota(Arc::clone(&quota))?;
    match vm.run(40) {
        VmResult::Complete(_) => {
            // r0 still holds the 40-byte replacement; the dict holds the
            // same buffer; the 100-byte previous value was overwritten.
            assert_eq!(quota.mem_used(), 80);
            Ok(())
        }
        other => Err(format!("expected Complete, got {other:?}").into()),
    }
}

#[test]
fn dictionaries_are_isolated_between_vms() -> TestResult {
    let mut writer = ChunkBuilder::new("t");
    writer.begin_function("main", 0, 3);
    writer.emit_load_imm(0, 9);
    writer.emit_load_imm(1, 1);
    writer.emit_dict_put(2, 1, 0);
    writer.emit_halt();
    let mut vm = Vm::new(Arc::new(writer.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(20) {
        VmResult::Complete(_) => {}
        other => return Err(format!("writer: {other:?}").into()),
    }
    let mut reader = ChunkBuilder::new("t");
    reader.begin_function("main", 0, 2);
    reader.emit_load_imm(1, 1);
    reader.emit_dict_get(0, 1);
    reader.emit_halt();
    let mut other_vm = Vm::new(Arc::new(reader.finish()), NativeTable::empty(), 0, &[])?;
    match other_vm.run(20) {
        VmResult::Complete(Value::Unit) => Ok(()),
        other => Err(format!("reader saw {other:?}").into()),
    }
}

#[test]
fn tag_bounds_and_non_integers_stay_strict() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 0, 2);
    b.emit_load_imm(0, i32::from(u16::MAX));
    b.emit_receive_match(1, 0);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Receive {
            match_tag: Some(tag),
            ..
        } if tag == u16::MAX => {}
        other => return Err(format!("u16::MAX tag: {other:?}").into()),
    }
    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 0, 2);
    b.emit_load_imm(0, i32::from(u16::MAX) + 1);
    b.emit_receive_match(1, 0);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Trap(Fault::TypeMismatch {
            expected: "tag u16",
            got: "int-out-of-range",
        }) => {}
        other => return Err(format!("tag overflow: {other:?}").into()),
    }
    let mut b = ChunkBuilder::new("t");
    let text = b.const_(Value::str("nope"));
    b.begin_function("main", 0, 1);
    b.emit_load_const(0, text);
    b.emit_sleep(0);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Trap(Fault::TypeMismatch {
            expected: "int",
            got: "str",
        }) => Ok(()),
        other => Err(format!("str sleep: {other:?}").into()),
    }
}

#[test]
fn sub_imm_and_lt_imm_follow_integer_and_float_rules() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 0, 2);
    b.emit_load_imm(0, 10);
    b.emit_sub_imm(1, 0, 3);
    b.emit_return(1);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Complete(Value::Int(7)) => {}
        other => return Err(format!("10 - 3: {other:?}").into()),
    }
    let mut b = ChunkBuilder::new("t");
    let big = b.const_(Value::Int(i64::MAX));
    b.begin_function("main", 0, 2);
    b.emit_load_const(0, big);
    b.emit_sub_imm(1, 0, -1);
    b.emit_return(1);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Complete(Value::Int(n)) if n == i64::MIN => {}
        other => return Err(format!("wrap: {other:?}").into()),
    }
    let mut b = ChunkBuilder::new("t");
    let flag = b.const_(Value::Bool(true));
    b.begin_function("main", 0, 2);
    b.emit_load_const(0, flag);
    b.emit_sub_imm(1, 0, 1);
    b.emit_return(1);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Trap(Fault::TypeMismatch {
            expected: "int or float",
            got: "bool",
        }) => {}
        other => return Err(format!("bool sub: {other:?}").into()),
    }
    let mut b = ChunkBuilder::new("t");
    let num = b.const_(Value::Float(1.5));
    b.begin_function("main", 0, 2);
    b.emit_load_const(0, num);
    b.emit_lt_imm(1, 0, 2);
    b.emit_return(1);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Complete(Value::Bool(true)) => {}
        other => return Err(format!("1.5 < 2: {other:?}").into()),
    }
    let mut b = ChunkBuilder::new("t");
    let nan = b.const_(Value::Float(f64::NAN));
    b.begin_function("main", 0, 2);
    b.emit_load_const(0, nan);
    b.emit_lt_imm(1, 0, 0);
    b.emit_return(1);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Complete(Value::Bool(false)) => Ok(()),
        other => Err(format!("nan < 0: {other:?}").into()),
    }
}

#[test]
fn mul_eq_le_imm_and_bitwise_follow_the_integer_rules() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    let big = b.const_(Value::Int(i64::MIN));
    b.begin_function("main", 0, 2);
    b.emit_load_const(0, big);
    b.emit_mul_imm(1, 0, -1);
    b.emit_return(1);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Complete(Value::Int(n)) if n == i64::MIN => {}
        other => return Err(format!("min * -1: {other:?}").into()),
    }

    let mut b = ChunkBuilder::new("t");
    let num = b.const_(Value::Float(1.5));
    b.begin_function("main", 0, 3);
    b.emit_load_const(0, num);
    b.emit_mul_imm(1, 0, 2);
    b.emit_eq_imm(2, 1, 3);
    b.emit_return(2);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Complete(Value::Bool(false)) => {}
        other => return Err(format!("float eq imm: {other:?}").into()),
    }

    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 0, 2);
    b.emit_load_imm(0, 3);
    b.emit_le_imm(1, 0, 3);
    b.emit_return(1);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Complete(Value::Bool(true)) => {}
        other => return Err(format!("3 <= 3: {other:?}").into()),
    }

    let mut b = ChunkBuilder::new("t");
    let nan = b.const_(Value::Float(f64::NAN));
    b.begin_function("main", 0, 2);
    b.emit_load_const(0, nan);
    b.emit_le_imm(1, 0, 1);
    b.emit_return(1);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Complete(Value::Bool(false)) => {}
        other => return Err(format!("nan <= 1: {other:?}").into()),
    }

    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 0, 4);
    b.emit_load_imm(0, 0b1100);
    b.emit_load_imm(1, 0b1010);
    b.emit_bit_binop(Opcode::BitAnd, 2, 0, 1);
    b.emit_bit_binop(Opcode::BitOr, 3, 0, 1);
    b.emit_return(2);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Complete(Value::Int(0b1000)) => {}
        other => return Err(format!("bitand: {other:?}").into()),
    }

    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 0, 4);
    b.emit_load_imm(0, 0b1100);
    b.emit_load_imm(1, 0b1010);
    b.emit_bit_binop(Opcode::BitXor, 2, 0, 1);
    b.emit_bit_not(3, 2);
    b.emit_return(3);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Complete(Value::Int(n)) if n == !0b0110 => {}
        other => return Err(format!("bitnot: {other:?}").into()),
    }

    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 0, 3);
    b.emit_load_imm(0, 1);
    b.emit_load_imm(1, 4);
    b.emit_bit_binop(Opcode::Shl, 2, 0, 1);
    b.emit_return(2);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Complete(Value::Int(16)) => {}
        other => return Err(format!("shl: {other:?}").into()),
    }

    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 0, 3);
    b.emit_load_imm(0, -8);
    b.emit_load_imm(1, 1);
    b.emit_bit_binop(Opcode::Shr, 2, 0, 1);
    b.emit_return(2);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Complete(Value::Int(-4)) => {}
        other => return Err(format!("shr: {other:?}").into()),
    }

    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 0, 3);
    b.emit_load_imm(0, 5);
    b.emit_load_imm(1, -1);
    b.emit_bit_binop(Opcode::Shl, 2, 0, 1);
    b.emit_return(2);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Complete(Value::Int(n)) if n == i64::MIN => {}
        other => return Err(format!("shl mask: {other:?}").into()),
    }

    let mut b = ChunkBuilder::new("t");
    let flag = b.const_(Value::Bool(true));
    b.begin_function("main", 0, 2);
    b.emit_load_const(0, flag);
    b.emit_load_imm(1, 1);
    b.emit_bit_binop(Opcode::BitAnd, 0, 0, 1);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(10) {
        VmResult::Trap(Fault::TypeMismatch {
            expected: "int",
            got: "bool",
        }) => Ok(()),
        other => Err(format!("bool bitand: {other:?}").into()),
    }
}

#[test]
fn list_pack_get_len_and_quota() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 0, 4);
    b.emit_load_imm(0, 7);
    b.emit_load_imm(1, 8);
    b.emit_list_pack(2, 0, 2);
    b.emit_list_get(3, 2, 1);
    b.emit_return(3);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(20) {
        VmResult::Complete(Value::Int(8)) => {}
        other => return Err(format!("list get: {other:?}").into()),
    }
    let mut b = ChunkBuilder::new("t");
    let text = b.const_(Value::Str("z".repeat(100).into()));
    b.begin_function("main", 0, 2);
    b.emit_load_const(0, text);
    b.emit_list_pack(1, 0, 1);
    b.emit_halt();
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    let quota = Arc::new(FlowQuota::new(10_000, 1_000, 1, 1, 1, 1));
    vm.set_quota(Arc::clone(&quota))?;
    let slot = std::mem::size_of::<Value>();
    match vm.run(10) {
        VmResult::Complete(_) => {
            assert_eq!(quota.mem_used(), 100 + slot + 100);
            Ok(())
        }
        other => Err(format!("list quota: {other:?}").into()),
    }
}

#[test]
fn map_pack_get_len_and_duplicate_key() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 0, 6);
    b.emit_load_imm(0, 1);
    b.emit_load_imm(1, 7);
    b.emit_load_imm(2, 2);
    b.emit_load_imm(3, 8);
    b.emit_map_pack(4, 0, 2);
    b.emit_map_get(5, 4, 2);
    b.emit_return(5);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(20) {
        VmResult::Complete(Value::Int(8)) => {}
        other => return Err(format!("map get: {other:?}").into()),
    }
    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 0, 5);
    b.emit_load_imm(0, 1);
    b.emit_load_imm(1, 7);
    b.emit_load_imm(2, 1);
    b.emit_load_imm(3, 8);
    b.emit_map_pack(4, 0, 2);
    b.emit_halt();
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    match vm.run(20) {
        VmResult::Trap(fault) => {
            let text = fault.to_string();
            if text.contains("duplicate") {
                Ok(())
            } else {
                Err(format!("duplicate key: {text}").into())
            }
        }
        other => Err(format!("duplicate key: {other:?}").into()),
    }
}

#[test]
fn fresh_request_id_stays_in_i64_domain() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 0, 1);
    b.emit_return(0);
    let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
    vm.set_next_request_id_for_test(i64::MAX as u64);
    let id = vm.fresh_request_id();
    assert_eq!(id, i64::MAX as u64);
    assert_eq!(vm.fresh_request_id(), i64::MAX as u64);
    assert!((id as i64) >= 0);
    Ok(())
}

#[test]
fn call_native_without_gate_is_typed_denied() -> TestResult {
    let mut b = ChunkBuilder::new("t");
    b.begin_function("main", 0, 1);
    b.emit_call_native(0, 0, 0);
    b.emit_return(0);
    let table = NativeTable::builder().register("noop", |_| Ok(Value::Unit))?;
    let mut vm = Vm::new(Arc::new(b.finish()), table.build(), 0, &[])?;
    match vm.run(10) {
        VmResult::Trap(Fault::NativeDenied(NativeCallError::NoNativeRight)) => Ok(()),
        other => Err(format!("expected NativeDenied(NoNativeRight), got {other:?}").into()),
    }
}
