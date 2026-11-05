# Limitations

Sandbox ≠ separate OS process. All flows, VMs, natives, and host bridges share
one address space.

- Compromised native / host `unsafe` bypasses Caps and quotas.
- `MemoryBudget` counts runtime heap charges (`Str`/`Bytes`/shared heaps), not
  every host allocation before hand-off.
- `policy_fingerprint` / `attest_capabilities` are audit aids, not hardware RoT.
- Sockets, files, Tokio stay host-owned; sandbox bounds scheduler-visible resources only.

For OS-level isolation of adversarial guests, run the host process under an
external sandbox (container / seccomp / separate process) **in addition to**
`RuntimeConfig::sandbox()`.
