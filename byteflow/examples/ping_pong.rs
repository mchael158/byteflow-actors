//! Two flows, one Atomic Hop round-trip (`Value::Message`).
//!
//! Uses the canonical [`byteflow::samples::ping_pong`] chunk so the example
//! cannot drift from the Tier-1 regression sample.
//!
//! ```text
//! cargo run -p byteflow-actors --example ping_pong
//! ```

use byteflow::{samples, std_native_table, FlowOutcome, Runtime, RuntimeConfig, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rt = Runtime::with_natives_and_config(
        samples::ping_pong(),
        std_native_table(),
        RuntimeConfig {
            workers: 1,
            ..Default::default()
        },
    )?;
    let main = rt.function_index("main").ok_or("missing main")?;
    let outcome = rt.spawn(main, &[])?.join();
    let metrics = rt.metrics();
    rt.shutdown();

    match outcome {
        FlowOutcome::Completed(Value::Int(2)) => {
            println!("pong replied 2 (Atomic Hop)");
            println!("{metrics}");
            Ok(())
        }
        other => Err(format!("unexpected {other:?}").into()),
    }
}
