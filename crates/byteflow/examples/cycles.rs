//! Latency of one bytecode add, in timestamp-counter cycles.
//!
//! The timed region is only `Vm::run` (the machine is built outside it).
//! The printed sample is the minimum of the trials.
//!
//! ```text
//! cargo run -p byteflow-actors --example cycles --release
//! ```

#![allow(unsafe_code)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use byteflow::{Chunk, NativeTable, Program, Value, Vm, VmResult};

const WARMUP: u32 = 64;
const TRIALS: u32 = 256;

fn add_chunk() -> Chunk {
    let mut program = Program::new("cycles");
    program.function("main", 0, |f| {
        let a = f.load_i32(40);
        let b = f.load_i32(2);
        let sum = f.add(a, b);
        f.return_(sum);
    });
    program.build()
}

#[cfg(target_arch = "x86_64")]
fn tsc() -> u64 {
    // The fences keep the counter read on either side of `Vm::run`.
    unsafe {
        std::arch::x86_64::_mm_lfence();
        let cycles = std::arch::x86_64::_rdtsc();
        std::arch::x86_64::_mm_lfence();
        cycles
    }
}

#[cfg(target_arch = "x86_64")]
fn clock_ghz() -> f64 {
    let started = Instant::now();
    let c0 = tsc();
    while started.elapsed() < Duration::from_millis(80) {
        std::hint::spin_loop();
    }
    let elapsed = started.elapsed().as_secs_f64();
    let cycles = tsc().wrapping_sub(c0) as f64;
    cycles / elapsed.max(1e-9) / 1e9
}

fn fresh_vm(chunk: &Arc<Chunk>) -> Result<Vm, Box<dyn std::error::Error>> {
    Ok(Vm::new(Arc::clone(chunk), NativeTable::empty(), 0, &[])?)
}

fn run_add(vm: &mut Vm) -> Result<(), Box<dyn std::error::Error>> {
    match vm.run(64) {
        VmResult::Complete(Value::Int(42)) => Ok(()),
        other => Err(format!("expected Int(42), got {other:?}").into()),
    }
}

#[cfg(target_arch = "x86_64")]
fn min_cycles(
    chunk: &Arc<Chunk>,
    mut run: impl FnMut(&mut Vm) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<u64, Box<dyn std::error::Error>> {
    for _ in 0..WARMUP {
        let mut vm = fresh_vm(chunk)?;
        run(&mut vm)?;
    }
    let mut best = u64::MAX;
    for _ in 0..TRIALS {
        let mut vm = fresh_vm(chunk)?;
        let start = tsc();
        run(&mut vm)?;
        let elapsed = tsc().wrapping_sub(start);
        if elapsed < best {
            best = elapsed;
        }
    }
    Ok(best)
}

#[cfg(target_arch = "x86_64")]
fn report(label: &str, ghz: f64, cycles: u64) {
    let ns = (cycles as f64 / ghz).round() as u64;
    println!("{label}");
    println!("[Clock {ghz:.1} GHZ] → Cycles: {cycles} ⇒ {ns} ns");
}

#[cfg(target_arch = "x86_64")]
fn measure() -> Result<(), Box<dyn std::error::Error>> {
    let ghz = clock_ghz();
    if !(ghz.is_finite() && ghz > 0.1) {
        return Err("could not calibrate the timestamp counter".into());
    }
    let chunk = Arc::new(add_chunk());
    let cycles = min_cycles(&chunk, run_add)?;
    report("interpreter", ghz, cycles);

    #[cfg(feature = "jit")]
    {
        use byteflow::{force_compile, run_vm_with_jit, JitContext, TraceKey};
        let mut ctx = JitContext::new(Arc::clone(&chunk))?;
        force_compile(
            &mut ctx,
            chunk.as_ref(),
            TraceKey {
                function: 0,
                entry_pc: 0,
            },
        )?;
        let jit_cycles = min_cycles(&chunk, |vm| match run_vm_with_jit(vm, 64, &mut ctx) {
            VmResult::Complete(Value::Int(42)) => Ok(()),
            other => Err(format!("jit expected Int(42), got {other:?}").into()),
        })?;
        report("jit", ghz, jit_cycles);
    }
    Ok(())
}

fn main() {
    #[cfg(not(target_arch = "x86_64"))]
    {
        eprintln!("cycles: the timestamp counter is read on x86_64");
        std::process::exit(1);
    }
    #[cfg(target_arch = "x86_64")]
    if let Err(e) = measure() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
