# Bounded mailbox

Every flow has one inbox. Unbounded `VecDeque` growth is not a capacity
API — it is an OOM path when many flows share few workers. This crate
treats mailbox size as a **memory contract**, not a raw `usize`.

## Contract

| Piece | Role |
|-------|------|
| [`MailboxCapacity`](../src/scheduler/mailbox/capacity.rs) | Validated hop-count bound (`MIN=1`, `MAX=1<<20`, default **256**) |
| [`MailboxBytes`](../src/scheduler/mailbox/capacity.rs) | Validated byte budget (`MIN=1 KiB`, `MAX=1 GiB`, default **4 MiB**) |
| [`OverflowPolicy`](../src/scheduler/mailbox/policy.rs) | `Reject` / `DropNewest` / `DropOldest` |
| [`MailboxConfig`](../src/scheduler/mailbox/policy.rs) | The three applied to **every** mailbox the runtime spawns |

There is **no** `Block` policy. Parking an OS worker on a full inbox
would stall every other flow on that thread. Scheduler-level
`WAITING_SEND` parks the **sender flow** in the target mailbox. Each
successful pop admits **one** waiter (no wake storm). Host `Runtime::send`
still returns `MailboxFull` — only bytecode `Send` / `Ask` wait.

## Why two bounds

A hop count stopped being a memory bound at ABI v4, when hops gained
`Str` / `Bytes` and the `.bf` decoder began accepting blobs up to 1 MiB.
At the default capacity, 256 hops is ~12 KiB of scalars **or** ~256 MiB
of blobs — so a count alone bounds queue length, not RSS.

Both bounds are checked in the same critical section as the push; the
first one reached refuses the hop, and
[`MailboxFull::reason`](../src/scheduler/mailbox/mod.rs) reports which.
When both are exhausted the hop count is reported, since that is the
bound embedders configure.

Per-hop cost comes from `Value::memory_size()`: `size_of::<Value>()` plus
the length of any `Str` / `Bytes` payload. `Arc`-shared payloads are
charged in full to **every** inbox that holds them. That over-counts
deliberately — a budget that discounts shared buffers is not a bound,
because one producer could fan a single large `Arc` out to every inbox
and stay "within budget" everywhere.

The charge is refunded when the hop leaves the queue. Push and take both
go through `MailboxQueue`, which is why that type owns the filtered take
instead of exposing its `VecDeque`: a leaked charge is never refunded,
and an inbox whose budget has drifted upward rejects forever.

Logical capacity ≠ physical allocation. The queue grows geometrically
and stops at the limit — a flow that receives one hop with `limit=4096`
does not pre-pay 4096 slots.

`MailboxQueue` is a `pub(crate)` abstraction so a future ring buffer can
replace `VecDeque` without touching FlowCap, Ask, or the worker loop.
`#![forbid(unsafe_code)]` — no `MaybeUninit` ring in this revision.

## Wake (lost-wakeup)

`park` and `push` share **one mutex**. A hop that matches a parked
waiter is a [`Delivery::Handoff`](../src/scheduler/mailbox/mod.rs) — it
does **not** consume a queue slot. Non-matching hops are queued (if the
bound allows) and the waiter stays parked (FIFO skip).

Nobody parked → no wake. Overflow that **drops** a hop never produces
Handoff.

## Wait epoch (stale deadlines)

`ReceiveTimeout` cancellation is lazy: the timer never removes its entry
when a hop wakes the receiver early, it just expects to find nobody
parked when it fires. That expectation breaks the moment the flow parks
*again* before the old deadline — which a receive loop does constantly:

```text
  t=0    ReceiveTimeout(r5, 100ms)  -> park A, deadline armed
  t=20   hop arrives                -> handoff, park A over, flow runs
  t=30   Receive(r7)                -> park B (no timeout)
  t=100  deadline for park A fires  -> would take park B, and write
                                       Unit into r5 instead of r7
```

So every park install bumps a per-inbox counter and returns a
[`WaitEpoch`](../src/scheduler/mailbox/mod.rs). The timer carries that
epoch and `Mailbox::take_parked_at(epoch)` hands the flow over only while
the epoch is still current — otherwise the deadline is a no-op.

Two details matter:

The epoch counter **wraps**, it does not saturate. A saturated counter
would make every later epoch compare equal, silently restoring the very
bug the epoch prevents.

A stale call must **not** clear `parked_filter`. Doing so would downgrade
a live `ReceiveMatch` / `Ask` waiter to "any hop wakes me", so the filter
is reset only when the flow is really taken.

`WaitEpoch` has no public constructor: it can only come from parking, so
a deadline cannot present an epoch for a wait that never happened.

## Overflow

| Policy | Host `Runtime::send` | Bytecode `Send` / `Ask` |
|--------|----------------------|-------------------------|
| **Reject** (default) | `SendError::MailboxFull { flow, reason }` | hop logged (with reason) and discarded; sender flow is **not** failed (worker must not stall) |
| **DropNewest** | `Ok` (incoming hop gone) | same |
| **DropOldest** | `Ok` (oldest queued hops gone) | same |

`DropOldest` evicts as many hops as the byte budget requires, not just
one. A hop larger than the **whole** budget is refused under every
policy except `DropNewest` (whose semantics — discard the incoming hop —
are satisfiable at any size): evicting the entire queue would still not
make room, so reporting a delivery that never happened would be a lie.

Configure via `RuntimeConfig.mailbox`:

```rust
use byteflow::{
    MailboxBytes, MailboxCapacity, MailboxConfig, OverflowPolicy, RuntimeConfig, DEFAULT_QUANTUM,
};

let mailbox = MailboxConfig::new(
    MailboxCapacity::DEFAULT,
    OverflowPolicy::Reject,
);
// Narrow the byte budget only when the default 4 MiB is wrong for the
// workload; `new` already applies it.
let mailbox = match MailboxBytes::new(64 * 1024) {
    Some(budget) => mailbox.with_bytes(budget),
    None => mailbox,
};
let cfg = RuntimeConfig {
    workers: 2,
    quantum: DEFAULT_QUANTUM,
    mailbox,
    ..Default::default()
};
let _ = cfg;
```

`MailboxConfig::DEFAULT` is compile-time valid (256 hops, 4 MiB, Reject)
— no `expect` in production.

Under `Reject`, the host learns **which** bound refused the hop:

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use byteflow::{
    Program, MailboxCapacity, MailboxConfig, MailboxFullReason, Message,
    OverflowPolicy, Runtime, RuntimeConfig, SendError, Value, DEFAULT_QUANTUM,
};

let mut program = Program::new("deaf");
program.function("main", 0, |f| {
    let ms = f.load_i32(60_000);
    f.sleep(ms);
    f.return_(ms);
});

let capacity = match MailboxCapacity::new(2) {
    Some(c) => c,
    None => return Err("2 is within MailboxCapacity's range".into()),
};
let rt = Runtime::with_config(
    program.build(),
    RuntimeConfig {
        workers: 1,
        quantum: DEFAULT_QUANTUM,
        mailbox: MailboxConfig::new(capacity, OverflowPolicy::Reject),
        ..Default::default()
    },
)?;
let handle = rt.spawn(0, &[])?;

let hop = |n: u64| Value::Message(Message::new(0, n, 1, n));
rt.send(handle.id(), hop(1))?;
rt.send(handle.id(), hop(2))?;
// Third hop: the inbox is at its hop bound and nobody is draining it.
let refused = rt.send(handle.id(), hop(3));
rt.shutdown();

match refused {
    Err(SendError::MailboxFull { reason, .. }) => {
        assert_eq!(reason, MailboxFullReason::MessageLimit);
    }
    Err(e) => return Err(format!("unexpected error: {e}").into()),
    Ok(()) => return Err("a bounded inbox must refuse the third hop".into()),
}
# Ok(())
# }
```

Host `Runtime::send` to a full inbox under `Reject` returns
`SendError::MailboxFull` and does **not** fail the sending flow (the
host is not a bytecode actor). Bytecode `Send` / `Ask` to a full inbox
park the **sender flow** (`WAITING_SEND`) and wake it when a slot
frees. A hop larger than the whole byte budget is never parked — that
wait could never complete.

The default budget is deliberately larger than the decoder's 1 MiB blob
ceiling: a bound must refuse abuse, not refuse a legal constant. Equal
values would make a max-size payload permanently undeliverable, so
`byte_budget_default_fits_one_max_hop` pins the relationship.

## Observability

`Mailbox::stats()` reports occupancy and counters from the same critical
section, so they describe one instant:

| Field | Use |
|-------|-----|
| `queued_messages` / `queued_bytes` | How close this inbox is to each bound right now |
| `rejected` | Total refusals under `Reject` |
| `rejected_byte_limit` | The byte-budget share; the remainder is hop count |

The split matters because the fixes differ: a hop-count refusal means the
receiver is not draining, a byte refusal means the payloads are too large
for the budget.

This is what "whichever bound is hit first" looks like when the hop count
is nowhere near its limit:

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use byteflow::{
    Delivery, Mailbox, MailboxBytes, MailboxCapacity, MailboxConfig,
    MailboxFullReason, OverflowPolicy, Value,
};

// Room for 1024 hops, but only 2 KiB of payload across all of them.
let capacity = match MailboxCapacity::new(1024) {
    Some(c) => c,
    None => return Err("1024 is within range".into()),
};
let budget = match MailboxBytes::new(2 * 1024) {
    Some(b) => b,
    None => return Err("2 KiB is within range".into()),
};
let mb = Mailbox::with_config(
    MailboxConfig::new(capacity, OverflowPolicy::Reject).with_bytes(budget),
);

// One 1 KiB blob fits comfortably.
assert!(matches!(
    mb.push(Value::bytes(vec![0u8; 1024]))?,
    Ok(Delivery::Queued)
));

// The second does not — and the reason is the budget, not the hop count,
// which is still 1 of 1024.
match mb.push(Value::bytes(vec![0u8; 1024]))? {
    Err(full) => assert_eq!(full.reason(), MailboxFullReason::ByteLimit),
    Ok(_) => return Err("two 1 KiB payloads cannot share a 2 KiB budget".into()),
}

let stats = mb.stats()?;
assert_eq!(stats.queued_messages, 1);
assert!(stats.queued_bytes >= 1024);
assert_eq!(stats.rejected_byte_limit, 1);
# Ok(())
# }
```

Each hop is charged its payload length **plus** the size of the `Value`
itself, so the accounting reflects what the inbox actually holds rather
than just the bytes the sender thinks it sent.

## Hard rules

1. Preserve park+push under one mutex (comment + race diagram in
   `scheduler/mailbox/mod.rs`).
2. Matching waiter → Handoff, never a queue slot.
3. No `Block`. No `unwrap` on the push path.
4. Capacity is [`MailboxCapacity`](crate::MailboxCapacity) and the budget is
   [`MailboxBytes`](crate::MailboxBytes), never a raw `usize` at the API.
5. Every hop that enters or leaves the queue passes through
   `MailboxQueue`, so the byte charge cannot drift.
6. A `ReceiveTimeout` deadline must carry the
   [`WaitEpoch`](crate::WaitEpoch) of the park it was armed for. Never wake
   "whoever is parked".
