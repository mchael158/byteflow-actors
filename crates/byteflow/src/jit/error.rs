use std::fmt;

#[derive(Debug)]
pub enum CompileError {
    Backend(String),
    EmptyTrace { function: u32, pc: u32 },
    UndefinedRegister { reg: u8, pc: u32 },
    UnsupportedOpcode { opcode: crate::Opcode, pc: u32 },
    TraceTooLong { pc: u32, limit: usize },
    Module(String),
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompileError::Backend(msg) => write!(f, "cranelift backend: {msg}"),
            CompileError::EmptyTrace { function, pc } => {
                write!(f, "trace empty at function {function} pc {pc}")
            }
            CompileError::UndefinedRegister { reg, pc } => {
                write!(f, "undefined register {reg} at pc {pc}")
            }
            CompileError::UnsupportedOpcode { opcode, pc } => {
                write!(f, "unsupported opcode {opcode:?} at pc {pc}")
            }
            CompileError::TraceTooLong { pc, limit } => {
                write!(f, "trace longer than {limit} at pc {pc}")
            }
            CompileError::Module(msg) => write!(f, "module error: {msg}"),
        }
    }
}

impl std::error::Error for CompileError {}
