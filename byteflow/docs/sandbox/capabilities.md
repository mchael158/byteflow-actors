# Capabilities

- `CapId` is a 128-bit OS CSPRNG token (`CapId::random`). Possession of the
  bits alone is not authorization.
- Authorization requires CapTable lookup + holder + target + rights + live
  revocation epoch.
- Delegation always intersects rights (`Cap::attenuate` /
  `CapTable::attenuate`). Escalation is impossible by construction.
- `revoke_flow` bumps the flow epoch and drops held/targeted entries so
  derived Caps fail closed under concurrent resolve.
- Host attestation: `Runtime::attest_capabilities(flow)` returns a snapshot
  of grants (not a cryptographic proof of process integrity).
