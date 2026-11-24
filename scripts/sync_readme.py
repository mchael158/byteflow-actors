#!/usr/bin/env python3
"""Regenerate byteflow/README.md (the crates.io page) from README.md.

The crate README is the top-level README with links rewritten for the
package root, plus an API-contracts section. Edit README.md, then run:

    python scripts/sync_readme.py          # rewrite byteflow/README.md
    python scripts/sync_readme.py --check  # CI: fail if it is stale
"""
import os
import sys

os.chdir(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))

root=open('README.md',encoding='utf-8').read()
s=root
s=s.replace('# Byteflow\n','# byteflow-actors\n',1)
s=s.replace('](byteflow/examples)','](examples)')
s=s.replace('](byteflow/benches)','](benches)')
s=s.replace('[`byteflow/','[`')
s=s.replace('](byteflow/docs/','](docs/')
s=s.replace('''The [crate README](byteflow/README.md) lists the API contracts.
[CHANGELOG](byteflow/CHANGELOG.md).''','''[CHANGELOG](CHANGELOG.md).''')
s=s.replace('[CONTRIBUTING.md](CONTRIBUTING.md)','[CONTRIBUTING.md](https://github.com/mchael158/byteflow-actors/blob/main/CONTRIBUTING.md)')
contracts='''## API contracts

The package on crates.io is `byteflow-actors`. The library is imported as
`byteflow` (`use byteflow::...`).

- `#![deny(unsafe_code)]`. The `jit` feature allows `unsafe` only inside the
  Cranelift backend.
- Default dependency: [`getrandom`](https://docs.rs/getrandom), for `CapId`.
  Cranelift is optional, behind `jit`.
- A panic inside a flow stays on the worker boundary. Malformed bytecode
  produces a `Fault` on that flow.
- Natives must not block, because they run on a worker thread.
- Host APIs return `Result`. See [`docs/error-model.md`](docs/error-model.md).
- Values: `Unit | Bool | Int | Float | Pid | Message | Cap | Str | Bytes | List | Map | Record`.
- `Send` accepts only `Value::Message` (an Atomic Hop). The scheduler stamps
  the sender and attaches a reply capability.
- FlowCap: a 128-bit CSPRNG `CapId`. A grant happens only through
  `Cap::attenuate`, `Fn::delegate` or a confined spawn.
- `CALL_NATIVE` checks `CapRights::NATIVE` and the `NativeMask` before the
  table index.
- Quotas: `QuotaConfig::permissive` (the default) or `sandbox`. Process heap is
  capped with `max_runtime_bytes` / `MemoryBudget`.
- Host `Runtime::spawn` defaults to `RootAuthorityPolicy::TrustedHost`
  (`ROOT` + full natives). `RuntimeConfig::sandbox()` does **not** change that.
  Use `spawn_untrusted` or `root_authority`.
- Mailboxes are bounded (default 256 hops / 4 MiB / Reject). A bytecode
  sender to a full mailbox parks until a slot frees, in FIFO order; the
  waiters are bounded by `max_flows` ([`docs/mailbox.md`](docs/mailbox.md)).
- Registry `whereis` returns a SEND Cap, never a FlowId.
- `.bf` ABI: `encode` picks the oldest header that fits the constant pool
  (v5 scalars, v6 lists, v7 maps, v8 records). `decode` accepts all four.
- Remote: `encode_frame` / `decode_frame` / `deliver_frame`, an optional
  transactional `remote_dedup_window`, and a bridge via `DistributionConfig`.

Collecting a result without blocking forever:

| Call | Behavior |
|---|---|
| `try_join()` | Non-blocking; `None` if still running |
| `join_timeout(d)` / `join_deadline(t)` | Bounded wait; `None` on timeout |
| `join()` | Blocks until the outcome |

Std native indices: `print = 0`, `now_ms = 1`, `make_msg = 2`, `msg_* = 3–6`,
`msg_reply_cap = 7`. Use `Runtime::with_natives(..., std_native_table())`.

Security detail: [`docs/security.md`](docs/security.md),
[`docs/untrusted.md`](docs/untrusted.md), [`docs/sandbox/`](docs/sandbox/).

'''
s=s.replace('## Status\n',contracts+'## Status\n',1)
assert all(p not in s for p in ('](byteflow/', '[`byteflow/')), [l for l in s.splitlines() if 'byteflow/' in l and 'docs.rs' not in l]

if "--check" in sys.argv:
    with open("byteflow/README.md", encoding="utf-8") as f:
        if f.read() != s:
            sys.exit("byteflow/README.md is stale: run python scripts/sync_readme.py")
    print("byteflow/README.md is up to date")
else:
    with open("byteflow/README.md", "w", encoding="utf-8", newline="\n") as f:
        f.write(s)
    print("wrote byteflow/README.md")
