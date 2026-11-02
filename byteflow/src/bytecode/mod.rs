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
//! Headers: [`ABI_VERSION`] (scalars, v5), [`ABI_VERSION_LISTS`] (v6),
//! [`ABI_VERSION_MAPS`] (v7), and [`ABI_VERSION_RECORDS`] (v8). [`encode`]
//! writes the oldest of these that can hold the constant pool. [`decode`]
//! accepts all four.

mod asm;
pub(crate) mod builder;
mod cap;
mod chunk;
mod disasm;
mod format;
mod instruction;
mod opcode;
mod pattern;
mod program;
mod restart;
mod value;
mod verify;

pub use asm::{assemble, AssembleError};
pub use cap::{
    Cap, CapId, CapIdError, CapRights, CapTarget, NativeIdx, NativeMask, RevocationCell,
};
pub use chunk::{
    Chunk, FunctionDef, ABI_VERSION, ABI_VERSION_LISTS, ABI_VERSION_MAPS, ABI_VERSION_RECORDS,
    MAGIC,
};
pub use disasm::disassemble;
pub use format::{decode, decode_with, encode, FormatError};
pub(crate) use format::{decode_value, encode_value, MAX_VALUE_DEPTH};
pub use instruction::Instruction;
pub use opcode::{IntCmp, Opcode};
pub use pattern::payload_matches;
pub use program::{Fn, FuncId, Label, Program, Reg, RegWindow};
pub use restart::RestartPolicy;
pub use value::{
    MapKey, Message, Value, MAX_LIST_LEN, MAX_MAP_LEN, MAX_RECORD_LEN, TAG_SYS_AWAIT, TAG_SYS_DOWN,
    TAG_SYS_EXIT,
};
pub use verify::{verify, verify_with, ConstantKind, TrustLevel, VerifyConfig, VerifyError};
