# Sandbox architecture

Host-opt-in policy over the existing runtime. CapTable, mailboxes, and I/O stay
outside the VM.

```text
HOST
  → RuntimeConfig / TrustLevel / root_authority / policy_fingerprint
  → Runtime (M:N)
       Caps · Quotas · MemoryBudget · verify(trust)
  → Flow → Vm → VmResult / Fault
```

## Layers

1. **TrustLevel** — constant-pool Cap/Pid/Message at decode/verify (bytecode cannot raise).
2. **Capabilities** — 128-bit `CapId`, CapTable, attenuate-only, epoch revoke.
3. **Quotas** — per-flow `FlowQuota` + `max_flows` / `max_ask_waits` / `max_host_awaits` / `MemoryBudget`.
4. **verify / decode_with** — before admission.
5. **Enforcement** — NativeGate, hop auth, finalize cleanup.
6. **RootAuthorityPolicy** — Cap minted for host `spawn` (independent of TrustLevel).

## Config

| Knob | Effect |
|------|--------|
| Default `RuntimeConfig` | Permissive (`max_* = 0`) |
| `sandbox()` / `sandbox_restricted()` | Fail-closed quotas + relation caps + untrusted pool; **does not** change `root_authority` |
| `root_authority` | `TrustedHost` \| `Restricted` \| `Untrusted` for host `spawn` |
| `spawn_untrusted` | Forces Untrusted root Cap for that spawn |
| `applies_sandbox_caps()` | Whether process ceilings are armed |
| `policy_fingerprint()` | Digest of security knobs (includes `root_authority`) |
| `security_audit` | Module reject / Cap revoke / native deny (no full CapId) |

`trust-model.md` · `capabilities.md` · `quotas.md` · `threat-model.md` · `limitations.md`
