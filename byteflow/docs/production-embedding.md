# Production embedding

In-process runtime: the host owns I/O, policy, and shutdown.

Pair with [`audit-checklist.md`](audit-checklist.md).

## Untrusted bytecode

1. Build with [`Runtime::with_natives_and_config`] or [`Runtime::with_attested`]
   so [`verify_with`] runs before workers start.
2. Keep [`TrustLevel::Untrusted`] unless every constant is host-controlled.
3. Use [`RuntimeConfig::sandbox`] (or explicit `max_flows` / `max_ask_waits` /
   relation caps + [`QuotaConfig::sandbox`]).
   `sandbox()` does **not** change host [`Runtime::spawn`] authority (still
   TrustedHost `ROOT` + full natives). Guest entry: [`Runtime::spawn_untrusted`]
   or `root_authority = Untrusted`.
4. Do not expose [`Runtime::send`] to plugins; use Caps / `spawn_confined`.
5. At-least-once wire: set `remote_dedup_window` (e.g. `4096`).
   [`RemoteFrameDedup`] reserves on accept path, commits after mailbox accept,
   rolls back on `NoSuchName` / mailbox full (retransmit allowed). After commit,
   duplicates return `Ok(())`.

## Shutdown

1. Stop accepting host work.
2. [`Runtime::shutdown`] — parked receives fail with `"runtime shutdown"`;
   [`FlowHandle::join`] does not hang.
3. With a bridge: [`wait_bridge_drained`] → `wait_wire_drained` →
   [`TcpOutboundHandle::stop`].
4. Drop [`TcpInboundHandle`] (`stop` waits ≤ 5s for sessions).

## Remote hops

No peer auth inside Byteflow.

| Responsibility | API |
|----------------|-----|
| TLS / firewall | Host |
| `OutboundHop::node` → socket | [`NodeRouter`] |
| Bridge → wire | [`spawn_tcp_outbound`] |
| Wire → inbox | [`spawn_tcp_inbound`] → [`Runtime::deliver_frame`] |
| Bridge budget | [`DistributionBridge::with_config`] ([`DistributionConfig`]) |
| Replay filter | [`RemoteFrameDedup`] via `remote_dedup_window` |

See [`remote-transport.md`](remote-transport.md) and `examples/remote_hop_tcp.rs`
(channel deliver on the runtime thread — no `Mutex<Runtime>`).

## Observability

- [`Runtime::metrics`]
- [`Runtime::snapshot`] (flows, bridge depth, memory)
- `BYTEFLOW_LOG=info` or [`RuntimeConfig::log`]

## Boundaries

- Async host I/O: [`HostAwaitBridge`], not Tokio inside the core.
- No transparent Cap/Pid across nodes.
- Tables are per owning flow (not global ETS).

[`Runtime::with_natives_and_config`]: ../src/scheduler/runtime.rs
[`Runtime::with_attested`]: ../src/scheduler/runtime.rs
[`verify_with`]: ../src/bytecode/verify.rs
[`Runtime::shutdown`]: ../src/scheduler/runtime.rs
[`FlowHandle::join`]: ../src/scheduler/handle.rs
[`DistributionBridge`]: ../src/scheduler/distribution.rs
[`wait_bridge_drained`]: ../src/transport/mod.rs
[`TcpOutboundHandle`]: ../src/transport/mod.rs
[`TcpInboundHandle`]: ../src/transport/mod.rs
[`NodeRouter`]: ../src/transport/router.rs
[`spawn_tcp_outbound`]: ../src/transport/mod.rs
[`spawn_tcp_inbound`]: ../src/transport/mod.rs
[`Runtime::deliver_frame`]: ../src/scheduler/runtime.rs
[`RemoteFrameDedup`]: ../src/scheduler/distribution.rs
[`Runtime::metrics`]: ../src/scheduler/runtime.rs
[`Runtime::snapshot`]: ../src/scheduler/runtime.rs
[`RuntimeConfig::log`]: ../src/scheduler/runtime.rs
[`HostAwaitBridge`]: ../src/scheduler/host_await.rs
[`TrustLevel::Untrusted`]: ../src/bytecode/mod.rs
[`QuotaConfig::sandbox`]: ../src/scheduler/quota.rs
[`RuntimeConfig::max_flows`]: ../src/scheduler/runtime.rs
[`RuntimeConfig::max_ask_waits`]: ../src/scheduler/runtime.rs
[`RuntimeConfig::remote_dedup_window`]: ../src/scheduler/runtime.rs
[`RuntimeConfig::sandbox`]: ../src/scheduler/runtime.rs
[`RuntimeConfig::root_authority`]: ../src/scheduler/runtime.rs
[`RootAuthorityPolicy::Untrusted`]: ../src/scheduler/runtime.rs
[`Runtime::spawn`]: ../src/scheduler/runtime.rs
[`Runtime::spawn_untrusted`]: ../src/scheduler/runtime.rs
[`DistributionBridge::with_config`]: ../src/scheduler/distribution.rs
[`DistributionConfig`]: ../src/scheduler/distribution.rs
[`Runtime::send`]: ../src/scheduler/runtime.rs
