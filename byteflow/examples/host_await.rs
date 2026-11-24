//! HostAwait bridge: the flow keeps running; the host completes with a hop.
//!
//! ```text
//! cargo run -p byteflow-actors --example host_await
//! ```

use std::sync::Arc;
use std::time::Duration;

use byteflow::{
    FlowOutcome, HostAwaitBridge, HostAwaitCompleter, HostAwaitRequest, Program, Runtime,
    RuntimeConfig, Value,
};

#[derive(Debug)]
struct DelayEcho;

impl HostAwaitBridge for DelayEcho {
    fn submit(&self, req: HostAwaitRequest, done: HostAwaitCompleter) {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            let value = match req.args {
                Value::Int(n) => Value::Int(n + 1),
                other => other,
            };
            if let Err(e) = done.complete(value) {
                eprintln!("completer: {e}");
            }
        });
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut program = Program::new("host-await-demo");
    program.function("main", 0, |f| {
        let args = f.load_i32(41);
        let _ticket = f.host_await(1, args);
        let msg = f.receive();
        f.return_(msg);
    });

    let rt = Runtime::with_config(
        program.build(),
        RuntimeConfig {
            workers: 1,
            host_await: Some(Arc::new(DelayEcho)),
            ..Default::default()
        },
    )?;
    let outcome = rt.spawn(0, &[])?.join();
    rt.shutdown();

    match outcome {
        FlowOutcome::Completed(Value::Message(msg))
            if matches!(msg.payload.as_ref(), Value::Int(42)) =>
        {
            println!("host_await ok: bridge returned 42");
            Ok(())
        }
        other => Err(format!("unexpected {other:?}").into()),
    }
}
