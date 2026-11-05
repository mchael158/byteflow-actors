# Limitations

**The Byteflow sandbox is not equivalent to a separate OS process or
container.** All Flows, VMs, natives, and host bridges share one address
space and one Rust process.

Implications:

- A compromised native or `unsafe` in host code bypasses Caps and quotas.
- Memory accounting covers runtime-managed heap charges, not every Rust
  allocation the host performs before handing buffers over.
- Policy fingerprints and capability attestations are audit aids, not
  hardware roots of trust.
- Tokio / sockets / files remain host responsibilities; the sandbox only
  bounds scheduler-visible resources.

Deployment recommendation: pair `RuntimeConfig::sandbox()` with process
isolation when the guest must be treated as fully adversarial at the OS
boundary.
