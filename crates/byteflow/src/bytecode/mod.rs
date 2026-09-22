//! Instruction set, `.bf` (BFV0) wire format, assembler and verifier.
//!
//! Pure data plane — no threads, no mailboxes. The scheduler and VM consume
//! [`Chunk`] values produced here.
//!
//! | Piece | Purpose |
//! |-------|---------|
//! | [`Opcode`] / [`Instruction`] | ISA (opcodes are **append-only**) |
//! | [`Value`] / [`Message`] | Runtime / constant-pool tags (ABI versioned) |
//! | [`Program`] / [`Fn`] | Host-side assembler with named registers |
//! | [`encode`] / [`decode`] | `.bf` module bytes (`MAGIC` + [`ABI_VERSION`]) |
//! | [`verify`] | Structural checks before a [`crate::Runtime`] starts |
//!
//! Current ABI: see [`ABI_VERSION`] (FlowCap + `Str` / `Bytes`).

pub(crate) mod builder;
mod cap;
mod chunk;
mod disasm;
mod format;
mod instruction;
mod macros;
mod opcode;
mod program;
mod restart;
mod value;
mod verify;

pub use cap::{
    Cap, CapId, CapIdError, CapRights, CapTarget, NativeIdx, NativeMask, RevocationCell,
};
pub use chunk::{Chunk, FunctionDef, ABI_VERSION, MAGIC};
pub use disasm::disassemble;
pub use format::{decode, decode_with, encode, FormatError};
pub use instruction::Instruction;
pub use macros::asm_macros;
pub use opcode::Opcode;
pub use program::{Fn, FuncId, Label, Program, Reg, RegWindow};
pub use restart::RestartPolicy;
pub use value::{Message, Value, TAG_SYS_DOWN, TAG_SYS_EXIT};
pub use verify::{verify, verify_with, ConstantKind, TrustLevel, VerifyConfig, VerifyError};
