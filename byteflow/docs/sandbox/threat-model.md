# Threat model (sandbox)

## In scope

- Hostile `.bf` (malformed wire, Cap/Pid/Message constants)
- Exhaustion via spawn / send / ask / host-await / heap
- Cap confusion (wrong holder, revoked token, rights escalation)
- `CallNative` without `NATIVE` / mask

## Out of scope

- Kernel isolation (containers, seccomp, separate AS)
- Hostile host Rust (natives, bridges, embedder bugs)
- Crypto module attestation (host signing on `fingerprint_bf`)
- Network peers (transport is host-owned)

## Fail-closed

- Default trust = Untrusted
- No HostAwait bridge → opcode fails
- Quota / flow limit hit → error
- Revoked Cap → resolve fails; natives → `NativeCallError::Revoked`
