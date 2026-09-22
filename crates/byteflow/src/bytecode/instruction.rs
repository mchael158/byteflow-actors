use std::fmt;

use super::opcode::Opcode;

/// A single packed instruction word.
///
/// Layout is 8 bytes (`op` + `a`/`b`/`c` + signed `imm`) so a `Vec<Instruction>`
/// stays dense and the interpreter can fetch with a single aligned load.
/// Operand meaning is opcode-specific; see [`Opcode`] and
/// [`Program`](crate::Program) / [`Fn`](crate::Fn).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Instruction {
    pub op: Opcode,
    pub a: u8,
    pub b: u8,
    pub c: u8,
    pub imm: i32,
}

impl Instruction {
    pub fn new(op: Opcode, a: u8, b: u8, c: u8, imm: i32) -> Self {
        Instruction { op, a, b, c, imm }
    }

    pub fn nullary(op: Opcode) -> Self {
        Instruction::new(op, 0, 0, 0, 0)
    }

    pub fn abc(op: Opcode, a: u8, b: u8, c: u8) -> Self {
        Instruction::new(op, a, b, c, 0)
    }

    pub fn a_imm(op: Opcode, a: u8, imm: i32) -> Self {
        Instruction::new(op, a, 0, 0, imm)
    }

    pub fn only_imm(op: Opcode, imm: i32) -> Self {
        Instruction::new(op, 0, 0, 0, imm)
    }
}

impl fmt::Display for Instruction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.op {
            Opcode::Halt | Opcode::Yield | Opcode::Nop => write!(f, "{}", self.op),
            Opcode::LoadConst => write!(f, "{} r{}, const[{}]", self.op, self.a, self.imm),
            Opcode::Move => write!(f, "{} r{}, r{}", self.op, self.a, self.b),
            Opcode::LoadImm => write!(f, "{} r{}, {}", self.op, self.a, self.imm),
            Opcode::Add
            | Opcode::Sub
            | Opcode::Mul
            | Opcode::Div
            | Opcode::Mod
            | Opcode::Eq
            | Opcode::Lt
            | Opcode::Le => {
                write!(f, "{} r{}, r{}, r{}", self.op, self.a, self.b, self.c)
            }
            Opcode::Neg => write!(f, "{} r{}, r{}", self.op, self.a, self.b),
            Opcode::Jump => write!(f, "{} {:+}", self.op, self.imm),
            Opcode::Branch => write!(f, "{} r{}, {:+}", self.op, self.a, self.imm),
            Opcode::Call | Opcode::CallNative => {
                write!(
                    f,
                    "{} r{}, fn[{}], argc={}",
                    self.op, self.a, self.imm, self.b
                )
            }
            Opcode::Return
            | Opcode::Exit
            | Opcode::Sleep
            | Opcode::Receive
            | Opcode::SelfPid
            | Opcode::FreshRequestId => {
                write!(f, "{} r{}", self.op, self.a)
            }
            Opcode::Spawn => write!(
                f,
                "{} r{}, fn[{}], argc={}, rights={:#x}",
                self.op, self.a, self.imm, self.b, self.c
            ),
            Opcode::Send => write!(f, "{} r{}, r{}", self.op, self.a, self.b),
            Opcode::ReceiveTimeout | Opcode::ReceiveMatch => {
                write!(f, "{} r{}, r{}", self.op, self.a, self.b)
            }
            Opcode::ReceiveMatchImm => write!(f, "{} r{}, tag={}", self.op, self.a, self.imm),
            Opcode::ReceiveMatchKind => {
                write!(f, "{} r{}, kind={}", self.op, self.a, self.imm)
            }
            Opcode::ReceiveMatchCorr => write!(
                f,
                "{} r{}, tag=r{}, id=r{}",
                self.op, self.a, self.b, self.c
            ),
            Opcode::ReceiveMatchCorrImm => write!(
                f,
                "{} r{}, tag={}, id=r{}",
                self.op, self.a, self.imm, self.b
            ),
            Opcode::Ask => write!(f, "{} r{}, r{}, r{}", self.op, self.a, self.b, self.c),
            Opcode::AskTimeout => write!(
                f,
                "{} r{}, r{}, r{}, timeout=r{}",
                self.op, self.a, self.b, self.c, self.imm
            ),
            Opcode::Monitor | Opcode::Link => {
                write!(f, "{} r{}, r{}", self.op, self.a, self.b)
            }
            Opcode::Demonitor | Opcode::Unlink | Opcode::RegisterName | Opcode::SetTrapExit => {
                write!(f, "{} r{}", self.op, self.a)
            }
            Opcode::Whereis => write!(f, "{} r{}, r{}", self.op, self.a, self.b),
            Opcode::Delegate => write!(
                f,
                "{} r{}, r{}, rights={:#x}, native=r{}",
                self.op, self.a, self.b, self.imm as u32, self.c
            ),
            Opcode::SetRestartPolicy => write!(f, "{} {}", self.op, self.imm),
            Opcode::Trap => write!(f, "{} {}", self.op, self.imm),
        }
    }
}
