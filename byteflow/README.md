# byteflow-actors

[![crates.io](https://img.shields.io/crates/v/byteflow-actors.svg)](https://crates.io/crates/byteflow-actors)
[![docs.rs](https://docs.rs/byteflow-actors/badge.svg)](https://docs.rs/byteflow-actors)
[![license](https://img.shields.io/crates/l/byteflow-actors.svg)](https://github.com/mchael158/byteflow-actors)

In-process flow runtime: register bytecode (`Program` / `Fn`), M:N scheduler,
Atomic Hop mailboxes (`Value::Message` only on `Send`), FlowCap, quotas, and an
OTP-style supervisor. Host Rust owns sockets and files; Byteflow owns flows.

> **Package:** `byteflow-actors` on [crates.io](https://crates.io/crates/byteflow-actors)  
> **Import:** `use byteflow::...`

Not Tokio. Not a cluster protocol. Not a JVM.

---

## Install

```toml
[dependencies]
byteflow-actors = "0.9.23"
```

```rust
use byteflow::{FlowOutcome, Program, Runtime, Value};
```

```text
cargo install byteflow-actors
byteflow demo ping-pong
```

## Module ABI

`encode` picks the oldest header that fits the constant pool: v5 scalars, v6
lists, v7 maps, v8 records. Decode accepts all four. Lists / maps / records max
256 elements. See [`docs/beam-mapping.md`](docs/beam-mapping.md).

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

### Join without parking the host forever

| Call | Behavior |
|---|---|
| `try_join()` | Non-blocking; `None` if still running |
| `join_timeout(d)` / `join_deadline(t)` | Bound wait; `None` on timeout |
| `join()` | Blocks until outcome |

Bounds use an absolute `Instant`. `shutdown` that abandons a suspended flow
wakes the joiner with failure — [`docs/error-model.md`](docs/error-model.md).

### Std natives

Indices: `print = 0`, `now_ms = 1`, `make_msg = 2`, `msg_* = 3–6`,
`msg_reply_cap = 7`. Use `Runtime::with_natives(..., std_native_table())` or
`std_natives()` for a name→index map.

### Messaging

```text
cargo run --example ping_pong
cargo run --example atomic_actors
# BYTEFLOW_LOG=info cargo run --example atomic_actors
```

Hop helpers on `Fn`: `hop_fresh`, `send`, `receive`, `ask`. Samples:
`byteflow::samples::{ping_pong, atomic_actors, named_service, …}`.

---

## Architecture

| Layer | Role |
|---|---|
| Bytecode | ISA, `Program`/`Fn`, BFV0 encode/decode, `verify` |
| VM | One flow: registers, call stack, slice `min(quantum, CPU quota)`, gated `CallNative` |
| Scheduler | M:N workers, bounded mailboxes, CapTable, quotas, timer wheel, per-flow table, supervisor |
| JIT (`feature = "jit"`) | Cranelift trace JIT |
| Facade | Public API, std natives, samples, `byteflow` CLI |

Worker slice default quantum: 10 000 instructions. Empty `Receive` parks in the
mailbox under the same lock as push. Fault → `FlowOutcome::Failed` → supervisor
(`Always` / `OnFailure` / `Never`). Untrusted `.bf` goes through `verify` before
workers start — [`docs/vm-safety.md`](docs/vm-safety.md).

---

## CLI

```text
byteflow demo [ping-pong|atomic|selective|selective-after|match-eq|exit|dict|ask|ask-timeout|server-loop|monitor|alive|add]
byteflow pack  <demo> <out.bf>
byteflow verify <file.bf>
byteflow disasm <file.bf>
byteflow run    <file.bf> [function]
```

---

## Contracts

- `#![deny(unsafe_code)]` — `jit` allows `unsafe` only in the Cranelift backend
- Default dep: [`getrandom`](https://docs.rs/getrandom) for `CapId`; optional Cranelift under `jit`
- Flow panics stay on the worker boundary; malformed bytecode → `Fault` on that flow
- Natives must not block (they run on a worker)
- Host APIs return `Result` — [`docs/error-model.md`](docs/error-model.md)
- Values: `Unit | Bool | Int | Float | Pid | Message | Cap | Str | Bytes | List | Map | Record`
- Send path: only `Value::Message`
- FlowCap: 128-bit CSPRNG `CapId`; grant only via `Cap::attenuate` / `Fn::delegate` / confined spawn
- `CALL_NATIVE`: `CapRights::NATIVE` + `NativeMask` before table index
- Quotas: `QuotaConfig::permissive` (default) or `sandbox`; process heap via `max_runtime_bytes` / `MemoryBudget`
- Host `Runtime::spawn` defaults to `RootAuthorityPolicy::TrustedHost` (`ROOT` + full natives). `RuntimeConfig::sandbox()` does **not** change that — use `spawn_untrusted` or `root_authority`
- Registry `whereis` returns a SEND Cap, never a FlowId
- Remote: `encode_frame` / `decode_frame` / `deliver_frame`; optional `remote_dedup_window` (transactional); bridge via `DistributionConfig` (hops + bytes)
- Security detail: [`docs/security.md`](docs/security.md), [`docs/untrusted.md`](docs/untrusted.md), [`docs/sandbox/`](docs/sandbox/)

---

## Status (v0.9.23)

Register ISA + assembler, BFV0 ABI v5–v8, verifier (`TrustLevel::Untrusted` rejects
Cap/Pid/Message constants), M:N scheduler, bounded mailboxes (default 256 hops /
4 MiB / Reject), Atomic Hop, FlowCap + `Delegate` + `NativeMask`, quotas,
`register_name` / `whereis`, host `send` on the hop auth path, link/monitor/admin,
`spawn_confined`, `trap_exit`, selective receive (+ `after`), process dictionary,
`StartSupervisor` / `StartChild`, HostAwait, `fingerprint_bf` / `with_attested`,
relation caps, OTP supervisor strategies, std natives, CLI, examples, optional
JIT, Criterion benches, hermetic CI.

Also: maps/records opcodes, per-flow tables, remote frames + reference TCP/UDP
transport ([`docs/remote-transport.md`](docs/remote-transport.md)),
`DistributionConfig` byte budget, transactional `RemoteFrameDedup`,
`RootAuthorityPolicy` / `spawn_untrusted`, hierarchical timing wheel.

Out of scope: cluster protocol, global ETS, tuples.

### Guides

[`atomic-hop`](docs/atomic-hop.md) ·
[`beam-mapping`](docs/beam-mapping.md) ·
[`lifecycle`](docs/lifecycle.md) ·
[`mailbox`](docs/mailbox.md) ·
[`properties`](docs/properties.md) ·
[`host-await`](docs/host-await.md) ·
[`remote-transport`](docs/remote-transport.md) ·
[`vm-safety`](docs/vm-safety.md) ·
[`error-model`](docs/error-model.md) ·
[`untrusted`](docs/untrusted.md) ·
[`security`](docs/security.md) ·
[`production-embedding`](docs/production-embedding.md) ·
[`audit-checklist`](docs/audit-checklist.md) ·
[`sandbox/`](docs/sandbox/)

[`CHANGELOG.md`](CHANGELOG.md)

---

## Links

- [github.com/mchael158/byteflow-actors](https://github.com/mchael158/byteflow-actors)
- [docs.rs/byteflow-actors](https://docs.rs/byteflow-actors)
- License: **MIT OR Apache-2.0**
