//! Structural payload match for selective receive.
//!
//! [`super::Opcode::ReceiveMatchEq`] compares an `Int`.
//! [`super::Opcode::ReceiveMatchValue`] compares the hop payload to any
//! [`super::Value`] (`Str`, `Bytes`, `Bool`, `Float`, nested `Message`, …).
//! Equality is [`PartialEq`]: `NaN` does not match `NaN`. This is not a
//! guard language and not a tuple/map matcher.

use super::value::Value;

/// `true` when `hop` is a [`Value::Message`] with `tag` and a payload equal
/// to `expected`.
pub fn payload_matches(tag: u16, expected: &Value, hop: &Value) -> bool {
    match hop.as_message() {
        Some(message) => message.tag == tag && message.payload.as_ref() == expected,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn str_payload_skips_a_different_body() {
        let hop_no = Value::Message(crate::Message::new(1, 1, 2, Value::str("no")));
        let hop_yes = Value::Message(crate::Message::new(1, 2, 2, Value::str("xyz")));
        let want = Value::str("xyz");
        assert!(!payload_matches(2, &want, &hop_no));
        assert!(payload_matches(2, &want, &hop_yes));
        assert!(!payload_matches(9, &want, &hop_yes));
    }

    #[test]
    fn receive_skips_a_different_string() -> Result<(), Box<dyn std::error::Error>> {
        use crate::{FlowOutcome, Program, Runtime, Value};

        let mut program = Program::new("pat");
        let sender = program.function("sender", 1, |f| {
            let parent = f.reg(0);
            let bad = f.load_str("no");
            let bad_msg = f.hop_fresh(2, bad);
            f.send(parent, bad_msg);
            let good = f.load_str("xyz");
            let good_msg = f.hop_fresh(2, good);
            f.send(parent, good_msg);
            f.return_(good);
        });
        program.function("main", 0, |f| {
            let me = f.self_cap();
            let _child = f.spawn(sender, &[me]);
            let tag = f.load_i32(2);
            let want = f.load_str("xyz");
            let got = f.receive_match_value(tag, want);
            let out = f.hop_payload(got);
            f.return_(out);
        });
        let rt = Runtime::with_natives(program.build(), crate::std_native_table())?;
        let main = match rt.function_index("main") {
            Some(index) => index,
            None => return Err("missing main".into()),
        };
        let outcome = rt.spawn(main, &[])?.join();
        rt.shutdown();
        match outcome {
            FlowOutcome::Completed(Value::Str(s)) if s.as_ref() == "xyz" => Ok(()),
            other => Err(format!("pattern receive got {other:?}").into()),
        }
    }
}
