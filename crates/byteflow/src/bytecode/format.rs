use super::chunk::{Chunk, FunctionDef, ABI_VERSION, MAGIC};
use super::instruction::Instruction;
use super::opcode::Opcode;
use super::value::Value;
use super::verify::TrustLevel;

const MAX_NAME: u32 = 64 * 1024;
const MAX_ITEMS: u32 = 1_000_000;
/// Max length for [`Value::Str`] / [`Value::Bytes`] constant payloads.
const MAX_BLOB: u32 = 1_048_576;
/// Nested `Message.payload` depth (defense against hostile `.bf` files).
const MAX_VALUE_DEPTH: u32 = 16;

/// Why a `.bf` buffer failed to decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    BadMagic { found: [u8; 4] },
    UnsupportedAbi(u32),
    Truncated,
    LimitExceeded { what: &'static str, got: u32 },
    BadUtf8,
    UnknownValueTag(u8),
    UnknownOpcode { at: usize, byte: u8 },
    ForbiddenConstant(super::verify::ConstantKind),
    ValueTooNested,
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
        }
    }
}

impl std::error::Error for FormatError {}

/// Encode `chunk` as a BFV0 module (little-endian). Pure data — no I/O.
pub fn encode(chunk: &Chunk) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&ABI_VERSION.to_le_bytes());
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
    if abi != ABI_VERSION {
        return Err(FormatError::UnsupportedAbi(abi));
    }
    let name = r.read_string()?;
    let n_const = r.read_count("constants")?;
    let mut constants = Vec::with_capacity(n_const as usize);
    for _ in 0..n_const {
        constants.push(r.read_value(trust, 0)?);
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
    }
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

    fn read_value(&mut self, trust: TrustLevel, depth: u32) -> Result<Value, FormatError> {
        if depth > MAX_VALUE_DEPTH {
            return Err(FormatError::ValueTooNested);
        }
        match self.read_u8()? {
            0 => Ok(Value::Unit),
            1 => Ok(Value::Bool(self.read_u8()? != 0)),
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
                let payload = self.read_value(trust, depth + 1)?;
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
}
