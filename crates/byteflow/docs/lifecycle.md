# Flow lifecycle (monitors, links, registry)

Byteflow coordinates **exit** in one place: `finalize_flow` (iterative
work list — not recursion). The VM only produces
[`FlowOutcome`](../src/scheduler/process.rs); the runtime revokes Caps,
delivers `DOWN`, propagates links, and sweeps the registry.

`FlowExitReason` is an explicit argument to finalize, not inferred from
the join string. A linked kill therefore reports `Link` on `DOWN`, not
`Fault`.

## Identity vs address

| Type | Role |
|------|------|
| [`FlowId`](../src/scheduler/process.rs) | Identity. Never reused. Host `monitor` / `link` take this. |
| [`CapId`](../src/bytecode/cap.rs) | Address. 128-bit CSPRNG token. Bytecode uses Caps. |

There is no generation on Caps: a dead flow's Caps stop resolving
([`CapTable::revoke_flow`](../src/scheduler/capability.rs) bumps the epoch
and sweeps every entry **held by or targeting** that flow). A guessed
`CapId` never grants Send/Ask. `LINK` / `MONITOR` require those bits on
the addressing Cap; `ADMIN` is a scheduler Cap, never a default spawn grant.

## Monitors (`A ──monitor──> B`)

`Runtime::monitor(owner, target)` or bytecode `Fn::monitor(cap)` creates a
[`MonitorRef`](crate::MonitorRef). When `target` exits, `owner` receives an Atomic Hop:

```text
tag        = TAG_SYS_DOWN (0xFF01)
request_id = MonitorRef
sender     = target FlowId  (identity, not a Cap)
payload    = FlowExitReason as u64
```

Selective receive: `receive_match_imm(TAG_SYS_DOWN)`.

## Links (`A <──────────> B`)

Abnormal exit (`Fault`, `Link`, …) **kills** the peer (cooperative: parked
flows are taken out of the mailbox; running flows see a kill signal at the
next quantum). `Normal` (clean `return` / `Exit`) only drops the link.

With [`Fn::set_trap_exit`](../src/bytecode/program.rs) /
[`Runtime::set_trap_exit`](../src/scheduler/runtime.rs) enabled on a peer
(BEAM `process_flag(trap_exit, true)`), every exit of the linked flow —
including `Normal` — delivers a [`TAG_SYS_EXIT`](../src/bytecode/value.rs)
(`0xFF02`) hop (`Message::linked_exit`) instead of killing that peer.

`Link` / `Unlink` ids fit in `i64` (bytecode `Int`). The counter fails
closed (`RuntimeError::LinkIdExhausted`) instead of wrapping. Insert
confirms both endpoints are still in the directory **after** the link
mutex is released (`link_if_live`), so a peer that already finalized does
not leave a zombie row. Lookup is O(1) on the pair; teardown is O(degree).

## `exit/2` (`ExitSignal`, `0x6A`)

[`Fn::exit_signal`](../src/bytecode/program.rs)`(cap, reason)` sends an exit
signal to the flow named by `cap`. The Cap must carry `LINK` (a spawn Cap
does: `ADDRESSING` includes it). `reason` is [`FlowExitReason`](../src/scheduler/monitor.rs)
as `Int` (`0..=6`); anything else traps the caller.

| Reason | Effect |
|--------|--------|
| `Normal` (`0`) | No-op |
| `Killed` (`2`) | Cooperative kill. `trap_exit` does not catch it |
| any other abnormal | `trap_exit` → `TAG_SYS_EXIT` hop; otherwise kill with that reason |

Host [`Runtime::kill`](../src/scheduler/runtime.rs) / `admin_kill` stay the
untrappable `Killed` path and do not need a Cap. Sample:
[`samples::exit_signal`](../src/samples.rs).

## Process dictionary (`0x6B`–`0x6D`)

Per-flow, on the VM (not the scheduler). Keys are `Int`. Values are
scalars only (`Unit`, `Bool`, `Int`, `Float`) so a dict entry cannot
outlive a register heap charge. Missing key → `Unit`.

| Opcode | `Fn` | BEAM |
|--------|------|------|
| `DictPut` | `dict_put(key, value)` → previous or `Unit` | `put/2` |
| `DictGet` | `dict_get(key)` | `get/1` |
| `DictErase` | `dict_erase(key)` → removed or `Unit` | `erase/1` |

Sample: [`samples::process_dict`](../src/samples.rs) (no natives).

## Bytecode `StartChild` (`0x6F`)

[`Fn::start_child`](../src/bytecode/program.rs)`(function, RestartPolicy)`
spawns a child on the **same** host [`Supervisor`](../src/scheduler/supervisor.rs)
that started the caller, and writes an addressing Cap. The restart
strategy (`OneForOne` / `OneForAll` / `RestForOne`) is the supervisor's,
not an opcode. A flow from `Runtime::spawn` has no supervisor link, so
`StartChild` fails that flow. Sample: [`samples::supervised_tree`](../src/samples.rs)
(host `Supervisor::start_child` on `root`, then bytecode `start_child`).

## Registry

`register_name(name, CapId)` stores a **Cap**, never a FlowId.
Host `whereis` returns that stored Cap. Bytecode `Whereis` mints a
**SEND** Cap for the *caller* (`mint_or_reuse`) so discovery does not
bypass the holder model. Finalize unregisters every name for the dead
flow. Bytecode `RegisterName` only publishes the calling flow and
requires `SEND` on self-authority.

## Ask vs target death

An `Ask` / `AskTimeout` parked for a reply is indexed on the **target**.
When that flow finalizes, the asker is taken off its mailbox and resumed
with [`TAG_SYS_EXIT`](../src/bytecode/value.rs) (`Message::linked_exit`:
`sender` = dead FlowId, `payload` = `FlowExitReason`). This is not a hang
and is distinct from `AskTimeout` writing `Unit`.

Process-wide, outstanding Ask waiters are capped by
[`RuntimeConfig::max_ask_waits`](../src/scheduler/runtime.rs) (`0` =
unlimited). A park that would exceed the cap fails the asker closed.

## `WAITING_SEND`

Bytecode `Send` / `Ask` against a full `Reject` inbox **park the sender**
in the target mailbox. Each pop admits **one** waiter (no wake storm).
Host `Runtime::send` still returns `SendError::MailboxFull`.

`Mailbox::close` runs before directory unregister so a sender that lost
the Full/park race cannot park on a dead inbox (that would leak the flow).

## Kill

`Runtime::kill(id)` and bytecode `ExitSignal` with reason `Killed` are
cooperative: a parked receiver / `WAITING_SEND` finalizes immediately
(`FlowExitReason::Killed`); a running flow dies at the next quantum.
`ExitSignal` aimed at **self** while the flow is on a worker finalizes
that flow immediately (the worker already holds it, so the parked-extract
path cannot see it).

## Supervisor strategies

Host [`Supervisor`](../src/scheduler/supervisor.rs) supports
`OneForOne`, `OneForAll`, and `RestForOne`. Sibling abort uses
`FlowExitReason::Supervisor` and an `expected_shutdown` flag so those
exits do not start another cascade. Intensity still counts the triggering
restart, not the sibling kills.

## Resource governor

[`RuntimeConfig::max_flows`](../src/scheduler/runtime.rs) (`0` = unlimited)
is checked on every host and bytecode `spawn`. Over the cap →
`SpawnError::FlowLimit`. Outstanding Ask waiters are capped by
[`RuntimeConfig::max_ask_waits`](../src/scheduler/runtime.rs); HostAwait
parkers by [`max_host_awaits`](../src/scheduler/runtime.rs); live links /
monitors / registry names by [`max_links`](../src/scheduler/runtime.rs) /
[`max_monitors`](../src/scheduler/runtime.rs) /
[`max_registry_names`](../src/scheduler/runtime.rs) (`0` = unlimited;
[`sandbox`](../src/scheduler/runtime.rs) sets finite values). Accounting for
relation tables lives inside each store under the same mutex as the table.

Each flow also carries a [`FlowQuota`](../src/scheduler/quota.rs) from
[`RuntimeConfig::quota`](../src/scheduler/runtime.rs): remaining CPU
(distinct from the scheduler *quantum*), heap charge (`Str`/`Bytes` on
register store, with release-on-overwrite delta accounting), and
spawn/send token buckets. Process-wide heap is capped by
[`RuntimeConfig::max_runtime_bytes`](../src/scheduler/runtime.rs) via
[`MemoryBudget`](../src/memory.rs). Default is
[`QuotaConfig::permissive`](../src/scheduler/quota.rs);
[`QuotaConfig::sandbox`](../src/scheduler/quota.rs) is the isolation
starting point. Exhaustion fails closed (`QuotaError`). An ADMIN Cap can
top up CPU, heap limit, or the send bucket.
