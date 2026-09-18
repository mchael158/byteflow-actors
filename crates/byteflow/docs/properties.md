# Property / stress tests (Tier 1)

Byteflow treats malformed input and capacity edges as **first-class contracts**.
Coverage uses **in-house** logic (`std` + our PRNG) — no nested fuzz crate.

## Stable suite (`cargo test`)

| Harness | Surface | Invariants |
|---------|---------|------------|
| `tests/properties_mailbox.rs` | [`Mailbox`] + overflow policies | Occupancy ≤ capacity; `DropNewest` keeps the head; `Reject` reports `MessageLimit` |
| `tests/properties_decode.rs` | [`decode`] / [`verify`] / encode round-trip | Arbitrary bytes never panic; verified programs survive encode→decode |
| `src/scheduler/capability.rs` (`properties`) | `CapTable` mint / resolve / revoke | Holder-only resolve; `mint_or_reuse` stable; revoke invalidates tokens |

```bash
cargo test -p byteflow-actors --test properties_mailbox --test properties_decode
cargo test -p byteflow-actors scheduler::capability::properties
```

## Design notes

- Default library deps: **none** (`std` only). Optional `feature = "jit"` → Cranelift.
- Failures must be `Result` / `Fault` / `FormatError` / `VerifyError`, never
  unwind across the trust boundary (see [`vm-safety.md`](vm-safety.md)).
