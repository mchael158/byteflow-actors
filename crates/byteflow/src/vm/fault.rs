use std::fmt;

use crate::bytecode::NativeIdx;

/// Why a `CallNative` was refused before the function pointer ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeCallError {
    NoNativeRight,
    IndexNotAllowlisted(NativeIdx),
    IndexOutOfRange(NativeIdx),
    Revoked,
}

impl fmt::Display for NativeCallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NativeCallError::NoNativeRight => f.write_str("CALL_NATIVE: flow lacks NATIVE right"),
            NativeCallError::IndexNotAllowlisted(idx) => {
                write!(f, "CALL_NATIVE: index {idx} not on allowlist")
            }
            NativeCallError::IndexOutOfRange(idx) => {
                write!(f, "CALL_NATIVE: index {idx} out of range")
            }
            NativeCallError::Revoked => f.write_str("CALL_NATIVE: capability revoked"),
        }
    }
}

impl std::error::Error for NativeCallError {}

/// Anything that can go wrong *inside* a running Flow's VM.
///
/// A `Fault` is never a Rust panic — panics are reserved for genuine host
/// bugs and are caught at the worker boundary (see
/// `crate::scheduler::worker`) precisely so that one Flow's
/// bug (division by zero, a corrupt jump target that slipped past the
/// verifier, an out-of-range register) can never take down a worker thread,
/// let alone the whole runtime. A `Fault` instead becomes
/// [`crate::FlowOutcome::Failed`] and is handed to the Flow's supervisor, which
/// decides whether to restart it (design notes §15-16).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fault {
    DivideByZero,
    RegisterOutOfRange { reg: u8, frame_size: u8 },
    /// An instruction's register operand plus the offset it gathers at does
    /// not fit the register index space *at all* — e.g. `Spawn a=255`, which
    /// reads its arguments from `a+1..`.
    ///
    /// Distinct from [`Fault::RegisterOutOfRange`], which is about an index
    /// that is perfectly representable and merely absent from this frame.
    /// Kept separate so the fault cannot lie: reporting "register 255 is out
    /// of range" for a request that was really for register 256 would send
    /// whoever reads it looking in the wrong place.
    RegisterIndexOverflow { base: u8, offset: u8 },
    BadConstant { index: u32, pool_size: u32 },
    BadFunction { index: u32, table_size: u32 },
    /// Function call nesting exceeded `Vm::MAX_CALL_DEPTH`. Bytecode has no
    /// native stack overflow (frames are heap-allocated `Vec<Value>`s), so
    /// this is a deliberate, checked limit rather than a segfault.
    CallStackOverflow { depth: usize },
    TypeMismatch { expected: &'static str, got: &'static str },
    /// `CallNative` referenced a slot outside the runtime's registered
    /// native function table (design notes §30-31). Distinct from
    /// `BadFunction`, which is about the *bytecode* function table baked
    /// into the chunk — natives are supplied by the embedder at `Vm`
    /// construction time and can't be range-checked by
    /// `crate::bytecode::verify`, which has no visibility into them.
    BadNative { index: u32, table_size: u32 },
    /// A native function returned an error (host-side failure — I/O,
    /// invalid argument the Rust side rejected, capability denied, etc).
    /// The message is native-function-defined.
    NativeError(String),
    /// S7: `CallNative` failed the allowlist / NATIVE-right / revocation
    /// check before the function pointer was touched.
    NativeDenied(NativeCallError),
    /// Explicit `Trap` opcode, e.g. an assertion emitted by a compiler.
    Explicit(i32),
    /// Broken VM invariant (e.g. empty frame stack while running). Category D
    /// in the error model — surfaced as a Flow fault, never as `unwrap`.
    Invariant(&'static str),
    /// Per-flow heap quota refused a `Str` / `Bytes` register write.
    QuotaExceeded(String),
    /// Relative `Jump` / `Branch` target outside the code (paranoid mode only).
    ///
    /// Verified chunks never hit this — `verify` already proved every offset.
    /// Without paranoid mode, a corrupt jump fail-opens as an implicit
    /// `return Unit` (see `docs::vm_safety`).
    BadJump { target: usize, code_len: usize },
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fault::DivideByZero => write!(f, "division by zero"),
            Fault::RegisterOutOfRange { reg, frame_size } => {
                write!(f, "register r{reg} out of range (frame has {frame_size} registers)")
            }
            Fault::BadConstant { index, pool_size } => {
                write!(f, "constant index {index} out of range (pool size {pool_size})")
            }
            Fault::BadFunction { index, table_size } => {
                write!(f, "function index {index} out of range (table size {table_size})")
            }
            Fault::RegisterIndexOverflow { base, offset } => write!(
                f,
                "register index r{base}+{offset} overflows the register index space (max r255)"
            ),
            Fault::CallStackOverflow { depth } => write!(f, "call stack overflow at depth {depth}"),
            Fault::TypeMismatch { expected, got } => {
                write!(f, "type mismatch: expected {expected}, got {got}")
            }
            Fault::BadNative { index, table_size } => {
                write!(f, "native function index {index} out of range (table size {table_size})")
            }
            Fault::NativeError(msg) => write!(f, "native function error: {msg}"),
            Fault::NativeDenied(err) => write!(f, "{err}"),
            Fault::Explicit(code) => write!(f, "explicit trap (code {code})"),
            Fault::Invariant(msg) => write!(f, "vm invariant broken: {msg}"),
            Fault::QuotaExceeded(msg) => write!(f, "{msg}"),
            Fault::BadJump { target, code_len } => {
                write!(f, "jump target {target} out of range (code length {code_len})")
            }
        }
    }
}

impl std::error::Error for Fault {}
