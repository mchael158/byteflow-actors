# Production embedding checklist

Byteflow is an **in-process** flow runtime. Production means the **host**
owns I/O, policy, and shutdown — not a separate cluster daemon.

Use this page with [`audit-checklist.md`](audit-checklist.md) when hardening
a deployment.

## Untrusted bytecode

1. Always construct the runtime through [`Runtime::with_natives_and_config`]
   (or [`Runtime::with_attested`]) so [`verify_with`] runs **before** workers
   start.
2. Keep [`TrustLevel::Untrusted`] unless you control every constant in the chunk.
3. Prefer [`QuotaConfig::sandbox`] and explicit [`RuntimeConfig::max_flows`],
   [`RuntimeConfig::max_ask_waits`], and relation caps (`max_links`, `max_monitors`,
   `max_registry_names`).
4. Do **not** expose [`Runtime::send`] to untrusted plugins; use Caps and confined spawns.
5. For replayed remote frames, set [`RuntimeConfig::remote_dedup_window`] to a non-zero
   window (for example `4096`). [`RemoteFrameDedup`] is wired automatically; duplicates
   are acknowledged with `Ok(())` and are not delivered twice.

## Shutdown (liveness)

Order for a clean exit:

1. Stop accepting new host work (HTTP, sockets, etc.).
2. Let flows finish or call [`Runtime::shutdown`] — parked receives wake with
   `FlowOutcome::Failed("runtime shutdown")`, not a hung [`FlowHandle::join`].
3. If using [`DistributionBridge`]: [`wait_bridge_drained`], then
   [`TcpOutboundHandle::wait_wire_drained`], then [`TcpOutboundHandle::stop`].
4. Drop inbound [`TcpInboundHandle`] after sessions finish (`stop` waits up to 5s).

## Remote hops (host-owned socket)

Byteflow does **not** authenticate TCP peers.

| Responsibility | Owner |
|----------------|--------|
| TLS, mTLS, firewall | Host |
| Map `OutboundHop::node` → socket | [`NodeRouter`] / your code |
| Drain bridge → wire | [`spawn_tcp_outbound`] or custom pump |
| Wire → inbox | [`spawn_tcp_inbound`] → [`Runtime::deliver_frame`] |
| Replay / at-least-once | Host ([`RemoteFrameDedup`] optional) |

Minimum viable remote path: see [`remote-transport.md`](remote-transport.md) and
`examples/remote_hop_tcp.rs` (channel deliver on the runtime thread — no mutex
around the whole [`Runtime`]).

## Observability

- Counters: [`Runtime::metrics`].
- Point-in-time: [`Runtime::snapshot`] (flows, bridge depth, memory ceiling).
- Diagnostics: `BYTEFLOW_LOG=info` or [`RuntimeConfig::log`].

## What this runtime is not

- Not Tokio — use [`HostAwaitBridge`] for async host I/O.
- Not BEAM distribution — no transparent `Cap` or `Pid` across nodes.
- Not a global ETS — tables are per owning flow unless you build a registry service.

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
[`Runtime::send`]: ../src/scheduler/runtime.rs
[`Runtime`]: ../src/scheduler/runtime.rs
[`remote-transport.md`]: remote-transport.md
