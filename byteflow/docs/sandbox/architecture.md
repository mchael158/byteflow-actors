# Sandbox architecture

Byteflow’s sandbox is a **host-opt-in** policy layer over the existing runtime
boundary. It does not move CapTable, mailboxes, or I/O into the VM.

```text
HOST APPLICATION
       |
       v
RuntimeConfig / TrustLevel / policy_fingerprint
       |
       v
  Runtime (scheduler M:N)
       |
  +----+----+----+
  |    |    |    |
 Caps Quotas MemoryBudget  verify(trust)
  |    |    |    |
  +----+----+----+
       |
     Flow → single Vm → VmResult / Fault
```

## Layers

1. **Trust policy** — `TrustLevel::{Trusted, Restricted, Untrusted}` controls
   constant-pool authority tags at decode/verify. Bytecode cannot raise trust.
2. **Capabilities** — `CapId` (128-bit CSPRNG), `CapTable`, attenuation-only
   delegation, epoch revocation.
3. **Resource quotas** — per-flow `FlowQuota` + process-wide `max_flows`,
   `max_ask_waits`, `max_host_awaits`, `MemoryBudget`.
4. **Bytecode validation** — `verify` / `decode_with` before admission.
5. **Runtime enforcement** — NativeGate, hop auth, finalize cleanup.

## Opt-in

- Default `RuntimeConfig` stays **permissive** (`max_* = 0` unlimited).
- `RuntimeConfig::sandbox()` / `sandbox_restricted()` enable fail-closed caps.
- `RuntimeConfig::applies_sandbox_caps()` reports whether process-wide ceilings
  are armed (independent of `TrustLevel`).
- `RuntimeConfig::policy_fingerprint()` identifies the active policy.
- Optional `security_audit` sink receives module rejects, Cap revoke, and
  native denials without full CapId tokens.

See also `trust-model.md`, `capabilities.md`, `quotas.md`, `threat-model.md`,
`limitations.md`.
