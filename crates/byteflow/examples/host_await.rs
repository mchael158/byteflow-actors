//! HostAwait bridge: park a flow, complete from a host thread.
//!
//! ```text
//! cargo run -p byteflow-actors --example host_await
//! ```

use std::sync::Arc;
use std::time::Duration;

use byteflow::{
    FlowOutcome, HostAwaitBridge, HostAwaitCompleter, HostAwaitRequest, MailboxConfig, Program,
    Runtime, RuntimeConfig, Value, DEFAULT_QUANTUM,
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

fn main() {
    let mut program = Program::new("host-await-demo");
    program.function("main", 0, |f| {
        let args = f.load_i32(41);
        let out = f.host_await(1, args);
        f.return_(out);
    });

    let rt = match Runtime::with_config(
        program.build(),
        RuntimeConfig {
            workers: 1,
            quantum: DEFAULT_QUANTUM,
            mailbox: MailboxConfig::DEFAULT,
            host_await: Some(Arc::new(DelayEcho)),
            ..Default::default()
        },
    ) {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("runtime: {e}");
            std::process::exit(1);
        }
    };

    let outcome = match rt.spawn(0, &[]) {
        Ok(h) => h.join(),
        Err(e) => {
            eprintln!("spawn: {e}");
            std::process::exit(1);
        }
    };
    rt.shutdown();

    match outcome {
        FlowOutcome::Completed(Value::Int(42)) => {
            println!("host_await ok: bridge returned 42");
        }
        other => {
            eprintln!("unexpected {other:?}");
            std::process::exit(1);
        }
    }
}
