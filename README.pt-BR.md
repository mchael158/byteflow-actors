# Byteflow

[![crates.io](https://img.shields.io/crates/v/byteflow-actors.svg)](https://crates.io/crates/byteflow-actors)
[![docs.rs](https://docs.rs/byteflow-actors/badge.svg)](https://docs.rs/byteflow-actors)

[English](README.md) · [Português (Brasil)](README.pt-BR.md)

Concorrência embutível em Rust: bytecode com **`Program`** / **`Fn`**, **flows**, mailboxes, **Atomic Hop** (`Value::Message` só no `Send`), FlowCap (ABI v5) com atenuação e quotas por flow, descoberta por nome (`register_name` / `whereis`), e supervisor — **uma única crate**.

```toml
[dependencies]
byteflow-actors = "0.9.3"
```

```rust
use byteflow::{Program, Runtime, Value};
```

Documentação completa: [crates.io/crates/byteflow-actors](https://crates.io/crates/byteflow-actors) e [docs.rs](https://docs.rs/byteflow-actors).

```text
cargo run -p byteflow-actors --example ping_pong
cargo run -p byteflow-actors --example atomic_actors
cargo run -p byteflow-actors --bin byteflow -- demo ping-pong
cargo install byteflow-actors
```

**Não** substitui Tokio / **não** é OTP distribuído. O Rust hospedeiro fica com o I/O.

Atomic Hop (`Value::Message`): ver `crates/byteflow/docs/atomic-hop.md`. Use `BYTEFLOW_LOG=info` para logs do scheduler no stderr.

**Guias de design** (todo exemplo é um doctest, então não podem divergir da API):
[atomic-hop](crates/byteflow/docs/atomic-hop.md) ·
[beam-mapping](crates/byteflow/docs/beam-mapping.md) ·
[lifecycle](crates/byteflow/docs/lifecycle.md) ·
[mailbox](crates/byteflow/docs/mailbox.md) ·
[vm-safety](crates/byteflow/docs/vm-safety.md) ·
[error-model](crates/byteflow/docs/error-model.md) ·
[security](crates/byteflow/docs/security.md)

Licença: MIT OR Apache-2.0 · [github.com/mchael158/bytecode-vm](https://github.com/mchael158/bytecode-vm)
