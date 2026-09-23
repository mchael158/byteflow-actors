# HostAwait — async bridge (host ↔ flow)

Byteflow workers must never block on I/O. [`Opcode::HostAwait`](../src/bytecode/opcode.rs)
(`0x66`) parks the **flow** and hands work to the embedder; when the host is
done it writes a [`Value`](../src/bytecode/value.rs) back into a register
(same writeback path as `Receive` / `Ask`).

This is **not** a `CallNative` slot and is **not** in `std_native_*`. Tokio
(or any thread pool) stays outside the crate.

## Contract

```text
bytecode HostAwait(op, args)
        │
        ▼
worker parks flow ──► HostAwaitBridge::submit(req, Completer)
        │                      │
        │                      ▼
        │              host thread / Tokio / …
        │                      │
        │                      ▼
        │              Completer::complete(Value)
        │              Completer::fail(msg)
        ▼
resume_or_fail → re-enqueue Ready
```

| Piece | Role |
|-------|------|
| [`HostAwaitBridge`](../src/scheduler/host_await.rs) | Host hook; `submit` must return quickly |
| [`HostAwaitRequest`](../src/scheduler/host_await.rs) | `{ flow, op, args }` |
| [`HostAwaitCompleter`](../src/scheduler/host_await.rs) | One-shot `complete` / `fail` |
| [`RuntimeConfig::host_await`](../src/scheduler/runtime.rs) | `None` ⇒ HostAwait fails closed |
| [`RuntimeConfig::max_host_awaits`](../src/scheduler/runtime.rs) | Process-wide park cap (`0` = unlimited) |

One outstanding `HostAwait` per flow (same shape as Ask). Finalize / kill /
runtime shutdown invalidate the ticket; a late `complete` returns
[`HostAwaitError::Stale`](../src/scheduler/host_await.rs).
Dropping an unused completer fails the parked flow closed (avoids a permanent
wait if the host forgets to settle). Completers hold a weak handle to the
runtime so a bridge that retains them cannot pin the scheduler in a cycle.

## Assembler

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use std::sync::Arc;
use byteflow::{
    FlowOutcome, HostAwaitBridge, HostAwaitCompleter, HostAwaitRequest,
    Program, Runtime, RuntimeConfig, Value,
};

#[derive(Debug)]
struct Echo;
impl HostAwaitBridge for Echo {
    fn submit(&self, req: HostAwaitRequest, done: HostAwaitCompleter) {
        let _ = done.complete(req.args);
    }
}

let mut program = Program::new("demo");
program.function("main", 0, |f| {
    let args = f.load_i32(7);
    let out = f.host_await(1, args);
    f.return_(out);
});

let rt = Runtime::with_config(
    program.build(),
    RuntimeConfig {
        host_await: Some(Arc::new(Echo)),
        ..Default::default()
    },
)?;
let outcome = rt.spawn(0, &[])?.join();
rt.shutdown();
assert!(matches!(outcome, FlowOutcome::Completed(Value::Int(7))));
# Ok(())
# }
```

Example: `cargo run -p byteflow-actors --example host_await`.
