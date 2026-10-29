//! Two flows, one Atomic Hop round-trip (`Value::Message`).
//!
//! Uses the canonical [`byteflow::samples::ping_pong`] chunk so the example
//! cannot drift from the Tier-1 regression sample.
//!
//! ```text
//! cargo run -p byteflow-actors --example ping_pong
//! ```

use byteflow::{
    samples, std_native_table, FlowOutcome, MailboxConfig, Runtime, RuntimeConfig, Value,
    DEFAULT_QUANTUM,
};

fn main() {
    let rt = match Runtime::with_natives_and_config(
        samples::ping_pong(),
        std_native_table(),
        RuntimeConfig {
            workers: 1,
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
    let Some(main_fn) = rt.function_index("main") else {
        eprintln!("missing main");
        std::process::exit(1);
    };
    let handle = match rt.spawn(main_fn, &[]) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("spawn: {e}");
            std::process::exit(1);
        }
    };
    let outcome = handle.join();
    let metrics = rt.metrics();
    rt.shutdown();

    match outcome {
        FlowOutcome::Completed(Value::Int(2)) => {
            println!("pong replied 2 (Atomic Hop)");
            println!("{metrics}");
        }
        other => {
            eprintln!("unexpected {other:?}");
            std::process::exit(1);
        }
    }
}
