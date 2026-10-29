# Audit checklist (engineering)

Track hardening work derived from the internal architecture audit. Status:
**open** / **partial** / **done** (update as PRs land).

## P0 — remote exposure

| ID | Item | Status |
|----|------|--------|
| S-01 | Document that inbound TCP has no auth; require TLS/firewall in production | **done** — [`production-embedding.md`](production-embedding.md) |
| S-01 | Example uses channel deliver (no `Mutex<Runtime>`) | **done** — `remote_hop_tcp.rs` |

## P1 — robustness

| ID | Item | Status |
|----|------|--------|
| S-02 | Optional replay filter on `deliver_frame` | **done** — [`RemoteFrameDedup`] + `RuntimeConfig::remote_dedup_window` |
| C-01 | Outbound pump stops on `encode_frame` failure (fail closed) | **done** |
| C-01 | Document single-hop loss if encode fails after bridge dequeue | **partial** — [`remote-transport.md`](remote-transport.md) |
| T-01 | Stress tests: random bytes → wire/frame decode never panic | **done** — `tests/wire_decode_stress.rs` |
| — | [`NodeRouter`] for `node → SocketAddr` | **done** — [`transport/router.rs`](../src/transport/router.rs) |

## P2 — operations

| ID | Item | Status |
|----|------|--------|
| — | [`Runtime::snapshot`] for dashboards | **done** |
| — | Structured event stream (OpenTelemetry-style) | **open** |
| T-02 | Miri / loom in CI | **open** |
| T-03 | Compile all examples in CI | **open** |
| T-04 | Long soak test | **open** |

## P3 — product gaps (documented)

| Item | Status |
|------|--------|
| BEAM cluster protocol | **out of scope** |
| Global ETS | **open** (per-flow table exists) |
| JIT deopt metrics in snapshot | **partial** — JIT counters in [`RuntimeMetricsSnapshot`] when feature enabled |

## CI baseline (keep green)

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings` (std + `jit`)
- `cargo test --workspace --locked --offline` (after `cargo fetch`)

[`RemoteFrameDedup`]: ../src/scheduler/distribution.rs
[`NodeRouter`]: ../src/transport/router.rs
[`Runtime::snapshot`]: ../src/scheduler/runtime.rs
[`RuntimeMetricsSnapshot`]: ../src/scheduler/metrics.rs
