//! Register-based interpreter for **one** virtual flow.
//!
//! The VM never touches threads, mailboxes, timers or the Cap table. It runs
//! until it finishes, hits an instruction budget, or needs a scheduler
//! effect — then returns a [`VmResult`] and stops. That split is what lets
//! the M:N scheduler move a suspended [`Vm`] between workers freely.
//!
//! | Type | Role |
//! |------|------|
//! | [`Vm`] | Per-flow machine state (frames, pc, natives handle) |
//! | [`VmResult`] | Complete / Yield / Sleep / Spawn / Send / Receive / Ask / … |
//! | [`Fault`] | Type errors, traps, native failures → flow failure |
//! | [`NativeTable`] | Host FFI slots indexed by `CallNative` |

mod fault;
mod frame;
mod machine;
mod native;
mod result;

pub use fault::{Fault, NativeCallError};
pub use machine::{Vm, MAX_CALL_DEPTH};
pub use native::{
    check_native_call, check_native_gate, expect_arg, expect_bool, expect_int, expect_message,
    expect_u64, NativeFn, NativeGate, NativeResult, NativeTable, NativeTableBuilder,
    NativeTableError,
};
pub use result::VmResult;
