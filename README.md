# Byteflow

[![crates.io](https://img.shields.io/crates/v/byteflow-actors.svg)](https://crates.io/crates/byteflow-actors)
[![docs.rs](https://docs.rs/byteflow-actors/badge.svg)](https://docs.rs/byteflow-actors)
[![license](https://img.shields.io/crates/l/byteflow-actors.svg)](LICENSE)

In-process flow runtime for Rust: `Program` / `Fn` bytecode, M:N flows, Atomic
Hop (`Value::Message` on `Send`), FlowCap, quotas, named discovery, supervisor.

```toml
[dependencies]
byteflow-actors = "0.9.23"
```

```rust
use byteflow::{Program, Runtime, Value};
```

Docs and examples: [crates.io/crates/byteflow-actors](https://crates.io/crates/byteflow-actors) ·
[docs.rs/byteflow-actors](https://docs.rs/byteflow-actors) · crate README in
[`byteflow/README.md`](byteflow/README.md).

```text
cargo run -p byteflow-actors --example ping_pong
cargo run -p byteflow-actors --example atomic_actors
cargo run -p byteflow-actors --bin byteflow -- demo ping-pong
cargo install byteflow-actors
```

Not Tokio. Not a cluster. Host owns I/O; Byteflow owns flows. Remote hops are
`.bf` frames the host writes on its own socket —
[`byteflow/docs/beam-mapping.md`](byteflow/docs/beam-mapping.md),
[`byteflow/docs/remote-transport.md`](byteflow/docs/remote-transport.md).

Atomic Hop: [`byteflow/docs/atomic-hop.md`](byteflow/docs/atomic-hop.md).  
Scheduler logs: `BYTEFLOW_LOG=info`.

**Guides** (doctested where applicable):
[atomic-hop](byteflow/docs/atomic-hop.md) ·
[beam-mapping](byteflow/docs/beam-mapping.md) ·
[lifecycle](byteflow/docs/lifecycle.md) ·
[mailbox](byteflow/docs/mailbox.md) ·
[vm-safety](byteflow/docs/vm-safety.md) ·
[error-model](byteflow/docs/error-model.md) ·
[properties](byteflow/docs/properties.md) ·
[host-await](byteflow/docs/host-await.md) ·
[untrusted](byteflow/docs/untrusted.md) ·
[security](byteflow/docs/security.md) ·
[remote-transport](byteflow/docs/remote-transport.md) ·
[production-embedding](byteflow/docs/production-embedding.md) ·
[sandbox](byteflow/docs/sandbox/)

License: MIT OR Apache-2.0 · [github.com/mchael158/byteflow-actors](https://github.com/mchael158/byteflow-actors)
