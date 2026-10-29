# Untrusted bytecode

The verifier, the capability table, and the quotas answer different
questions. Passing one of them does not make a module safe.

| Control | What it decides | What it does not decide |
| --- | --- | --- |
| `verify` / `decode` | The module is structurally a chunk this runtime can load: opcodes, jumps, arities, gather windows (`Call` / `Spawn` / `CallNative` / `StartChild`), pack windows, receive immediates, and constant tags allowed for the trust level. A repeated map key is `FormatError::DuplicateMapKey` | Whether the program should be allowed to run, or what it will do |
| Capabilities | Which caps a flow may send on, spawn with, or delegate. `Fn::spawn_confined` starts at `CapRights::NONE`. `Delegate` only intersects | Resource consumption. A flow with no rights can still spin |
| Quotas and `RuntimeConfig::sandbox` | CPU, heap, spawn/send rate, live flows, links, asks, host awaits, registry names, process memory | That the bytecode is correct or that the host's other code is isolated by the type system |
| Attestation (`fingerprint_bf` + `Runtime::with_attested`) | The bytes are the module the host intended to load | That those bytes are harmless |
| Native table / `NativeMask` | Which host functions `CallNative` may reach | Scheduler effects such as `Spawn`, `Send`, and `Jump` |

`Runtime::new` stays permissive (`QuotaConfig::permissive`, no process-wide
caps). A sandbox is opt-in via `RuntimeConfig::sandbox` and
`QuotaConfig::sandbox`. The embedder still owns sockets, files, and Tokio.

`examples/untrusted.rs` loads a `.bf` produced by `assemble`, rejects a bad
listing and a bad byte buffer, checks the digest, spawns the guest with an
empty native table, and uses bytecode `Spawn` with rights `0` plus
`Delegate` of that same empty mask. The child is a jump loop. A tight
`cpu_budget` finalizes that child. The host process prints the outcome and
returns.

See also `docs/beam-mapping.md` for which ABI a module is written as, and
`docs/host-await.md` for host work that must not run on a worker.
