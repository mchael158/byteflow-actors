# Reference remote transport (host I/O)

Byteflow is **not** a cluster runtime. Remote delivery is:

1. Bytecode `SendRemote` → [`DistributionBridge`](../src/scheduler/distribution.rs)
2. Host drains the bridge → [`encode_frame`](../src/scheduler/distribution.rs) (`BFH1`)
3. Host writes bytes on **its** socket
4. Peer host reads bytes → [`Runtime::deliver_frame`](../src/scheduler/runtime.rs)

This document describes the **optional** [`transport`](../src/transport/mod.rs) helpers
embedders can copy or depend on. They add a length-prefixed wire envelope (`BFT1`) and
bounded queues for backpressure.

## Wire format

```text
BFT1  u8 version (=1)  u32 le payload_len  payload
```

`payload` is exactly the output of [`encode_frame`] — do not re-encode the hop by hand.

Default maximum `payload_len`: 4 MiB ([`DEFAULT_MAX_WIRE_BYTES`]).

## TCP (streaming)

| API | Role |
|-----|------|
| [`spawn_tcp_outbound`] | Pump `DistributionBridge` → bounded channel → `TcpStream` |
| [`spawn_tcp_inbound`] | Accept connections → `read_wire_frame` → your `deliver` closure (typically [`Runtime::deliver_frame`]). [`TcpInboundConfig::max_sessions`] caps concurrent session threads (`0` = unlimited). |

When the outbound queue is full, the bridge pump **blocks** on `send`. The bridge itself
caps at 1024 pending hops; a slow peer eventually fails `SendRemote` on the origin flow
instead of buffering without limit.

Reconnect: on write failure the outbound writer drops the TCP session, reconnects, and
**retries the same wire frame** before dequeuing the next one (reference behavior;
production code should add jitter, metrics, and circuit breaking).

After the sender runtime finishes, call [`wait_bridge_drained`], then
[`TcpOutboundHandle::wait_wire_drained`], then [`TcpOutboundHandle::stop`] so hops
are not dropped while still on the bridge or in the bounded wire queue.

Diagnostics (`encode_frame` failure stops the outbound pump, I/O, `deliver_frame`
failure, session limit) go to stderr when `BYTEFLOW_LOG=info` (same gate as the
scheduler). Hops left on the bridge after a pump stop are not drained until the
embedder stops the handle or fixes the encoder input.

If `encode_frame` fails **after** the hop was already removed from the bridge, that
hop is lost (the pump stops fail-closed; it does not skip and continue). Fix the
hop or reject it before it reaches the bridge.

## UDP (datagram)

| API | Role |
|-----|------|
| [`send_wire_frame_udp`] | One datagram = one full `BFT1` frame |
| [`recv_wire_frame_udp`] | Receive and strip the envelope |

Datagram size limits apply (path MTU). There is no ordering or retry — fire-and-forget only.

## Example

In-process demo (listener on `127.0.0.1:0`):

```text
cargo run -p byteflow-actors --example remote_hop_tcp
```

Same-process delivery without sockets: [`examples/remote_hop.rs`](../examples/remote_hop.rs).

## What you still own

- Mapping [`OutboundHop::node`] to `SocketAddr` (or DNS) — [`NodeRouter`] /
  [`StaticNodeRouter`], or your own table
- TLS, authentication, rate limits
- Optional idempotency: peer [`RuntimeConfig::remote_dedup_window`] (see
  [`production-embedding.md`](production-embedding.md))
- Multi-hop routing and persistence
- Handling [`RemoteFrameError::Mailbox`] (peer inbox full)
- Tuning [`TcpInboundConfig::max_sessions`] for your expected fan-in (default `64`)

[`wait_bridge_drained`]: ../src/transport/mod.rs
[`encode_frame`]: ../src/scheduler/distribution.rs
[`spawn_tcp_outbound`]: ../src/transport/mod.rs
[`spawn_tcp_inbound`]: ../src/transport/mod.rs
[`send_wire_frame_udp`]: ../src/transport/mod.rs
[`recv_wire_frame_udp`]: ../src/transport/mod.rs
[`DEFAULT_MAX_WIRE_BYTES`]: ../src/transport/mod.rs
[`TcpOutboundHandle::wait_wire_drained`]: ../src/transport/mod.rs
[`TcpOutboundHandle::stop`]: ../src/transport/mod.rs
[`TcpInboundConfig::max_sessions`]: ../src/transport/mod.rs
[`OutboundHop::node`]: ../src/scheduler/distribution.rs
[`RemoteFrameError::Mailbox`]: ../src/scheduler/distribution.rs
[`NodeRouter`]: ../src/transport/router.rs
[`StaticNodeRouter`]: ../src/transport/router.rs
[`Runtime::deliver_frame`]: ../src/scheduler/runtime.rs
[`DistributionBridge`]: ../src/scheduler/distribution.rs
[`RuntimeConfig::remote_dedup_window`]: ../src/scheduler/runtime.rs
