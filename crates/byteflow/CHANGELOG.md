# Changelog

All notable changes to **byteflow-actors** are documented here.

## [0.9.4] — 2026-09-14

Scheduler correctness for Ask handoff writeback, plus examples/docs that
assemble and run only through the public `byteflow` facade.

### Fixed

- Ask insert→park race: after `park_filter`, revalidate `ask_waits`
  membership (`target_of`) and target liveness; self-wake with
  `TAG_SYS_EXIT` if finalize already cleared the waiter.
- `resume_with` faults are no longer swallowed — `resume_or_fail`
  finalizes the flow on quota / register writeback errors (worker,
  handoff, timer, host `send`, DOWN / orphaned Ask paths).

### Changed

- Examples and design-guide doctests build with `Program` / `Fn` and
  `use byteflow::{...}` (plus `std_native_table` when hops need
  `make_msg`); they no longer wrap `samples::*` as the only demo path.

## [0.9.3] — 2026-09-06

Sandbox parity: named discovery in bytecode, host hops on the same auth
path, and tighter quota presets / top-ups.

### Added

- [`Opcode::RegisterName`] / [`Opcode::Whereis`] (`0x62` / `0x63`) —
  bytecode name registry. `whereis` mints a **SEND** Cap for the caller
  (`mint_or_reuse`); it never returns a raw FlowId. Registering requires
  `SEND` on self-authority (confined spawn cannot squat names). Host
  `whereis` still returns the stored Cap. Sample [`named_service`].
- [`QuotaConfig::permissive`] / [`QuotaConfig::sandbox`] — default remains
  permissive. [`Runtime::admin_top_up_mem`] / [`Runtime::admin_top_up_send`]
  are symmetric to CPU top-up.

### Changed

- Interim heap quota on `Str` / `Bytes` register stores (charge on write,
  no release until the flow exits; same `Arc` in the same slot is not
  charged twice). Hop payloads stay charged at `Send` / `Ask`.
- Host [`Runtime::send`] goes through the same hop authentication
  choke-point: `sender = FlowId::HOST` (`0`), `request_id` minted when
  unset, payload Caps reissued via `reissue_for`. Host has no mailbox, so
  `reply_cap` is `CapId::NONE`. `finalize_flow` refuses the reserved host
  id.
- Scheduler fail-closed: flow-id wrap, undeliverable mailbox hops, poison
  paths that still hold a `Flow`, spawn mint failure kills the orphan
  child, host send poison is `SendError::Unavailable`.

[`Opcode::RegisterName`]: https://docs.rs/byteflow-actors/latest/byteflow/enum.Opcode.html
[`Opcode::Whereis`]: https://docs.rs/byteflow-actors/latest/byteflow/enum.Opcode.html
[`QuotaConfig::permissive`]: https://docs.rs/byteflow-actors/latest/byteflow/struct.QuotaConfig.html
[`QuotaConfig::sandbox`]: https://docs.rs/byteflow-actors/latest/byteflow/struct.QuotaConfig.html
[`Runtime::admin_top_up_mem`]: https://docs.rs/byteflow-actors/latest/byteflow/struct.Runtime.html
[`Runtime::admin_top_up_send`]: https://docs.rs/byteflow-actors/latest/byteflow/struct.Runtime.html
[`Runtime::send`]: https://docs.rs/byteflow-actors/latest/byteflow/struct.Runtime.html
[`named_service`]: https://docs.rs/byteflow-actors/latest/byteflow/samples/fn.named_service.html

## [0.9.2] — 2026-09-04

Security Phase 3: attenuation is the only grant path, natives and lifecycle
rights are gated, and each flow carries fail-closed quotas.

### Added

- [`Cap::attenuate`] — sole grant-derivation path (AND of rights and
  [`NativeMask`]). Bytecode [`Opcode::Delegate`] (`0x5B`; `Trap` stays `0x60`)
  and confined spawn both go through it.
- [`NativeGate`] + [`check_native_call`] — `CALL_NATIVE` is denied unless the
  flow holds `NATIVE` and the index bit in its mask. Host `Runtime::spawn`
  still mints a full mask; bytecode spawn attenuates.
- [`QuotaConfig`] / [`FlowQuota`] — per-flow CPU budget (distinct from the
  scheduler quantum), heap charge, and spawn/send token buckets. Defaults stay
  generous; tighten via [`RuntimeConfig::quota`].
- `LINK` / `MONITOR` / `ADMIN` rights: addressing Caps carry `LINK|MONITOR`;
  [`Runtime::admin_kill`] / [`Runtime::admin_top_up_cpu`] require a scheduler
  ADMIN Cap. No ADMIN opcode in the ISA.
- [`Fn::spawn_confined`] — child rights `NONE` unless the parent delegates.
- `make_msg` is 3-arg (`request_id`, `tag`, `payload`); `sender = 0` until
  `Send` / `Ask` stamp. Legacy 4-arg encoding discards the first operand.

### Changed

- `RECEIVE` requires `CapRights::RECV`.
- SEND/ASK charge `FlowQuota` (`alloc` on enqueue, `free` on deliver).
- Interpreter and JIT honor the remaining CPU budget as the slice length.
- Hop `reply_cap` is **reused** per `(recipient, sender)` pair
  (`CapTable::mint_or_reuse`). `revoke_flow` also drops the reverse index.
  Correlation stays on `request_id`.
- `Opcode::FreshRequestId` (`0x5C`) + `Fn::hop_fresh`. `Send`/`Ask` mint
  when `request_id == 0`. A second in-flight `Ask` with a pending id fails.
- `ReceiveMatchCorr` / `ReceiveMatchCorrImm` (`0x5D`/`0x5E`) — receive by
  `(tag, request_id)` without consuming unrelated hops.
- Caps in a hop payload are reissued to the recipient (holder check, no
  escalation). Sample [`atomic_actors`]: server loop + two Ask clients.

[`Cap::attenuate`]: https://docs.rs/byteflow-actors/latest/byteflow/struct.Cap.html
[`NativeMask`]: https://docs.rs/byteflow-actors/latest/byteflow/struct.NativeMask.html
[`NativeGate`]: https://docs.rs/byteflow-actors/latest/byteflow/struct.NativeGate.html
[`check_native_call`]: https://docs.rs/byteflow-actors/latest/byteflow/fn.check_native_call.html
[`QuotaConfig`]: https://docs.rs/byteflow-actors/latest/byteflow/struct.QuotaConfig.html
[`FlowQuota`]: https://docs.rs/byteflow-actors/latest/byteflow/struct.FlowQuota.html
[`RuntimeConfig::quota`]: https://docs.rs/byteflow-actors/latest/byteflow/struct.RuntimeConfig.html
[`Opcode::Delegate`]: https://docs.rs/byteflow-actors/latest/byteflow/enum.Opcode.html
[`Fn::spawn_confined`]: https://docs.rs/byteflow-actors/latest/byteflow/struct.Fn.html
[`Runtime::admin_kill`]: https://docs.rs/byteflow-actors/latest/byteflow/struct.Runtime.html
[`Runtime::admin_top_up_cpu`]: https://docs.rs/byteflow-actors/latest/byteflow/struct.Runtime.html
[`atomic_actors`]: https://docs.rs/byteflow-actors/latest/byteflow/samples/fn.atomic_actors.html

## [0.9.0] — 2026-09-02

### Breaking

- **ABI v5:** `CapId` is a 128-bit CSPRNG token (was `u64`). `Message.reply_cap`
  is `CapId`. `Message.payload` is a nested [`Value`] (was `u64` scalar).
- **`CapTable::resolve(cap, holder, rights)`** — resolution requires the calling
  flow to *hold* the token with sufficient rights (`SEND` / `ASK`).
- **`SpawnError::VerifyFailed`** is now [`VerifyError`] (no `String`).
- **Untrusted load is fail-closed:** `verify` / `decode` reject `Cap`, `Pid`,
  and `Message` in the constant pool unless [`TrustLevel::Trusted`] is set.
- **`RuntimeConfig::trust`** defaults to [`TrustLevel::Untrusted`].
- **`msg_payload` native** returns the full [`Value`] payload (not always `Int`).

### Added

- [`CapId::random`] — OS CSPRNG minting; `CapId::NONE` for host hops without reply.
- **Holder model:** each capability records `{ holder, target, rights }`;
  [`CapTable::revoke_flow`] sweeps caps held by or targeting an exiting flow.
- [`TrustLevel`] + [`VerifyConfig`] for load-time constant-pool policy.
- [`decode_with`] mirrors verifier trust when loading `.bf` files.
- [`OutputSink`], [`NullSink`] (default), [`StdoutSink`]; [`RuntimeConfig::output`]
  for embedder-controlled `print`.
- [`Runtime::with_std_natives_and_config`] — std natives + configurable print sink.
- [`Runtime::grant_cap`] — host mints a cap for one flow to address another.
- Integration tests in `tests/security_caps.rs`.

### Changed

- Outgoing hops mint `reply_cap` with **SEND-only** rights (attenuation).
- [`Runtime`] `Drop` requests shutdown if the embedder forgot to call it.
- Mailbox byte budget uses [`Value::memory_size`] on nested `Message.payload`.

[`Value`]: https://docs.rs/byteflow-actors/latest/byteflow/enum.Value.html
[`VerifyError`]: https://docs.rs/byteflow-actors/latest/byteflow/enum.VerifyError.html
[`TrustLevel`]: https://docs.rs/byteflow-actors/latest/byteflow/enum.TrustLevel.html
[`CapId::random`]: https://docs.rs/byteflow-actors/latest/byteflow/struct.CapId.html
[`OutputSink`]: https://docs.rs/byteflow-actors/latest/byteflow/trait.OutputSink.html
[`NullSink`]: https://docs.rs/byteflow-actors/latest/byteflow/struct.NullSink.html
[`StdoutSink`]: https://docs.rs/byteflow-actors/latest/byteflow/struct.StdoutSink.html
[`decode_with`]: https://docs.rs/byteflow-actors/latest/byteflow/fn.decode_with.html

## [0.8.1] — 2026-08-30

### Added

- **Monitors / links:** `Runtime::monitor` / `link` and bytecode
  `Fn::monitor` / `Fn::link`. Target exit is coordinated in `finalize_flow`.
  Monitors deliver [`TAG_SYS_DOWN`] Atomic Hops; abnormal link exit kills
  the peer. See `docs/lifecycle.md`.
- **`WAITING_SEND`:** bytecode `Send` / `Ask` park on a full `Reject` inbox;
  one waiter is admitted per freed slot.
- **Registry:** `register_name` / `whereis` store a `CapId` (never `FlowId`);
  entries are swept on flow exit.
- Opcodes `Monitor` `0x56`, `Demonitor` `0x57`, `Link` `0x58`, `Unlink` `0x59`,
  `AskTimeout` `0x5A` (append-only ABI; `Trap` stays `0x60`).
- **`Runtime::kill`:** cooperative abort (`FlowExitReason::Killed`).
- **OTP strategies:** `RestartStrategy::{OneForOne, OneForAll, RestForOne}`
  on `SupervisorConfig`. Sibling kills use `expected_shutdown` so they do
  not cascade.
- **`RuntimeConfig.max_flows`:** hard cap on live flows (`0` = unlimited);
  overflow is `SpawnError::FlowLimit`.
- **Ask target death:** parked `Ask` / `AskTimeout` resume with `TAG_SYS_EXIT`
  (`Message::linked_exit`) instead of hanging. `AskTimeout` clock expiry
  still writes `Unit`.
- **`ChildSpec.name`:** non-empty names are `register_name`'d (Cap) for that
  incarnation; duplicate → `SpawnError::NameTaken`.
- Removed unused public `FlowState` (scheduler never stored it).

## [0.8.0] — 2026-08-29

### Breaking

- **Bytecode assembly API:** public `ChunkBuilder`, `FnBuilder`, and `emit_*`
  removed from the crate root. Use [`Program`] and [`Fn`] (`Program::new`,
  `function`, `build`). All samples, examples, and design-guide doctests
  migrated.

### Added

- **[`Program`] / [`Fn`]** — named registers, control flow (`while_lt`, labels),
  messaging (`send`, `receive`, `receive_match_imm`, `ask`), natives
  (`native1_from`, `make_msg`, `call_native0`), and layout helpers
  (`function_raw`, `spawn_at`, `reg`, `window`).
- **Trace JIT** (`feature = "jit"`): intra-chunk `Call`, trace-cache
  invalidation on chunk reload, JIT attempts interleaved with `vm.run()`.
- Example **`jit_loop`** (interpreter vs JIT benchmark).

### Changed

- README and design guides document `Program`/`Fn` as the only public
  assembly path. `ChunkBuilder` remains crate-internal.

[`Program`]: https://docs.rs/byteflow-actors/latest/byteflow/struct.Program.html
[`Fn`]: https://docs.rs/byteflow-actors/latest/byteflow/struct.Fn.html

## [0.7.0] — 2026-08-27

### Breaking
- **`RuntimeConfig.mailbox`:** every runtime carries a [`MailboxConfig`]
  (capacity + overflow). Struct literals that only set `workers` / `quantum`
  must add `mailbox: MailboxConfig::DEFAULT` (or `..Default::default()`).
- **`SendError::MailboxFull { flow, reason }`:** host `Runtime::send` fails
  when the target inbox is at either bound under `OverflowPolicy::Reject`.
  Was a tuple variant carrying only the `FlowId`.
- **`MailboxFull`** carries a [`MailboxFullReason`] (read it via
  `MailboxFull::reason()`). Note that `Err(MailboxFull)` in a pattern now
  binds a *variable* instead of matching the type, so such matches compile
  but assert nothing — bind the error and check `reason()`.
- **Mailboxes now enforce a byte budget** (default 4 MiB per inbox) on top
  of the hop count. A workload that relied on 256 hops × 1 MiB blobs per
  inbox must raise it via `MailboxConfig::with_bytes`.
- **`Mailbox::park` / `park_match`** return `WaitEpoch` instead of `()` on a
  successful park, and **`Mailbox::take_parked()` is now
  `take_parked_at(epoch)`**. Required to fix the stale-deadline bug below;
  only a timeout scheduler could sensibly call these.
- **`NativeTableBuilder::register` / `register_at`** return `Result`
  (`NativeTableError`) instead of panicking on duplicate name/slot.
- **`VerifyError::ArityExceedsRegisters`** — `verify` now rejects a function
  declaring more parameters than it has registers. New variant (exhaustive
  `match` must handle it), and chunks that previously loaded now fail with
  `SpawnError::VerifyFailed`. Not a capability regression: such a function
  could never be entered, it panicked on the first call instead.
- **`Fault::RegisterIndexOverflow { base, offset }`** — new variant for a
  register index that does not fit the index space at all.

### Added
- Bounded mailboxes: [`MailboxCapacity`] (`1..=1<<20`, default 256),
  [`OverflowPolicy`] (`Reject` / `DropNewest` / `DropOldest` — no `Block`),
  [`MailboxStats`], [`Delivery`]. Logical bound ≠ physical allocation.
- [`MailboxBytes`] (`1 KiB..=1 GiB`, default 4 MiB — strictly larger than
  the decoder's 1 MiB max blob, so a legal constant is never undeliverable)
  and
  [`MailboxConfig::with_bytes`]. A hop count alone stopped bounding memory
  at ABI v4, when hops gained `Str` / `Bytes`: 256 hops is ~12 KiB of
  scalars or ~256 MiB of 1 MiB blobs. Whichever bound is hit first refuses
  the hop.
- [`Value::memory_size`] / [`Value::heap_size`] — the per-hop charge model.
  `Arc`-shared payloads are charged in full to every inbox holding them,
  on purpose (a budget that discounts sharing is not a bound).
- `MailboxStats` gained `queued_messages`, `queued_bytes`, and
  `rejected_byte_limit`, so a hop-count refusal (receiver not draining) is
  distinguishable from a byte refusal (payloads too large).
- [`RuntimeError::Abandoned`] — a flow was destroyed before producing an
  outcome. New variant: exhaustive `match` on `RuntimeError` must handle it.
- **Bounded and non-blocking joins:** [`FlowHandle::try_join`] (poll, never
  waits), [`FlowHandle::join_timeout`] and [`FlowHandle::join_deadline`]
  (wait under a bound the *caller* chooses), all returning
  `Option<FlowOutcome>` where `None` means "still running". `join` was the
  only way to read an outcome and it commits the calling thread for however
  long the bytecode takes — unusable from a control loop, a watchdog, or any
  embedder with its own event loop. All three take `&self`, so the handle
  survives an expired bound and can be retried.
  The wait is anchored on an absolute `Instant`, not a duration re-fed into
  the condvar loop, so spurious wakeups cannot restart the budget and
  silently make the bound unlimited.
- [`RuntimeError::AlreadyCollected`] — a second non-consuming join after the
  outcome was handed out. New variant: exhaustive `match` must handle it. It
  exists so a repeat poll is not answered with `Abandoned`, which would
  blame the runtime for a flow that in fact completed and was observed.
- [`docs/mailbox.md`](docs/mailbox.md) + `byteflow::docs::mailbox`.

### Fixed
- **A crafted chunk could kill a worker thread outright.** A function with
  `arity > num_registers` passed verification, and both `Vm::new` and
  `Opcode::Call` then copied arguments in with a direct `frame.registers[i]`
  write — an out-of-bounds index panic in debug *and* release. `Vm::new` runs
  on the caller's thread: the embedder's under `Runtime::spawn`, or a
  worker's under bytecode `Opcode::Spawn`, which sits outside the
  `catch_unwind` that isolates `Vm::run`. With no worker replacement yet, a
  handful of such spawns took the runtime down. Now rejected statically by
  `verify`, and the writes are checked (`Fault::RegisterOutOfRange`) for
  callers that build a `Vm` without verifying.
- **`Opcode::Spawn` with `a = 255` overflowed a `u8` register index.**
  Arguments are gathered from `a+1..`, computed as `instr.a + 1 + i`: a debug
  panic, and in release a silent wraparound that read `r0` instead of
  faulting — the two builds disagreed, and the silent one was the shipped
  one. All operand gathering now goes through a checked helper that computes
  in a wider type and reports `Fault::RegisterIndexOverflow`. The `Call` /
  `CallNative` gathers were safe only because `u8` register counts cap frames
  at 255 registers, so the bounds check happened to fire first; they are
  checked too, since that accident disappears the moment register counts
  widen.
- **`FlowHandle::join()` could block the embedder's thread forever.** The
  completion oneshot only ever notified on `send`, so a flow destroyed
  without producing an outcome — `Runtime::shutdown` while it slept in the
  timer or sat in a worker deque, or a value lost to mutex poison — left the
  joining thread parked on a condvar nobody would notify again, with no way
  to tell "still running" from "never will". Dropping the sender without
  sending is now a first-class event: `join` returns
  [`RuntimeError::Abandoned`] and `FlowHandle::join` reports
  `FlowOutcome::Failed` with that message.
- **A stale `ReceiveTimeout` deadline could wake the wrong wait.** Timer
  cancellation is lazy, and `Mailbox::take_parked` took whoever was parked.
  A flow whose `ReceiveTimeout` was satisfied early and then parked again
  on another `Receive` could be resumed by the *old* deadline, which also
  wrote `Unit` into the previous wait's destination register — a spurious
  wake plus silent register corruption, reachable from ordinary bytecode.
  Every park now returns a [`WaitEpoch`]; the deadline carries it and
  [`Mailbox::take_parked_at`] only fires while that epoch is current.
- A stale deadline no longer clears `parked_filter`, which would have
  downgraded a live `ReceiveMatch` / `Ask` waiter to "any hop wakes me".
- `DropOldest` now evicts as many hops as the byte budget requires rather
  than exactly one, and refuses a hop larger than the entire budget
  instead of reporting a delivery that could not happen.

### Changed
- Production **and tests** use `Result` / `?` — no `unwrap` / `expect` /
  `unwrap_or*`. Clippy `unwrap_used` + `expect_used` denied crate-wide.

### Docs
- New guide [`docs/vm-safety.md`](docs/vm-safety.md) +
  `byteflow::docs::vm_safety`: the trust boundary from `.bf` bytes to
  `FlowOutcome`, a per-check table of what `verify` settles statically vs
  what the `Vm` checks per step (and *why* each fact lives where it does),
  the debug/release divergence that raw `u8` register arithmetic causes, and
  the one place the VM trusts `verify` instead of re-checking —
  `Jump` / `Branch`, where an unverified chunk degrades to a silent implicit
  return rather than a fault.
- Every guide example is now a **doctest**, compiled and run by
  `cargo test --doc` (5 → 16). Documentation that drifts from the API now
  fails the build instead of misleading a reader.
- `docs/error-model.md`: runnable examples for category A (`SpawnError`),
  for the bounded joins, and for a flow abandoned by `shutdown`. Documents
  that an abandoned verdict is *repeatable* while a successful outcome is
  handed out once.
- `docs/mailbox.md`: a `Runtime::send` refusal showing
  `MailboxFullReason::MessageLimit`, plus a byte-budget refusal read back
  through `Mailbox::stats()` while the hop count is nowhere near its bound.
- Crate front page and `README`: bounded joins with the wait table, and the
  verifier's guarantee.
- All 12 unresolved / private rustdoc intra-doc links fixed; `cargo doc`
  is warning-free.

## [0.5.1] — 2026-08-26

### Docs
- Crate rustdoc rewritten for docs.rs: Atomic Hop, FlowCap, value table,
  scalar + ping-pong examples.
- Guides rendered on docs.rs via `byteflow::docs::{atomic_hop, security, error_model}`.

## [0.5.0] — 2026-08-26

### Breaking
- **FlowCap (security phase 2):** bytecode `Send` / `Ask` require `Value::Cap`.
  `SelfPid` / `Spawn` write Caps (`SEND|ASK`). `Value::Pid` is identity only
  (`Message.sender` / `msg_sender`).
- **`Message.reply_cap`:** stamped at the hop boundary with SEND-only rights;
  replies use `msg_reply_cap` (native index **7**).
- **ABI v4:** wire includes `reply_cap`, Cap tag `6`, **`Str` tag `7`**,
  **`Bytes` tag `8`**.

### Added
- **`Value::Str` / `Value::Bytes`** (`Arc`-backed): constant pool, registers,
  `print`, `Eq` by content; empty Str/Bytes are falsy for `Branch`.

### Docs
- `docs/security.md` — FlowCap as current model; phase 2 marked done.
- `docs/atomic-hop.md` — Cap addressing + `msg_reply_cap`.

## [0.4.0] — 2026-08-25

### Breaking
- Public concurrent unit renamed **flow**: `Flow`, `FlowId`, `FlowHandle`,
  `FlowOutcome`, `FlowState`, `FlowMetrics` (replaces `Process*`).
- **Atomic Hop:** `Send` / `Runtime::send` accept only `Value::Message`
  (`SendError::NotAHop` / VM type trap otherwise).
- `Runtime::live_processes` → `live_flows`.
- `samples::ping_pong` now uses Message hops (requires std natives).
- **Selective receive:** `ReceiveMatch` / `ReceiveMatchImm` (FIFO skip by `tag`);
  `samples::selective_receive`.
- **`Ask` (`0x55`):** atomic request/reply hop with `WaitFilter::Correlation`
  (`request_id` + `sender == target`); `samples::ask_reply`.
- **Authenticated sender (security phase 1):** bytecode `Send`/`Ask` stamp
  `Message.sender` with the executing flow id before delivery; forged
  `make_msg` sender is ignored (`docs/security.md`, samples
  `forged_sender_send` / `forged_sender_ask`).

### Docs
- `docs/atomic-hop.md` (flows + Atomic Hop + Ask). `atomic-actors.md` redirects.
- `docs/security.md` — threat model + authenticated sender contract.

## [0.3.0] — 2026-08-25

### Breaking
- `Runtime::new` / `with_*` return `Result<Runtime, SpawnError>` (no panic on verify / thread spawn).
- `Runtime::spawn`, `RuntimeSpawner::spawn`, `Supervisor::{new,with_config,start_child}` return `Result`.
- Std native table grew Message helpers at frozen slots **2–6** (`make_msg`, `msg_*`). Slots **0–1** unchanged.

### Added
- `Value::Message` / ABI v2 envelopes (`sender`, `request_id`, `tag`, `payload`).
- `samples::atomic_request_reply` + example `atomic_actors`.
- Scheduler logs via `BYTEFLOW_LOG` (`crate::log`).
- Fail-closed mutex helpers + `RuntimeError` / `SpawnError` (`docs/error-model.md`).
- Clippy: `unwrap_used` and `expect_used` denied (including tests).

### Docs
- `docs/atomic-actors.md`, `docs/error-model.md`.

## [0.2.1] — prior

Initial crates.io line: ISA, VM, M:N scheduler, mailboxes, supervisor, std natives (`print`, `now_ms`), CLI.
