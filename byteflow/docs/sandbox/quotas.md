# Quotas

Independent ceilings (never a single generic counter):

| Resource | Config | Enforcement |
| --- | --- | --- |
| Live flows | `max_flows` (`0` = unlimited) | `FlowLimit` CAS reserve/release |
| Ask waits | `max_ask_waits` | `AskWaitIndex` |
| HostAwait | `max_host_awaits` | `HostAwaitIndex` |
| Runtime heap | `max_runtime_bytes` | `MemoryBudget` CAS |
| Per-flow CPU/mem/spawn/send | `QuotaConfig` | `FlowQuota` / `TokenBucket` |
| Mailbox hops/bytes | `MailboxConfig` | Per-mailbox bounds |

`RuntimeConfig::sandbox()` starting points: 128 flows, 64 ask waits, 128
host awaits, 64 MiB runtime bytes, `QuotaConfig::sandbox`, default mailbox
256 hops / 4 MiB / Reject.

Charge before mutate; failure leaves no partial reservation. Finalize
releases flow quota memory against `MemoryBudget` and revokes Caps.
