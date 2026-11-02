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

use byteflow::{
    samples, std_native_table, FlowOutcome, MailboxConfig, Runtime, RuntimeConfig, Value,
    DEFAULT_QUANTUM,
};

fn main() {
    let rt = match Runtime::with_natives_and_config(
        samples::atomic_actors(),
        std_native_table(),
        RuntimeConfig {
            workers: 2,
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
        FlowOutcome::Completed(Value::Int(72)) => {
            println!("atomic actors ok: two clients × 8 Ask = 72");
            assert!(
                metrics.messages_sent >= 34,
                "expected ≥34 hops (8 Ask+8 reply ×2 + 2 DONE), got {}",
                metrics.messages_sent
            );
            println!("{metrics}");
        }
        other => {
            eprintln!("unexpected {other:?}");
            std::process::exit(1);
        }
    }
}
