# Audit checklist

Status: **open** / **partial** / **done**.

## Remote / delivery

| ID | Item | Status |
|----|------|--------|
| S-01 | Inbound TCP has no auth; host supplies TLS/firewall | **done** — [`production-embedding.md`](production-embedding.md) |
| S-02 | Example delivers via channel (no `Mutex<Runtime>`) | **done** — `remote_hop_tcp.rs` |
| S-03 | Replay filter on `deliver_frame` | **done** — transactional [`RemoteFrameDedup`] + `remote_dedup_window` |
| S-04 | Bridge bound by bytes and hop count | **done** — [`DistributionConfig`] + RAII charge on [`OutboundHop`] |
| S-05 | Host admission authority ≠ sandbox quotas | **done** — [`RootAuthorityPolicy`] + [`Runtime::spawn_untrusted`] |
| C-01 | Outbound pump stops on `encode_frame` failure | **done** |
| C-02 | Document hop loss if encode fails after dequeue | **done** — [`remote-transport.md`](remote-transport.md) |
| T-01 | Random bytes → wire/frame decode never panics | **done** — `tests/wire_decode_stress.rs` |
| — | `node → SocketAddr` | **done** — [`transport/router.rs`](../src/transport/router.rs) |

## Ops / CI

| ID | Item | Status |
|----|------|--------|
| — | [`Runtime::snapshot`] | **done** |
| — | Structured event stream | **open** |
| T-02 | Miri / Loom in CI | **open** |
| T-03 | Compile all examples in CI | **open** |
| T-04 | Soak test | **open** |

## Product gaps

| Item | Status |
|------|--------|
| BEAM cluster protocol | **out of scope** |
| Global ETS | **open** (per-flow table exists) |
| JIT deopt in snapshot | **partial** — JIT counters in [`RuntimeMetricsSnapshot`] when `jit` |

## CI

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings` (std + `jit`)
- `cargo test --workspace --locked --offline` (after `cargo fetch`)

[`RemoteFrameDedup`]: ../src/scheduler/distribution.rs
[`DistributionConfig`]: ../src/scheduler/distribution.rs
[`OutboundHop`]: ../src/scheduler/distribution.rs
[`RootAuthorityPolicy`]: ../src/scheduler/runtime.rs
[`Runtime::spawn_untrusted`]: ../src/scheduler/runtime.rs
[`NodeRouter`]: ../src/transport/router.rs
[`Runtime::snapshot`]: ../src/scheduler/runtime.rs
[`RuntimeMetricsSnapshot`]: ../src/scheduler/metrics.rs
