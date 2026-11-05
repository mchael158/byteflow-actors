# Trust model

| Level | Constant pool Cap/Pid/Message | Typical host use |
| --- | --- | --- |
| `Trusted` | Allowed | Host assemblers embedding authority |
| `Restricted` | Rejected | Guest module + explicit Caps after load |
| `Untrusted` | Rejected | Deny-by-default guest (`RuntimeConfig` default trust) |

Elevation is **host-only**. Flows never mutate `RuntimeConfig::trust`.

`Restricted` and `Untrusted` share the same constant-pool denial
(`TrustLevel::forbids_authority_constants`). The difference is operational:
Restricted signals that the host intends to grant Caps/natives after
admission; Untrusted is the default fail-closed guest tier.
