use super::chunk::Chunk;
use super::opcode::Opcode;
use super::value::Value;
use std::fmt;

/// Whether the chunk may contain authority-bearing constants.
///
/// Default is [`TrustLevel::Untrusted`] (fail closed). Host assemblers that
/// intentionally embed `Cap` / `Pid` / `Message` in the pool must pass
/// [`TrustLevel::Trusted`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustLevel {
    Trusted,
    Untrusted,
}

/// Knob for [`verify_with`]. Default trust is untrusted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerifyConfig {
    pub trust: TrustLevel,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        Self {
            trust: TrustLevel::Untrusted,
        }
    }
}

/// Constant-pool tags that untrusted modules must not embed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstantKind {
    Capability,
    ProcessId,
    Message,
}

impl fmt::Display for ConstantKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConstantKind::Capability => f.write_str("capability"),
            ConstantKind::ProcessId => f.write_str("pid"),
            ConstantKind::Message => f.write_str("message"),
        }
    }
}

fn validate_constant(value: &Value, trust: TrustLevel, index: usize) -> Result<(), VerifyError> {
    if trust == TrustLevel::Trusted {
        return Ok(());
    }
    if !value.within_nesting_limit() {
        return Err(VerifyError::ForbiddenConstant {
            index,
            kind: ConstantKind::Capability,
        });
    }
    reject_untrusted_constant(value, index)
}

/// `Cap`, `Pid`, and `Message` are rejected at every depth. A list is not
/// authority by itself, but it must not smuggle those tags past the pool check.
fn reject_untrusted_constant(value: &Value, index: usize) -> Result<(), VerifyError> {
    match value {
        Value::Cap(_) => Err(VerifyError::ForbiddenConstant {
            index,
            kind: ConstantKind::Capability,
        }),
        Value::Pid(_) => Err(VerifyError::ForbiddenConstant {
            index,
            kind: ConstantKind::ProcessId,
        }),
        Value::Message(_) => Err(VerifyError::ForbiddenConstant {
            index,
            kind: ConstantKind::Message,
        }),
        Value::List(items) => {
            for item in items.iter() {
                reject_untrusted_constant(item, index)?;
            }
            Ok(())
        }
        Value::Map(entries) => {
            for (_, value) in entries.iter() {
                reject_untrusted_constant(value, index)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Why a [`Chunk`] failed verification.
///
/// The verifier's job is to make every fact the interpreter's hot loop
/// relies on (jump targets in range, constant/function indices in range)
/// true *before* a single instruction runs, so `byteflow-vm` never has to
/// re-check them per-step. Skipping this on trusted, compiler-generated
/// bytecode is fine; it is mandatory before loading anything that crossed a
/// trust boundary (a plugin, a network-fetched module, see design notes
/// §24 capability/sandboxing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    UnknownOpcode {
        at: usize,
        byte: u8,
    },
    ConstOutOfRange {
        at: usize,
        index: u32,
        len: usize,
    },
    FunctionOutOfRange {
        at: usize,
        index: u32,
        len: usize,
    },
    JumpOutOfRange {
        at: usize,
        target: i64,
        len: usize,
    },
    EmptyFunctionTable,
    EntryOutOfRange {
        function: usize,
        entry: u32,
        len: usize,
    },
    /// A function declares more parameters than it has registers to hold
    /// them. The VM loads `r0..arity` on entry, so this makes the very first
    /// thing a call does — copying arguments in — reach past the register
    /// file. Cheap to settle here: it is a static property of the function
    /// table, one comparison per function, and no amount of runtime checking
    /// makes such a function callable.
    ArityExceedsRegisters {
        function: usize,
        arity: u8,
        num_registers: u8,
    },
    /// `Call` or `Spawn` passes a different argument count than the callee
    /// declares. `StartChild` is the same fact with `argc == 0`: the opcode
    /// has no argument window, so a non-zero arity would enter as `Unit`.
    CallArity {
        at: usize,
        function: u32,
        argc: u8,
        arity: u8,
    },
    /// Untrusted chunk embedded a Cap, Pid, or Message in the constant pool.
    ForbiddenConstant {
        index: usize,
        kind: ConstantKind,
    },
    /// The instruction before the next function's `entry` can continue, so
    /// the VM would run that body on this function's register window.
    /// `Jump`, `Return`, `Halt`, `Exit`, and `Trap` close the span.
    /// `Branch` and `Call` do not: both can execute the following instruction.
    FallThrough {
        function: usize,
        at: usize,
        next_entry: usize,
    },
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VerifyError::UnknownOpcode { at, byte } => {
                write!(f, "unknown opcode 0x{byte:02X} at instruction {at}")
            }
            VerifyError::ConstOutOfRange { at, index, len } => write!(
                f,
                "instruction {at} references constant {index}, pool has {len} entries"
            ),
            VerifyError::FunctionOutOfRange { at, index, len } => write!(
                f,
                "instruction {at} references function {index}, table has {len} entries"
            ),
            VerifyError::JumpOutOfRange { at, target, len } => write!(
                f,
                "instruction {at} jumps to {target}, out of code bounds (len={len})"
            ),
            VerifyError::EmptyFunctionTable => write!(f, "chunk has no entry function"),
            VerifyError::EntryOutOfRange {
                function,
                entry,
                len,
            } => write!(
                f,
                "function {function} entry point {entry} is out of code bounds (len={len})"
            ),
            VerifyError::ArityExceedsRegisters {
                function,
                arity,
                num_registers,
            } => write!(
                f,
                "function {function} declares arity {arity} but only {num_registers} registers"
            ),
            VerifyError::CallArity {
                at,
                function,
                argc,
                arity,
            } => write!(
                f,
                "instruction {at} invokes function {function} with {argc} arguments, arity is {arity}"
            ),
            VerifyError::ForbiddenConstant { index, kind } => {
                write!(f, "untrusted constant[{index}] must not embed {kind}")
            }
            VerifyError::FallThrough {
                function,
                at,
                next_entry,
            } => write!(
                f,
                "function {function} falls through at instruction {at} into the function at {next_entry}"
            ),
        }
    }
}

impl std::error::Error for VerifyError {}

/// Verify structural invariants of `chunk`. See [`VerifyError`] for what is
/// checked. This does **not** perform full dataflow/register-liveness
/// verification (unlike, say, the JVM verifier) — v0 trades that off
/// against implementation complexity, and instead the VM bounds-checks
/// register indices at runtime (cheap: it's an array index against a fixed
/// small register file, not worth statically proving away yet).
///
/// The one register fact that *is* settled statically is
/// [`VerifyError::ArityExceedsRegisters`]. It belongs here rather than in
/// the VM because it is a property of the function table, not of an
/// execution: such a function cannot be entered at all, so letting it reach
/// the interpreter only moves the same rejection later and per call.
/// [`VerifyError::FallThrough`] is the same kind of fact for control flow:
/// a span that reaches the next `entry` runs that body on the wrong frame.
///
/// `Opcode::CallNative` targets are deliberately **not** range-checked
/// here: native functions live in a `byteflow_vm::NativeTable` supplied by
/// the embedder at `Vm` construction time, entirely outside this crate's
/// (and this `Chunk`'s) knowledge. An out-of-range `CallNative` is instead
/// caught at runtime as `Fault::BadNative`.
pub fn verify(chunk: &Chunk) -> Result<(), VerifyError> {
    verify_with(chunk, VerifyConfig::default())
}

/// Like [`verify`], with an explicit [`VerifyConfig`].
pub fn verify_with(chunk: &Chunk, config: VerifyConfig) -> Result<(), VerifyError> {
    if chunk.functions.is_empty() {
        return Err(VerifyError::EmptyFunctionTable);
    }

    for (index, value) in chunk.constants.iter().enumerate() {
        validate_constant(value, config.trust, index)?;
    }

    let len = chunk.code.len();

    // `enumerate` rather than looking the index back up by name: two
    // functions may share a name, and a search would then report the wrong
    // one (and cost O(n²) doing it).
    for (function, def) in chunk.functions.iter().enumerate() {
        if def.entry as usize >= len {
            return Err(VerifyError::EntryOutOfRange {
                function,
                entry: def.entry,
                len,
            });
        }
        if def.arity > def.num_registers {
            return Err(VerifyError::ArityExceedsRegisters {
                function,
                arity: def.arity,
                num_registers: def.num_registers,
            });
        }
    }

    for (at, instr) in chunk.code.iter().enumerate() {
        match instr.op {
            Opcode::LoadConst => {
                let idx = instr.imm as u32;
                if idx as usize >= chunk.constants.len() {
                    return Err(VerifyError::ConstOutOfRange {
                        at,
                        index: idx,
                        len: chunk.constants.len(),
                    });
                }
            }
            Opcode::Call | Opcode::Spawn => {
                let idx = instr.imm as u32;
                if idx as usize >= chunk.functions.len() {
                    return Err(VerifyError::FunctionOutOfRange {
                        at,
                        index: idx,
                        len: chunk.functions.len(),
                    });
                }
                let arity = chunk.functions[idx as usize].arity;
                if instr.b != arity {
                    return Err(VerifyError::CallArity {
                        at,
                        function: idx,
                        argc: instr.b,
                        arity,
                    });
                }
            }
            Opcode::StartChild => {
                let idx = instr.imm as u32;
                if idx as usize >= chunk.functions.len() {
                    return Err(VerifyError::FunctionOutOfRange {
                        at,
                        index: idx,
                        len: chunk.functions.len(),
                    });
                }
                // The opcode carries a restart policy, not an argument list.
                // The supervisor starts the child with an empty window.
                let arity = chunk.functions[idx as usize].arity;
                if arity != 0 {
                    return Err(VerifyError::CallArity {
                        at,
                        function: idx,
                        argc: 0,
                        arity,
                    });
                }
            }
            // `CallNative` targets a `byteflow_vm::NativeTable` supplied at
            // runtime, outside this chunk — see this function's doc
            // comment. Not checked here; checked as `Fault::BadNative` at
            // call time instead.
            Opcode::CallNative | Opcode::CallNativeFrom => {}
            Opcode::Jump | Opcode::Branch => {
                let target = at as i64 + 1 + instr.imm as i64;
                if target < 0 || target as usize > len {
                    // == len is allowed: jumping to "one past the end" is a
                    // valid way to fall off the end of the chunk.
                    return Err(VerifyError::JumpOutOfRange { at, target, len });
                }
                // A jump may stay inside the function that contains `at`, or
                // land exactly at `code.len()` (implicit return). Landing on
                // another function's body would run that code with this frame.
                if let Some((start, end)) = function_span(&chunk.functions, at, len) {
                    let t = target as usize;
                    if t != len && (t < start || t >= end) {
                        return Err(VerifyError::JumpOutOfRange { at, target, len });
                    }
                }
            }
            _ => {}
        }
    }

    reject_fallthrough(chunk)?;
    Ok(())
}

/// `Jump`, `Return`, `Halt`, `Exit`, and `Trap` never execute the next
/// instruction. Every other opcode can: `Branch` falls through when the
/// condition is false, and `Call` / `Yield` / `Send` resume at `pc + 1`.
fn is_terminator(op: Opcode) -> bool {
    matches!(
        op,
        Opcode::Jump | Opcode::Return | Opcode::Halt | Opcode::Exit | Opcode::Trap
    )
}

/// Smallest function `entry` strictly after `start`, or `code_len` when
/// this span runs to the end of the chunk.
fn exclusive_end(functions: &[super::chunk::FunctionDef], start: usize, code_len: usize) -> usize {
    let mut end = code_len;
    for def in functions {
        let entry = def.entry as usize;
        if entry > start && entry < end {
            end = entry;
        }
    }
    end
}

/// Each function whose span ends at another `entry` must close with a
/// terminator. Falling off `code.len()` is the implicit return and stays
/// legal. A span that does not extend past `start` has no instruction to
/// check (two functions sharing an `entry`).
fn reject_fallthrough(chunk: &Chunk) -> Result<(), VerifyError> {
    let len = chunk.code.len();
    for (function, def) in chunk.functions.iter().enumerate() {
        let start = def.entry as usize;
        let end = exclusive_end(&chunk.functions, start, len);
        if end >= len || end <= start {
            continue;
        }
        let at = end - 1;
        if !is_terminator(chunk.code[at].op) {
            return Err(VerifyError::FallThrough {
                function,
                at,
                next_entry: end,
            });
        }
    }
    Ok(())
}

/// Instruction range owned by the function whose `entry` is the greatest
/// entry `<= at`. The end is the next function entry, or `code_len`.
fn function_span(
    functions: &[super::chunk::FunctionDef],
    at: usize,
    code_len: usize,
) -> Option<(usize, usize)> {
    let mut start = None;
    for def in functions {
        let entry = def.entry as usize;
        if entry <= at {
            start = Some(match start {
                Some(prev) if prev >= entry => prev,
                _ => entry,
            });
        }
    }
    let start = start?;
    let end = exclusive_end(functions, start, code_len);
    if at >= end {
        None
    } else {
        Some((start, end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::builder::ChunkBuilder;
    use crate::bytecode::opcode::Opcode;

    #[test]
    fn rejects_empty_function_table() {
        let chunk = Chunk::default();
        assert_eq!(verify(&chunk), Err(VerifyError::EmptyFunctionTable));
    }

    #[test]
    fn accepts_well_formed_chunk() {
        let mut b = ChunkBuilder::new("test");
        b.begin_function("main", 0, 2);
        let k = b.const_(crate::bytecode::value::Value::Int(41));
        b.emit_load_const(0, k);
        b.emit_load_imm(1, 1);
        b.emit_binop(Opcode::Add, 0, 0, 1);
        b.emit_return(0);
        let chunk = b.finish();
        assert!(verify(&chunk).is_ok());
    }

    /// A function the VM cannot even enter: entry copies `r0..arity`, but
    /// the frame only has `num_registers` slots. Used to be accepted here and
    /// then panic with an out-of-bounds index inside `Vm::new` / `Call`.
    #[test]
    fn rejects_arity_larger_than_the_register_file() {
        let mut b = ChunkBuilder::new("test");
        b.begin_function("main", 3, 1);
        b.emit_return(0);
        let chunk = b.finish();
        assert_eq!(
            verify(&chunk),
            Err(VerifyError::ArityExceedsRegisters {
                function: 0,
                arity: 3,
                num_registers: 1,
            })
        );
    }

    #[test]
    fn accepts_arity_equal_to_the_register_file() {
        let mut b = ChunkBuilder::new("test");
        b.begin_function("main", 2, 2);
        b.emit_return(0);
        let chunk = b.finish();
        assert!(verify(&chunk).is_ok());
    }

    #[test]
    fn rejects_start_child_function_out_of_range() {
        use crate::bytecode::instruction::Instruction;
        let mut chunk = Chunk::default();
        chunk.functions.push(crate::bytecode::chunk::FunctionDef {
            name: "main".into(),
            entry: 0,
            arity: 0,
            num_registers: 1,
        });
        chunk
            .code
            .push(Instruction::new(Opcode::StartChild, 0, 0, 0, 99));
        assert!(matches!(
            verify(&chunk),
            Err(VerifyError::FunctionOutOfRange { index: 99, .. })
        ));
    }

    #[test]
    fn rejects_jump_into_another_function() {
        use crate::bytecode::instruction::Instruction;
        let mut chunk = Chunk::default();
        chunk.functions.push(crate::bytecode::chunk::FunctionDef {
            name: "main".into(),
            entry: 0,
            arity: 0,
            num_registers: 1,
        });
        chunk.functions.push(crate::bytecode::chunk::FunctionDef {
            name: "other".into(),
            entry: 2,
            arity: 0,
            num_registers: 1,
        });
        // at 0, imm 1 → target 2, the other function's entry.
        chunk.code.push(Instruction::only_imm(Opcode::Jump, 1));
        chunk.code.push(Instruction::nullary(Opcode::Nop));
        chunk.code.push(Instruction::nullary(Opcode::Nop));
        assert!(matches!(
            verify(&chunk),
            Err(VerifyError::JumpOutOfRange { target: 2, .. })
        ));
    }

    #[test]
    fn rejects_out_of_range_jump() {
        use crate::bytecode::instruction::Instruction;
        let mut chunk = Chunk::default();
        chunk.functions.push(crate::bytecode::chunk::FunctionDef {
            name: "main".into(),
            entry: 0,
            arity: 0,
            num_registers: 1,
        });
        chunk.code.push(Instruction::only_imm(Opcode::Jump, 999));
        assert!(matches!(
            verify(&chunk),
            Err(VerifyError::JumpOutOfRange { .. })
        ));
    }

    fn two_functions(first: crate::bytecode::instruction::Instruction) -> Chunk {
        use crate::bytecode::instruction::Instruction;
        let mut chunk = Chunk::default();
        for (name, entry) in [("main", 0u32), ("other", 1)] {
            chunk.functions.push(crate::bytecode::chunk::FunctionDef {
                name: name.into(),
                entry,
                arity: 0,
                num_registers: 1,
            });
        }
        chunk.code.push(first);
        chunk.code.push(Instruction::abc(Opcode::Return, 0, 0, 0));
        chunk
    }

    #[test]
    fn rejects_fallthrough_into_the_next_function() {
        use crate::bytecode::instruction::Instruction;
        let boundary = VerifyError::FallThrough {
            function: 0,
            at: 0,
            next_entry: 1,
        };
        for first in [
            Instruction::nullary(Opcode::Nop),
            Instruction::a_imm(Opcode::Branch, 0, -1),
            Instruction::new(Opcode::Call, 0, 0, 0, 1),
        ] {
            assert_eq!(verify(&two_functions(first)), Err(boundary.clone()));
        }
    }

    #[test]
    fn accepts_jump_closing_a_function_before_the_next() {
        use crate::bytecode::instruction::Instruction;
        // target = 0 + 1 + (-1) = 0, still inside `main`.
        let chunk = two_functions(Instruction::only_imm(Opcode::Jump, -1));
        assert!(verify(&chunk).is_ok());
    }

    #[test]
    fn rejects_call_whose_argc_differs_from_the_callee() {
        use crate::bytecode::instruction::Instruction;
        let mut chunk = Chunk::default();
        chunk.functions.push(crate::bytecode::chunk::FunctionDef {
            name: "main".into(),
            entry: 0,
            arity: 0,
            num_registers: 1,
        });
        chunk.code.push(Instruction::new(Opcode::Call, 0, 1, 0, 0));
        chunk.code.push(Instruction::abc(Opcode::Return, 0, 0, 0));
        assert_eq!(
            verify(&chunk),
            Err(VerifyError::CallArity {
                at: 0,
                function: 0,
                argc: 1,
                arity: 0,
            })
        );
    }

    #[test]
    fn rejects_spawn_whose_argc_differs_from_the_callee() {
        use crate::bytecode::instruction::Instruction;
        let mut chunk = Chunk::default();
        chunk.functions.push(crate::bytecode::chunk::FunctionDef {
            name: "main".into(),
            entry: 0,
            arity: 2,
            num_registers: 2,
        });
        chunk.code.push(Instruction::new(Opcode::Spawn, 0, 0, 0, 0));
        chunk.code.push(Instruction::abc(Opcode::Halt, 0, 0, 0));
        assert_eq!(
            verify(&chunk),
            Err(VerifyError::CallArity {
                at: 0,
                function: 0,
                argc: 0,
                arity: 2,
            })
        );
    }

    #[test]
    fn rejects_start_child_of_a_function_that_expects_arguments() {
        use crate::bytecode::instruction::Instruction;
        let mut chunk = Chunk::default();
        chunk.functions.push(crate::bytecode::chunk::FunctionDef {
            name: "main".into(),
            entry: 0,
            arity: 1,
            num_registers: 1,
        });
        chunk
            .code
            .push(Instruction::new(Opcode::StartChild, 0, 0, 0, 0));
        chunk.code.push(Instruction::abc(Opcode::Halt, 0, 0, 0));
        assert_eq!(
            verify(&chunk),
            Err(VerifyError::CallArity {
                at: 0,
                function: 0,
                argc: 0,
                arity: 1,
            })
        );
    }

    #[test]
    fn accepts_implicit_return_on_the_last_function() {
        use crate::bytecode::instruction::Instruction;
        let mut chunk = Chunk::default();
        chunk.functions.push(crate::bytecode::chunk::FunctionDef {
            name: "main".into(),
            entry: 0,
            arity: 0,
            num_registers: 1,
        });
        chunk.code.push(Instruction::nullary(Opcode::Nop));
        assert!(verify(&chunk).is_ok());
    }

    #[test]
    fn untrusted_rejects_cap_pid_message_constants() {
        use crate::bytecode::cap::CapId;
        use crate::bytecode::value::Message;
        for (value, kind) in [
            (
                crate::bytecode::value::Value::Cap(CapId::from_raw(1)),
                ConstantKind::Capability,
            ),
            (
                crate::bytecode::value::Value::Pid(7),
                ConstantKind::ProcessId,
            ),
            (
                crate::bytecode::value::Value::Message(Message::new(1, 2, 3, 4u64)),
                ConstantKind::Message,
            ),
        ] {
            let mut b = ChunkBuilder::new("forge");
            b.begin_function("main", 0, 1);
            let k = b.const_(value);
            b.emit_load_const(0, k);
            b.emit_return(0);
            let chunk = b.finish();
            assert_eq!(
                verify(&chunk),
                Err(VerifyError::ForbiddenConstant { index: 0, kind })
            );
            assert!(verify_with(
                &chunk,
                VerifyConfig {
                    trust: TrustLevel::Trusted
                }
            )
            .is_ok());
        }
    }

    #[test]
    fn untrusted_rejects_authority_nested_in_a_list() {
        use crate::bytecode::cap::CapId;
        use crate::bytecode::value::{Message, Value};
        let cases = [
            (
                Value::try_list(vec![Value::Int(1), Value::Cap(CapId::from_raw(1))]),
                ConstantKind::Capability,
            ),
            (
                Value::try_list(vec![Value::Pid(7)]),
                ConstantKind::ProcessId,
            ),
            (
                Value::try_list(vec![Value::Message(Message::new(1, 2, 3, 4u64))]),
                ConstantKind::Message,
            ),
        ];
        for (value, kind) in cases {
            assert!(value.is_some());
            let Some(value) = value else {
                return;
            };
            let mut b = ChunkBuilder::new("nested-forge");
            b.begin_function("main", 0, 1);
            let k = b.const_(value);
            b.emit_load_const(0, k);
            b.emit_return(0);
            let chunk = b.finish();
            assert_eq!(
                verify(&chunk),
                Err(VerifyError::ForbiddenConstant { index: 0, kind })
            );
            assert!(verify_with(
                &chunk,
                VerifyConfig {
                    trust: TrustLevel::Trusted
                }
            )
            .is_ok());
        }
    }
}
