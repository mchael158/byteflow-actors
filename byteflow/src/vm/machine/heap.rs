//! Heap charge for a register value and for one dictionary slot.
//!
//! The interpreter calls these before it stores. A refused charge does not
//! change the register or the dictionary.

use crate::bytecode::Value;
use crate::scheduler::FlowQuota;

use crate::vm::fault::Fault;
use crate::vm::frame::Frame;

/// Bytes a dictionary slot owns. Scalars are free. `Str` / `Bytes` charge
/// their length even when a register still holds the same `Arc`: the
/// dictionary keeps the buffer after that register is overwritten.
pub(super) fn dict_owned_bytes(value: &Value) -> Result<usize, Fault> {
    match value {
        Value::Unit | Value::Bool(_) | Value::Int(_) | Value::Float(_) => Ok(0),
        Value::Str(s) => Ok(s.len()),
        Value::Bytes(b) => Ok(b.len()),
        other => Err(Fault::TypeMismatch {
            expected: "dict value (scalar, str, or bytes)",
            got: other.type_name(),
        }),
    }
}

/// Copy call arguments into a frame that is not on the quota yet.
/// [`Vm::new`](super::Vm::new) charges them later, in one reservation.
#[inline]
pub(super) fn load_frame_args(
    frame: &mut Frame,
    args: impl IntoIterator<Item = Value>,
    arity: u8,
) -> Result<(), Fault> {
    let frame_size = frame.registers().len() as u8;
    for (i, a) in args.into_iter().enumerate().take(arity as usize) {
        match frame.registers_mut().get_mut(i) {
            Some(slot) => *slot = a,
            None => {
                return Err(Fault::RegisterOutOfRange {
                    reg: i as u8,
                    frame_size,
                })
            }
        }
    }
    Ok(())
}

/// The register index `base + offset`, or [`Fault::RegisterIndexOverflow`]
/// if that sum leaves the register index space.
///
/// Every multi-operand opcode gathers its arguments from consecutive
/// registers. Written as a plain `base + offset` on `u8`, that addition
/// panics in debug and **wraps** in release — so a release build silently
/// reads the wrong register instead of failing, which is the worst of the
/// two outcomes and the one a debug-mode test suite never sees. The sum is
/// therefore computed in a wider type and narrowed explicitly.
#[inline]
pub(super) fn reg_at(base: u8, offset: u16) -> Result<u8, Fault> {
    match u8::try_from(u32::from(base) + u32::from(offset)) {
        Ok(reg) => Ok(reg),
        Err(_) => Err(Fault::RegisterIndexOverflow {
            base,
            offset: match u8::try_from(offset) {
                Ok(o) => o,
                Err(_) => u8::MAX,
            },
        }),
    }
}

/// Bytes charged for a register-resident heap value.
///
/// `Str` / `Bytes` charge their length. A [`Value::Message`] charges the
/// payload, down to the same ceiling as the `.bf` decoder. Stopping at that
/// ceiling and still storing the value would leave the tail unpaid.
#[inline]
pub(super) fn heap_charge_of(value: &Value) -> Result<usize, Fault> {
    fn walk(value: &Value, depth: u32) -> Result<usize, Fault> {
        if depth > crate::bytecode::MAX_VALUE_DEPTH {
            return Err(Fault::QuotaExceeded("value nesting exceeds limit".into()));
        }
        match value {
            Value::Str(s) => Ok(s.len()),
            Value::Bytes(b) => Ok(b.len()),
            Value::Message(message) => walk(message.payload.as_ref(), depth + 1),
            Value::List(items) | Value::Record(items) => {
                let mut sum = items
                    .len()
                    .checked_mul(std::mem::size_of::<Value>())
                    .ok_or_else(|| Fault::QuotaExceeded("heap charge does not fit".into()))?;
                for item in items.iter() {
                    let part = walk(item, depth + 1)?;
                    sum = match sum.checked_add(part) {
                        Some(total) => total,
                        None => {
                            return Err(Fault::QuotaExceeded("heap charge does not fit".into()));
                        }
                    };
                }
                Ok(sum)
            }
            Value::Map(entries) => {
                let mut sum = entries
                    .len()
                    .checked_mul(std::mem::size_of::<Value>())
                    .ok_or_else(|| Fault::QuotaExceeded("heap charge does not fit".into()))?;
                for (key, value) in entries.iter() {
                    if let crate::bytecode::MapKey::Str(text) = key {
                        sum = match sum.checked_add(text.len()) {
                            Some(total) => total,
                            None => {
                                return Err(Fault::QuotaExceeded("heap charge does not fit".into()))
                            }
                        };
                    }
                    let part = walk(value, depth + 1)?;
                    sum = match sum.checked_add(part) {
                        Some(total) => total,
                        None => {
                            return Err(Fault::QuotaExceeded("heap charge does not fit".into()));
                        }
                    };
                }
                Ok(sum)
            }
            Value::Unit
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Pid(_)
            | Value::Cap(_) => Ok(0),
        }
    }
    walk(value, 0)
}

/// Bytes to reserve on the sender quota while `value` is in flight.
///
/// The hop is still in a register, and that register already pays for the
/// `Str` / `Bytes` inside it. Charging [`Value::memory_size`] on top of that
/// counts the same buffers twice and rejects a send the quota can hold.
#[inline]
pub(crate) fn in_flight_hop_charge(value: &Value) -> usize {
    match heap_charge_of(value) {
        Ok(held) => value.memory_size().saturating_sub(held),
        Err(_) => value.memory_size(),
    }
}

/// Reserve `bytes` on the flow quota and optional runtime budget.
/// Rolls back the flow charge if the global budget refuses.
pub(super) fn charge_pair(
    quota: &FlowQuota,
    memory: Option<&crate::MemoryBudget>,
    bytes: usize,
) -> Result<(), Fault> {
    if bytes == 0 {
        return Ok(());
    }
    quota
        .alloc(bytes)
        .map_err(|e| Fault::QuotaExceeded(e.to_string()))?;
    if let Some(mem) = memory {
        if let Err(e) = mem.try_charge(bytes) {
            quota.free(bytes);
            return Err(Fault::QuotaExceeded(e.to_string()));
        }
    }
    Ok(())
}

pub(super) fn release_pair(quota: &FlowQuota, memory: Option<&crate::MemoryBudget>, bytes: usize) {
    if bytes == 0 {
        return;
    }
    quota.free(bytes);
    if let Some(mem) = memory {
        mem.release(bytes);
    }
}

pub(super) fn as_f64(v: &Value) -> Result<f64, Fault> {
    match v {
        Value::Int(i) => Ok(*i as f64),
        Value::Float(f) => Ok(*f),
        other => Err(Fault::TypeMismatch {
            expected: "int or float",
            got: other.type_name(),
        }),
    }
}

/// Decode a Message tag from a register value (`Int` in `0..=u16::MAX`).
#[inline]
pub(super) fn tag_from_imm(imm: i32) -> Result<u16, Fault> {
    match u16::try_from(imm) {
        Ok(t) if imm >= 0 => Ok(t),
        _ => Err(Fault::TypeMismatch {
            expected: "tag u16",
            got: "imm-out-of-range",
        }),
    }
}

/// `Int` only. [`Value::as_int`] still coerces `Bool` for natives.
pub(super) fn require_int(v: &Value) -> Result<i64, Fault> {
    match v {
        Value::Int(n) => Ok(*n),
        other => Err(Fault::TypeMismatch {
            expected: "int",
            got: other.type_name(),
        }),
    }
}

pub(super) fn tag_from_value(v: &Value) -> Result<u16, Fault> {
    match require_int(v) {
        Ok(i) if (0..=i64::from(u16::MAX)).contains(&i) => Ok(i as u16),
        Ok(_) => Err(Fault::TypeMismatch {
            expected: "tag u16",
            got: "int-out-of-range",
        }),
        Err(fault) => Err(fault),
    }
}

/// Decode a `request_id` from a register (`Int` in `0..=i64::MAX`).
#[inline]
pub(super) fn request_id_from_value(v: &Value) -> Result<u64, Fault> {
    match require_int(v) {
        Ok(i) if i >= 0 => Ok(i as u64),
        Ok(_) => Err(Fault::TypeMismatch {
            expected: "request_id u64",
            got: "negative-int",
        }),
        Err(fault) => Err(fault),
    }
}
