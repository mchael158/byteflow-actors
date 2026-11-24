//! Measured demo: spawn N trivial flows and join them all.
//!
//! Built only with the public [`byteflow`] facade (`Program` / `Runtime`).
//!
//! ```text
//! cargo run -p byteflow-actors --example throughput --release
//! cargo run -p byteflow-actors --example throughput --release -- 200000
//! ```

use std::time::Instant;

use byteflow::{Chunk, FlowOutcome, Program, Runtime, RuntimeConfig, Value};

fn trivial_chunk() -> Chunk {
    let mut program = Program::new("throughput");
    program.function("worker", 0, |f| {
        let one = f.load_i32(1);
        f.return_(one);
    });
    program.build()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let n: u32 = match std::env::args().nth(1) {
        Some(s) => s.parse()?,
        None => 50_000,
    };
    let workers = std::thread::available_parallelism().map_or(1, |p| p.get());

    let rt = Runtime::with_config(
        trivial_chunk(),
        RuntimeConfig {
            workers,
            ..Default::default()
        },
    )?;
    let worker = rt.function_index("worker").ok_or("missing worker")?;

    let start = Instant::now();
    let mut handles = Vec::with_capacity(n as usize);
    for _ in 0..n {
        handles.push(rt.spawn(worker, &[])?);
    }
    let ok = handles
        .into_iter()
        .map(|h| h.join())
        .filter(|outcome| matches!(outcome, FlowOutcome::Completed(Value::Int(1))))
        .count();
    let elapsed = start.elapsed();
    let metrics = rt.metrics();
    rt.shutdown();

    let secs = elapsed.as_secs_f64().max(1e-9);
    println!("processes={ok}/{n}");
    println!("workers={workers}");
    println!("elapsed_ms={:.2}", elapsed.as_secs_f64() * 1000.0);
    println!("spawns_per_sec={:.0}", ok as f64 / secs);
    println!("{metrics}");
    Ok(())
}
