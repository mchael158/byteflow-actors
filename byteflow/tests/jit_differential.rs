//! Smoke differential: interpreter vs JIT on a shared sample (full matrix lives in `jit::parity`).

use std::sync::Arc;

use byteflow::jit::{force_compile, run_vm_with_jit, JitContext, TraceKey};
use byteflow::{samples, NativeTable, Vm, VmResult};

#[test]
fn add_forty_two_matches_interpreter_and_jit() -> Result<(), Box<dyn std::error::Error>> {
    let chunk = Arc::new(samples::add_forty_two());
    let mut plain = Vm::new(Arc::clone(&chunk), NativeTable::empty(), 0, &[])?;
    let interp = plain.run(u32::MAX);
    let entry = chunk.functions[0].entry;
    let mut ctx = JitContext::new(Arc::clone(&chunk))?;
    force_compile(
        &mut ctx,
        &chunk,
        TraceKey {
            function: 0,
            entry_pc: entry,
            floats: false,
        },
    )?;
    let mut vm = Vm::new(chunk, NativeTable::empty(), 0, &[])?;
    let jit = run_vm_with_jit(&mut vm, u32::MAX, &mut ctx);
    if std::mem::discriminant(&interp) != std::mem::discriminant(&jit) {
        return Err(format!("discriminant mismatch: {interp:?} vs {jit:?}").into());
    }
    match (&interp, &jit) {
        (VmResult::Complete(a), VmResult::Complete(b)) if *a == *b => Ok(()),
        (VmResult::Trap(a), VmResult::Trap(b)) if *a == *b => Ok(()),
        _ => Err(format!("value mismatch: {interp:?} vs {jit:?}").into()),
    }
}
