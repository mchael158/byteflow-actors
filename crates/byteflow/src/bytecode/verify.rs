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
        Value::List(items) | Value::Record(items) => {
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
    /// An immediate or a register window is not a value the ISA can execute.
    ///
    /// Pack lengths above the value cap, a window that would address register
    /// 255, a `ReceiveMatchKind` outside `0..=10`, a `ReceiveMatchCmp`
    /// selector outside `Eq` / `Lt` / `Le`, a `ReceiveMatch*Imm` tag outside
    /// `u16`, a `SetRestartPolicy` / `StartChild` policy outside `0..=2`,
    /// and a register window of `Call` / `Spawn` / `CallNative` /
    /// `CallNativeFrom` / `StartChild` that reaches past the enclosing
    /// function's register file are properties of the instruction word. The
    /// VM still traps if `verify` was skipped.
    BadImmediate {
        at: usize,
        what: &'static str,
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
            VerifyError::BadImmediate { at, what } => {
                write!(f, "instruction {at} has an invalid {what}")
            }
        }
    }
}

impl std::error::Error for VerifyError {}

/// Verify structural invariants of `chunk`. See [`VerifyError`] for what is
/// checked. This does **not** perform full dataflow/register-liveness
/// verification (unlike, say, the JVM verifier) — v0 trades that off
/// against implementation complexity.
///
/// Register facts that *are* settled statically:
///
/// - [`VerifyError::ArityExceedsRegisters`] — a table property: such a
///   function cannot be entered at all.
/// - The destination / gather windows of `Call`, `Spawn`, `CallNative`,
///   `CallNativeFrom`, and `StartChild` — checked against the enclosing
///   function's `num_registers`. These are the multi-register gathers the
///   hot loop (and the return path) address as a span; a single out-of-range
///   register is still cheap to trap at runtime, but a window that reaches
///   past the frame is the same class of fact as the pack windows below, so
///   it settles here too.
///
/// Single-register operands stay per-step checks in the VM (cheap: an array
/// index against a fixed small register file, not worth statically proving
/// away in full). Sorting is not needed for validity: skipping `verify` on
/// compiler-generated bytecode is fine, but it is mandatory before loading
/// anything that crossed a trust boundary (a plugin, a network-fetched
/// module, see design notes §24 capability/sandboxing).
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
    let limits = register_limits(chunk);

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
                // `Call` gathers args `r[a..a+argc]` and returns into
                // `r[a]`. `Spawn` reserves `r[a]` for the child Cap and
                // gathers `r[a+1..a+1+argc]`.
                if let Some(limit) = limits[at] {
                    check_slot(at, instr.a, limit, "destination register")?;
                    check_window(
                        at,
                        instr.a,
                        if instr.op == Opcode::Spawn { 1 } else { 0 },
                        u32::from(instr.b),
                        limit,
                    )?;
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
                if instr.b > 2 {
                    return Err(VerifyError::BadImmediate {
                        at,
                        what: "restart policy",
                    });
                }
                // The child Cap arrives in `r[a]` — a register, not a hole.
                if let Some(limit) = limits[at] {
                    check_slot(at, instr.a, limit, "destination register")?;
                }
            }
            // `CallNative` targets a `byteflow_vm::NativeTable` supplied at
            // runtime, outside this chunk — see this function's doc
            // comment. Not checked here; checked as `Fault::BadNative` at
            // call time instead.
            Opcode::CallNative | Opcode::CallNativeFrom => {
                // `CallNative` gathers `r[a..a+argc]` and writes the result
                // to `r[a]`. `CallNativeFrom` gathers only `r[b]` and writes
                // `r[a]`.
                if let Some(limit) = limits[at] {
                    if instr.op == Opcode::CallNativeFrom {
                        check_slot(at, instr.a, limit, "destination register")?;
                        check_slot(at, instr.b, limit, "source register")?;
                    } else {
                        check_window(at, instr.a, 0, u32::from(instr.b), limit)?;
                    }
                }
            }
            Opcode::ListPack | Opcode::RecordPack => {
                let max = if instr.op == Opcode::ListPack {
                    super::value::MAX_LIST_LEN
                } else {
                    super::value::MAX_RECORD_LEN
                };
                check_pack(at, instr.b, instr.imm, 1, max)?;
            }
            Opcode::MapPack => {
                check_pack(at, instr.b, instr.imm, 2, super::value::MAX_MAP_LEN)?;
            }
            Opcode::ReceiveMatchKind => {
                if !(0..=10).contains(&instr.imm) {
                    return Err(VerifyError::BadImmediate {
                        at,
                        what: "payload kind",
                    });
                }
            }
            Opcode::ReceiveMatchCmp => {
                if super::opcode::IntCmp::from_imm(instr.imm).is_none() {
                    return Err(VerifyError::BadImmediate {
                        at,
                        what: "compare immediate",
                    });
                }
            }
            Opcode::ReceiveMatchImm | Opcode::ReceiveMatchCorrImm => {
                // The ISA passes message tags as `u16` (see the `Opcode`
                // encoding notes); anything wider truncates at runtime, so
                // it is not a value the ISA can execute.
                if !(0..=u32::from(u16::MAX))
                    .contains(&u32::try_from(instr.imm).unwrap_or(u32::MAX))
                {
                    return Err(VerifyError::BadImmediate {
                        at,
                        what: "tag immediate",
                    });
                }
            }
            Opcode::SetRestartPolicy => {
                if !(0..=2).contains(&instr.imm) {
                    return Err(VerifyError::BadImmediate {
                        at,
                        what: "restart policy",
                    });
                }
            }
            Opcode::AskTimeout => {
                // The timeout lives in a register of the calling frame,
                // packed into `imm` (see the `Opcode` encoding notes).
                if let Some(limit) = limits[at] {
                    match u32::try_from(instr.imm) {
                        Ok(reg) if reg < u32::from(limit) => {}
                        Ok(_) | Err(_) => {
                            return Err(VerifyError::BadImmediate {
                                at,
                                what: "millis register",
                            })
                        }
                    }
                }
            }
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

/// `width` is 1 for a list or record and 2 for a map pair.
///
/// Register 255 is not a slot: `num_registers` is a `u8`, so the highest
/// index a frame can name is 254. A pack whose exclusive end is above 255
/// would gather that index.
fn check_pack(
    at: usize,
    base: u8,
    count: i32,
    width: u32,
    max_len: usize,
) -> Result<(), VerifyError> {
    let max = match i32::try_from(max_len) {
        Ok(n) => n,
        Err(_) => i32::MAX,
    };
    if !(0..=max).contains(&count) {
        return Err(VerifyError::BadImmediate {
            at,
            what: "pack length",
        });
    }
    if count == 0 {
        return Ok(());
    }
    let span = match u32::try_from(count).ok().and_then(|n| n.checked_mul(width)) {
        Some(n) => n,
        None => {
            return Err(VerifyError::BadImmediate {
                at,
                what: "pack window",
            })
        }
    };
    let end = match u32::from(base).checked_add(span) {
        Some(n) => n,
        None => {
            return Err(VerifyError::BadImmediate {
                at,
                what: "pack window",
            })
        }
    };
    if end > 255 {
        return Err(VerifyError::BadImmediate {
            at,
            what: "pack window",
        });
    }
    Ok(())
}

/// The register file that runs each instruction: the `num_registers` of the
/// enclosing function's span, or `None` for instructions outside any span
/// (unreachable from any entry — entries are `< code.len()` by the time this
/// runs, and jumps are confined to their own span).
///
/// Functions sharing an `entry` pool the **smallest** `num_registers` of the
/// group: the span belongs to both bodies, so a window that fits only the
/// larger one is not a fact the interpreter can rely on.
fn register_limits(chunk: &Chunk) -> Vec<Option<u8>> {
    let mut entries: Vec<u32> = chunk.functions.iter().map(|def| def.entry).collect();
    entries.sort_unstable();
    entries.dedup();
    let mut limits = vec![None; chunk.code.len()];
    for (i, &entry) in entries.iter().enumerate() {
        let start = entry as usize;
        let end = match entries.get(i + 1) {
            Some(&next) => next as usize,
            None => chunk.code.len(),
        };
        let limit = chunk
            .functions
            .iter()
            .filter(|def| def.entry == entry)
            .map(|def| def.num_registers)
            .min()
            .unwrap_or_default();
        for slot in limits.iter_mut().take(end).skip(start) {
            *slot = Some(limit);
        }
    }
    limits
}

/// One named register must hold a value: index strictly below the frame's
/// register file (register 255 is never a slot, so the `u8` limit covers it).
fn check_slot(at: usize, reg: u8, limit: u8, what: &'static str) -> Result<(), VerifyError> {
    if u32::from(reg) < u32::from(limit) {
        Ok(())
    } else {
        Err(VerifyError::BadImmediate { at, what })
    }
}

/// A gathered register window `[base + extra, base + extra + count)` must
/// stay inside the caller's register file. An empty window is skipped here:
/// the destination slot (if any) is checked by [`check_slot`] instead.
fn check_window(at: usize, base: u8, extra: u32, count: u32, limit: u8) -> Result<(), VerifyError> {
    if count == 0 {
        return Ok(());
    }
    let end = match u32::from(base)
        .checked_add(extra)
        .and_then(|n| n.checked_add(count))
    {
        Some(n) => n,
        None => {
            return Err(VerifyError::BadImmediate {
                at,
                what: "register window",
            })
        }
    };
    if end > u32::from(limit) {
        return Err(VerifyError::BadImmediate {
            at,
            what: "register window",
        });
    }
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

    #[test]
    fn pack_window_past_the_register_file_is_rejected() {
        let mut b = ChunkBuilder::new("wide");
        b.begin_function("main", 0, 255);
        b.emit_list_pack(0, 250, 10);
        b.emit_return(0);
        let chunk = b.finish();
        assert_eq!(
            verify(&chunk),
            Err(VerifyError::BadImmediate {
                at: 0,
                what: "pack window",
            })
        );
    }

    #[test]
    fn pack_length_above_the_value_cap_is_rejected() {
        let mut b = ChunkBuilder::new("long");
        b.begin_function("main", 0, 1);
        b.emit_record_pack(0, 0, 257);
        b.emit_return(0);
        let chunk = b.finish();
        assert_eq!(
            verify(&chunk),
            Err(VerifyError::BadImmediate {
                at: 0,
                what: "pack length",
            })
        );
    }

    #[test]
    fn receive_kind_outside_the_scalar_range_is_rejected() {
        let mut b = ChunkBuilder::new("kind");
        b.begin_function("main", 0, 1);
        b.emit_receive_match_kind(0, 11);
        b.emit_return(0);
        let chunk = b.finish();
        assert_eq!(
            verify(&chunk),
            Err(VerifyError::BadImmediate {
                at: 0,
                what: "payload kind",
            })
        );
    }

    #[test]
    fn receive_compare_outside_int_cmp_is_rejected() {
        let mut b = ChunkBuilder::new("cmp");
        b.begin_function("main", 0, 2);
        b.emit_receive_match_cmp(0, 0, 1, crate::bytecode::opcode::IntCmp::Eq);
        b.emit_return(0);
        let mut chunk = b.finish();
        chunk.code[0].imm = 9;
        assert_eq!(
            verify(&chunk),
            Err(VerifyError::BadImmediate {
                at: 0,
                what: "compare immediate",
            })
        );
    }

    #[test]
    fn a_short_list_pack_still_verifies() {
        let mut b = ChunkBuilder::new("ok-pack");
        b.begin_function("main", 0, 3);
        b.emit_list_pack(2, 0, 2);
        b.emit_return(2);
        let chunk = b.finish();
        assert!(verify(&chunk).is_ok());
    }

    // --- register windows (Call / Spawn / CallNative / StartChild) ---

    #[test]
    fn call_window_past_the_caller_register_file_is_rejected() {
        let mut b = ChunkBuilder::new("wide-call");
        let callee = b.begin_function("callee", 2, 2);
        b.emit_return(0);
        b.begin_function("main", 0, 2);
        // Args at r1..r3 — the frame ends at r2.
        b.emit_call(1, callee, 2);
        b.emit_return(1);
        let chunk = b.finish();
        assert_eq!(
            verify(&chunk),
            Err(VerifyError::BadImmediate {
                at: 1,
                what: "register window",
            })
        );
    }

    #[test]
    fn call_window_filling_the_register_file_still_verifies() {
        let mut b = ChunkBuilder::new("ok-call");
        let callee = b.begin_function("callee", 2, 2);
        b.emit_return(0);
        b.begin_function("main", 0, 2);
        // Args at r0..r2 with `num_registers == 2`: exactly full, no hole.
        b.emit_call(0, callee, 2);
        b.emit_return(0);
        let chunk = b.finish();
        assert!(verify(&chunk).is_ok());
    }

    #[test]
    fn call_with_no_arguments_still_needs_a_real_destination() {
        let mut b = ChunkBuilder::new("hole-call");
        let callee = b.begin_function("callee", 0, 1);
        b.emit_return(0);
        b.begin_function("main", 0, 1);
        // argc == 0 has no window, but `r[1]` is still not a slot.
        b.emit_call(1, callee, 0);
        b.emit_return(1);
        let chunk = b.finish();
        assert_eq!(
            verify(&chunk),
            Err(VerifyError::BadImmediate {
                at: 1,
                what: "destination register",
            })
        );
    }

    #[test]
    fn spawn_window_with_the_cap_slot_past_the_register_file_is_rejected() {
        let mut b = ChunkBuilder::new("wide-spawn");
        let child = b.begin_function("child", 1, 1);
        b.emit_halt();
        b.begin_function("main", 0, 2);
        // Cap reserved at r1, args gathered from r2 — past the file.
        b.emit_spawn(1, child, 1);
        b.emit_trap(0);
        let chunk = b.finish();
        assert_eq!(
            verify(&chunk),
            Err(VerifyError::BadImmediate {
                at: 1,
                what: "register window",
            })
        );
    }

    #[test]
    fn spawn_with_no_arguments_still_needs_a_real_cap_slot() {
        let mut b = ChunkBuilder::new("hole-spawn");
        let child = b.begin_function("child", 0, 1);
        b.emit_halt();
        b.begin_function("main", 0, 1);
        b.emit_spawn(1, child, 0);
        b.emit_trap(0);
        let chunk = b.finish();
        assert_eq!(
            verify(&chunk),
            Err(VerifyError::BadImmediate {
                at: 1,
                what: "destination register",
            })
        );
    }

    #[test]
    fn native_one_arg_source_must_be_inside_the_frame() {
        let mut b = ChunkBuilder::new("gen-nat1");
        b.begin_function("main", 0, 1);
        // `r[a] = native(r[b])` — r1 is past the 1-register frame.
        b.emit_native1_from(0, 1, 0);
        b.emit_return(0);
        let chunk = b.finish();
        assert_eq!(
            verify(&chunk),
            Err(VerifyError::BadImmediate {
                at: 0,
                what: "source register",
            })
        );
    }

    #[test]
    fn start_child_past_the_register_file_is_rejected() {
        let mut b = ChunkBuilder::new("wide-child");
        let child = b.begin_function("child", 0, 1);
        b.emit_halt();
        b.begin_function("main", 0, 1);
        b.emit_start_child(1, child, 0);
        b.emit_halt();
        let chunk = b.finish();
        assert_eq!(
            verify(&chunk),
            Err(VerifyError::BadImmediate {
                at: 1,
                what: "destination register",
            })
        );
    }

    #[test]
    fn start_child_policy_must_be_zero_to_two() {
        let mut b = ChunkBuilder::new("child-policy");
        let child = b.begin_function("child", 0, 1);
        b.emit_halt();
        b.begin_function("main", 0, 1);
        // The policy rides in `b` (see `emit_start_child`); 3 traps at runtime.
        b.emit_start_child(0, child, 3);
        b.emit_halt();
        let chunk = b.finish();
        assert_eq!(
            verify(&chunk),
            Err(VerifyError::BadImmediate {
                at: 1,
                what: "restart policy",
            })
        );
    }

    // --- immediates: tag u16, restart policy, millis register ---

    #[test]
    fn receive_tag_immediate_must_fit_a_u16() {
        let mut b = ChunkBuilder::new("wide-tag");
        b.begin_function("main", 0, 1);
        b.emit_receive_match_imm(0, 7);
        b.emit_return(0);
        let mut chunk = b.finish();
        chunk.code[0].imm = 70_000;
        assert_eq!(
            verify(&chunk),
            Err(VerifyError::BadImmediate {
                at: 0,
                what: "tag immediate",
            })
        );
    }

    #[test]
    fn receive_tag_immediate_until_u16_max_still_verifies() {
        let mut b = ChunkBuilder::new("edge-tag");
        b.begin_function("main", 0, 1);
        b.emit_receive_match_imm(0, u16::MAX);
        b.emit_return(0);
        let chunk = b.finish();
        assert!(verify(&chunk).is_ok());
    }

    #[test]
    fn restart_policy_immediate_must_be_zero_to_two() {
        let mut b = ChunkBuilder::new("policy-imm");
        b.begin_function("main", 0, 1);
        b.emit_set_restart_policy(3);
        b.emit_return(0);
        let mut chunk = b.finish();
        chunk.code[0].imm = 3;
        assert_eq!(
            verify(&chunk),
            Err(VerifyError::BadImmediate {
                at: 0,
                what: "restart policy",
            })
        );
    }

    #[test]
    fn ask_timeout_millis_register_must_be_inside_the_frame() {
        let mut b = ChunkBuilder::new("ask-far");
        b.begin_function("main", 0, 4);
        b.emit_ask_timeout(0, 1, 2, 3);
        b.emit_return(0);
        let mut chunk = b.finish();
        chunk.code[0].imm = 9; // the timeout register rides in `imm`
        assert_eq!(
            verify(&chunk),
            Err(VerifyError::BadImmediate {
                at: 0,
                what: "millis register",
            })
        );
    }

    #[test]
    fn ask_timeout_with_a_local_millis_register_still_verifies() {
        let mut b = ChunkBuilder::new("ask-ok");
        b.begin_function("main", 0, 4);
        b.emit_load_imm(3, 50);
        b.emit_ask_timeout(0, 1, 2, 3);
        b.emit_return(0);
        let chunk = b.finish();
        assert!(verify(&chunk).is_ok());
    }

    /// Instructions outside any function span are unreachable from any
    /// entry (jumps stay inside their own span), so their windows are not
    /// a fact the hot loop relies on and `verify` leaves them alone.
    #[test]
    fn code_outside_any_function_span_is_not_window_checked() {
        use crate::bytecode::instruction::Instruction;
        let mut chunk = Chunk::default();
        chunk.functions.push(crate::bytecode::chunk::FunctionDef {
            name: "main".into(),
            entry: 1,
            arity: 0,
            num_registers: 1,
        });
        // Unreachable: CallNativeFrom(src r1) past the 1-register frame.
        chunk
            .code
            .push(Instruction::new(Opcode::CallNativeFrom, 0, 1, 0, 0));
        chunk.code.push(Instruction::abc(Opcode::Return, 0, 0, 0));
        assert!(verify(&chunk).is_ok());
    }
}
