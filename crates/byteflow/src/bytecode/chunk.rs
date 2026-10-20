use super::instruction::Instruction;
use super::value::Value;

/// On-disk magic for the `.bf` module format (see design notes §32).
/// Chosen so a corrupted/truncated file is rejected in the first 4 bytes
/// rather than partway through decoding.
pub const MAGIC: [u8; 4] = *b"BFV0";

/// Current ABI version. Bump on any breaking change to instruction
/// encoding, constant representation, or function-table layout.
///
/// v2: [`Value::Message`] wire tag `5`.
/// v3: `Message.reply_cap`; [`Value::Cap`] wire tag `6` (FlowCap).
/// v4: [`Value::Str`] tag `7`, [`Value::Bytes`] tag `8`.
/// v5: CapId is 128-bit; `Message.payload` is a nested [`Value`].
pub const ABI_VERSION: u32 = 5;

/// A callable entry point inside a [`Chunk`]: either bytecode-defined or a
/// slot reserved for a native (Rust) function registered with the runtime
/// via the FFI table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionDef {
    pub name: String,
    /// Index into `Chunk::code` where execution starts.
    pub entry: u32,
    /// Number of parameters. The VM copies them into `r0..arity`
    /// (exclusive end): arity 2 fills `r0` and `r1`.
    pub arity: u8,
    /// Upper bound on registers this function uses; the VM allocates
    /// exactly this many per call frame instead of a fixed worst-case size.
    pub num_registers: u8,
}

/// A compiled unit of Byteflow bytecode: code, constants and the function
/// table. One `Chunk` can back many concurrently-running flows — it is
/// immutable after construction, so it is shared behind an `Arc` rather than
/// copied per flow (see [`crate::Vm`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Chunk {
    pub name: String,
    pub constants: Vec<Value>,
    pub code: Vec<Instruction>,
    pub functions: Vec<FunctionDef>,
}

impl Chunk {
    pub fn function(&self, index: u32) -> Option<&FunctionDef> {
        self.functions.get(index as usize)
    }

    pub fn constant(&self, index: u32) -> Option<&Value> {
        self.constants.get(index as usize)
    }

    /// Number of instructions, used by the verifier to bound-check jump
    /// targets ahead of time instead of on every branch at runtime.
    pub fn len(&self) -> usize {
        self.code.len()
    }

    pub fn is_empty(&self) -> bool {
        self.code.is_empty()
    }
}
