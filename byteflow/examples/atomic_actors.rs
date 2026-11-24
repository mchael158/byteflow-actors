//! Atomic Hop: server loop + two `Ask` clients.
//!
//! Uses the canonical [`byteflow::samples::atomic_actors`] chunk so the
//! example cannot drift from the Tier-1 regression sample.
//!
//! ```text
//! cargo run -p byteflow-actors --example atomic_actors
//!
//! # scheduler stderr (spawn/send/recv/finish) — separate from print's stdout
//! $env:BYTEFLOW_LOG="info"
//! cargo run -p byteflow-actors --example atomic_actors
//! ```

use byteflow::{samples, std_native_table, FlowOutcome, Runtime, RuntimeConfig, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rt = Runtime::with_natives_and_config(
        samples::atomic_actors(),
        std_native_table(),
        RuntimeConfig {
            workers: 2,
            ..Default::default()
        },
    )?;
    let main = rt.function_index("main").ok_or("missing main")?;
    let outcome = rt.spawn(main, &[])?.join();
    let metrics = rt.metrics();
    rt.shutdown();

    match outcome {
        FlowOutcome::Completed(Value::Int(72)) => {
            println!("atomic actors ok: two clients × 8 Ask = 72");
            if metrics.messages_sent < 34 {
                return Err(format!(
                    "expected ≥34 hops (8 Ask+8 reply ×2 + 2 DONE), got {}",
                    metrics.messages_sent
                )
                .into());
            }
            println!("{metrics}");
            Ok(())
        }
        other => Err(format!("unexpected {other:?}").into()),
    }
}
