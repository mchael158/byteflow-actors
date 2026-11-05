# Trust model

| Level | Cap/Pid/Message in constant pool | Use |
| --- | --- | --- |
| `Trusted` | Allowed | Host-assembled modules with embedded authority |
| `Restricted` | Rejected | Guest module; host grants Caps/natives after load |
| `Untrusted` | Rejected | Default guest (`RuntimeConfig` default trust) |

Flows cannot change `RuntimeConfig::trust`.

`Restricted` and `Untrusted` share `forbids_authority_constants`. Restricted
means the host plans post-admission grants; Untrusted is the default deny tier.

**≠ [`RootAuthorityPolicy`](../../src/scheduler/runtime.rs).** TrustLevel gates
verify constants. Root authority gates the Cap for host `spawn`. An Untrusted
module started with `Runtime::spawn` still gets TrustedHost `ROOT` unless the
host uses `spawn_untrusted` or sets `root_authority`.
