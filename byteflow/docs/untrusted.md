# Untrusted bytecode

`verify`, Caps, and quotas answer different questions. Passing one does not
imply the others.

| Control | Decides | Does not decide |
| --- | --- | --- |
| `verify` / `decode` | Chunk shape: opcodes, jumps, arities, gather/pack windows, receive immediates, constant tags for the trust level. Duplicate map key → `FormatError::DuplicateMapKey` | Whether the program should run |
| Capabilities | Send / spawn / delegate rights. `Fn::spawn_confined` → `CapRights::NONE`. `Delegate` only intersects | Resource use (a rightsless flow can still spin) |
| Quotas / `RuntimeConfig::sandbox` | CPU, heap, spawn/send rate, live flows, links, asks, host awaits, registry names, process memory | Correctness of bytecode or host isolation |
| `fingerprint_bf` + `Runtime::with_attested` | Bytes match the digest the host expected | Harmlessness of those bytes |
| Native table / `NativeMask` | Which `CallNative` indices run | Scheduler opcodes (`Spawn`, `Send`, `Jump`, …) |

`Runtime::new` is permissive (`QuotaConfig::permissive`, unlimited `max_*`).
Sandbox is opt-in: `RuntimeConfig::sandbox` / `sandbox_restricted` /
`QuotaConfig::sandbox`. `applies_sandbox_caps()` reports armed process ceilings.

`sandbox()` does **not** shrink host [`Runtime::spawn`] authority (default
[`RootAuthorityPolicy::TrustedHost`]: `ROOT` + full native mask). For a guest
root with no rights/natives: [`Runtime::spawn_untrusted`] or
`root_authority = Untrusted` / `Restricted`. Bytecode `spawn_confined` stays
`NONE`.

`examples/untrusted.rs`: assemble → reject bad listing/bytes → digest check →
empty native table → `Spawn` rights `0` + `Delegate` of empty mask → child jump
loop killed by tight `cpu_budget`.

Also: [`beam-mapping.md`](beam-mapping.md), [`host-await.md`](host-await.md),
[`sandbox/`](sandbox/).

[`Runtime::spawn`]: ../src/scheduler/runtime.rs
[`Runtime::spawn_untrusted`]: ../src/scheduler/runtime.rs
[`RootAuthorityPolicy::TrustedHost`]: ../src/scheduler/runtime.rs
