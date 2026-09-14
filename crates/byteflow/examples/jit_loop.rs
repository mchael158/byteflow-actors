//! Compare interpreter vs JIT on a tight counting loop (`feature = "jit"`).
//!
//! Built only with the public [`byteflow`] facade.
//!
//! ```text
//! cargo run -p byteflow-actors --example jit_loop --features jit --release
//! ```

use std::time::Instant;

use byteflow::{
    Chunk, FlowOutcome, JitConfig, Program, Runtime, RuntimeConfig, Value,
};

fn loop_chunk(iterations: i32) -> Chunk {
    let mut program = Program::new("jit-loop");
    program.function("main", 0, |f| {
        let limit = f.load_int(i64::from(iterations));
        let counter = f.load_i32(0);
        f.while_lt(counter, limit, |f| f.add_imm(counter, 1));
        f.return_(counter);
    });
    program.build()
}

fn run_once(jit: bool) -> Result<(), Box<dyn std::error::Error>> {
    let chunk = loop_chunk(1_000_000);
    let rt = Runtime::with_config(
        chunk,
        RuntimeConfig {
            workers: 1,
            quantum: 50_000_000,
            jit: JitConfig {
                enabled: jit,
                hot_threshold: 1,
            },
            ..Default::default()
        },
    )?;
    let start = Instant::now();
    let outcome = rt.spawn(0, &[])?.join();
    let snapshot = rt.metrics();
    rt.shutdown();
    let elapsed = start.elapsed();
    match outcome {
        FlowOutcome::Completed(Value::Int(n)) => {
            println!("jit={jit} result={n} elapsed={elapsed:?} metrics={snapshot}");
            Ok(())
        }
        other => Err(format!("unexpected outcome: {other:?}").into()),
    }
}

fn main() {
    println!("interpreter:");
    if let Err(e) = run_once(false) {
        eprintln!("{e}");
        std::process::exit(1);
    }
    println!("jit:");
    if let Err(e) = run_once(true) {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
