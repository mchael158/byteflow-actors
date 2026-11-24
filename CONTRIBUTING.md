# Contributing to Byteflow

Thanks for taking the time. Bug reports, examples, docs fixes and code are
all welcome.

## 60-second setup

```console
git clone https://github.com/mchael158/byteflow-actors
cd byteflow-actors
cargo test                                  # about a minute on a laptop
cargo run --example ping_pong
cargo run --bin byteflow -- demo ping-pong
```

`rust-toolchain.toml` pins the toolchain CI uses for development. The
library itself builds on Rust 1.88 or newer with default features.

> **Windows + Git Bash:** if linking fails with `link: missing operand`,
> Git's coreutils `link` is shadowing MSVC's `link.exe`. Run cargo from
> PowerShell or `cmd`, or put the MSVC tools first on `PATH`.

## What CI checks

Run these before opening a pull request. They are the same commands as
[`.github/workflows/ci.yml`](.github/workflows/ci.yml):

```console
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --features jit -- -D warnings
cargo test --workspace
cargo test --workspace --features jit
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
python scripts/sync_readme.py --check
```

## House rules

- **No `unwrap` / `expect` in library code.** Clippy denies them. Return a
  `Result`, or fail closed with a `Trap` / `Fault`. Tests may opt out with
  `#![allow(clippy::unwrap_used, clippy::expect_used)]`.
- **No `unsafe`** outside the Cranelift backend behind `feature = "jit"`.
- **No new runtime dependencies** without discussing it in an issue first.
  Default builds depend on `getrandom` only.
- **Opcodes and std native indices are append-only.** Compiled `.bf` modules
  embed them, so renumbering one breaks every module in the wild.
- **Security-relevant changes** (verifier, capabilities, quotas, mailbox
  bounds) need a test in `tests/` that fails without the change.
- **README:** edit the top-level `README.md`, then run
  `python scripts/sync_readme.py` to regenerate `byteflow/README.md`.
- **README code is tested.** Every `rust` block in the README runs as a
  doctest (`cargo test --doc`), so keep each one a complete program.

## Commit messages

Say what changed and why, in the imperative mood, so the history explains
itself:

```text
mailbox: park every sender instead of failing past capacity

A fan-in with more than 2 × capacity senders in flight finalized the
extras with `waiting senders full`, and the collector hung. Parked
senders are existing flows whose hops are charged to their own quota,
so max_flows already bounds the waiter list.
```

A short prefix naming the area (`vm:`, `scheduler:`, `cli:`, `docs:`,
`ci:`) helps reviewers. Please avoid ticket-only messages such as `FIX:1234`.
Put the ticket in the body instead.

## Changelog

User-visible changes get a line under `## [Unreleased]` in
[`byteflow/CHANGELOG.md`](byteflow/CHANGELOG.md), in the matching
`Added` / `Changed` / `Fixed` / `Breaking` section.

## Reporting a security issue

Please do not open a public issue for a sandbox escape, verifier bypass or
capability leak. Use GitHub's private vulnerability reporting on this
repository instead
(**Security → Report a vulnerability**).
