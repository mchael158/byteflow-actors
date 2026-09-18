# Byteflow

[![crates.io](https://img.shields.io/crates/v/byteflow-actors.svg)](https://crates.io/crates/byteflow-actors)
[![docs.rs](https://docs.rs/byteflow-actors/badge.svg)](https://docs.rs/byteflow-actors)
[![license](https://img.shields.io/crates/l/byteflow-actors.svg)](LICENSE)

[English](README.md) · [Português (Brasil)](README.pt-BR.md)

Embeddable concurrency for Rust: register bytecode via **`Program`** / **`Fn`**, **flows**, mailboxes, **Atomic Hop** (`Value::Message` only on `Send`), FlowCap (ABI v5) with attenuation and per-flow quotas, named discovery (`register_name` / `whereis`), and a supervisor — **one crate**.

```toml
[dependencies]
byteflow-actors = "0.9.5"
```

```rust
use byteflow::{Program, Runtime, Value};
```

Full documentation and examples: the crate README on [crates.io/crates/byteflow-actors](https://crates.io/crates/byteflow-actors) and [docs.rs](https://docs.rs/byteflow-actors).

```text
cargo run -p byteflow-actors --example ping_pong
cargo run -p byteflow-actors --example atomic_actors
cargo run -p byteflow-actors --bin byteflow -- demo ping-pong
cargo install byteflow-actors
```

**Not** a Tokio replacement / not distributed OTP. Host Rust owns I/O; Byteflow owns cheap flows.

Atomic Hop (`Value::Message`): see `crates/byteflow/docs/atomic-hop.md`. Set `BYTEFLOW_LOG=info` for scheduler stderr logs.

**Design guides** (every example is a doctest, so they cannot drift from the API):
[atomic-hop](crates/byteflow/docs/atomic-hop.md) ·
[beam-mapping](crates/byteflow/docs/beam-mapping.md) ·
[lifecycle](crates/byteflow/docs/lifecycle.md) ·
[mailbox](crates/byteflow/docs/mailbox.md) ·
[vm-safety](crates/byteflow/docs/vm-safety.md) ·
[error-model](crates/byteflow/docs/error-model.md) ·
[security](crates/byteflow/docs/security.md)

License: MIT OR Apache-2.0 · [github.com/mchael158/bytecode-vm](https://github.com/mchael158/bytecode-vm)
