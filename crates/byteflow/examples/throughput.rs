//! Measured demo: spawn N trivial flows and join them all.
//!
//! Built only with the public [`byteflow`] facade (`Program` / `Runtime`).
//!
//! ```text
//! cargo run -p byteflow-actors --example throughput --release
//! ```

use std::time::Instant;

use byteflow::{
    Chunk, FlowOutcome, MailboxConfig, Program, Runtime, RuntimeConfig, Value, DEFAULT_QUANTUM,
};

fn trivial_chunk() -> Chunk {
    let mut program = Program::new("throughput");
    program.function("worker", 0, |f| {
        let one = f.load_i32(1);
        f.return_(one);
    });
    program.build()
}

fn main() {
    let n: u32 = match std::env::args().nth(1) {
        Some(s) => match s.parse() {
            Ok(v) => v,
            Err(_) => 50_000,
        },
        None => 50_000,
    };

    let workers = match std::thread::available_parallelism() {
        Ok(p) => p.get(),
        Err(_) => 1,
    };

    let rt = match Runtime::with_config(
        trivial_chunk(),
        RuntimeConfig {
            workers,
            quantum: DEFAULT_QUANTUM,
            mailbox: MailboxConfig::DEFAULT,
            ..Default::default()
        },
    ) {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("runtime: {e}");
            std::process::exit(1);
        }
    };
    let Some(worker_fn) = rt.function_index("worker") else {
        eprintln!("missing worker");
        std::process::exit(1);
    };

    let start = Instant::now();
    let mut handles = Vec::with_capacity(n as usize);
    for _ in 0..n {
        match rt.spawn(worker_fn, &[]) {
            Ok(h) => handles.push(h),
            Err(e) => {
                eprintln!("spawn: {e}");
                std::process::exit(1);
            }
        }
    }
    let mut ok = 0u32;
    for h in handles {
        if matches!(h.join(), FlowOutcome::Completed(Value::Int(1))) {
            ok += 1;
        }
    }
    let elapsed = start.elapsed();
    let metrics = rt.metrics();
    rt.shutdown();

    let secs = elapsed.as_secs_f64().max(1e-9);
    println!("processes={ok}/{n}");
    println!("workers={workers}");
    println!("elapsed_ms={:.2}", elapsed.as_secs_f64() * 1000.0);
    println!("spawns_per_sec={:.0}", ok as f64 / secs);
    println!("{metrics}");
}
