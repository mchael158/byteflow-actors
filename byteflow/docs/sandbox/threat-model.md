# Threat model (sandbox)

## In scope

- Hostile `.bf` modules (malformed wire, forged Cap/Pid/Message constants).
- Resource exhaustion via spawn/send/ask/host-await/heap growth.
- Capability confusion (wrong holder, revoked token, rights escalation).
- Native calls without NATIVE right / allowlist mask.

## Out of scope

- Kernel / process isolation (containers, seccomp, separate address spaces).
- Hostile **host** Rust (natives, bridges, embedder bugs).
- Cryptographic module attestation (use host signing on top of
  `fingerprint_bf`).
- Network peers; transport is host-owned.

## Fail-closed defaults

- Verify/decode trust = Untrusted.
- Missing HostAwait bridge → opcode fails.
- Quota / flow limit exhaustion → error, not silent unlimited.
- Revoked Cap → resolve fails; natives see `NativeCallError::Revoked`.
