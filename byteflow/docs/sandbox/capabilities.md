# Capabilities

- `CapId::random` — 128-bit OS CSPRNG. Bits alone are not authorization.
- Resolve needs CapTable + holder + target + rights + live revocation epoch.
- Grants only shrink: `Cap::attenuate` / `CapTable::attenuate`.
- `revoke_flow` bumps epoch and drops held/targeted entries (fail-closed under concurrent resolve).
- `Runtime::attest_capabilities(flow)` — grant snapshot for audit, not a crypto proof.
- Host root Cap: `RootAuthorityPolicy` / `Runtime::spawn_untrusted` (see `architecture.md`).
