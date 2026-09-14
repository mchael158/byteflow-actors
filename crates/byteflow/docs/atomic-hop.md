# Atomic Hop (Byteflow core)

Handoff context for humans / other AIs working on **this repo only** (`byteflow-actors`).
Hardware (`byteflow-hw`) was removed from the monorepo — do not restore it here.

## Units: **flows**

Byteflow's concurrent unit is a **flow** (`Flow`, `FlowId`, `FlowHandle`, `FlowOutcome`) — not an “actor” API surface.

Wire identity still uses `Value::Pid` (FlowId as `u64`) inside messages.
**Addressing** for bytecode `Send` / `Ask` uses `Value::Cap` (FlowCap, ABI v5 / 0.9.3).
Scalars include `Value::Str` / `Value::Bytes` in the constant pool; hops remain
`Message`-only. Untrusted `.bf` loads reject `Cap` / `Pid` / `Message` in the
pool unless [`TrustLevel::Trusted`](../src/bytecode/verify.rs) is set.

## What “Atomic Hop” means

Every `Send` (bytecode or `Runtime::send`) must carry a full [`Message`](../src/bytecode/value.rs) envelope:

```text
Message { sender, reply_cap, request_id, tag, payload }
```

- **sender** — authenticated origin FlowId (stamped by the worker)
- **reply_cap** — SEND-only Cap back to the sender. One live token per
  `(recipient, sender)` pair (`CapTable::mint_or_reuse`); correlation is
  `request_id`, not a fresh Cap per hop.
- **request_id** — correlation token. Prefer [`Fn::fresh_request_id`] /
  [`Fn::hop_fresh`] (per-flow, starts at 1). `0` is “unset”: `Send` / `Ask`
  mint a fresh id at the hop boundary. `Ask` refuses a second in-flight
  `Ask` that reuses a still-pending id.
- **tag** — protocol discriminator (opaque to the VM)
- **payload** — arbitrary [`Value`](../src/bytecode/value.rs) (nested scalars, blobs, etc.)

**Authenticated sender (security S1):** bytecode `Send` / `Ask` overwrite
`Message.sender` and attach `reply_cap` before delivery.
Host `Runtime::send` uses the same choke-point: `sender = FlowId::HOST`
(`0`), `reply_cap = CapId::NONE` (host has no mailbox), and an unset
`request_id` is minted. The `make_msg` native has **no sender operand**
(3-arg: request_id, tag, payload). A 4-arg legacy form discards the first
register and still writes `sender = 0` until the hop is authenticated —
see [`security.md`](security.md).

**FlowCap (security S6):** bytecode `Send` / `Ask` targets must be `Value::Cap`.
`SelfPid` / `Spawn` return Caps. Reply with `msg_reply_cap`, not `msg_sender`.

Bare scalars (`Int`, `Pid`, …) on `Send` → VM trap / `SendError::NotAHop`.  
Mailbox park/push share one mutex → no lost-wakeup (`scheduler/mailbox/`).
Inboxes are **bounded** — see [`mailbox.md`](mailbox.md).

### Selective receive (`ReceiveMatch`)

`Receive` takes the next hop. `ReceiveMatch` / `ReceiveMatchImm` wait for a
hop whose `Message.tag` matches — earlier non-matching hops stay in the
mailbox (**FIFO skip**, never drop). A parked selective waiter is woken only
by a matching hop; junk is queued behind the same lock.

| Opcode | Form |
|--------|------|
| `ReceiveMatch` `0x53` | `ra, rb` — tag from `r[b]` (`Int` in `0..=u16::MAX`) |
| `ReceiveMatchImm` `0x54` | `ra, imm` — immediate tag |
| `FreshRequestId` `0x5C` | `ra` — next per-flow correlation id |
| `ReceiveMatchCorr` `0x5D` | `ra, rb, rc` — `tag == r[b]` and `request_id == r[c]` |
| `ReceiveMatchCorrImm` `0x5E` | `ra, rb, imm` — immediate tag + `request_id` from `r[b]` |

Sample: [`samples::selective_receive`](../src/samples.rs) (`TAG_JUNK` then `TAG_REQ`).

### `Ask` — atomic RPC hop (`0x55`)

`Ask ra, rb, rc` delivers `r[c]` (`Message`) to `r[b]` (`Cap`), then parks the
caller until a reply matches:

```text
reply.request_id == request.request_id
&& reply.sender  == resolved_FlowId(target_cap)
```

Implemented via mailbox `WaitFilter::Correlation { expect_request_id, expect_sender: Some(flow_id) }`.
FIFO skip applies: unrelated hops (wrong id or wrong sender) stay queued.

`AskTimeout` (`0x5A`) is the same hop plus a deadline: the dest register
gets `Value::Unit` if no correlated reply arrives in time (same writeback
as `ReceiveTimeout`). If the **target exits** first, dest is a
`TAG_SYS_EXIT` hop instead (see [`lifecycle.md`](lifecycle.md)). Sample:
[`samples::ask_reply`](../src/samples.rs),
[`samples::ask_timeout_expires`](../src/samples.rs),
[`samples::ask_target_exits`](../src/samples.rs).

**Cap in payload:** any `Value::Cap` the sender *holds* is reissued so the
recipient becomes the new holder (`CapTable::delegate`). `NONE` is left
alone. This is additive (the sender keeps their token).

That is the deliberate difference vs classic actor runtimes that allow any value on send.

## Make / unpack (std natives)

Stable indices in [`natives.rs`](../src/natives.rs):

| Idx | Name | Args → result |
|----:|------|----------------|
| 0 | `print` | values… → `Unit` (flow-visible log) |
| 1 | `now_ms` | → `Int` |
| 2 | `make_msg` | request_id, tag, payload → `Message` (`sender` stamped on Send) |
| 3 | `msg_sender` | msg → `Pid` (identity) |
| 4 | `msg_request_id` | msg → `Int` |
| 5 | `msg_tag` | msg → `Int` |
| 6 | `msg_payload` | msg → any `Value` |
| 7 | `msg_reply_cap` | msg → `Cap` (SEND grant) |

Runtime must use `Runtime::with_natives(chunk, std_native_table())`,
[`Runtime::with_std_natives_and_config`](../src/scheduler/runtime.rs), or a custom table.

## Sample + refresh

Runnable host sketch (same API the examples use):

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use byteflow::{FlowOutcome, Program, Runtime, Value, std_native_table};

const TAG_PING: i32 = 10;
const TAG_PONG: i32 = 11;

let mut program = Program::new("ping-pong");
let pong = program.function("pong", 0, |f| {
    let msg = f.receive();
    let payload = f.hop_payload(msg);
    f.add_imm(payload, 1);
    f.send_reply(msg, TAG_PONG, payload);
    f.exit(payload);
});
program.function("main", 0, |f| {
    let child = f.spawn(pong, 0);
    let payload = f.load_i32(1);
    let req = f.hop_fresh(TAG_PING, payload);
    let rid = f.hop_request_id(req);
    f.send(child, req);
    let reply = f.receive_match_corr_imm(TAG_PONG as u16, rid);
    let out = f.hop_payload(reply);
    f.return_(out);
});

let rt = Runtime::with_natives(program.build(), std_native_table())?;
let Some(main) = rt.function_index("main") else {
    return Err("missing main".into());
};
let outcome = rt.spawn(main, &[])?.join();
rt.shutdown();
assert!(matches!(outcome, FlowOutcome::Completed(Value::Int(2))));
# Ok(())
# }
```

```text
# unit + sample tests
cargo test -p byteflow-actors

# demos (assemble Program + run Runtime — see examples/)
cargo run -p byteflow-actors --example ping_pong
cargo run -p byteflow-actors --example atomic_actors

# Windows PowerShell — scheduler logs
$env:BYTEFLOW_LOG="info"
cargo run -p byteflow-actors --example atomic_actors
```

## Hard rules (do not regress)

1. Opcodes are **append-only** — never renumber.
2. Std natives **0–6 frozen**; **7** is `msg_reply_cap` (append-only thereafter).
3. Fail-closed: no `PoisonError::into_inner()`; mutex helpers → `Result`.
4. Preserve long design comments (mailbox, directory, oneshot, timer, sync_lock).
5. Clippy: `unwrap_used` + `expect_used` = deny (including tests).
