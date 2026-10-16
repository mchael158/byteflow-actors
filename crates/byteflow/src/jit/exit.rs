//! Native exit codes returned by compiled traces.

/// Why a compiled trace stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// Keep executing natively at `pc`.
    Continue { pc: u32 },
    /// Delegate to the interpreter / scheduler at `pc` (Send, Ask, …).
    Effect { pc: u32 },
    /// Outermost `Return` — value lives in `return_reg`.
    Return { return_reg: u8 },
    /// Instruction quantum exhausted mid-trace.
    Budget { pc: u32 },
    /// Semantic fault (divide-by-zero, explicit trap, …).
    Trap { pc: u32 },
    /// Type guard failed — resume in the interpreter.
    Deopt { pc: u32 },
}

/// Wire format returned across the native boundary.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitReturn {
    pub kind: u32,
    pub pc: u32,
    pub return_reg: u32,
    /// Copied out of [`super::frame::JitFrame::written`] after the trace returns.
    pub written: [u64; 4],
    /// Copied out of [`super::frame::JitFrame::as_bool`].
    pub as_bool: [u64; 4],
    /// Copied out of [`super::frame::JitFrame::as_unit`].
    pub as_unit: [u64; 4],
}

pub const JIT_CONTINUE: u32 = 0;
pub const JIT_EFFECT: u32 = 1;
pub const JIT_RETURN: u32 = 2;
pub const JIT_BUDGET: u32 = 3;
pub const JIT_TRAP: u32 = 4;
pub const JIT_DEOPT: u32 = 5;

impl JitReturn {
    pub fn from_reason(reason: ExitReason) -> Self {
        let mut ret = Self {
            kind: 0,
            pc: 0,
            return_reg: 0,
            written: [0; 4],
            as_bool: [0; 4],
            as_unit: [0; 4],
        };
        match reason {
            ExitReason::Continue { pc } => {
                ret.kind = JIT_CONTINUE;
                ret.pc = pc;
            }
            ExitReason::Effect { pc } => {
                ret.kind = JIT_EFFECT;
                ret.pc = pc;
            }
            ExitReason::Return { return_reg } => {
                ret.kind = JIT_RETURN;
                ret.return_reg = u32::from(return_reg);
            }
            ExitReason::Budget { pc } => {
                ret.kind = JIT_BUDGET;
                ret.pc = pc;
            }
            ExitReason::Trap { pc } => {
                ret.kind = JIT_TRAP;
                ret.pc = pc;
            }
            ExitReason::Deopt { pc } => {
                ret.kind = JIT_DEOPT;
                ret.pc = pc;
            }
        }
        ret
    }

    pub fn into_reason(self) -> ExitReason {
        match self.kind {
            JIT_CONTINUE => ExitReason::Continue { pc: self.pc },
            JIT_EFFECT => ExitReason::Effect { pc: self.pc },
            JIT_RETURN => ExitReason::Return {
                return_reg: self.return_reg as u8,
            },
            JIT_BUDGET => ExitReason::Budget { pc: self.pc },
            JIT_TRAP => ExitReason::Trap { pc: self.pc },
            JIT_DEOPT => ExitReason::Deopt { pc: self.pc },
            _ => ExitReason::Deopt { pc: self.pc },
        }
    }
}
