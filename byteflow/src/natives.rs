//! Standard native (FFI) table shipped with the facade.
//!
//! # Why Message make/unpack lives here (not as new opcodes)
//!
//! [`crate::Value::Message`] is already a first-class wire/register value
//! (ABI v2). Building and tearing it apart in bytecode could be ISA ops
//! (`MakeMessage`, `MsgSender`, …), but that would burn opcode space and
//! force every embedder that never does request-reply to carry the
//! dispatch cost. Natives keep the envelope **generic in the core** while
//! the std table opts into the helpers — same pattern as `print` / `now_ms`
//! (design notes §30-31): host Rust owns the operation, bytecode only
//! picks a slot.
//!
//! # Stable indices are an embed contract
//!
//! Flash / separately-built `.bf` modules hard-code `CallNative` indices.
//! Renumbering an existing slot is a **breaking** change. Keep
//! [`std_native_map`] and [`std_native_table`] in lockstep; append only.
//!
//! | Index | Name | Role |
//! |------:|------|------|
//! | 0 | `print` | host [`crate::OutputSink`] (CLI uses stdout) |
//! | 1 | `now_ms` | wall-clock millis as `Value::Int` |
//! | 2 | `make_msg` | build [`crate::Message`]; no sender operand (stamped on `Send`) |
//! | 3 | `msg_sender` | extract `sender` → `Value::Pid` (authenticated **after** delivery) |
//! | 4 | `msg_request_id` | extract `request_id` → `Value::Int` |
//! | 5 | `msg_tag` | extract `tag` → `Value::Int` |
//! | 6 | `msg_payload` | extract `payload` → any [`crate::Value`] |
//! | 7 | `msg_reply_cap` | extract `reply_cap` → `Value::Cap` (SEND grant) |
//!
//! # `CallNative` argument layout
//!
//! `CallNative ra, fb, nc` takes args from `r[a .. a+nc]` and writes the
//! result back into `r[a]` — so a call **destroys** the first argument
//! register. One-arg unpacking uses `CallNativeFrom ra, rb, native`
//! (`r[a] = native(r[b])`), which leaves the source `Message` in place.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::bytecode::{Message, Value};
use crate::output::{OutputSink, StdoutSink};
use crate::vm::{expect_arg, expect_message, expect_u64, Fault, NativeTable};

/// Stable `CallNative` indices for [`std_native_table`]. Bytecode embeds these
/// once assembled — append only; never renumber.
pub mod std_native {
    pub const PRINT: u32 = 0;
    pub const NOW_MS: u32 = 1;
    pub const MAKE_MSG: u32 = 2;
    pub const MSG_SENDER: u32 = 3;
    pub const MSG_REQUEST_ID: u32 = 4;
    pub const MSG_TAG: u32 = 5;
    pub const MSG_PAYLOAD: u32 = 6;
    pub const MSG_REPLY_CAP: u32 = 7;
}

/// Name → `CallNative` index for documentation / host-side lookups.
///
/// Prefer this (or [`std_natives`]) over hard-coding integers in host code
/// so renames stay discoverable; bytecode itself still embeds the numeric
/// slot once assembled.
pub fn std_native_map() -> HashMap<String, u32> {
    HashMap::from([
        ("print".to_owned(), std_native::PRINT),
        ("now_ms".to_owned(), std_native::NOW_MS),
        ("make_msg".to_owned(), std_native::MAKE_MSG),
        ("msg_sender".to_owned(), std_native::MSG_SENDER),
        ("msg_request_id".to_owned(), std_native::MSG_REQUEST_ID),
        ("msg_tag".to_owned(), std_native::MSG_TAG),
        ("msg_payload".to_owned(), std_native::MSG_PAYLOAD),
        ("msg_reply_cap".to_owned(), std_native::MSG_REPLY_CAP),
    ])
}

/// Default FFI table with [`StdoutSink`]. Prefer [`std_native_table_with`]
/// in embedders that must not touch stdout.
pub fn std_native_table() -> Arc<NativeTable> {
    std_native_table_with(Arc::new(StdoutSink))
}

/// Std natives with an explicit print sink (`NullSink` in tests / libraries).
pub fn std_native_table_with(output: Arc<dyn OutputSink>) -> Arc<NativeTable> {
    let built = NativeTable::builder()
        .register_at(std_native::PRINT, "print", {
            let output = Arc::clone(&output);
            move |args| {
                output.write(args);
                Ok(Value::Unit)
            }
        })
        .and_then(|b| {
            b.register_at(std_native::NOW_MS, "now_ms", |_| {
                let duration = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|e| Fault::NativeError(format!("system clock error: {e}")))?;
                let millis = i64::try_from(duration.as_millis())
                    .map_err(|_| Fault::NativeError("system clock value exceeds i64".into()))?;
                Ok(Value::Int(millis))
            })
        })
        .and_then(|b| {
            b.register_at(std_native::MAKE_MSG, "make_msg", |args| {
                // 3-arg form: (request_id, tag, payload).
                // 4-arg legacy: (sender, request_id, tag, payload) — the sender
                // operand is discarded. Message.sender is always 0 here;
                // only authenticate_outgoing_message / send() writes identity.
                let (request_id, tag, payload) = if args.len() >= 4 {
                    (
                        expect_u64(args, 1, "make_msg")?,
                        expect_u64(args, 2, "make_msg")?,
                        expect_arg(args, 3, "make_msg")?.clone(),
                    )
                } else {
                    (
                        expect_u64(args, 0, "make_msg")?,
                        expect_u64(args, 1, "make_msg")?,
                        expect_arg(args, 2, "make_msg")?.clone(),
                    )
                };
                let tag = u16::try_from(tag).map_err(|_| {
                    Fault::NativeError(format!("make_msg: tag {tag} does not fit in u16"))
                })?;
                Ok(Value::Message(Message::new(0, request_id, tag, payload)))
            })
        })
        .and_then(|b| {
            b.register_at(std_native::MSG_SENDER, "msg_sender", |args| {
                Ok(Value::Pid(expect_message(args, 0, "msg_sender")?.sender))
            })
        })
        .and_then(|b| {
            b.register_at(std_native::MSG_REQUEST_ID, "msg_request_id", |args| {
                let id = expect_message(args, 0, "msg_request_id")?.request_id;
                let id = i64::try_from(id).map_err(|_| {
                    Fault::NativeError(format!("msg_request_id: request_id {id} exceeds i64"))
                })?;
                Ok(Value::Int(id))
            })
        })
        .and_then(|b| {
            b.register_at(std_native::MSG_TAG, "msg_tag", |args| {
                Ok(Value::Int(i64::from(
                    expect_message(args, 0, "msg_tag")?.tag,
                )))
            })
        })
        .and_then(|b| {
            b.register_at(std_native::MSG_PAYLOAD, "msg_payload", |args| {
                Ok(expect_message(args, 0, "msg_payload")?
                    .payload
                    .as_ref()
                    .clone())
            })
        })
        .and_then(|b| {
            b.register_at(std_native::MSG_REPLY_CAP, "msg_reply_cap", |args| {
                Ok(Value::Cap(
                    expect_message(args, 0, "msg_reply_cap")?.reply_cap,
                ))
            })
        });
    match built {
        Ok(b) => b.build(),
        // The eight names are distinct and the slots are the stable constants.
        // An empty table would hide a collision and turn every CallNative
        // into a missing native.
        Err(error) => panic!("std native table: {error}"),
    }
}

/// Build the default native table and its matching name map together.
///
/// Prefer this when the host both registers natives and resolves names to
/// indices for `ChunkBuilder` — one source of truth, no drift between map
/// and table.
pub fn std_natives() -> (Arc<NativeTable>, HashMap<String, u32>) {
    let table = std_native_table();
    let map: HashMap<String, u32> = table
        .names()
        .filter_map(|n| table.index_of(n).map(|i| (n.to_string(), i)))
        .collect();
    (table, map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{CapId, MapKey};
    use crate::output::NullSink;
    use crate::Fault;
    use std::sync::Arc;

    #[test]
    fn std_native_indices_are_stable() {
        let map = std_native_map();
        assert_eq!(map.get("print"), Some(&0));
        assert_eq!(map.get("now_ms"), Some(&1));
        assert_eq!(map.get("make_msg"), Some(&2));
        assert_eq!(map.get("msg_sender"), Some(&3));
        assert_eq!(map.get("msg_request_id"), Some(&4));
        assert_eq!(map.get("msg_tag"), Some(&5));
        assert_eq!(map.get("msg_payload"), Some(&6));
        assert_eq!(map.get("msg_reply_cap"), Some(&7));
    }

    #[test]
    fn std_native_table_matches_map() {
        let (table, map) = std_natives();
        let declared = std_native_map();
        assert_eq!(table.len(), 8);
        assert_eq!(map, declared);
        for (name, idx) in &declared {
            assert_eq!(table.index_of(name), Some(*idx));
        }
    }

    #[test]
    fn now_ms_returns_non_negative_int() -> Result<(), Box<dyn std::error::Error>> {
        let table = std_native_table();
        let now_ms = table.get(1).ok_or("now_ms native")?;
        let value = now_ms(&[])?;
        assert!(matches!(value, Value::Int(ms) if ms >= 0));
        Ok(())
    }

    #[test]
    fn make_msg_and_unpack_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let table = std_native_table();
        let make = table.get(2).ok_or("make_msg")?;
        let msg = make(&[Value::Int(3), Value::Int(7), Value::Int(42)])?;
        let expected = Message::new(0, 3, 7, 42);
        assert_eq!(msg.as_message(), Some(&expected));
        let sender = table.get(3).ok_or("msg_sender")?;
        let req = table.get(4).ok_or("msg_request_id")?;
        let tag = table.get(5).ok_or("msg_tag")?;
        let payload = table.get(6).ok_or("msg_payload")?;
        let cap = table.get(7).ok_or("msg_reply_cap")?;
        assert_eq!(sender(std::slice::from_ref(&msg))?, Value::Pid(0));
        assert_eq!(req(std::slice::from_ref(&msg))?, Value::Int(3));
        assert_eq!(tag(std::slice::from_ref(&msg))?, Value::Int(7));
        assert_eq!(payload(std::slice::from_ref(&msg))?, Value::Int(42));
        assert_eq!(cap(std::slice::from_ref(&msg))?, Value::Cap(CapId::NONE));
        Ok(())
    }

    #[test]
    fn msg_request_id_rejects_values_past_i64() -> Result<(), Box<dyn std::error::Error>> {
        let table = std_native_table_with(Arc::new(NullSink));
        let unpack = table
            .get(std_native::MSG_REQUEST_ID)
            .ok_or("msg_request_id")?;
        let fits = Value::Message(Message::new(0, i64::MAX as u64, 1, Value::Int(0)));
        assert_eq!(unpack(std::slice::from_ref(&fits))?, Value::Int(i64::MAX));
        let too_wide = Value::Message(Message::new(0, u64::MAX, 1, Value::Int(0)));
        match unpack(std::slice::from_ref(&too_wide)) {
            Err(Fault::NativeError(text)) => assert!(text.contains("exceeds i64")),
            other => panic!("expected NativeError, got {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn print_accepts_all_current_values() -> Result<(), Box<dyn std::error::Error>> {
        let table = std_native_table_with(Arc::new(NullSink));
        let print = table.get(std_native::PRINT).ok_or("print native")?;
        let list = Value::try_list(vec![Value::Int(1)]).ok_or("list")?;
        let map = Value::try_map(vec![(MapKey::Int(1), Value::Int(2))]).ok_or("map")?;
        let values = [
            Value::Unit,
            Value::Bool(true),
            Value::Int(42),
            Value::Float(1.5),
            Value::Pid(7),
            Value::Message(Message::new(1, 2, 3, 4)),
            Value::Cap(CapId::from_raw(9)),
            Value::str("hello"),
            Value::bytes([1u8, 2, 3]),
            list,
            map,
        ];
        assert_eq!(print(&values)?, Value::Unit);
        Ok(())
    }

    #[test]
    fn print_with_no_args_still_returns_unit() -> Result<(), Box<dyn std::error::Error>> {
        let table = std_native_table();
        let print = table.get(0).ok_or("print native")?;
        assert_eq!(print(&[])?, Value::Unit);
        Ok(())
    }
}
