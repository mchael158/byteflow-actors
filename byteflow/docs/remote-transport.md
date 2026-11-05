# Reference remote transport

Byteflow has no cluster protocol. Remote path:

1. `SendRemote` → [`DistributionBridge`](../src/scheduler/distribution.rs)
2. Host drains bridge → [`encode_frame`](../src/scheduler/distribution.rs) (`BFH1`)
3. Host writes bytes on its socket
4. Peer → [`Runtime::deliver_frame`](../src/scheduler/runtime.rs)

Optional helpers in [`transport`](../src/transport/mod.rs): `BFT1` length-prefixed
envelope + bounded queues.

## Wire (`BFT1`)

```text
BFT1  u8 version (=1)  u32 le payload_len  payload
```

`payload` = output of `encode_frame`. Max `payload_len` default: 4 MiB
([`DEFAULT_MAX_WIRE_BYTES`]).

## TCP

| API | Role |
|-----|------|
| [`spawn_tcp_outbound`] | Bridge → bounded channel → one `TcpStream` |
| [`spawn_tcp_outbound_routed`] | Same; `node` via [`NodeRouter`] |
| [`spawn_tcp_inbound`] | Accept → `read_wire_frame` → `deliver` (usually `deliver_frame`). `max_sessions` default `64` (`0` = unlimited) |

Outbound queue full → pump blocks on `send`. Bridge caps pending work by
**hop count and bytes** ([`DistributionConfig`], default 1024 / 64 MiB). Charge
stays on [`OutboundHop`] until the host drops it after encode/send.

Write failure: drop TCP, reconnect, retry the **same** wire frame, then continue.
Shutdown order: [`wait_bridge_drained`] → `wait_wire_drained` → `stop`.

`BYTEFLOW_LOG=info` logs encode/deliver/session failures. If `encode_frame`
fails after dequeue, that hop is lost (pump stops fail-closed).

## UDP

| API | Role |
|-----|------|
| [`send_wire_frame_udp`] | One datagram = one `BFT1` frame |
| [`recv_wire_frame_udp`] | Strip envelope |

No order/retry. MTU limits apply.

## Examples

```text
cargo run -p byteflow-actors --example remote_hop_tcp
```

No sockets: [`examples/remote_hop.rs`](../examples/remote_hop.rs).

## Host responsibilities

- `OutboundHop::node` → `SocketAddr` ([`NodeRouter`] / [`StaticNodeRouter`] /
  [`FixedPeerRouter`] / [`resolve_hop_peer`])
- TLS, auth, rate limits
- `remote_dedup_window`: transactional dedup — rollback on `NoSuchName` /
  mailbox full; commit only after mailbox accept
- Retransmit after [`RemoteFrameError::Mailbox`] when dedup is on
- [`DistributionBridge::with_config`] if defaults are wrong
- `TcpInboundConfig::max_sessions`

[`wait_bridge_drained`]: ../src/transport/mod.rs
[`encode_frame`]: ../src/scheduler/distribution.rs
[`spawn_tcp_outbound`]: ../src/transport/mod.rs
[`spawn_tcp_outbound_routed`]: ../src/transport/mod.rs
[`spawn_tcp_inbound`]: ../src/transport/mod.rs
[`send_wire_frame_udp`]: ../src/transport/mod.rs
[`recv_wire_frame_udp`]: ../src/transport/mod.rs
[`DEFAULT_MAX_WIRE_BYTES`]: ../src/transport/mod.rs
[`TcpOutboundHandle::wait_wire_drained`]: ../src/transport/mod.rs
[`TcpOutboundHandle::stop`]: ../src/transport/mod.rs
[`TcpInboundConfig::max_sessions`]: ../src/transport/mod.rs
[`OutboundHop`]: ../src/scheduler/distribution.rs
[`RemoteFrameError::Mailbox`]: ../src/scheduler/distribution.rs
[`NodeRouter`]: ../src/transport/router.rs
[`StaticNodeRouter`]: ../src/transport/router.rs
[`FixedPeerRouter`]: ../src/transport/router.rs
[`resolve_hop_peer`]: ../src/transport/router.rs
[`Runtime::deliver_frame`]: ../src/scheduler/runtime.rs
[`DistributionBridge`]: ../src/scheduler/distribution.rs
[`DistributionBridge::with_config`]: ../src/scheduler/distribution.rs
[`DistributionConfig`]: ../src/scheduler/distribution.rs
[`RuntimeConfig::remote_dedup_window`]: ../src/scheduler/runtime.rs
