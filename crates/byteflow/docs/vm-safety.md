# VM safety: the trust boundary

Bytecode is **untrusted input**. A `.bf` file may come from a compiler you
wrote, or from a plugin, a download, or a corrupted disk. The register
indices, jump offsets and table indices inside it are just bytes, and none
of them are guaranteed to make sense.

The crate is `#![deny(unsafe_code)]` (the optional `jit` feature allows
`unsafe` only inside the Cranelift backend), so a malformed chunk can never
corrupt host memory through the interpreter path. That is the floor, not
the goal: an index that is merely *checked* still has to fail the right
way — as one flow's fault, not as a panic that takes down the OS thread
running it and every flow queued behind it.

```text
  .bf bytes
      │
      ▼
   decode   ── FormatError   (structure, ABI version, blob ceiling)
      │
      ▼
   verify   ── VerifyError   (facts about the chunk; once per load)
      │
      ▼
     Vm     ── Fault         (facts about this step; per execution)
      │                          │
      ▼                          ▼
   worker  ──────────────► FlowOutcome::Failed ──► Supervisor
      │
      └─ catch_unwind ── backstop for a host bug in the interpreter,
                         not the path a Fault takes
```

## Who checks what, and why there

A fact belongs in `verify` when it is a property of the **chunk** — true or
false before anything runs, so checking it once per load beats checking it
on every execution of the same instruction. A fact belongs in the VM when it
depends on the **running state** (which frame is on top, what a register
holds right now), which no static pass can settle.

| Settled statically by `verify` | Why not the VM |
| ------------------------------ | ---------------- |
| `EmptyFunctionTable` | Nothing to enter; no execution to fault |
| `EntryOutOfRange` | Function entry is chunk data |
| `ConstOutOfRange` | Constant pool is chunk data |
| `FunctionOutOfRange` (`Call` / `Spawn`) | Function table is chunk data |
| `JumpOutOfRange` | Offset is chunk data; the hot loop must not re-add it per jump |
| `ArityExceedsRegisters` | Such a function cannot be entered *at all*; per-call checking only repeats the same verdict |
| `UnknownOpcode` | Opcode byte is chunk data |

| Checked per step by the `Vm` | Why not `verify` |
| ---------------------------- | ------------------ |
| `RegisterOutOfRange` | v0 has no dataflow pass; a cheap array bounds check beats proving liveness statically |
| `RegisterIndexOverflow` | Depends on the operand plus the gather offset |
| `TypeMismatch` | Depends on what the register holds at that instant |
| `DivideByZero` | Depends on a runtime value |
| `CallStackOverflow` | Depends on the depth reached, not on the code |
| `BadNative` | The native table is supplied by the embedder, outside this chunk |
| `Invariant` | A broken VM assumption; a fault instead of `unwrap` |

`Opcode::CallNative` is deliberately **not** range-checked by `verify`:
natives live in a [`NativeTable`](crate::NativeTable) the embedder builds at
runtime, which the chunk knows nothing about. An out-of-range slot is
[`Fault::BadNative`](crate::Fault::BadNative).

## Verification is not optional for untrusted input

[`Runtime::new`](crate::Runtime::new) verifies for you and refuses to start
on failure:

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use byteflow::{Program, Runtime, SpawnError};

let mut program = Program::new("malformed");
program.function_raw("main", 3, 1, |f| {
    let r0 = f.reg(0);
    f.return_(r0);
});

match Runtime::new(program.build()) {
    Err(SpawnError::VerifyFailed(_)) => {}
    Err(e) => return Err(format!("unexpected error: {e}").into()),
    Ok(_) => return Err("a runtime must not start on an unverifiable chunk".into()),
}
# Ok(())
# }
```

Calling [`verify`](crate::verify) directly gives the specific reason, which
is what a loader should log:

```rust
use byteflow::{verify, Program, VerifyError};

let mut program = Program::new("malformed");
program.function_raw("main", 3, 1, |f| {
    let r0 = f.reg(0);
    f.return_(r0);
});

assert!(matches!(
    verify(&program.build()),
    Err(VerifyError::ArityExceedsRegisters {
        function: 0,
        arity: 3,
        num_registers: 1,
    })
));
```

`arity > num_registers` matters because entering a function is the first
thing a call does: the VM copies `r0..arity` into a frame sized by
`num_registers`. Nothing about that is recoverable at runtime, and it used
to be an out-of-bounds index panic on the *caller's* thread — the embedder's
under `Runtime::spawn`, or a worker's under bytecode `Opcode::Spawn`, which
runs outside the `catch_unwind` that isolates `Vm::run`.

## Register indices are never plain arithmetic

Multi-operand opcodes gather arguments from consecutive registers. `Spawn`
is the sharp case: it reads them from `a+1..`, so `a = 255` leaves the index
space on the very first argument, before any bounds check gets a say.

Written as `instr.a + 1 + i` on `u8`, that addition has two behaviours and
both are wrong:

| Build   | Behaviour                                                       |
| ------- | --------------------------------------------------------------- |
| debug   | panics — `attempt to add with overflow`                         |
| release | **wraps to `r0`** and silently spawns with the wrong argument   |

The release one is worse, and it is the one that ships. Worse still, a debug
test suite is green either way. So every gather goes through a helper that
widens the sum and narrows it explicitly, yielding a fault:

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use byteflow::{Program, Fault, NativeTable, Vm, VmResult};
use std::sync::Arc;

let mut program = Program::new("overflow");
program.function_raw("main", 0, 2, |f| {
    let dst = f.reg(255);
    f.spawn_at(dst, 0, 1);
    let out = f.load_i32(0);
    f.return_(out);
});

let mut vm = Vm::new(Arc::new(program.build()), NativeTable::empty(), 0, &[])?;
assert!(matches!(
    vm.run(10),
    VmResult::Trap(Fault::RegisterIndexOverflow {
        base: 255,
        offset: 1,
    })
));
# Ok(())
# }
```

[`Fault::RegisterIndexOverflow`](crate::Fault::RegisterIndexOverflow) is
separate from [`Fault::RegisterOutOfRange`](crate::Fault::RegisterOutOfRange)
on purpose. The latter means "that register is representable, this frame just
does not have it":

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use byteflow::{Program, Fault, NativeTable, Vm, VmResult};
use std::sync::Arc;

let mut program = Program::new("out-of-range");
program.function_raw("main", 0, 2, |f| {
    let zero = f.reg(0);
    let bad = f.reg(200);
    f.mov(zero, bad);
    f.return_(zero);
});

let mut vm = Vm::new(Arc::new(program.build()), NativeTable::empty(), 0, &[])?;
assert!(matches!(
    vm.run(10),
    VmResult::Trap(Fault::RegisterOutOfRange {
        reg: 200,
        frame_size: 2,
    })
));
# Ok(())
# }
```

Collapsing the two would make the fault lie: reporting "register 255 out of
range" for a request that was really for register 256 sends whoever reads
the log looking in the wrong place. `Call` and `CallNative` gather through
the same helper even though their `dst + i` cannot currently overflow — the
bounds check happens to fire first, but only because `u8` register counts
cap a frame at 255 slots. That accident disappears the moment register
counts widen, and a latent overflow is not worth the two saved lines.

## Where the VM trusts `verify` instead of re-checking

Not every static fact is re-checked at runtime, and the doc should say so
rather than imply a uniform belt-and-braces.

`Jump` / `Branch` compute `pc + offset` and store it without a bounds test.
For a verified chunk that is safe — `JumpOutOfRange` already proved every
target lands inside the code, or exactly one past the end. For an
**unverified** chunk it degrades quietly: a negative target becomes a huge
`pc`, the instruction fetch misses, and the VM treats it as falling off the
end of the function — an implicit `return Unit` instead of a fault.

No memory is touched out of bounds (the fetch goes through a checked
`get`), but a corrupt jump turning into a silent early return is fail-open,
and it is the reason `verify` is mandatory rather than advisory for anything
that crossed a trust boundary.

## Faults never become panics

A `Fault` is a value, not an unwind. It travels out of `Vm::run` as
[`VmResult::Trap`](crate::VmResult::Trap), the worker turns it into
[`FlowOutcome::Failed`](crate::FlowOutcome::Failed), and the supervisor
decides whether to restart. One flow's bad bytecode cannot take down the
worker thread, and therefore cannot take down the flows queued behind it.

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use byteflow::{Program, FlowOutcome, Runtime};

let mut program = Program::new("trap");
program.function("main", 0, |f| f.trap(7));
let rt = Runtime::new(program.build())?;
let outcome = rt.spawn(0, &[])?.join();
rt.shutdown();

// The flow failed; the worker that ran it did not.
assert!(matches!(outcome, FlowOutcome::Failed(_)));
# Ok(())
# }
```

The `catch_unwind` around `Vm::run` is a backstop for a genuine host bug in
the interpreter, not the mechanism faults use. When it does fire, the flow
fails with `"flow panicked"` — a message with no specific cause, which is
exactly why turning a would-be panic into a typed `Fault` is worth the
effort every time the choice comes up.

## Hard rules

1. Untrusted bytecode goes through `decode` + `verify` before it reaches a
   `Vm`. `Runtime::new` does this; a hand-built `Vm` must do it too.
2. Register indices are never computed with plain `u8` arithmetic. Widen,
   then narrow explicitly, then fault.
3. Register writes are never `frame.registers[i] = _`. Use checked access so
   a malformed chunk faults instead of panicking on the caller's thread.
4. A new static fact about a chunk belongs in `verify`, not in the hot loop.
5. A fault must not lie about which index or which category failed. Add a
   variant rather than stretch a nearby one.
