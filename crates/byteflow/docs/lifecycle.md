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

`Runtime::kill(id)` is cooperative: a parked receiver / `WAITING_SEND`
finalizes immediately (`FlowExitReason::Killed`); a running flow dies at
the next quantum.

## Supervisor strategies

Host [`Supervisor`](../src/scheduler/supervisor.rs) supports
`OneForOne`, `OneForAll`, and `RestForOne`. Sibling abort uses
`FlowExitReason::Supervisor` and an `expected_shutdown` flag so those
exits do not start another cascade. Intensity still counts the triggering
restart, not the sibling kills.

## Resource governor

[`RuntimeConfig::max_flows`](../src/scheduler/runtime.rs) (`0` = unlimited)
is checked on every host and bytecode `spawn`. Over the cap →
`SpawnError::FlowLimit`. Outstanding Ask waiters are similarly capped by
[`RuntimeConfig::max_ask_waits`](../src/scheduler/runtime.rs).

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
