# Property / stress tests (Tier 1)

Byteflow treats malformed input and capacity edges as **first-class contracts**.
Coverage uses **in-house** logic (`std` + our PRNG) — no nested fuzz crate.

## Stable suite (`cargo test`)

| Harness | Surface | Invariants |
|---------|---------|------------|
| `tests/properties_mailbox.rs` | [`crate::Mailbox`] + overflow policies | Occupancy ≤ capacity; `DropNewest` keeps the head; `Reject` reports `MessageLimit` |
| `tests/properties_decode.rs` | [`crate::decode`] / [`crate::verify`] / encode round-trip | Arbitrary bytes never panic; a `LoadImm` module survives encode→decode. Nested values round-trip and the header is the oldest ABI that can hold them. A lowered header rejects the newer tag. A repeated map key is `DuplicateMapKey`; any other key byte is `UnknownMapKey`. A bool byte other than `0` or `1` is `NonCanonicalBool` |
| `src/scheduler/capability.rs` (`properties`) | `CapTable` mint / resolve / revoke | Holder-only resolve; `mint_or_reuse` stable; revoke invalidates tokens |

```bash
cargo test -p byteflow-actors --test properties_mailbox --test properties_decode
cargo test -p byteflow-actors scheduler::capability::properties
```

## Design notes

- Default library deps: **`getrandom`** (OS CSPRNG for `CapId`). Optional `feature = "jit"` → Cranelift.
- Failures must be `Result` / `Fault` / `FormatError` / `VerifyError`, never
  unwind across the trust boundary (see [`vm-safety.md`](vm-safety.md)).
