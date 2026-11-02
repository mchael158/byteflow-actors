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
| `spawn(fun)` | `Fn::spawn(fn, &[args])` | `Spawn` → Cap |
| `Pid ! Msg` | `Fn::send(cap, hop)` | `Send` |
| `receive` | `Fn::receive()` | `Receive` |
| selective receive (pattern) | `Fn::receive_match_imm(tag)` / `Fn::receive_match_kind(kind)` / `Fn::receive_match_eq(tag, int)` / `Fn::receive_match_cmp(tag, bound, cmp)` / `Fn::receive_match_value(tag, value)` | Tag, payload wire-tag (`0..=10`), tag + `Int`, tag + `Eq`/`Lt`/`Le` on an `Int`, or tag + any `Value`. No tuples |
| map | `Fn::map_pack` / `map_get` / `map_len` | At most 256 pairs. Keys are `Int` or `Str`. A missing key is `Unit`. A module that stores a map and no record is ABI v7 |
| record | `RecordPack` / `RecordGet` / `RecordLen` | Positional fields, at most 256. Not a tuple and not a receive pattern. A module that stores a record is ABI v8 |
| `ets` | `Fn::table_open` / `table_get` / `table_put` | One table per owning flow. `RECV` reads, `SEND` writes. Gone when the owner exits |
| `receive … after T` | `Fn::receive_match_imm_timeout(tag, ms)` / `Fn::receive_timeout` | Match + timeout writes `Unit` (hops stay queued) |
| `is_process_alive(Pid)` | `Fn::is_alive(cap)` | Held Cap → live flow? `false` if revoked / not held |
| `exit(Pid, Reason)` | `Fn::exit_signal(cap, reason)` | Needs `LINK`. `Normal` ignored. `Killed` untrappable |
| process dictionary `put` / `get` / `erase` | `Fn::dict_put` / `dict_get` / `dict_erase` | Per-flow dict, `Int` keys, scalars plus `Str` / `Bytes` |
| `supervisor:start_child` | `Fn::start_supervisor(strategy)` then `Fn::start_child(fn, policy)` | `StartSupervisor` (`0x72`) owns the strategy. Without it, `StartChild` uses the host supervisor that started the caller |
| `gen_server:call` | `Fn::ask(cap, hop)` / `Fn::ask_timeout` | `Ask` / `AskTimeout` |
| `gen_server:cast` | `Fn::send(cap, hop)` | fire-and-forget |
| host async I/O (no BEAM equiv.) | `Fn::host_await(op, args)` + [`HostAwaitBridge`](../src/scheduler/host_await.rs) | `HostAwait` — see [`host-await.md`](host-await.md) |
| reply to caller | `Fn::send_reply(req, tag, payload)` | uses `msg_reply_cap` |
| build message | `Fn::hop(req_id, tag, payload)` | scheduler stamps sender |
| unpack message | `Fn::hop_payload(msg)` etc. | std natives |

### Message shape

BEAM messages are arbitrary terms. Byteflow **Atomic Hop** is a fixed envelope:

```text
Message { sender, reply_cap, request_id, tag, payload }
```

- `payload` is a nested [`Value`](../src/bytecode/value.rs) — scalars,
  `Str` / `Bytes`, a `List` of at most 256 elements (ABI v6), a `Map` of at
  most 256 pairs with `Int` or `Str` keys (ABI v7), a positional `Record` of
  at most 256 fields (ABI v8), or another hop.
  A module stays on the oldest ABI that can hold its constants: v8 when a
  record is present, otherwise v7 when a map is present, otherwise v6 when a
  list is present, otherwise v5. Readers accept v5, v6, v7, and v8.
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
    let cap = f.spawn(server, &[]);
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
| **links** | `Runtime::link` / `Fn::link` — abnormal exit kills the peer unless `trap_exit` |
| **monitors** `{'DOWN', ...}` | `Runtime::monitor` / `Fn::monitor` → `TAG_SYS_DOWN` hop |
| **OTP supervisor strategies** | Host `Supervisor` + `RestartStrategy` (`OneForOne` / `OneForAll` / `RestForOne`) |
| **`register` / `whereis`** | `Fn::register_name` / `Fn::whereis` (and host `Runtime::*`). Cap, not FlowId |
| **`exit(Pid, Reason)`** | `Fn::exit_signal(cap, reason)` (`LINK` required). Host `Runtime::kill` / `admin_kill` are the untrappable `Killed` path |
| **`process_flag(trap_exit, …)`** | `Fn::set_trap_exit` / `Runtime::set_trap_exit` → `TAG_SYS_EXIT` hop (**done**) |
| **capability pass** | `Fn::delegate` / `Cap::attenuate` (AND of rights + native mask) |
| **confined spawn** | `Fn::spawn_confined` (child rights `NONE`) |
| **`child_spec` restart** | `Fn::start_supervisor` chooses `OneForOne` / `OneForAll` / `RestForOne`. `Fn::start_child` attaches to that supervisor, or to the host supervisor that started this flow. `Fn::set_restart_policy` overrides this flow's own restart at exit |
| **process dictionary** | `Fn::dict_put` / `dict_get` / `dict_erase` — `Int` keys, scalars plus `Str` / `Bytes` |

## Module ABI

`encode` writes the oldest ABI that can represent the constant pool:

| Header | When |
|--------|------|
| v5 | No list, map, or record |
| v6 | A list is present, and no map or record |
| v7 | A map is present, and no record |
| v8 | A record is present (`Value::Record`, wire tag `11`) |

Decode accepts v5, v6, v7, and v8. A module whose header is v5 rejects list tag `9`, map tag `10`, and record tag `11`. v6 rejects map and record. v7 rejects record. A module that does not use a record is not written as v8. Lists and maps hold at most 256 elements. A record holds at most 256 positional fields (`RecordPack` / `RecordGet` / `RecordLen`, opcodes `0x8A`–`0x8C`). `receive` does not gain a record pattern; it still matches tag, kind, integer comparison, and `Value` equality.

## What BEAM has that Byteflow does not (yet)

| BEAM / OTP | Byteflow today |
|------------|----------------|
| **distribution** | No cluster protocol and no sockets. `encode_frame` / `decode_frame` / `Runtime::deliver_frame` move one hop. The envelope is a `.bf` value. The host opens the connection. The header sender and the envelope sender are the same id, and that id is not `0`. A `Cap` or `Pid` does not cross |
| **arbitrary term patterns** | Tag, payload kind (`0..=10`), `Int` equality (`ReceiveMatchEq`), `Eq`/`Lt`/`Le` on an `Int` (`ReceiveMatchCmp`), structural equality (`ReceiveMatchValue`), and `after`. Maps are a value (`MapPack`), not a pattern language. No tuples |
| **ETS** | No global table. One named table per owning flow (`TableOpen`); `RECV` reads, `SEND` writes; it disappears when the owner finalizes |

Failures without `trap_exit` / monitors surface as `FlowOutcome::Failed` on
`join`, not as mailbox messages.

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
| restart policy | `Fn::start_supervisor`, or host `Supervisor` |
| protocol in flows | `Program` / `Fn` bytecode |

The supervisor thread is host Rust ([`Supervisor`](../src/scheduler/supervisor.rs) /
[`ChildSpec`](../src/scheduler/supervisor.rs)), or a supervisor this flow
started with [`Fn::start_supervisor`](../src/bytecode/program.rs).
`StartChild` attaches to the flow's own supervisor when one exists, and
otherwise to the host supervisor that started the caller
(`samples::supervised_tree`). `SetRestartPolicy` still overrides that
flow's own restart decision at exit. A flow started with `Runtime::spawn`
that has not called `StartSupervisor` has no supervisor, so `StartChild`
fails that flow.

## Quick equivalence cheat sheet

```text
self()              →  hop_sender on received msg; self_address() for Cap
!                   →  send(cap, hop(...))
receive             →  receive() / receive_match_imm(TAG) / receive_match_kind(kind) / receive_match_cmp(tag, bound, Eq\|Lt\|Le)
receive … after     →  receive_match_imm_timeout(TAG, ms) / receive_timeout(ms)
is_process_alive    →  is_alive(cap)
exit(Pid, Reason)   →  exit_signal(cap, reason)   // reason = FlowExitReason as Int
put/get/erase       →  dict_put / dict_get / dict_erase
map                 →  map_pack / map_get / map_len
ets                 →  table_open / table_get / table_put   // one table per flow, not a global store
start_child         →  start_supervisor(strategy) then start_child(fn, RestartPolicy)
actor loop          →  actor_loop(TAG, \|f, req\| { … })
cast                →  cast(cap, hop(...))   // alias of send
call                →  ask(cap, hop(...)) / ask_timeout(cap, hop, ms)
reply               →  send_reply(req, TAG_REP, payload)
spawn               →  spawn(fn, &[args]) → Cap; spawn_confined(fn, &[args]) → Cap with rights NONE
register/whereis    →  Fn::register_name / whereis (Cap reminted for caller); host Runtime::*; ChildSpec.name also registers
link/monitor        →  Fn::link / Fn::monitor (need LINK / MONITOR on the addressing Cap)
delegate            →  Fn::delegate(cap, rights) / delegate_with_native(cap, rights, native_cap)
trap_exit           →  Fn::set_trap_exit / Runtime::set_trap_exit
restart policy      →  Fn::set_restart_policy / ChildSpec.restart
```

## Further reading

- [`atomic-hop.md`](atomic-hop.md) — protocol details
- [`mailbox.md`](mailbox.md) — bounded queues, overflow
- [`security.md`](security.md) — S1 authenticated sender, S6 FlowCap, S7 native mask
- [`samples.rs`](../src/samples.rs) — runnable specs
