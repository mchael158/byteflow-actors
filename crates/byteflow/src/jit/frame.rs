//! Execution frame passed into compiled traces.

/// Byte offsets for [`JitFrame`] fields (must match `repr(C)` layout).
pub const OFF_REGISTER_COUNT: i32 = 8;
pub const OFF_PC: i32 = 12;
pub const OFF_BUDGET: i32 = 16;
pub const OFF_FUNCTION: i32 = 20;
pub const OFF_EXIT_KIND: i32 = 24;
pub const OFF_RETURN_REG: i32 = 28;
pub const OFF_CALL_DEPTH: i32 = 32;
pub const OFF_CALL_STACK: i32 = 40;
/// Four `u64` words: bit `reg` is set when the trace stored that register.
/// Must match [`JitFrame::written`].
pub const OFF_WRITTEN: i32 = 48;
/// Four `u64` words: bit `reg` means the stored value is a `Bool`, not an `Int`.
/// Must match [`JitFrame::as_bool`].
pub const OFF_AS_BOOL: i32 = 80;
/// Four `u64` words: bit `reg` means the stored value is `Unit`.
/// Must match [`JitFrame::as_unit`].
pub const OFF_AS_UNIT: i32 = 112;
/// Tags of the registers as they were when the trace was entered.
/// A `Move` of a slot the trace has not assigned copies these bits unless
/// that register was already written earlier in the same execution.
/// Must match [`JitFrame::entry_as_bool`] and [`JitFrame::entry_as_unit`].
pub const OFF_ENTRY_BOOL: i32 = 144;
pub const OFF_ENTRY_UNIT: i32 = 176;
/// Spill arena for in-trace calls. One window of [`JitFrame::spill_stride`]
/// slots per [`MAX_JIT_CALL_DEPTH`] level.
pub const OFF_SPILL: i32 = 208;
pub const OFF_SPILL_STRIDE: i32 = 216;

/// Byte offsets inside [`JitCallRecord`].
pub const REC_RETURN_PC: i32 = 0;
pub const REC_RETURN_FN: i32 = 4;
pub const REC_DEST: i32 = 8;
pub const REC_CALLER_REGS: i32 = 12;
pub const REC_WRITTEN: i32 = 16;
pub const REC_AS_BOOL: i32 = 48;
pub const REC_AS_UNIT: i32 = 80;

/// Maximum nested intra-chunk calls handled inside one compiled trace.
pub const MAX_JIT_CALL_DEPTH: usize = 32;

/// Saved caller state for one `Opcode::Call`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct JitCallRecord {
    pub return_pc: u32,
    pub return_function: u32,
    pub dest_reg: u32,
    pub caller_register_count: u32,
    /// Caller [`JitFrame::written`] saved across the leaf.
    pub written: [u64; 4],
    pub as_bool: [u64; 4],
    pub as_unit: [u64; 4],
}

/// Mutable state for one JIT invocation on the current bytecode frame.
///
/// v1 keeps scalar work in a parallel `i64` slot table (Int specialization).
/// The dispatcher copies [`crate::Value::Int`] in before the call and
/// writes results back after `Return`.
#[repr(C)]
pub struct JitFrame {
    pub slots: *mut i64,
    pub register_count: u32,
    pub pc: u32,
    pub budget: u32,
    pub function: u32,
    pub exit_kind: u32,
    pub return_reg: u32,
    pub call_depth: u32,
    pub call_stack: *mut JitCallRecord,
    /// Registers this trace assigned. Unset bits keep the VM's previous value
    /// on the way back, so a `Unit` the trace never wrote does not become `Int(0)`.
    pub written: [u64; 4],
    /// Among [`Self::written`] bits, which results are bytecode `Bool`s
    /// (`Eq` / `Lt` / `Le` / `LoadConst` of a bool, or a `Move` of one).
    pub as_bool: [u64; 4],
    /// Among [`Self::written`] bits, which results are `Unit`.
    pub as_unit: [u64; 4],
    /// `Bool` bits observed on entry. Read-only for the compiled trace.
    pub entry_as_bool: [u64; 4],
    /// `Unit` bits observed on entry. Read-only for the compiled trace.
    pub entry_as_unit: [u64; 4],
    /// `spill_stride * MAX_JIT_CALL_DEPTH` i64 slots, owned by the caller
    /// of the trace for the duration of the call.
    pub spill: *mut i64,
    pub spill_stride: u32,
}

pub type JitEntry = unsafe extern "C" fn(*mut JitFrame);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_words_sit_where_the_compiler_stores_them() {
        assert_eq!(
            std::mem::offset_of!(JitFrame, written),
            OFF_WRITTEN as usize
        );
        assert_eq!(
            std::mem::offset_of!(JitFrame, as_bool),
            OFF_AS_BOOL as usize
        );
        assert_eq!(
            std::mem::offset_of!(JitFrame, as_unit),
            OFF_AS_UNIT as usize
        );
        assert_eq!(
            std::mem::offset_of!(JitFrame, entry_as_bool),
            OFF_ENTRY_BOOL as usize
        );
        assert_eq!(
            std::mem::offset_of!(JitFrame, entry_as_unit),
            OFF_ENTRY_UNIT as usize
        );
        assert_eq!(std::mem::offset_of!(JitFrame, spill), OFF_SPILL as usize);
        assert_eq!(
            std::mem::offset_of!(JitFrame, spill_stride),
            OFF_SPILL_STRIDE as usize
        );
        assert_eq!(
            std::mem::offset_of!(JitCallRecord, written),
            REC_WRITTEN as usize
        );
        assert_eq!(
            std::mem::offset_of!(JitCallRecord, as_bool),
            REC_AS_BOOL as usize
        );
        assert_eq!(
            std::mem::offset_of!(JitCallRecord, as_unit),
            REC_AS_UNIT as usize
        );
    }
}
