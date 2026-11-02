//! Property-style decode / verify stress tests (in-house PRNG — no external crates).

use byteflow::prng::XorShift64;
use byteflow::{
    decode, decode_with, encode, verify, verify_with, CapId, Chunk, ConstantKind, FormatError,
    FunctionDef, Instruction, MapKey, Message, Opcode, Program, TrustLevel, Value, VerifyConfig,
    ABI_VERSION, ABI_VERSION_LISTS, ABI_VERSION_MAPS, ABI_VERSION_RECORDS, MAX_LIST_LEN,
    MAX_MAP_LEN, MAX_RECORD_LEN,
};

#[test]
fn decode_arbitrary_bytes_never_panics() {
    let mut rng = XorShift64::new(0xDEC0_0001);
    for _ in 0..128 {
        let len = rng.next_u32_inclusive(0, 511) as usize;
        let mut data = vec![0u8; len];
        rng.fill_bytes(&mut data);
        let _ = decode(&data);
        let _ = decode_with(&data, TrustLevel::Untrusted);
        let _ = decode_with(&data, TrustLevel::Trusted);
    }
}

#[test]
fn verify_after_decode_never_panics() {
    let mut rng = XorShift64::new(0xDEC0_0002);
    for _ in 0..128 {
        let len = rng.next_u32_inclusive(0, 511) as usize;
        let mut data = vec![0u8; len];
        rng.fill_bytes(&mut data);
        if let Ok(chunk) = decode_with(&data, TrustLevel::Trusted) {
            let _ = verify(&chunk);
            let _ = verify_with(
                &chunk,
                VerifyConfig {
                    trust: TrustLevel::Untrusted,
                },
            );
            let _ = verify_with(
                &chunk,
                VerifyConfig {
                    trust: TrustLevel::Trusted,
                },
            );
        }
    }
}

#[test]
fn encode_decode_roundtrip_preserves_verify() -> Result<(), String> {
    let mut rng = XorShift64::new(0xDEC0_0003);
    for _ in 0..64 {
        let imm = rng.next_i32();
        let mut program = Program::new("prop-roundtrip");
        program.function("main", 0, |f| {
            let r = f.load_i32(imm);
            f.return_(r);
        });
        let chunk = program.build();
        verify(&chunk).map_err(|e| e.to_string())?;
        let bytes = encode(&chunk);
        let decoded = decode(&bytes).map_err(|e| e.to_string())?;
        verify(&decoded).map_err(|e| e.to_string())?;
        if decoded.functions.len() != chunk.functions.len() {
            return Err("function count mismatch".into());
        }
        if decoded.constants.len() != chunk.constants.len() {
            return Err("constant count mismatch".into());
        }
    }
    Ok(())
}

const VALUE_SEED: u64 = 0xA11C_E000;
/// Random nests stay well under the decoder ceiling (16). The limit itself
/// is a separate case.
const GEN_DEPTH: u32 = 4;

#[test]
fn structured_values_round_trip_and_select_abi() -> Result<(), String> {
    let mut rng = XorShift64::new(VALUE_SEED);
    for _ in 0..48 {
        let case_seed = rng.next_u64();
        let mut local = XorShift64::new(case_seed);
        let value = gen_value(&mut local, 0);
        round_trip(case_seed, value, false)?;
    }
    round_trip(VALUE_SEED, Value::Unit, false)?;
    round_trip(VALUE_SEED, Value::Bool(false), false)?;
    round_trip(VALUE_SEED, Value::Bool(true), false)?;
    round_trip(VALUE_SEED, Value::Int(i64::MIN), false)?;
    round_trip(VALUE_SEED, Value::Int(i64::MAX), false)?;
    round_trip(VALUE_SEED, Value::Float(0.0), false)?;
    round_trip(VALUE_SEED, Value::Float(-0.0), false)?;
    round_trip(
        VALUE_SEED,
        Value::Float(f64::from_bits(0x7ff8_0000_0000_0001)),
        false,
    )?;
    round_trip(VALUE_SEED, Value::str(""), false)?;
    round_trip(VALUE_SEED, Value::str("héllo"), false)?;
    round_trip(VALUE_SEED, Value::str("a".repeat(256)), false)?;
    round_trip(VALUE_SEED, Value::bytes([]), false)?;
    round_trip(VALUE_SEED, Value::bytes([0u8, 255]), false)?;
    let list = Value::try_list((0..MAX_LIST_LEN).map(|n| Value::Int(n as i64)).collect())
        .ok_or("list of 256 was rejected by the constructor")?;
    round_trip(VALUE_SEED, list, false)?;
    let map = Value::try_map(
        (0..MAX_MAP_LEN)
            .map(|n| (MapKey::int(n as i64), Value::Int(n as i64)))
            .collect(),
    )
    .ok_or("map of 256 was rejected by the constructor")?;
    round_trip(VALUE_SEED, map, false)?;
    let record = Value::try_record((0..MAX_RECORD_LEN).map(|n| Value::Int(n as i64)).collect())
        .ok_or("record of 256 was rejected by the constructor")?;
    round_trip(VALUE_SEED, record, false)?;
    round_trip(VALUE_SEED, nested_lists(16), false)?;
    round_trip(0xA11C_E001, Value::Pid(7), true)?;
    round_trip(0xA11C_E002, Value::Cap(CapId::NONE), true)?;
    round_trip(
        0xA11C_E003,
        Value::Message(Message::new(3, 9, 1, Value::Int(4))),
        true,
    )?;
    let payload = Value::try_record(vec![Value::Int(1)]).ok_or("record payload")?;
    round_trip(
        0xA11C_E004,
        Value::Message(Message::new(1, 2, 3, payload)),
        true,
    )?;
    Ok(())
}

#[test]
fn lowered_abi_header_rejects_the_newer_tag() -> Result<(), String> {
    let list = Value::try_list(vec![Value::Int(1)]).ok_or("list")?;
    expect_lowered(list, ABI_VERSION_LISTS, ABI_VERSION, 9)?;
    let map = Value::try_map(vec![(MapKey::int(1), Value::Int(2))]).ok_or("map")?;
    expect_lowered(map, ABI_VERSION_MAPS, ABI_VERSION_LISTS, 10)?;
    let record = Value::try_record(vec![Value::Int(1)]).ok_or("record")?;
    expect_lowered(record, ABI_VERSION_RECORDS, ABI_VERSION_MAPS, 11)?;
    let message = Value::Message(Message::new(
        1,
        2,
        3,
        Value::try_list(vec![Value::Unit]).ok_or("msg list")?,
    ));
    expect_lowered(message, ABI_VERSION_LISTS, ABI_VERSION, 9)?;
    Ok(())
}

#[test]
fn duplicate_and_unknown_map_keys_stay_distinct() -> Result<(), String> {
    let duplicate = Value::Map(std::sync::Arc::from(vec![
        (MapKey::int(1), Value::Int(1)),
        (MapKey::int(1), Value::Int(2)),
    ]));
    match decode(&encode(&module_of(duplicate))) {
        Err(FormatError::DuplicateMapKey) => {}
        other => return Err(format!("duplicate key: {other:?}")),
    }
    let nested = Value::Record(std::sync::Arc::from(vec![Value::Map(
        std::sync::Arc::from(vec![
            (MapKey::str("héllo"), Value::Int(1)),
            (MapKey::str("héllo"), Value::Unit),
        ]),
    )]));
    match decode(&encode(&module_of(nested))) {
        Err(FormatError::DuplicateMapKey) => {}
        other => return Err(format!("nested duplicate key: {other:?}")),
    }

    let valid = Value::try_map(vec![
        (MapKey::int(i64::MIN), Value::Int(1)),
        (MapKey::str(""), Value::str("héllo")),
        (MapKey::str("k"), Value::Unit),
    ])
    .ok_or("valid keys")?;
    round_trip(0xA11C_E010, valid, false)?;

    let mut top = encode(&module_of(
        Value::try_map(vec![(MapKey::int(1), Value::Int(7))]).ok_or("one key")?,
    ));
    let at = map_key_tag(&top, false)?;
    if top[at] != 0 {
        return Err(format!(
            "expected an int key tag at {at}, found {}",
            top[at]
        ));
    }
    top[at] = 2;
    match decode(&top) {
        Err(FormatError::UnknownMapKey(2)) => {}
        other => return Err(format!("unknown key: {other:?}")),
    }

    let mut nested_bytes = encode(&module_of(
        Value::try_record(vec![Value::try_map(vec![(
            MapKey::int(4),
            Value::Bool(true),
        )])
        .ok_or("inner")?])
        .ok_or("outer")?,
    ));
    let at = map_key_tag(&nested_bytes, true)?;
    nested_bytes[at] = 255;
    match decode(&nested_bytes) {
        Err(FormatError::UnknownMapKey(255)) => Ok(()),
        other => Err(format!("nested unknown key: {other:?}")),
    }
}

#[test]
fn value_past_the_depth_ceiling_is_rejected() -> Result<(), String> {
    match decode(&encode(&module_of(nested_lists(17)))) {
        Err(FormatError::ValueTooNested) => Ok(()),
        other => Err(format!("depth 17: {other:?}")),
    }
}

#[test]
fn encoder_bool_bytes_stay_canonical() -> Result<(), String> {
    for (value, byte) in [(Value::Bool(false), 0u8), (Value::Bool(true), 1)] {
        let bytes = encode(&module_of(value.clone()));
        let at = bool_byte(&bytes)?;
        if bytes[at] != byte {
            return Err(format!("encoder wrote {} for {value:?}", bytes[at]));
        }
        let back = decode(&bytes).map_err(|e| e.to_string())?;
        if back.constants.first() != Some(&value) {
            return Err(format!("round-trip changed {value:?}"));
        }
    }
    let mut bytes = encode(&module_of(Value::Bool(true)));
    let at = bool_byte(&bytes)?;
    for bad in [2u8, 127, 255] {
        bytes[at] = bad;
        match decode(&bytes) {
            Err(FormatError::NonCanonicalBool(found)) if found == bad => {}
            other => return Err(format!("byte {bad}: {other:?}")),
        }
    }
    Ok(())
}

#[test]
fn untrusted_decode_rejects_a_pid_constant() {
    let bytes = encode(&module_of(Value::Pid(1)));
    assert_eq!(
        decode(&bytes),
        Err(FormatError::ForbiddenConstant(ConstantKind::ProcessId))
    );
    assert!(decode_with(&bytes, TrustLevel::Trusted).is_ok());
}

fn round_trip(seed: u64, value: Value, trusted: bool) -> Result<(), String> {
    let chunk = module_of(value);
    let bytes = encode(&chunk);
    let header = read_header(&bytes).map_err(|e| format!("seed {seed:#x}: {e}"))?;
    let expect = abi_of(&chunk.constants);
    if header != expect {
        return Err(format!(
            "seed {seed:#x}: header {header} != oracle {expect}"
        ));
    }
    let decoded = if trusted {
        decode_with(&bytes, TrustLevel::Trusted)
    } else {
        decode(&bytes)
    }
    .map_err(|e| format!("seed {seed:#x}: {e}"))?;
    if !same_module(&chunk, &decoded) {
        return Err(format!("seed {seed:#x}: decode changed the module"));
    }
    let config = VerifyConfig {
        trust: if trusted {
            TrustLevel::Trusted
        } else {
            TrustLevel::Untrusted
        },
    };
    verify_with(&decoded, config).map_err(|e| format!("seed {seed:#x}: {e}"))?;
    let again = encode(&decoded);
    let second = if trusted {
        decode_with(&again, TrustLevel::Trusted)
    } else {
        decode(&again)
    }
    .map_err(|e| format!("seed {seed:#x}: {e}"))?;
    if !same_module(&decoded, &second) {
        return Err(format!("seed {seed:#x}: second encode drifted"));
    }
    Ok(())
}

fn expect_lowered(value: Value, written: u32, lowered: u32, tag: u8) -> Result<(), String> {
    let mut bytes = encode(&module_of(value));
    let header = read_header(&bytes)?;
    if header != written {
        return Err(format!("header {header}, expected {written}"));
    }
    bytes[4..8].copy_from_slice(&lowered.to_le_bytes());
    match decode_with(&bytes, TrustLevel::Trusted) {
        Err(FormatError::UnknownValueTag(found)) if found == tag => Ok(()),
        other => Err(format!("lowered {written} -> {lowered}: {other:?}")),
    }
}

fn module_of(value: Value) -> Chunk {
    Chunk {
        name: "k".into(),
        constants: vec![value],
        functions: vec![FunctionDef {
            name: "main".into(),
            entry: 0,
            arity: 0,
            num_registers: 1,
        }],
        code: vec![Instruction::abc(Opcode::Return, 0, 0, 0)],
    }
}

fn read_header(bytes: &[u8]) -> Result<u32, String> {
    let word = bytes.get(4..8).ok_or("module shorter than a header")?;
    let mut raw = [0u8; 4];
    raw.copy_from_slice(word);
    Ok(u32::from_le_bytes(raw))
}

fn abi_of(constants: &[Value]) -> u32 {
    fn walk(value: &Value) -> u32 {
        match value {
            Value::Record(items) => items
                .iter()
                .fold(ABI_VERSION_RECORDS, |abi, item| abi.max(walk(item))),
            Value::Map(entries) => entries
                .iter()
                .fold(ABI_VERSION_MAPS, |abi, (_, item)| abi.max(walk(item))),
            Value::List(items) => items
                .iter()
                .fold(ABI_VERSION_LISTS, |abi, item| abi.max(walk(item))),
            Value::Message(message) => walk(message.payload.as_ref()),
            _ => ABI_VERSION,
        }
    }
    constants
        .iter()
        .fold(ABI_VERSION, |abi, value| abi.max(walk(value)))
}

fn same_module(left: &Chunk, right: &Chunk) -> bool {
    left.name == right.name
        && left.functions == right.functions
        && left.code == right.code
        && left.constants.len() == right.constants.len()
        && left
            .constants
            .iter()
            .zip(right.constants.iter())
            .all(|(a, b)| same_value(a, b))
}

fn same_value(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
        (Value::List(a), Value::List(b)) | (Value::Record(a), Value::Record(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| same_value(x, y))
        }
        (Value::Map(a), Value::Map(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b.iter())
                    .all(|((ak, av), (bk, bv))| ak == bk && same_value(av, bv))
        }
        (Value::Message(a), Value::Message(b)) => {
            a.sender == b.sender
                && a.reply_cap == b.reply_cap
                && a.request_id == b.request_id
                && a.tag == b.tag
                && same_value(a.payload.as_ref(), b.payload.as_ref())
        }
        _ => left == right,
    }
}

fn nested_lists(depth: usize) -> Value {
    let mut value = Value::Unit;
    for _ in 0..depth {
        value = Value::List(std::sync::Arc::from(vec![value]));
    }
    value
}

fn gen_value(rng: &mut XorShift64, depth: u32) -> Value {
    let span = if depth >= GEN_DEPTH { 5 } else { 8 };
    match rng.next_u32_inclusive(0, span) {
        0 => Value::Unit,
        1 => Value::Bool(rng.next_u32() & 1 == 1),
        2 => gen_int(rng),
        3 => gen_float(rng),
        4 => Value::str(gen_str(rng)),
        5 => Value::bytes(gen_bytes(rng)),
        6 => gen_list(rng, depth),
        7 => gen_map(rng, depth),
        _ => gen_record(rng, depth),
    }
}

fn gen_int(rng: &mut XorShift64) -> Value {
    let pick = rng.next_u32_inclusive(0, 5);
    let n = match pick {
        0 => 0,
        1 => 1,
        2 => -1,
        3 => i64::MIN,
        4 => i64::MAX,
        _ => rng.next_i64(),
    };
    Value::Int(n)
}

fn gen_float(rng: &mut XorShift64) -> Value {
    let pick = rng.next_u32_inclusive(0, 6);
    let n = match pick {
        0 => 0.0,
        1 => -0.0,
        2 => f64::INFINITY,
        3 => f64::NEG_INFINITY,
        4 => f64::MIN,
        5 => f64::MAX,
        _ => f64::from_bits(rng.next_u64()),
    };
    Value::Float(n)
}

fn gen_str(rng: &mut XorShift64) -> String {
    match rng.next_u32_inclusive(0, 3) {
        0 => String::new(),
        1 => "héllo".to_string(),
        2 => "a".repeat(rng.next_u32_inclusive(1, 64) as usize),
        _ => {
            let n = rng.next_u32_inclusive(1, 12);
            (0..n)
                .map(|_| char::from(b'a' + rng.next_u32_inclusive(0, 25) as u8))
                .collect()
        }
    }
}

fn gen_bytes(rng: &mut XorShift64) -> Vec<u8> {
    let n = rng.next_u32_inclusive(0, 32) as usize;
    let mut buf = vec![0u8; n];
    rng.fill_bytes(&mut buf);
    buf
}

fn gen_list(rng: &mut XorShift64, depth: u32) -> Value {
    let n = rng.next_u32_inclusive(0, 3) as usize;
    let items: Vec<_> = (0..n).map(|_| gen_value(rng, depth + 1)).collect();
    Value::try_list(items).unwrap_or(Value::Unit)
}

fn gen_record(rng: &mut XorShift64, depth: u32) -> Value {
    let n = rng.next_u32_inclusive(0, 3) as usize;
    let fields: Vec<_> = (0..n).map(|_| gen_value(rng, depth + 1)).collect();
    Value::try_record(fields).unwrap_or(Value::Unit)
}

fn gen_map(rng: &mut XorShift64, depth: u32) -> Value {
    let n = rng.next_u32_inclusive(0, 3) as usize;
    let mut entries = Vec::with_capacity(n);
    for index in 0..n {
        let key = if index % 2 == 0 {
            MapKey::int(index as i64)
        } else if index == 1 {
            MapKey::str("héllo")
        } else {
            MapKey::str(format!("k{index}"))
        };
        entries.push((key, gen_value(rng, depth + 1)));
    }
    Value::try_map(entries).unwrap_or(Value::Unit)
}

fn bool_byte(bytes: &[u8]) -> Result<usize, String> {
    let start = 4 + 4 + 4 + 1 + 4;
    if bytes.get(start) != Some(&1) {
        return Err(format!("bool tag missing at {start}"));
    }
    Ok(start + 1)
}

fn map_key_tag(bytes: &[u8], nested_in_record: bool) -> Result<usize, String> {
    let constant = 4 + 4 + 4 + 1 + 4;
    let at = if nested_in_record {
        constant + 1 + 4 + 1 + 4
    } else {
        constant + 1 + 4
    };
    bytes
        .get(at)
        .map(|_| at)
        .ok_or_else(|| format!("key tag at {at} is past the module"))
}
