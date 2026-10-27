use super::chunk::{
    Chunk, FunctionDef, ABI_VERSION, ABI_VERSION_LISTS, ABI_VERSION_MAPS, ABI_VERSION_RECORDS,
    MAGIC,
};
use super::instruction::Instruction;
use super::opcode::Opcode;
use super::value::Value;
use super::verify::TrustLevel;

const MAX_NAME: u32 = 64 * 1024;
const MAX_ITEMS: u32 = 1_000_000;
/// Max length for [`Value::Str`] / [`Value::Bytes`] constant payloads.
const MAX_BLOB: u32 = 1_048_576;
/// Nested `Message.payload` depth (defense against hostile `.bf` files).
pub(crate) const MAX_VALUE_DEPTH: u32 = 16;

/// Why a `.bf` buffer failed to decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    BadMagic {
        found: [u8; 4],
    },
    UnsupportedAbi(u32),
    Truncated,
    LimitExceeded {
        what: &'static str,
        got: u32,
    },
    BadUtf8,
    UnknownValueTag(u8),
    /// The byte after a map tag is not a key kind (`0` = `Int`, `1` = `Str`).
    /// This used to be reported as [`FormatError::UnknownValueTag`]`(10)`,
    /// which is the map's own tag.
    UnknownMapKey(u8),
    /// Tag `1` was followed by a byte other than `0` or `1`. The encoder
    /// writes only those two. Any other byte used to decode as `true`.
    NonCanonicalBool(u8),
    UnknownOpcode {
        at: usize,
        byte: u8,
    },
    ForbiddenConstant(super::verify::ConstantKind),
    ValueTooNested,
    /// A map constant repeats a key. [`Value::try_map`] also rejects this,
    /// but the decoder used to report it as [`FormatError::ValueTooNested`].
    DuplicateMapKey,
    /// Bytes left after a complete module. Ignoring them would accept a
    /// buffer that is not the module the caller hashed or transmitted.
    Trailing {
        leftover: usize,
    },
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FormatError::BadMagic { found } => {
                write!(f, "bad magic {found:?}, expected {:?}", MAGIC)
            }
            FormatError::UnsupportedAbi(v) => write!(f, "unsupported ABI version {v}"),
            FormatError::Truncated => write!(f, "truncated .bf module"),
            FormatError::LimitExceeded { what, got } => {
                write!(f, "{what} count {got} exceeds decoder limit")
            }
            FormatError::BadUtf8 => write!(f, "name is not valid UTF-8"),
            FormatError::UnknownValueTag(t) => write!(f, "unknown value tag 0x{t:02X}"),
            FormatError::UnknownMapKey(t) => write!(f, "unknown map key tag 0x{t:02X}"),
            FormatError::NonCanonicalBool(b) => {
                write!(f, "non-canonical bool byte 0x{b:02X}")
            }
            FormatError::UnknownOpcode { at, byte } => {
                write!(f, "unknown opcode 0x{byte:02X} at instruction {at}")
            }
            FormatError::ForbiddenConstant(kind) => {
                write!(
                    f,
                    "untrusted module must not embed {kind} in the constant pool"
                )
            }
            FormatError::ValueTooNested => write!(f, "value nesting exceeds decoder limit"),
            FormatError::DuplicateMapKey => write!(f, "map constant repeats a key"),
            FormatError::Trailing { leftover } => {
                write!(f, "trailing {leftover} bytes after module")
            }
        }
    }
}

impl std::error::Error for FormatError {}

/// Oldest ABI that can hold `constants`: v8 for a record, otherwise v7 for a
/// map, otherwise v6 for a list, otherwise v5.
pub(crate) fn module_abi(constants: &[Value]) -> u32 {
    if constants_contain_record(constants) {
        ABI_VERSION_RECORDS
    } else if constants_contain_map(constants) {
        ABI_VERSION_MAPS
    } else if constants_contain_list(constants) {
        ABI_VERSION_LISTS
    } else {
        ABI_VERSION
    }
}

/// Encode `chunk` as a BFV0 module (little-endian). Pure data — no I/O.
///
/// The header is the oldest ABI that can represent the constant pool.
pub fn encode(chunk: &Chunk) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    let abi = module_abi(&chunk.constants);
    out.extend_from_slice(&abi.to_le_bytes());
    write_string(&mut out, &chunk.name);
    out.extend_from_slice(&(chunk.constants.len() as u32).to_le_bytes());
    for value in &chunk.constants {
        write_value(&mut out, value);
    }
    out.extend_from_slice(&(chunk.functions.len() as u32).to_le_bytes());
    for def in &chunk.functions {
        write_string(&mut out, &def.name);
        out.extend_from_slice(&def.entry.to_le_bytes());
        out.push(def.arity);
        out.push(def.num_registers);
    }
    out.extend_from_slice(&(chunk.code.len() as u32).to_le_bytes());
    for instr in &chunk.code {
        out.push(instr.op.as_u8());
        out.push(instr.a);
        out.push(instr.b);
        out.push(instr.c);
        out.extend_from_slice(&instr.imm.to_le_bytes());
    }
    out
}

/// Decode a BFV0 module. Untrusted by default: Cap / Pid / Message constants
/// are rejected (see [`decode_with`]).
pub fn decode(bytes: &[u8]) -> Result<Chunk, FormatError> {
    decode_with(bytes, TrustLevel::Untrusted)
}

/// Decode with an explicit trust level. Trusted decode is for host-packed
/// modules that may embed authority tags in the constant pool.
pub fn decode_with(bytes: &[u8], trust: TrustLevel) -> Result<Chunk, FormatError> {
    let mut r = Reader {
        data: bytes,
        pos: 0,
    };
    let magic = r.read_array::<4>()?;
    if magic != MAGIC {
        return Err(FormatError::BadMagic { found: magic });
    }
    let abi = r.read_u32()?;
    if abi != ABI_VERSION
        && abi != ABI_VERSION_LISTS
        && abi != ABI_VERSION_MAPS
        && abi != ABI_VERSION_RECORDS
    {
        return Err(FormatError::UnsupportedAbi(abi));
    }
    let name = r.read_string()?;
    let n_const = r.read_count("constants")?;
    let mut constants = Vec::with_capacity(n_const as usize);
    for _ in 0..n_const {
        constants.push(r.read_value(trust, 0, abi)?);
    }
    let n_fn = r.read_count("functions")?;
    let mut functions = Vec::with_capacity(n_fn as usize);
    for _ in 0..n_fn {
        functions.push(FunctionDef {
            name: r.read_string()?,
            entry: r.read_u32()?,
            arity: r.read_u8()?,
            num_registers: r.read_u8()?,
        });
    }
    let n_code = r.read_count("code")?;
    let mut code = Vec::with_capacity(n_code as usize);
    for at in 0..n_code as usize {
        let byte = r.read_u8()?;
        let op = Opcode::from_u8(byte).ok_or(FormatError::UnknownOpcode { at, byte })?;
        code.push(Instruction {
            op,
            a: r.read_u8()?,
            b: r.read_u8()?,
            c: r.read_u8()?,
            imm: r.read_i32()?,
        });
    }
    if r.pos != bytes.len() {
        return Err(FormatError::Trailing {
            leftover: bytes.len() - r.pos,
        });
    }
    Ok(Chunk {
        name,
        constants,
        code,
        functions,
    })
}

fn write_string(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn write_value(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Unit => out.push(0),
        Value::Bool(b) => {
            out.push(1);
            out.push(u8::from(*b));
        }
        Value::Int(i) => {
            out.push(2);
            out.extend_from_slice(&i.to_le_bytes());
        }
        Value::Float(x) => {
            out.push(3);
            out.extend_from_slice(&x.to_le_bytes());
        }
        Value::Pid(p) => {
            out.push(4);
            out.extend_from_slice(&p.to_le_bytes());
        }
        Value::Message(m) => {
            // Tag 5 — ABI v5 layout (reply_cap is 128-bit, payload is nested).
            out.push(5);
            out.extend_from_slice(&m.sender.to_le_bytes());
            out.extend_from_slice(&m.reply_cap.as_u128().to_le_bytes());
            out.extend_from_slice(&m.request_id.to_le_bytes());
            out.extend_from_slice(&m.tag.to_le_bytes());
            write_value(out, m.payload.as_ref());
        }
        Value::Cap(c) => {
            out.push(6);
            out.extend_from_slice(&c.as_u128().to_le_bytes());
        }
        Value::Str(s) => {
            out.push(7);
            write_blob(out, s.as_bytes());
        }
        Value::Bytes(b) => {
            out.push(8);
            write_blob(out, b);
        }
        Value::List(items) => {
            out.push(9);
            out.extend_from_slice(&(items.len() as u32).to_le_bytes());
            for item in items.iter() {
                write_value(out, item);
            }
        }
        Value::Map(entries) => {
            out.push(10);
            out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
            for (key, value) in entries.iter() {
                match key {
                    super::value::MapKey::Int(n) => {
                        out.push(0);
                        out.extend_from_slice(&n.to_le_bytes());
                    }
                    super::value::MapKey::Str(text) => {
                        out.push(1);
                        write_blob(out, text.as_bytes());
                    }
                }
                write_value(out, value);
            }
        }
        Value::Record(fields) => {
            out.push(11);
            out.extend_from_slice(&(fields.len() as u32).to_le_bytes());
            for field in fields.iter() {
                write_value(out, field);
            }
        }
    }
}

/// One value in the same layout as a `.bf` constant. The caller decides
/// whether a `Cap` or `Pid` is allowed.
pub(crate) fn encode_value(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write_value(&mut out, value);
    out
}

/// Decode one value from the front of `bytes`. The `usize` is how many
/// bytes that value occupied.
pub(crate) fn decode_value(bytes: &[u8]) -> Result<(Value, usize), FormatError> {
    let mut reader = Reader {
        data: bytes,
        pos: 0,
    };
    let value = reader.read_value(TrustLevel::Trusted, 0, ABI_VERSION_RECORDS)?;
    Ok((value, reader.pos))
}

fn constants_contain_record(constants: &[Value]) -> bool {
    fn walk(value: &Value) -> bool {
        match value {
            Value::Record(_) => true,
            Value::List(items) => items.iter().any(walk),
            Value::Map(entries) => entries.iter().any(|(_, value)| walk(value)),
            Value::Message(message) => walk(message.payload.as_ref()),
            _ => false,
        }
    }
    constants.iter().any(walk)
}

fn constants_contain_map(constants: &[Value]) -> bool {
    fn walk(value: &Value) -> bool {
        match value {
            Value::Map(_) => true,
            Value::List(items) | Value::Record(items) => items.iter().any(walk),
            Value::Message(message) => walk(message.payload.as_ref()),
            _ => false,
        }
    }
    constants.iter().any(walk)
}

fn constants_contain_list(constants: &[Value]) -> bool {
    fn walk(value: &Value) -> bool {
        match value {
            Value::List(_) => true,
            Value::Record(fields) => fields.iter().any(walk),
            Value::Map(entries) => entries.iter().any(|(_, value)| walk(value)),
            Value::Message(message) => walk(message.payload.as_ref()),
            _ => false,
        }
    }
    constants.iter().any(walk)
}

fn write_blob(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn rest(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], FormatError> {
        if self.rest() < n {
            return Err(FormatError::Truncated);
        }
        let slice = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn read_u8(&mut self) -> Result<u8, FormatError> {
        Ok(self.take(1)?[0])
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N], FormatError> {
        let slice = self.take(N)?;
        let mut arr = [0u8; N];
        arr.copy_from_slice(slice);
        Ok(arr)
    }

    fn read_u32(&mut self) -> Result<u32, FormatError> {
        Ok(u32::from_le_bytes(self.read_array()?))
    }

    fn read_u16(&mut self) -> Result<u16, FormatError> {
        Ok(u16::from_le_bytes(self.read_array()?))
    }

    fn read_i32(&mut self) -> Result<i32, FormatError> {
        Ok(i32::from_le_bytes(self.read_array()?))
    }

    fn read_i64(&mut self) -> Result<i64, FormatError> {
        Ok(i64::from_le_bytes(self.read_array()?))
    }

    fn read_u64(&mut self) -> Result<u64, FormatError> {
        Ok(u64::from_le_bytes(self.read_array()?))
    }

    fn read_u128(&mut self) -> Result<u128, FormatError> {
        Ok(u128::from_le_bytes(self.read_array()?))
    }

    fn read_f64(&mut self) -> Result<f64, FormatError> {
        Ok(f64::from_le_bytes(self.read_array()?))
    }

    fn read_count(&mut self, what: &'static str) -> Result<u32, FormatError> {
        let n = self.read_u32()?;
        if n > MAX_ITEMS {
            return Err(FormatError::LimitExceeded { what, got: n });
        }
        Ok(n)
    }

    fn read_string(&mut self) -> Result<String, FormatError> {
        let len = self.read_u32()?;
        if len > MAX_NAME {
            return Err(FormatError::LimitExceeded {
                what: "name",
                got: len,
            });
        }
        let bytes = self.take(len as usize)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| FormatError::BadUtf8)
    }

    fn read_value(
        &mut self,
        trust: TrustLevel,
        depth: u32,
        abi: u32,
    ) -> Result<Value, FormatError> {
        if depth > MAX_VALUE_DEPTH {
            return Err(FormatError::ValueTooNested);
        }
        match self.read_u8()? {
            0 => Ok(Value::Unit),
            1 => match self.read_u8()? {
                0 => Ok(Value::Bool(false)),
                1 => Ok(Value::Bool(true)),
                byte => Err(FormatError::NonCanonicalBool(byte)),
            },
            2 => Ok(Value::Int(self.read_i64()?)),
            3 => Ok(Value::Float(self.read_f64()?)),
            4 => {
                if trust == TrustLevel::Untrusted {
                    return Err(FormatError::ForbiddenConstant(
                        super::verify::ConstantKind::ProcessId,
                    ));
                }
                Ok(Value::Pid(self.read_u64()?))
            }
            5 => {
                if trust == TrustLevel::Untrusted {
                    return Err(FormatError::ForbiddenConstant(
                        super::verify::ConstantKind::Message,
                    ));
                }
                let sender = self.read_u64()?;
                let reply_cap = super::cap::CapId::from_raw(self.read_u128()?);
                let request_id = self.read_u64()?;
                let tag = self.read_u16()?;
                let payload = self.read_value(trust, depth + 1, abi)?;
                Ok(Value::Message(
                    super::value::Message::new(sender, request_id, tag, payload)
                        .authenticate(sender, reply_cap),
                ))
            }
            6 => {
                if trust == TrustLevel::Untrusted {
                    return Err(FormatError::ForbiddenConstant(
                        super::verify::ConstantKind::Capability,
                    ));
                }
                Ok(Value::Cap(super::cap::CapId::from_raw(self.read_u128()?)))
            }
            7 => {
                let bytes = self.read_blob("str")?;
                let s = std::str::from_utf8(bytes).map_err(|_| FormatError::BadUtf8)?;
                Ok(Value::str(s))
            }
            8 => Ok(Value::bytes(self.read_blob("bytes")?)),
            9 => {
                if abi < ABI_VERSION_LISTS {
                    return Err(FormatError::UnknownValueTag(9));
                }
                let n = self.read_u32()?;
                if n > super::value::MAX_LIST_LEN as u32 {
                    return Err(FormatError::LimitExceeded {
                        what: "list",
                        got: n,
                    });
                }
                let mut items = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    items.push(self.read_value(trust, depth + 1, abi)?);
                }
                Value::try_list(items).ok_or(FormatError::ValueTooNested)
            }
            10 => {
                if abi < ABI_VERSION_MAPS {
                    return Err(FormatError::UnknownValueTag(10));
                }
                let n = self.read_u32()?;
                if n > super::value::MAX_MAP_LEN as u32 {
                    return Err(FormatError::LimitExceeded {
                        what: "map",
                        got: n,
                    });
                }
                let mut entries = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    let key = match self.read_u8()? {
                        0 => super::value::MapKey::Int(i64::from_le_bytes(self.read_array()?)),
                        1 => {
                            let bytes = self.read_blob("map key")?;
                            let text =
                                std::str::from_utf8(bytes).map_err(|_| FormatError::BadUtf8)?;
                            super::value::MapKey::str(text)
                        }
                        tag => return Err(FormatError::UnknownMapKey(tag)),
                    };
                    let value = self.read_value(trust, depth + 1, abi)?;
                    entries.push((key, value));
                }
                if super::value::map_has_duplicate_key(&entries) {
                    return Err(FormatError::DuplicateMapKey);
                }
                super::value::Value::try_map(entries).ok_or(FormatError::ValueTooNested)
            }
            11 => {
                if abi < ABI_VERSION_RECORDS {
                    return Err(FormatError::UnknownValueTag(11));
                }
                let n = self.read_u32()?;
                if n > super::value::MAX_RECORD_LEN as u32 {
                    return Err(FormatError::LimitExceeded {
                        what: "record",
                        got: n,
                    });
                }
                let mut fields = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    fields.push(self.read_value(trust, depth + 1, abi)?);
                }
                super::value::Value::try_record(fields).ok_or(FormatError::ValueTooNested)
            }
            tag => Err(FormatError::UnknownValueTag(tag)),
        }
    }

    fn read_blob(&mut self, what: &'static str) -> Result<&'a [u8], FormatError> {
        let len = self.read_u32()?;
        if len > MAX_BLOB {
            return Err(FormatError::LimitExceeded { what, got: len });
        }
        self.take(len as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::builder::ChunkBuilder;
    use crate::bytecode::opcode::Opcode;
    use crate::bytecode::verify::{verify, TrustLevel};

    #[test]
    fn roundtrip_preserves_chunk() -> Result<(), FormatError> {
        let mut b = ChunkBuilder::new("roundtrip");
        b.begin_function("main", 0, 2);
        let k = b.const_(Value::Int(9));
        b.emit_load_const(0, k);
        b.emit_load_imm(1, 1);
        b.emit_binop(Opcode::Add, 0, 0, 1);
        b.emit_return(0);
        let original = b.finish();
        let bytes = encode(&original);
        assert_eq!(&bytes[..4], &MAGIC);
        let decoded = decode(&bytes)?;
        assert!(verify(&decoded).is_ok());
        assert_eq!(decoded.name, original.name);
        assert_eq!(decoded.constants, original.constants);
        assert_eq!(decoded.code, original.code);
        assert_eq!(decoded.functions, original.functions);
        Ok(())
    }

    #[test]
    fn roundtrip_preserves_message_constant() -> Result<(), FormatError> {
        use super::super::value::Message;
        let mut b = ChunkBuilder::new("msg-const");
        b.begin_function("main", 0, 1);
        let k = b.const_(Value::Message(Message::new(1, 2, 3, 4u64)));
        b.emit_load_const(0, k);
        b.emit_return(0);
        let original = b.finish();
        let decoded = decode_with(&encode(&original), TrustLevel::Trusted)?;
        assert_eq!(decoded.constants, original.constants);
        let got = decoded.constants[0]
            .as_message()
            .ok_or(FormatError::Truncated)?;
        assert_eq!(got.sender, 1);
        assert_eq!(got.request_id, 2);
        assert_eq!(got.tag, 3);
        assert_eq!(got.payload.as_ref(), &Value::Int(4));
        Ok(())
    }

    #[test]
    fn untrusted_decode_rejects_cap_constant() {
        use crate::bytecode::cap::CapId;
        let mut b = ChunkBuilder::new("cap-const");
        b.begin_function("main", 0, 1);
        let k = b.const_(Value::Cap(CapId::from_raw(1)));
        b.emit_load_const(0, k);
        b.emit_return(0);
        let bytes = encode(&b.finish());
        assert!(matches!(
            decode(&bytes),
            Err(FormatError::ForbiddenConstant(_))
        ));
        assert!(decode_with(&bytes, TrustLevel::Trusted).is_ok());
    }

    #[test]
    fn roundtrip_preserves_str_and_bytes_constants() -> Result<(), FormatError> {
        let mut b = ChunkBuilder::new("blob-const");
        b.begin_function("main", 0, 2);
        let ks = b.const_(Value::str("olá"));
        let kb = b.const_(Value::bytes([0u8, 255, 7]));
        b.emit_load_const(0, ks);
        b.emit_load_const(1, kb);
        b.emit_return(0);
        let original = b.finish();
        let decoded = decode(&encode(&original))?;
        assert_eq!(decoded.constants, original.constants);
        assert_eq!(decoded.constants[0].as_str(), Some("olá"));
        assert_eq!(decoded.constants[1].as_bytes(), Some(&[0, 255, 7][..]));
        Ok(())
    }

    #[test]
    fn rejects_bad_magic() {
        assert!(matches!(decode(b"XXXX"), Err(FormatError::BadMagic { .. })));
    }

    #[test]
    fn rejects_bytes_after_a_complete_module() {
        let mut b = ChunkBuilder::new("exact");
        b.begin_function("main", 0, 1);
        b.emit_return(0);
        let mut bytes = encode(&b.finish());
        bytes.push(0xFF);
        assert!(matches!(
            decode(&bytes),
            Err(FormatError::Trailing { leftover: 1 })
        ));
    }

    #[test]
    fn list_constant_roundtrips_as_abi_v6_and_v5_rejects_the_tag() -> Result<(), FormatError> {
        let list = match Value::try_list(vec![Value::Int(1), Value::Int(2)]) {
            Some(list) => list,
            None => return Err(FormatError::ValueTooNested),
        };
        let mut b = ChunkBuilder::new("list");
        b.begin_function("main", 0, 1);
        let k = b.const_(list);
        b.emit_load_const(0, k);
        b.emit_return(0);
        let original = b.finish();
        let bytes = encode(&original);
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap_or([0; 4])),
            6
        );
        let decoded = decode(&bytes)?;
        assert_eq!(decoded.constants, original.constants);
        let mut v5 = bytes.clone();
        v5[4..8].copy_from_slice(&5u32.to_le_bytes());
        assert!(matches!(decode(&v5), Err(FormatError::UnknownValueTag(9))));
        let wide = Value::List(std::sync::Arc::from(vec![Value::Int(1); 257]));
        let mut wide_chunk = Chunk {
            name: "wide".into(),
            ..Chunk::default()
        };
        wide_chunk.constants.push(wide);
        wide_chunk.functions.push(FunctionDef {
            name: "main".into(),
            entry: 0,
            arity: 0,
            num_registers: 1,
        });
        wide_chunk.code.push(Instruction::nullary(Opcode::Return));
        assert!(matches!(
            decode(&encode(&wide_chunk)),
            Err(FormatError::LimitExceeded { what: "list", .. })
        ));
        Ok(())
    }

    #[test]
    fn map_constant_roundtrips_as_abi_v7() -> Result<(), FormatError> {
        let map = Value::try_map(vec![
            (super::super::value::MapKey::int(1), Value::Int(2)),
            (super::super::value::MapKey::str("k"), Value::str("v")),
        ])
        .ok_or(FormatError::ValueTooNested)?;
        let mut chunk = Chunk {
            name: "map".into(),
            ..Chunk::default()
        };
        chunk.constants.push(map);
        chunk.functions.push(FunctionDef {
            name: "main".into(),
            entry: 0,
            arity: 0,
            num_registers: 1,
        });
        chunk.code.push(Instruction::abc(Opcode::Return, 0, 0, 0));
        let bytes = encode(&chunk);
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap_or([0; 4])),
            ABI_VERSION_MAPS
        );
        let back = decode_with(&bytes, TrustLevel::Trusted)?;
        match back.constants.first() {
            Some(Value::Map(entries)) => {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].1, Value::Int(2));
            }
            _ => return Err(FormatError::Truncated),
        }
        let pairs = (0..256)
            .map(|n| (super::super::value::MapKey::Int(n), Value::Int(n)))
            .collect();
        let full = Value::try_map(pairs).ok_or(FormatError::LimitExceeded {
            what: "map",
            got: 256,
        })?;
        let (decoded, _) = decode_value(&encode_value(&full))?;
        match decoded {
            Value::Map(entries) if entries.len() == 256 => {}
            _ => return Err(FormatError::Truncated),
        }
        let too_big = Value::Map(std::sync::Arc::from(vec![
            (
                super::super::value::MapKey::Int(0),
                Value::Int(0),
            );
            257
        ]));
        match decode_value(&encode_value(&too_big)) {
            Err(FormatError::LimitExceeded {
                what: "map",
                got: 257,
            }) => Ok(()),
            Err(err) => Err(err),
            Ok(_) => Err(FormatError::LimitExceeded {
                what: "map",
                got: 257,
            }),
        }
    }

    #[test]
    fn list_of_256_round_trips_and_257_is_rejected() -> Result<(), FormatError> {
        let items: Vec<Value> = (0..256).map(Value::Int).collect();
        let list = Value::try_list(items).ok_or(FormatError::LimitExceeded {
            what: "list",
            got: 256,
        })?;
        let mut chunk = Chunk {
            name: "cap".into(),
            ..Chunk::default()
        };
        chunk.constants.push(list);
        chunk.functions.push(FunctionDef {
            name: "main".into(),
            entry: 0,
            arity: 0,
            num_registers: 1,
        });
        chunk.code.push(Instruction::abc(Opcode::Return, 0, 0, 0));
        let bytes = encode(&chunk);
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap_or([0; 4])),
            ABI_VERSION_LISTS
        );
        let back = decode_with(&bytes, TrustLevel::Trusted)?;
        match back.constants.first() {
            Some(Value::List(items)) if items.len() == 256 => {}
            _ => return Err(FormatError::Truncated),
        }
        let too_big = Value::List(std::sync::Arc::from(vec![Value::Int(1); 257]));
        let encoded = encode_value(&too_big);
        match decode_value(&encoded) {
            Err(FormatError::LimitExceeded {
                what: "list",
                got: 257,
            }) => Ok(()),
            Err(err) => Err(err),
            Ok(_) => Err(FormatError::LimitExceeded {
                what: "list",
                got: 257,
            }),
        }
    }

    #[test]
    fn duplicate_map_key_is_not_reported_as_nesting() {
        let mut bytes = Vec::new();
        bytes.push(10);
        bytes.extend_from_slice(&2u32.to_le_bytes());
        for _ in 0..2 {
            bytes.push(0);
            bytes.extend_from_slice(&1i64.to_le_bytes());
            bytes.push(2);
            bytes.extend_from_slice(&7i64.to_le_bytes());
        }
        assert_eq!(decode_value(&bytes), Err(FormatError::DuplicateMapKey));
    }

    #[test]
    fn unknown_map_key_keeps_the_discriminant() {
        let mut bytes = Vec::new();
        bytes.push(10);
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.push(2);
        assert_eq!(decode_value(&bytes), Err(FormatError::UnknownMapKey(2)));
    }

    #[test]
    fn bool_false_and_true_round_trip_as_zero_and_one() -> Result<(), FormatError> {
        for (value, byte) in [(Value::Bool(false), 0u8), (Value::Bool(true), 1)] {
            let bytes = encode_value(&value);
            if bytes != [1, byte] {
                return Err(FormatError::NonCanonicalBool(byte));
            }
            let (back, used) = decode_value(&bytes)?;
            if used != 2 || back != value {
                return Err(FormatError::Truncated);
            }
        }
        Ok(())
    }

    #[test]
    fn non_canonical_bool_bytes_are_rejected() {
        for byte in [2u8, 127, 255] {
            assert_eq!(
                decode_value(&[1, byte]),
                Err(FormatError::NonCanonicalBool(byte))
            );
        }
    }

    #[test]
    fn encoded_header_is_module_abi() -> Result<(), FormatError> {
        let list = Value::try_list(vec![Value::Int(1)]).ok_or(FormatError::ValueTooNested)?;
        let map = Value::try_map(vec![(crate::MapKey::Int(1), list.clone())])
            .ok_or(FormatError::ValueTooNested)?;
        let record = Value::try_record(vec![map.clone()]).ok_or(FormatError::ValueTooNested)?;
        for value in [Value::Int(1), list, map, record] {
            let mut chunk = Chunk {
                name: "abi".into(),
                ..Chunk::default()
            };
            chunk.constants.push(value);
            chunk.functions.push(FunctionDef {
                name: "main".into(),
                entry: 0,
                arity: 0,
                num_registers: 1,
            });
            chunk.code.push(Instruction::abc(Opcode::Return, 0, 0, 0));
            let bytes = encode(&chunk);
            let header = u32::from_le_bytes(bytes[4..8].try_into().unwrap_or([0; 4]));
            if header != module_abi(&chunk.constants) {
                return Err(FormatError::UnsupportedAbi(header));
            }
        }
        Ok(())
    }

    #[test]
    fn list_of_32_still_decodes() -> Result<(), FormatError> {
        let list = Value::try_list((0..32).map(Value::Int).collect()).ok_or(
            FormatError::LimitExceeded {
                what: "list",
                got: 32,
            },
        )?;
        let (back, _) = decode_value(&encode_value(&list))?;
        match back {
            Value::List(items) if items.len() == 32 => Ok(()),
            _ => Err(FormatError::Truncated),
        }
    }

    #[test]
    fn record_selects_abi_v8_and_older_headers_reject_the_tag() -> Result<(), FormatError> {
        let record = Value::try_record(vec![Value::Int(1), Value::Int(2)])
            .ok_or(FormatError::ValueTooNested)?;
        let mut chunk = Chunk {
            name: "rec".into(),
            ..Chunk::default()
        };
        chunk.constants.push(record);
        chunk.functions.push(FunctionDef {
            name: "main".into(),
            entry: 0,
            arity: 0,
            num_registers: 1,
        });
        chunk.code.push(Instruction::abc(Opcode::Return, 0, 0, 0));
        let mut bytes = encode(&chunk);
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap_or([0; 4])),
            ABI_VERSION_RECORDS
        );
        let back = decode_with(&bytes, TrustLevel::Trusted)?;
        match back.constants.first() {
            Some(Value::Record(fields)) if fields.len() == 2 => {}
            _ => return Err(FormatError::Truncated),
        }
        bytes[4..8].copy_from_slice(&ABI_VERSION_MAPS.to_le_bytes());
        match decode_with(&bytes, TrustLevel::Trusted) {
            Err(FormatError::UnknownValueTag(11)) => Ok(()),
            Err(err) => Err(err),
            Ok(_) => Err(FormatError::UnknownValueTag(11)),
        }
    }

    #[test]
    fn record_nested_in_a_list_or_holding_a_map_selects_abi_v8() -> Result<(), FormatError> {
        let record = Value::try_record(vec![Value::Int(1)]).ok_or(FormatError::ValueTooNested)?;
        let list = Value::try_list(vec![record]).ok_or(FormatError::ValueTooNested)?;
        abi_v8_rejects_a_v7_header(list)?;

        let map = Value::try_map(vec![(
            crate::bytecode::value::MapKey::Int(1),
            Value::Int(2),
        )])
        .ok_or(FormatError::ValueTooNested)?;
        let holding = Value::try_record(vec![map]).ok_or(FormatError::ValueTooNested)?;
        abi_v8_rejects_a_v7_header(holding)
    }

    fn abi_v8_rejects_a_v7_header(constant: Value) -> Result<(), FormatError> {
        let mut chunk = Chunk {
            name: "nested".into(),
            ..Chunk::default()
        };
        chunk.constants.push(constant);
        chunk.functions.push(FunctionDef {
            name: "main".into(),
            entry: 0,
            arity: 0,
            num_registers: 1,
        });
        chunk.code.push(Instruction::abc(Opcode::Return, 0, 0, 0));
        let mut bytes = encode(&chunk);
        if u32::from_le_bytes(bytes[4..8].try_into().unwrap_or([0; 4])) != ABI_VERSION_RECORDS {
            return Err(FormatError::UnsupportedAbi(0));
        }
        bytes[4..8].copy_from_slice(&ABI_VERSION_MAPS.to_le_bytes());
        match decode_with(&bytes, TrustLevel::Trusted) {
            Err(FormatError::UnknownValueTag(11)) => Ok(()),
            Err(err) => Err(err),
            Ok(_) => Err(FormatError::UnknownValueTag(11)),
        }
    }
}
