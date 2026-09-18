# byteflow-actors

[![crates.io](https://img.shields.io/crates/v/byteflow-actors.svg)](https://crates.io/crates/byteflow-actors)
[![docs.rs](https://docs.rs/byteflow-actors/badge.svg)](https://docs.rs/byteflow-actors)
[![license](https://img.shields.io/crates/l/byteflow-actors.svg)](https://github.com/mchael158/bytecode-vm)

**Byteflow** is a small, embeddable **flow** runtime for Rust: register-based bytecode, lightweight flows, **Atomic Hop** messaging (`Value::Message` only on `Send`), cooperative scheduling, and an OTP-style supervisor (`one-for-one` / `one-for-all` / `rest-for-one`) — without a separate scripting language.

You assemble programs with [`Program`](https://docs.rs/byteflow-actors/latest/byteflow/struct.Program.html) and [`Fn`](https://docs.rs/byteflow-actors/latest/byteflow/struct.Fn.html) in host Rust. The host owns I/O; Byteflow owns cheap concurrency.

> **Package name:** `byteflow-actors` on [crates.io](https://crates.io/crates/byteflow-actors)  
> **Rust import:** `use byteflow::...` (the library crate is named `byteflow`)

---

## Why this exists

Use Byteflow when you need **many isolated units of work** that talk through messages, share a handful of OS threads, and can fail without taking the worker down:

- plugin / rule / workflow engines inside a larger binary  
- simulations and game logic (not the render loop)  
- sandboxed “virtual processes” with a host-defined FFI table  

It is **not** a Tokio replacement, not a distributed cluster, and not a JVM.

---

## Install

```toml
[dependencies]
byteflow-actors = "0.9.5"
```

```rust
use byteflow::{FlowOutcome, Program, Runtime, Value};
```

CLI (same package):

```text
cargo install byteflow-actors
byteflow demo ping-pong
```

---

## Quick start

```rust
use byteflow::{FlowOutcome, Program, Runtime, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut program = Program::new("demo");
    program.function("main", 0, |f| {
        let a = f.load_i32(41);
        let b = f.load_i32(1);
        let sum = f.add(a, b);
        f.return_(sum);
    });

    let rt = Runtime::new(program.build())?;
    let outcome = rt.spawn(0, &[])?.join();
    rt.shutdown();

    assert!(matches!(outcome, FlowOutcome::Completed(Value::Int(42))));
    Ok(())
}
```

### Collecting a result without committing the thread

`join()` blocks until the flow finishes, which is right for a `main` that
has nothing else to do. Anything with a deadline — a control loop, a
watchdog, a test harness — picks its own bound instead:

| Call | Waits | While the flow is still running |
|---|---|---|
| `try_join()` | never | `None` |
| `join_timeout(d)` / `join_deadline(t)` | up to the bound | `None` |
| `join()` | unbounded | (blocks) |

```rust
use byteflow::{FlowOutcome, Program, Runtime, Value};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut program = Program::new("slow");
    program.function("main", 0, |f| {
        let ms = f.load_i32(300);
        f.sleep(ms);
        let out = f.load_i32(7);
        f.return_(out);
    });

    let rt = Runtime::new(program.build())?;
    let handle = rt.spawn(0, &[])?;

    // Poll for free, or wait under a bound — neither consumes the handle.
    assert!(handle.try_join().is_none());
    assert!(handle.join_timeout(Duration::from_millis(10)).is_none());

    let outcome = handle.join_timeout(Duration::from_secs(10));
    rt.shutdown();

    assert!(matches!(outcome, Some(FlowOutcome::Completed(Value::Int(7)))));
    Ok(())
}
```

The bounds are anchored on an absolute `Instant`, so a spurious condvar
wakeup cannot silently restart the budget. And a flow destroyed before
producing an outcome (`shutdown` does not drain suspended flows) wakes its
joiner with a failure rather than leaving it parked forever — see
[`docs/error-model.md`](docs/error-model.md).

### Std natives (`print`, `now_ms`, `make_msg`, …)

Stable indices: **`print = 0`**, **`now_ms = 1`**, **`make_msg = 2`** (3-arg: request_id, tag, payload; sender stamped on `Send`/`Ask`), **`msg_*` = 3–6**, **`msg_reply_cap = 7`**.

```rust
use byteflow::{std_native_table, Program, Runtime};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut program = Program::new("clock");
    program.function("main", 0, |f| {
        let n = f.load_i32(42);
        f.native1_on(n, 0);
        let ms = f.call_native0(1);
        f.return_(ms);
    });

    let rt = Runtime::with_natives(program.build(), std_native_table())?;
    let _ = rt.spawn(0, &[])?.join();
    rt.shutdown();
    Ok(())
}
```

Or `std_natives()` → `(NativeTable, HashMap<name, index>)` so host registration stays aligned with bytecode.

### Messaging (Atomic Hop)

Every `Send` carries one `Value::Message` envelope (scalars trap):

```text
cargo run --example ping_pong
cargo run --example atomic_actors
# optional scheduler logs on stderr:
# BYTEFLOW_LOG=info cargo run --example atomic_actors
```

Hop-heavy code uses helpers such as `hop_fresh`, `send`, `receive`, and `ask` on [`Fn`](https://docs.rs/byteflow-actors/latest/byteflow/struct.Fn.html) — see [`examples/ping_pong.rs`](examples/ping_pong.rs) (assembles `Program` + runs `Runtime` via `use byteflow::{...}`).

Built-in test helpers: `byteflow::samples::{ping_pong, atomic_actors, named_service, …}`.

---

## Architecture

| Layer | Responsibility |
|---|---|
| **Bytecode** | ISA, `Program` / `Fn`, BFV0 (`.bf`) encode/decode, static `verify` |
| **VM** | One flow: registers, call stack, cooperative slice (`min(quantum, CPU quota)`), `CallNative` (gated) |
| **Scheduler** | M:N workers, **bounded** FIFO mailboxes (park/wake), CapTable, quotas, timer, supervisor |
| **JIT** (feature `jit`) | Trace JIT for hot loops via Cranelift |
| **Facade** | Public API + std natives + samples + `byteflow` CLI |

**Flow lifecycle (sketch):**

1. Worker runs at most `min(quantum, remaining CPU quota)` instructions (default quantum 10 000).  
2. `Yield` / budget → run queue (stealable). CPU quota exhaustion fails the flow (`QuotaError`).  
3. `Sleep` → timer thread → injector.  
4. Empty `Receive` → flow parks **inside its mailbox**; the next Atomic Hop wakes under the same lock (no lost wakeup).  
5. `Fault` / `Trap` → `FlowOutcome::Failed` → supervisor (`Always` / `OnFailure` / `Never`; default intensity 3 / 5s).

`join()` and its bounded forms are for the embedder’s native thread only —
workers never block on them.

Untrusted `.bf` files go through `Opcode::from_u8` + `verify` before
execution. The verifier settles what is a property of the chunk (jump
targets, constant/function indices, `arity ≤ num_registers`); the VM checks
what depends on runtime state (register bounds, index overflow, types,
division, call depth) and turns each into a `Fault` on that one flow — see
[`docs/vm-safety.md`](docs/vm-safety.md).

---

## CLI

```text
byteflow demo [ping-pong|atomic|add]
byteflow pack  <demo> <out.bf>
byteflow verify <file.bf>
byteflow disasm <file.bf>
byteflow run    <file.bf> [function]
```

`run` and hop demos attach the std native table (`print`, `now_ms`, `make_msg`, `msg_*`).

---

## Safety & design notes

- `#![forbid(unsafe_code)]`
- **Default = zero runtime dependencies** (`std` only). Optional `feature = "jit"` adds Cranelift.
- Flow panics are caught at the worker boundary so one bad flow cannot kill the OS thread.
- Malformed bytecode is a `Fault` on one flow, never a panic on the thread that spawned it (see [`docs/vm-safety.md`](docs/vm-safety.md)).
- Register indices are never computed with plain `u8` arithmetic — no “panics in debug, silently wraps in release” divergence.
- Native functions must **not block** — they run inline on a worker.
- Host APIs return `Result` (`SpawnError` / `RuntimeError`) — no `unwrap`/`expect`/`unwrap_or*` anywhere (see [`docs/error-model.md`](docs/error-model.md)).
- Blocking APIs are bounded by choice: `try_join` / `join_timeout` / `join_deadline`, and an abandoned flow wakes its joiner instead of hanging it.
- Values today: `Unit | Bool | Int | Float | Pid | Message | Cap | Str | Bytes`.
- **Atomic Hop:** only `Value::Message` may cross `Send`.
- **FlowCap (ABI v5):** 128-bit CSPRNG `CapId`; holder + rights resolution; [`Cap::attenuate`](https://docs.rs/byteflow-actors/latest/byteflow/struct.Cap.html) is the only grant path (`Fn::delegate`, confined spawn).
- **Natives (S7):** `CALL_NATIVE` is gated by `CapRights::NATIVE` + [`NativeMask`](https://docs.rs/byteflow-actors/latest/byteflow/struct.NativeMask.html) before the table is indexed.
- **Quotas:** per-flow CPU / heap / spawn-send buckets via [`QuotaConfig`](https://docs.rs/byteflow-actors/latest/byteflow/struct.QuotaConfig.html) (`RuntimeConfig::quota`). Default is [`permissive`](https://docs.rs/byteflow-actors/latest/byteflow/struct.QuotaConfig.html#method.permissive); [`sandbox`](https://docs.rs/byteflow-actors/latest/byteflow/struct.QuotaConfig.html#method.sandbox) is the isolation starting point. `Str`/`Bytes` register stores charge heap with release-on-overwrite (delta accounting). Process-wide ceiling: [`RuntimeConfig::max_runtime_bytes`](https://docs.rs/byteflow-actors/latest/byteflow/struct.RuntimeConfig.html) + [`MemoryBudget`](https://docs.rs/byteflow-actors/latest/byteflow/struct.MemoryBudget.html) / [`HeapStr`](https://docs.rs/byteflow-actors/latest/byteflow/struct.HeapStr.html). Distinct from the scheduler quantum.
- **Registry:** bytecode `register_name` / `whereis` — lookup returns a SEND Cap, never a FlowId.
- **`make_msg`:** 3-arg (`request_id`, `tag`, `payload`); `sender` is stamped only on `Send` / `Ask`.
- **Security:** authenticated hop sender + FlowCap + Phase 3 gates — see [`docs/security.md`](docs/security.md).

---

## Status (v0.9.5)

**Included:** register ISA + `Program`/`Fn` assembler, BFV0 (**ABI v5**: 128-bit `CapId`, nested `Message.payload`), verifier (`TrustLevel::Untrusted` rejects `Cap`/`Pid`/`Message` in the constant pool), per-flow VM, M:N scheduler, **bounded mailboxes** (`MailboxConfig`: 256 hops + 4 MiB / Reject by default), Atomic Hop, FlowCap holder model, `Cap::attenuate` / `Opcode::Delegate`, `NativeMask` gate on `CALL_NATIVE`, per-flow quotas (`QuotaConfig::permissive` / `sandbox`), bytecode `register_name` / `whereis` (SEND Cap, never FlowId), host `Runtime::send` on the same hop auth path (`sender = 0`, `reply_cap = NONE`), `LINK`/`MONITOR`/`ADMIN`, `Fn::spawn_confined`, 3-arg `make_msg`, monitors / links / registry, `WAITING_SEND`, `AskTimeout`, `RuntimeConfig.max_flows`, OTP supervisor strategies, [`OutputSink`](https://docs.rs/byteflow-actors/latest/byteflow/trait.OutputSink.html) for `print`, std natives, CLI, examples, fail-closed error model, optional trace JIT (`feature = "jit"`), **in-house stress suite** (mailbox / decode / CapTable).

**Not yet:** Criterion benches, distribution, `trap_exit`, hermetic CI.

Design guides: [`docs/atomic-hop.md`](docs/atomic-hop.md) ·
[`docs/beam-mapping.md`](docs/beam-mapping.md) ·
[`docs/lifecycle.md`](docs/lifecycle.md) ·
[`docs/mailbox.md`](docs/mailbox.md) ·
[`docs/properties.md`](docs/properties.md) ·
[`docs/vm-safety.md`](docs/vm-safety.md) ·
[`docs/error-model.md`](docs/error-model.md) ·
[`docs/security.md`](docs/security.md)

See [`CHANGELOG.md`](CHANGELOG.md).

---

## Links

- Repository: [github.com/mchael158/bytecode-vm](https://github.com/mchael158/bytecode-vm)
- Docs: [docs.rs/byteflow-actors](https://docs.rs/byteflow-actors)
- License: **MIT OR Apache-2.0**
