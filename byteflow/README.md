# byteflow-actors

**Run thousands of small, isolated, crash-proof flows inside your Rust app.**

[![crates.io](https://img.shields.io/crates/v/byteflow-actors.svg)](https://crates.io/crates/byteflow-actors)
[![docs.rs](https://docs.rs/byteflow-actors/badge.svg)](https://docs.rs/byteflow-actors)
[![ci](https://github.com/mchael158/byteflow-actors/actions/workflows/ci.yml/badge.svg)](https://github.com/mchael158/byteflow-actors/actions/workflows/ci.yml)
[![license](https://img.shields.io/crates/l/byteflow-actors.svg)](#license)

Byteflow is an embeddable runtime for **flows**: lightweight processes in the
Erlang mould. Every flow has its own registers and mailbox, is preempted after
a fixed number of instructions, and can crash without taking anything else
down. Supervisors restart what dies. A sandbox caps CPU, memory, spawns and
messages per flow, and capabilities decide which flow may talk to which.

It is one crate with one dependency (`getrandom`) and `#![deny(unsafe_code)]`.
You describe flows in Rust with a small builder, and they compile to verified
bytecode:

```rust
use byteflow::{std_native_table, FlowOutcome, Program, Runtime, Value};

const PING: i32 = 10;
const PONG: i32 = 11;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut program = Program::new("ping-pong");
    let pong = program.function("pong", 0, |f| {
        let msg = f.receive();
        let payload = f.hop_payload(msg);
        f.add_imm(payload, 1);
        f.send_reply(msg, PONG, payload); // answer through the sender's reply address
        f.exit(payload);
    });
    program.function("main", 0, |f| {
        let child = f.spawn(pong, &[]);
        let n = f.load_i32(41);
        let ping = f.hop_fresh(PING, n);
        let id = f.hop_request_id(ping);
        f.send(child, ping);
        let reply = f.receive_match_corr_imm(PONG as u16, id); // wait for *this* reply
        let out = f.hop_payload(reply);
        f.return_(out);
    });

    let rt = Runtime::with_natives(program.build(), std_native_table())?;
    let main = rt.function_index("main").ok_or("no main")?;
    let outcome = rt.spawn(main, &[])?.join();
    rt.shutdown();
    assert!(matches!(outcome, FlowOutcome::Completed(Value::Int(42))));
    Ok(())
}
```

```toml
[dependencies]
byteflow-actors = "0.9"   # imported as `byteflow`
```

## Why you might want it

- **Plugins and mods your users write.** Load them at runtime as `.bf`
  modules, which go through a static verifier before anything runs. A plugin
  stuck in an infinite loop ends as `Failed("flow CPU budget exhausted")`.
  Your server keeps running.
- **Rules, automations and scripts per customer.** Each tenant gets flows with
  their own quotas and only the host functions you grant them.
- **Simulations and game entities.** Give each entity its own flow and let
  entities talk through messages instead of shared state.
- **Concurrent protocol code** where "let it crash and restart it" is simpler
  than defending every error path.

## Three guarantees, in code

**A runaway guest is stopped, not waited on.** The guest below never returns.
Under `RuntimeConfig::sandbox()` it gets a CPU budget, no native functions
and minimum authority:

```rust
use std::time::Duration;
use byteflow::{FlowOutcome, NativeTable, Program, Runtime, RuntimeConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut guest = Program::new("guest");
    guest.function("spin", 0, |f| {
        let top = f.label();
        f.bind(top);
        f.jump(top); // loop forever
    });

    let rt = Runtime::with_natives_and_config(
        guest.build(),
        NativeTable::empty(),
        RuntimeConfig::sandbox(),
    )?;
    let outcome = rt
        .spawn_untrusted(0, &[])?
        .join_timeout(Duration::from_secs(5))
        .ok_or("the sandbox did not stop the guest")?;
    rt.shutdown();

    assert!(matches!(outcome, FlowOutcome::Failed(ref why) if why.contains("CPU budget")));
    Ok(())
}
```

**What crashes gets restarted.** A supervisor restarts a failing child until
it fails too often (OTP's "intensity"). Then it gives up and tells you:

```rust
use std::time::{Duration, Instant};
use byteflow::{
    ChildSpec, Program, RestartPolicy, RestartStrategy, Runtime, Supervisor, SupervisorConfig,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut program = Program::new("boom");
    let boom = program.function("boom", 0, |f| f.trap(1)); // always crashes
    let rt = Runtime::new(program.build())?;

    let sup = Supervisor::with_config(
        rt.spawner(),
        SupervisorConfig {
            max_restarts: 2,
            max_period: Duration::from_secs(5),
            strategy: RestartStrategy::OneForOne,
        },
    )?;
    sup.start_child(ChildSpec::new("boom", boom).restart(RestartPolicy::OnFailure))?;

    let deadline = Instant::now() + Duration::from_secs(5);
    while !sup.intensity_exceeded() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(sup.intensity_exceeded());
    assert_eq!(rt.metrics().processes_failed, 3); // first run + 2 restarts
    sup.shutdown();
    rt.shutdown();
    Ok(())
}
```

**Nothing reaches the VM unchecked.** `.bf` modules are decoded, then
verified (registers, jump targets, constants, function table) before a
runtime will start them. Bad bytes come back as an error value, not a panic:

```rust
use byteflow::{decode, encode, samples, verify};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    assert!(decode(b"not a module").is_err());

    let bytes = encode(&samples::ping_pong());
    let chunk = decode(&bytes)?;
    verify(&chunk)?;
    Ok(())
}
```

## From the command line

```console
$ cargo install byteflow-actors
$ byteflow demo ping-pong
2
spawned=2 completed=2 failed=0 messages=2 steals=0 reschedules=0

$ byteflow pack ping-pong ping_pong.bf
wrote ping_pong.bf (237 bytes, chunk "ping-pong")
$ byteflow verify ping_pong.bf
ok  ping-pong  2 functions  22 instructions
$ byteflow disasm ping_pong.bf
.chunk "ping-pong"
.abi 5
.fn "pong" entry=0 arity=0 regs=7
.fn "main" entry=11 arity=0 regs=8
0000  ReceiveMatchImm r0, tag=10  ; pong
0001  CallNativeFrom r1, r0, native[6]
0002  AddImm r1, r1, 1
...
```

`byteflow asm` turns a listing back into a module, and `byteflow run` runs one.

## How it compares

A rough guide. Each of these projects is excellent at what it was built for.

|                                            | Byteflow | Tokio tasks | Actix actors | Wasmtime | Erlang / BEAM |
|--------------------------------------------|:--------:|:-----------:|:------------:|:--------:|:-------------:|
| Embeds as a Rust library                   | ✓ | ✓ | ✓ | ✓ | – |
| A busy loop cannot starve its neighbours   | ✓ preempted by instruction count | – cooperative | – cooperative | ✓ fuel / epochs | ✓ |
| Supervisors and restart strategies         | ✓ OTP-style | – | ✓ | – | ✓ |
| Load untrusted code at runtime             | ✓ verified bytecode | – | – | ✓ | not sandboxed |
| Per-process CPU / memory / message quotas  | ✓ | – | – | partly | partly |
| Runs native-speed Rust inside the process  | via host functions | ✓ | ✓ | – | via NIFs |

Byteflow does not do networking or async I/O. Your host owns sockets and
files, and flows talk to the host through
[HostAwait](docs/host-await.md) and native functions. It is not a
cluster runtime either. Remote hops are frames that the host carries over its
own transport ([remote-transport](docs/remote-transport.md)).

## Numbers

Intel i5-9400F (6 cores), Windows 10, release build, default configuration.

| Workload | Result | Reproduce |
|----------|--------|-----------|
| spawn + join 50 000 trivial flows | **~225 000 flows/s** | `cargo run --release --example throughput` |
| one bytecode add, interpreter vs JIT | cycles per run | `cargo run --release --example cycles [--features jit]` |
| tight counting loop, interpreter vs JIT | wall time | `cargo run --release --example jit_loop --features jit` |

Criterion benches live in [`benches/`](benches).

## What's inside

| Layer | Role |
|-------|------|
| Bytecode | Register ISA, `Program`/`Fn` builder, `.bf` format, text assembler, static verifier |
| VM | One flow: registers, call stack, preemption slice, gated native calls |
| Scheduler | M:N work-stealing workers, bounded mailboxes, timer wheel, links, monitors, registry |
| Supervision | OTP strategies `OneForOne` / `OneForAll` / `RestForOne`, restart intensity |
| Security | Capabilities, quotas, trust levels, attestation, audit counters |
| JIT (optional) | Cranelift trace compiler for hot loops |
| CLI | `byteflow run / verify / disasm / asm / pack / demo` |

Runnable examples are in [`examples/`](examples). Guides
are rendered on docs.rs and tested where they contain code:
[atomic-hop](docs/atomic-hop.md) ·
[beam-mapping](docs/beam-mapping.md) ·
[lifecycle](docs/lifecycle.md) ·
[mailbox](docs/mailbox.md) ·
[vm-safety](docs/vm-safety.md) ·
[error-model](docs/error-model.md) ·
[host-await](docs/host-await.md) ·
[untrusted](docs/untrusted.md) ·
[security](docs/security.md) ·
[sandbox](docs/sandbox/) ·
[remote-transport](docs/remote-transport.md) ·
[production-embedding](docs/production-embedding.md)

[CHANGELOG](CHANGELOG.md).

## API contracts

The package on crates.io is `byteflow-actors`. The library is imported as
`byteflow` (`use byteflow::...`).

- `#![deny(unsafe_code)]`. The `jit` feature allows `unsafe` only inside the
  Cranelift backend.
- Default dependency: [`getrandom`](https://docs.rs/getrandom), for `CapId`.
  Cranelift is optional, behind `jit`.
- A panic inside a flow stays on the worker boundary. Malformed bytecode
  produces a `Fault` on that flow.
- Natives must not block, because they run on a worker thread.
- Host APIs return `Result`. See [`docs/error-model.md`](docs/error-model.md).
- Values: `Unit | Bool | Int | Float | Pid | Message | Cap | Str | Bytes | List | Map | Record`.
- `Send` accepts only `Value::Message` (an Atomic Hop). The scheduler stamps
  the sender and attaches a reply capability.
- FlowCap: a 128-bit CSPRNG `CapId`. A grant happens only through
  `Cap::attenuate`, `Fn::delegate` or a confined spawn.
- `CALL_NATIVE` checks `CapRights::NATIVE` and the `NativeMask` before the
  table index.
- Quotas: `QuotaConfig::permissive` (the default) or `sandbox`. Process heap is
  capped with `max_runtime_bytes` / `MemoryBudget`.
- Host `Runtime::spawn` defaults to `RootAuthorityPolicy::TrustedHost`
  (`ROOT` + full natives). `RuntimeConfig::sandbox()` does **not** change that.
  Use `spawn_untrusted` or `root_authority`.
- Mailboxes are bounded (default 256 hops / 4 MiB / Reject). A bytecode
  sender to a full mailbox parks until a slot frees, in FIFO order; the
  waiters are bounded by `max_flows` ([`docs/mailbox.md`](docs/mailbox.md)).
- Registry `whereis` returns a SEND Cap, never a FlowId.
- `.bf` ABI: `encode` picks the oldest header that fits the constant pool
  (v5 scalars, v6 lists, v7 maps, v8 records). `decode` accepts all four.
- Remote: `encode_frame` / `decode_frame` / `deliver_frame`, an optional
  transactional `remote_dedup_window`, and a bridge via `DistributionConfig`.

Collecting a result without blocking forever:

| Call | Behavior |
|---|---|
| `try_join()` | Non-blocking; `None` if still running |
| `join_timeout(d)` / `join_deadline(t)` | Bounded wait; `None` on timeout |
| `join()` | Blocks until the outcome |

Std native indices: `print = 0`, `now_ms = 1`, `make_msg = 2`, `msg_* = 3–6`,
`msg_reply_cap = 7`. Use `Runtime::with_natives(..., std_native_table())`.

Security detail: [`docs/security.md`](docs/security.md),
[`docs/untrusted.md`](docs/untrusted.md), [`docs/sandbox/`](docs/sandbox/).

## Status

Pre-1.0. The bytecode format is versioned and the decoder accepts every
published ABI, but the Rust API still changes between minor versions. The
CHANGELOG marks breaking changes. Default features build on Rust 1.88 or
newer, and CI tests Linux, Windows and macOS.

## Contributing

Issues and pull requests are welcome. See [CONTRIBUTING.md](https://github.com/mchael158/byteflow-actors/blob/main/CONTRIBUTING.md),
which includes the 60-second setup and the checks CI runs.

## License

MIT OR Apache-2.0, at your option.
