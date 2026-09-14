# BEAM → Byteflow (mental model)

Guide for Erlang/Elixir developers reading this codebase. Byteflow is **not**
BEAM — but many ideas rhyme once you map terminology.

## Units

| BEAM | Byteflow | Notes |
|------|----------|-------|
| process | **flow** | `FlowId`, `FlowHandle`, `FlowOutcome` |
| `pid()` | **`hop_sender(msg)`** | Identity inside a *delivered* hop |
| `pid()` as address | **`Value::Cap`** | `SelfPid` / `Spawn` return Caps, not Pids |
| registered name | **`Fn::register_name` / `Fn::whereis`** (also host `Runtime::*`) | Stores a **Cap**, not a FlowId. Bytecode `whereis` remints SEND for the caller. Swept on exit. |

**Key difference:** on BEAM, a Pid is both identity and delivery address. In
Byteflow, **Cap = address**, **Pid = identity** inside `Message.sender`.

```text
BEAM:     send(Pid, Term)
Byteflow: send(Cap, Message)   // Message envelope required
```

See [`atomic-hop.md`](atomic-hop.md) and [`security.md`](security.md) (S1, S6).

## Messaging

| BEAM | Byteflow API | Opcode / host |
|------|--------------|---------------|
| `spawn(fun)` | `Fn::spawn(fn, argc)` | `Spawn` → Cap |
| `Pid ! Msg` | `Fn::send(cap, hop)` | `Send` |
| `receive` | `Fn::receive()` | `Receive` |
| selective receive (pattern) | `Fn::receive_match_imm(tag)` | `ReceiveMatchImm` — **tag u16 only** |
| `gen_server:call` | `Fn::ask(cap, hop)` / `Fn::ask_timeout` | `Ask` / `AskTimeout` |
| `gen_server:cast` | `Fn::send(cap, hop)` | fire-and-forget |
| reply to caller | `Fn::send_reply(req, tag, payload)` | uses `msg_reply_cap` |
| build message | `Fn::hop(req_id, tag, payload)` | scheduler stamps sender |
| unpack message | `Fn::hop_payload(msg)` etc. | std natives |

### Message shape

BEAM messages are arbitrary terms. Byteflow **Atomic Hop** is a fixed envelope:

```text
Message { sender, reply_cap, request_id, tag, payload }
```

- `payload` is a nested [`Value`](../src/bytecode/value.rs) (ABI v5) — scalars,
  `Str` / `Bytes`, or another hop.
- `sender` is **authenticated by the runtime** on bytecode `Send` / `Ask`.
  `make_msg` has no sender operand (3-arg: request_id, tag, payload). A
  leftover 4-arg encoding is ignored — see
  [`samples::forged_sender_send`](../src/samples.rs).

### Typical server loop (BEAM-style)

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use byteflow::{FlowOutcome, Program, Runtime, Value, std_native_table};

const TAG_REQ: i32 = 1;
const TAG_REP: i32 = 2;

let mut program = Program::new("server");
let server = program.function("server", 0, |f| {
    let req = f.receive_match_imm(TAG_REQ as u16);
    let payload = f.hop_payload(req);
    f.add_imm(payload, 1);
    f.send_reply(req, TAG_REP, payload);
    f.exit(payload);
});
program.function("main", 0, |f| {
    let cap = f.spawn(server, 0);
    let n = f.load_i32(41);
    let req = f.hop_fresh(TAG_REQ, n);
    let reply = f.ask(cap, req);
    let out = f.hop_payload(reply);
    f.return_(out);
});

let rt = Runtime::with_natives(program.build(), std_native_table())?;
let Some(main) = rt.function_index("main") else {
    return Err("missing main".into());
};
let outcome = rt.spawn(main, &[])?.join();
rt.shutdown();
assert!(matches!(outcome, FlowOutcome::Completed(Value::Int(42))));
# Ok(())
# }
```

Sample helper (same shape): [`samples::server_loop`](../src/samples.rs).

## Lifecycle (mapped)

| BEAM / OTP | Byteflow today |
|------------|----------------|
| **links** | `Runtime::link` / `Fn::link` — abnormal exit kills the peer |
| **monitors** `{'DOWN', ...}` | `Runtime::monitor` / `Fn::monitor` → `TAG_SYS_DOWN` hop |
| **OTP supervisor strategies** | Host `Supervisor` + `RestartStrategy` (`OneForOne` / `OneForAll` / `RestForOne`) |
| **`register` / `whereis`** | `Fn::register_name` / `Fn::whereis` (and host `Runtime::*`). Cap, not FlowId |
| **`exit(Pid, kill)`** | `Runtime::kill` (cooperative) or `Runtime::admin_kill` (ADMIN Cap) |
| **capability pass** | `Fn::delegate` / `Cap::attenuate` (AND of rights + native mask) |
| **confined spawn** | `Fn::spawn_confined` (child rights `NONE`) |

## What BEAM has that Byteflow does not (yet)

| BEAM / OTP | Byteflow today |
|------------|----------------|
| **distribution** | Single process, in-memory |
| **pattern matching receive** | Tag-based selective receive only |
| **process dictionary** | No |
| **ETS** | No |
| **`trap_exit`** | No — links always kill on abnormal exit |

Failures surface as `FlowOutcome::Failed` on `join`, not as mailbox messages.

## Backpressure

| Path | Mailbox full + `Reject` |
|------|-------------------------|
| `Runtime::send(FlowId, …)` | `Err(SendError::MailboxFull)` |
| bytecode `Send` / `Ask` | sender parks (`WAITING_SEND`); one waiter per freed slot |

## Host Rust vs bytecode

| Task | Where |
|------|-------|
| spawn top-level flows | `Runtime::spawn` |
| trusted host send | `Runtime::send(FlowId, Value::Message)` |
| restart policy | `Supervisor` (Rust) |
| protocol in flows | `Program` / `Fn` bytecode |

Supervisor trees are **not** expressed as bytecode flows today — plan host
Rust for OTP-style supervision.

## Quick equivalence cheat sheet

```text
self()              →  hop_sender on received msg; self_address() for Cap
!                   →  send(cap, hop(...))
receive             →  receive() / receive_match_imm(TAG)
call                →  ask(cap, hop(...)) / ask_timeout(cap, hop, ms)
reply               →  send_reply(req, TAG_REP, payload)
spawn               →  spawn(fn) → Cap; spawn_confined(fn) → Cap with rights NONE
register/whereis    →  Fn::register_name / whereis (Cap reminted for caller); host Runtime::*; ChildSpec.name also registers
link/monitor        →  Fn::link / Fn::monitor (need LINK / MONITOR on the addressing Cap)
delegate            →  Fn::delegate(cap, rights) → weaker Cap
```

## Further reading

- [`atomic-hop.md`](atomic-hop.md) — protocol details
- [`mailbox.md`](mailbox.md) — bounded queues, overflow
- [`security.md`](security.md) — S1 authenticated sender, S6 FlowCap, S7 native mask
- [`samples.rs`](../src/samples.rs) — runnable specs
