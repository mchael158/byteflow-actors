# HostAwait — async bridge (host ↔ flow)

Byteflow workers must never block on I/O. [`Opcode::HostAwait`](../src/bytecode/opcode.rs)
(`0x66`) registers a host operation and writes its **ticket id** (`Int`) into
a register. The flow keeps running. When the host finishes, the runtime
delivers an authenticated [`TAG_SYS_AWAIT`](../src/bytecode/value.rs) hop
(`request_id` = ticket, `sender` = 0, `reply_cap` = none). Bytecode reads
that hop with `Receive`.

This is **not** a `CallNative` slot and is **not** in `std_native_*`. Tokio
(or any thread pool) stays outside the crate. The opcode does not open a
socket.

## Contract

```text
bytecode HostAwait(op, args)
        │
        ▼
worker registers a ticket, writes it into r[a], keeps the flow
        │
        ▼
HostAwaitBridge::submit(req, Completer)   // returns promptly
        │
        ▼
host thread / Tokio / …
        │
        ▼
Completer::complete(Value)  ──►  authenticated TAG_SYS_AWAIT hop
Completer::fail(msg)         ──►  flow fails
        │
        ▼
Receive  (the flow is still the one running, one quantum at a time)
```

| Piece | Role |
|-------|------|
| [`HostAwaitBridge`](../src/scheduler/host_await.rs) | Host hook; `submit` must return quickly. `cancel` aborts after the registry lock is released |
| [`HostAwaitRequest`](../src/scheduler/host_await.rs) | `{ flow, op, args }` |
| [`HostAwaitCompleter`](../src/scheduler/host_await.rs) | One-shot `complete` / `fail`. `ticket()` is the id in the register. `is_cancelled()` is the cooperative abort flag |
| [`RuntimeConfig::host_await`](../src/scheduler/runtime.rs) | `None` ⇒ HostAwait fails closed |
| [`RuntimeConfig::max_host_awaits`](../src/scheduler/runtime.rs) | Process-wide ticket cap (`0` = unlimited) |

A flow may have several tickets in flight. Each `complete` and each
cancellation compete to remove that ticket once. Finalize, kill, and
runtime shutdown cancel every ticket of the flow. A late `complete`
returns [`HostAwaitError::Stale`](../src/scheduler/host_await.rs) and does
not deliver. Dropping an unused completer fails the flow closed.
Completers hold a weak handle to the runtime so a bridge that retains
them cannot pin the scheduler in a cycle.

The ticket id is never reused. That id is the generation a late result
must still match: once the ticket is gone, the hop is not sent.

`cancel` does not run host code under the registry lock, and it does not
close a socket by itself. The host watches `is_cancelled` or the `cancel`
callback and aborts the I/O it owns.

`fail` records that text for the worker. A `return` in the same turn
sees it and ends `Failed` with the host's message. After the flow has
already committed an outcome, `fail` returns `Stale`.

Completion uses the same mailbox admission as `Runtime::send`. A closed
inbox returns `Stale`. A full inbox returns `Mailbox` and fails the flow
if it is still alive. A replay sequence that cannot be accepted returns
`Replay`. One refused hop does not discard the later ones in that script.

## Assembler

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use std::sync::Arc;
use byteflow::{
    FlowOutcome, HostAwaitBridge, HostAwaitCompleter, HostAwaitRequest,
    Program, Runtime, RuntimeConfig, Value, TAG_SYS_AWAIT,
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
    let _ticket = f.host_await(1, args);
    let msg = f.receive();
    f.return_(msg);
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
assert!(matches!(
    outcome,
    FlowOutcome::Completed(Value::Message(ref msg))
        if msg.tag == TAG_SYS_AWAIT && matches!(msg.payload.as_ref(), Value::Int(7))
));
# Ok(())
# }
```

Example: `cargo run -p byteflow-actors --example host_await`.
