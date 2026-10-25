//! Text listing that round-trips through [`super::disassemble`].
//!
//! This is not a language. Each line is a directive or one [`Instruction`]
//! in the same shape [`Instruction`]'s `Display` already prints.

use super::chunk::{Chunk, FunctionDef};
use super::format::module_abi;
use super::instruction::Instruction;
use super::opcode::Opcode;
use super::value::{MapKey, Value};
use super::verify::{verify, VerifyError};

/// Why a text listing could not become a verified [`Chunk`].
#[derive(Debug)]
pub enum AssembleError {
    Syntax {
        line: usize,
        message: String,
    },
    /// The `.abi` line does not match the ABI [`super::encode`] would write.
    Abi {
        declared: u32,
        encoded: u32,
    },
    Verify(VerifyError),
}

impl std::fmt::Display for AssembleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AssembleError::Syntax { line, message } => {
                write!(f, "assemble line {line}: {message}")
            }
            AssembleError::Abi { declared, encoded } => {
                write!(
                    f,
                    "listing abi {declared} does not match encoded abi {encoded}"
                )
            }
            AssembleError::Verify(err) => write!(f, "assemble verify: {err}"),
        }
    }
}

impl std::error::Error for AssembleError {}

impl From<VerifyError> for AssembleError {
    fn from(err: VerifyError) -> Self {
        AssembleError::Verify(err)
    }
}

/// Parse a listing produced by [`super::disassemble`] and verify it.
pub fn assemble(text: &str) -> Result<Chunk, AssembleError> {
    let mut name = None;
    let mut declared_abi = None;
    let mut constants = Vec::new();
    let mut functions = Vec::new();
    let mut code = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let line_no = idx + 1;
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix(".chunk ") {
            name = Some(parse_quoted(rest, line_no)?);
        } else if let Some(rest) = line.strip_prefix(".abi ") {
            declared_abi = Some(parse_u32(rest.trim(), line_no, "abi")?);
        } else if let Some(rest) = line.strip_prefix(".const ") {
            constants.push(parse_value(rest.trim(), line_no)?);
        } else if let Some(rest) = line.strip_prefix(".fn ") {
            functions.push(parse_fn(rest.trim(), line_no)?);
        } else {
            let body = match split_index(line) {
                Some((index, body)) => {
                    if index != code.len() {
                        return Err(syn(line_no, format!("instruction index {index}")));
                    }
                    body
                }
                None => line,
            };
            code.push(parse_instruction(body, line_no)?);
        }
    }
    let name = name.ok_or_else(|| syn(1, "missing .chunk"))?;
    let declared = declared_abi.ok_or_else(|| syn(1, "missing .abi"))?;
    let encoded = module_abi(&constants);
    if declared != encoded {
        return Err(AssembleError::Abi { declared, encoded });
    }
    let chunk = Chunk {
        name,
        constants,
        code,
        functions,
    };
    verify(&chunk)?;
    Ok(chunk)
}

fn syn(line: usize, message: impl Into<String>) -> AssembleError {
    AssembleError::Syntax {
        line,
        message: message.into(),
    }
}

fn strip_comment(line: &str) -> &str {
    let mut quote = false;
    let mut escape = false;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if escape {
            escape = false;
            i += 1;
            continue;
        }
        if c == '\\' && quote {
            escape = true;
            i += 1;
            continue;
        }
        if c == '"' {
            quote = !quote;
        }
        if !quote && c == ';' {
            return &line[..i];
        }
        i += 1;
    }
    line
}

fn split_index(line: &str) -> Option<(usize, &str)> {
    let bytes = line.as_bytes();
    if bytes.len() < 6 || !bytes[..4].iter().all(u8::is_ascii_digit) {
        return None;
    }
    if bytes[4] != b' ' {
        return None;
    }
    let index = line[..4].parse().ok()?;
    Some((index, line[4..].trim()))
}

fn parse_u32(text: &str, line: usize, what: &str) -> Result<u32, AssembleError> {
    text.parse().map_err(|_| syn(line, format!("bad {what}")))
}

fn parse_quoted(text: &str, line: usize) -> Result<String, AssembleError> {
    let text = text.trim();
    if !text.starts_with('"') {
        return Err(syn(line, "expected quoted string"));
    }
    let mut out = String::new();
    let mut chars = text[1..].chars();
    loop {
        match chars.next() {
            Some('\\') => match chars.next() {
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some('n') => out.push('\n'),
                _ => return Err(syn(line, "bad escape")),
            },
            Some('"') => {
                if chars.next().is_some() {
                    return Err(syn(line, "trailing junk after string"));
                }
                return Ok(out);
            }
            Some(c) => out.push(c),
            None => return Err(syn(line, "unterminated string")),
        }
    }
}

fn parse_fn(text: &str, line: usize) -> Result<FunctionDef, AssembleError> {
    let mut parts = text.split_whitespace();
    let name = parts
        .next()
        .ok_or_else(|| syn(line, "function name"))?
        .to_string();
    let entry = field(parts.next(), "entry=", line)?;
    let arity = field(parts.next(), "arity=", line)?;
    let regs = field(parts.next(), "regs=", line)?;
    if parts.next().is_some() {
        return Err(syn(line, "extra function field"));
    }
    let arity = u8::try_from(arity).map_err(|_| syn(line, "arity"))?;
    let num_registers = u8::try_from(regs).map_err(|_| syn(line, "regs"))?;
    Ok(FunctionDef {
        name,
        entry,
        arity,
        num_registers,
    })
}

fn field(token: Option<&str>, prefix: &str, line: usize) -> Result<u32, AssembleError> {
    let token = token.ok_or_else(|| syn(line, format!("missing {prefix}")))?;
    let rest = token
        .strip_prefix(prefix)
        .ok_or_else(|| syn(line, format!("expected {prefix}")))?;
    parse_u32(rest, line, prefix)
}

pub(crate) fn write_constant(out: &mut String, value: &Value) {
    use std::fmt::Write as _;
    let _ = write!(out, ".const ");
    write_value(out, value);
    let _ = writeln!(out);
}

fn write_value(out: &mut String, value: &Value) {
    use std::fmt::Write as _;
    match value {
        Value::Unit => out.push_str("unit"),
        Value::Bool(true) => out.push_str("bool true"),
        Value::Bool(false) => out.push_str("bool false"),
        Value::Int(n) => {
            let _ = write!(out, "int {n}");
        }
        Value::Float(n) => {
            let _ = write!(out, "float {n}");
        }
        Value::Pid(n) => {
            let _ = write!(out, "pid {n}");
        }
        Value::Cap(cap) => {
            let _ = write!(out, "cap {}", cap.as_u128());
        }
        Value::Str(text) => {
            out.push_str("str ");
            push_quoted(out, text);
        }
        Value::Bytes(bytes) => {
            out.push_str("bytes ");
            for byte in bytes.iter() {
                let _ = write!(out, "{byte:02x}");
            }
        }
        Value::List(items) => {
            out.push_str("list [");
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Record(fields) => {
            out.push_str("record [");
            for (index, field) in fields.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_value(out, field);
            }
            out.push(']');
        }
        Value::Map(entries) => {
            out.push_str("map [");
            for (index, (key, value)) in entries.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                match key {
                    MapKey::Int(n) => {
                        let _ = write!(out, "int {n}");
                    }
                    MapKey::Str(text) => {
                        out.push_str("str ");
                        push_quoted(out, text);
                    }
                }
                out.push_str(" -> ");
                write_value(out, value);
            }
            out.push(']');
        }
        Value::Message(message) => {
            let _ = write!(
                out,
                "msg {} {} {} {} ",
                message.sender,
                message.reply_cap.as_u128(),
                message.request_id,
                message.tag
            );
            write_value(out, message.payload.as_ref());
        }
    }
}

fn push_quoted(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn parse_value(text: &str, line: usize) -> Result<Value, AssembleError> {
    let (value, rest) = take_value(text, line)?;
    if !rest.trim().is_empty() {
        return Err(syn(line, "trailing junk after constant"));
    }
    Ok(value)
}

fn take_value(text: &str, line: usize) -> Result<(Value, &str), AssembleError> {
    let text = text.trim_start();
    if let Some(rest) = text.strip_prefix("unit") {
        return Ok((Value::Unit, rest));
    }
    if let Some(rest) = text.strip_prefix("bool ") {
        let rest = rest.trim_start();
        if let Some(rest) = rest.strip_prefix("true") {
            return Ok((Value::Bool(true), rest));
        }
        if let Some(rest) = rest.strip_prefix("false") {
            return Ok((Value::Bool(false), rest));
        }
        return Err(syn(line, "bad bool"));
    }
    if let Some(rest) = text.strip_prefix("int ") {
        let (token, rest) = token_span(rest.trim_start());
        let n = token.parse::<i64>().map_err(|_| syn(line, "bad int"))?;
        return Ok((Value::Int(n), rest));
    }
    if let Some(rest) = text.strip_prefix("float ") {
        let (token, rest) = token_span(rest.trim_start());
        let n = token.parse::<f64>().map_err(|_| syn(line, "bad float"))?;
        return Ok((Value::Float(n), rest));
    }
    if let Some(rest) = text.strip_prefix("pid ") {
        let (token, rest) = token_span(rest.trim_start());
        let n = token.parse::<u64>().map_err(|_| syn(line, "bad pid"))?;
        return Ok((Value::Pid(n), rest));
    }
    if let Some(rest) = text.strip_prefix("cap ") {
        let (token, rest) = token_span(rest.trim_start());
        let n = token.parse::<u128>().map_err(|_| syn(line, "bad cap"))?;
        return Ok((Value::Cap(super::cap::CapId::from_raw(n)), rest));
    }
    if let Some(rest) = text.strip_prefix("str ") {
        let rest = rest.trim_start();
        if !rest.starts_with('"') {
            return Err(syn(line, "bad str"));
        }
        let mut owned = String::new();
        let mut chars = rest[1..].chars();
        let mut consumed = 1usize;
        loop {
            match chars.next() {
                Some('\\') => {
                    consumed += 1;
                    match chars.next() {
                        Some('\\') => owned.push('\\'),
                        Some('"') => owned.push('"'),
                        Some('n') => owned.push('\n'),
                        _ => return Err(syn(line, "bad escape")),
                    }
                    consumed += 1;
                }
                Some('"') => {
                    consumed += 1;
                    let after = rest.get(consumed..).unwrap_or("");
                    return Ok((Value::str(owned), after));
                }
                Some(c) => {
                    owned.push(c);
                    consumed += c.len_utf8();
                }
                None => return Err(syn(line, "unterminated str")),
            }
        }
    }
    if let Some(rest) = text.strip_prefix("bytes ") {
        let (token, rest) = token_span(rest.trim_start());
        if token.len() % 2 != 0 {
            return Err(syn(line, "bad bytes"));
        }
        let mut bytes = Vec::with_capacity(token.len() / 2);
        let digits = token.as_bytes();
        let mut i = 0;
        while i < digits.len() {
            let hi = hex_val(digits[i]).ok_or_else(|| syn(line, "bad bytes"))?;
            let lo = hex_val(digits[i + 1]).ok_or_else(|| syn(line, "bad bytes"))?;
            bytes.push((hi << 4) | lo);
            i += 2;
        }
        return Ok((Value::bytes(bytes), rest));
    }
    if let Some(rest) = text.strip_prefix("list [") {
        return take_list(rest, line);
    }
    if let Some(rest) = text.strip_prefix("map [") {
        return take_map(rest, line);
    }
    if let Some(rest) = text.strip_prefix("record [") {
        let (value, rest) = take_list(rest, line)?;
        let Value::List(items) = value else {
            return Err(syn(line, "record"));
        };
        let record =
            Value::try_record(items.to_vec()).ok_or_else(|| syn(line, "record rejected"))?;
        return Ok((record, rest));
    }
    if let Some(rest) = text.strip_prefix("msg ") {
        return take_msg(rest, line);
    }
    Err(syn(line, "unknown constant"))
}

fn token_span(text: &str) -> (&str, &str) {
    let end = text
        .find(|c: char| c.is_whitespace() || c == ',' || c == ']')
        .unwrap_or(text.len());
    // A negative number's '-' is part of the token. Only split on " -> ".
    if text[..end].is_empty() {
        return ("", text);
    }
    (&text[..end], &text[end..])
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn take_list(mut text: &str, line: usize) -> Result<(Value, &str), AssembleError> {
    let mut items = Vec::new();
    text = text.trim_start();
    if let Some(rest) = text.strip_prefix(']') {
        let list = Value::try_list(items).ok_or_else(|| syn(line, "list rejected"))?;
        return Ok((list, rest));
    }
    loop {
        let (item, rest) = take_value(text, line)?;
        items.push(item);
        text = rest.trim_start();
        if let Some(rest) = text.strip_prefix(']') {
            let list = Value::try_list(items).ok_or_else(|| syn(line, "list rejected"))?;
            return Ok((list, rest));
        }
        text = text
            .strip_prefix(',')
            .ok_or_else(|| syn(line, "list separator"))?
            .trim_start();
    }
}

fn take_map(mut text: &str, line: usize) -> Result<(Value, &str), AssembleError> {
    let mut entries = Vec::new();
    text = text.trim_start();
    if let Some(rest) = text.strip_prefix(']') {
        let map = Value::try_map(entries).ok_or_else(|| syn(line, "map rejected"))?;
        return Ok((map, rest));
    }
    loop {
        let (key_value, rest) = take_value(text, line)?;
        let key = match key_value {
            Value::Int(n) => MapKey::Int(n),
            Value::Str(text) => MapKey::Str(text),
            _ => return Err(syn(line, "map key")),
        };
        let rest = rest.trim_start();
        let rest = rest
            .strip_prefix("->")
            .ok_or_else(|| syn(line, "map arrow"))?
            .trim_start();
        let (value, rest) = take_value(rest, line)?;
        entries.push((key, value));
        text = rest.trim_start();
        if let Some(rest) = text.strip_prefix(']') {
            let map = Value::try_map(entries).ok_or_else(|| syn(line, "map rejected"))?;
            return Ok((map, rest));
        }
        text = text
            .strip_prefix(',')
            .ok_or_else(|| syn(line, "map separator"))?
            .trim_start();
    }
}

fn take_msg(text: &str, line: usize) -> Result<(Value, &str), AssembleError> {
    let mut parts = text.splitn(5, char::is_whitespace);
    let sender = parts
        .next()
        .ok_or_else(|| syn(line, "msg sender"))?
        .parse::<u64>()
        .map_err(|_| syn(line, "msg sender"))?;
    let reply = parts
        .next()
        .ok_or_else(|| syn(line, "msg reply"))?
        .parse::<u128>()
        .map_err(|_| syn(line, "msg reply"))?;
    let request = parts
        .next()
        .ok_or_else(|| syn(line, "msg request"))?
        .parse::<u64>()
        .map_err(|_| syn(line, "msg request"))?;
    let tag = parts
        .next()
        .ok_or_else(|| syn(line, "msg tag"))?
        .parse::<u16>()
        .map_err(|_| syn(line, "msg tag"))?;
    let payload_text = parts.next().unwrap_or("");
    let (payload, rest) = take_value(payload_text, line)?;
    let message = super::value::Message::new(sender, request, tag, payload)
        .authenticate(sender, super::cap::CapId::from_raw(reply));
    Ok((Value::Message(message), rest))
}

struct Scan<'a> {
    line: usize,
    rest: &'a str,
}

impl<'a> Scan<'a> {
    fn new(line: usize, rest: &'a str) -> Self {
        Self { line, rest }
    }

    fn skip(&mut self) {
        self.rest = self.rest.trim_start();
    }

    fn eat(&mut self, lit: &str) -> Result<(), AssembleError> {
        self.skip();
        if let Some(rest) = self.rest.strip_prefix(lit) {
            self.rest = rest;
            Ok(())
        } else {
            Err(syn(self.line, format!("expected {lit}")))
        }
    }

    fn end(&mut self) -> Result<(), AssembleError> {
        self.skip();
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(syn(self.line, "trailing junk"))
        }
    }

    fn reg(&mut self) -> Result<u8, AssembleError> {
        self.eat("r")?;
        let (token, rest) = take_num(self.rest);
        self.rest = rest;
        let n = token
            .parse::<u16>()
            .map_err(|_| syn(self.line, "register"))?;
        u8::try_from(n).map_err(|_| syn(self.line, "register"))
    }

    fn i32(&mut self) -> Result<i32, AssembleError> {
        self.skip();
        if let Some(rest) = self.rest.strip_prefix('+') {
            self.rest = rest;
        }
        let (token, rest) = take_num(self.rest);
        if token.is_empty() {
            return Err(syn(self.line, "number"));
        }
        self.rest = rest;
        token.parse().map_err(|_| syn(self.line, "number"))
    }

    fn hex(&mut self) -> Result<u32, AssembleError> {
        self.eat("0x")?;
        let (token, rest) = take_hex(self.rest);
        self.rest = rest;
        if token.is_empty() {
            return Err(syn(self.line, "hex"));
        }
        u32::from_str_radix(token, 16).map_err(|_| syn(self.line, "hex"))
    }

    fn slot(&mut self, kind: &str) -> Result<i32, AssembleError> {
        self.eat(kind)?;
        self.eat("[")?;
        let n = self.i32()?;
        self.eat("]")?;
        Ok(n)
    }

    fn comma(&mut self) -> Result<(), AssembleError> {
        self.eat(",")?;
        Ok(())
    }
}

fn take_num(text: &str) -> (&str, &str) {
    let mut end = 0;
    let bytes = text.as_bytes();
    if bytes.first() == Some(&b'-') {
        end = 1;
    }
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    (&text[..end], &text[end..])
}

fn take_hex(text: &str) -> (&str, &str) {
    let end = text
        .find(|c: char| !c.is_ascii_hexdigit())
        .unwrap_or(text.len());
    (&text[..end], &text[end..])
}

fn parse_instruction(text: &str, line: usize) -> Result<Instruction, AssembleError> {
    let text = text.trim();
    let (name, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let op = opcode_from_name(name).ok_or_else(|| syn(line, format!("unknown opcode {name}")))?;
    let mut s = Scan::new(line, rest);
    let instr = match op {
        Opcode::Halt | Opcode::Yield | Opcode::Nop => Instruction::nullary(op),
        Opcode::LoadConst => {
            let a = s.reg()?;
            s.comma()?;
            let imm = s.slot("const")?;
            Instruction::a_imm(op, a, imm)
        }
        Opcode::Move
        | Opcode::Neg
        | Opcode::BitNot
        | Opcode::Send
        | Opcode::ReceiveTimeout
        | Opcode::ReceiveMatch
        | Opcode::IsAlive
        | Opcode::ExitSignal
        | Opcode::TableOpen
        | Opcode::Whereis
        | Opcode::Monitor
        | Opcode::Link => {
            let a = s.reg()?;
            s.comma()?;
            let b = s.reg()?;
            Instruction::abc(op, a, b, 0)
        }
        Opcode::LoadImm | Opcode::Trap | Opcode::SetRestartPolicy => {
            if op == Opcode::LoadImm {
                let a = s.reg()?;
                s.comma()?;
                let imm = s.i32()?;
                Instruction::a_imm(op, a, imm)
            } else if op == Opcode::Trap || op == Opcode::SetRestartPolicy {
                let imm = s.i32()?;
                Instruction::only_imm(op, imm)
            } else {
                Instruction::nullary(op)
            }
        }
        Opcode::Add
        | Opcode::Sub
        | Opcode::Mul
        | Opcode::Div
        | Opcode::Mod
        | Opcode::Eq
        | Opcode::Lt
        | Opcode::Le
        | Opcode::BitAnd
        | Opcode::BitOr
        | Opcode::BitXor
        | Opcode::Shl
        | Opcode::Shr
        | Opcode::MapGet
        | Opcode::TableGet
        | Opcode::TablePut
        | Opcode::Ask => {
            let a = s.reg()?;
            s.comma()?;
            let b = s.reg()?;
            s.comma()?;
            let c = s.reg()?;
            Instruction::abc(op, a, b, c)
        }
        Opcode::AddImm
        | Opcode::SubImm
        | Opcode::MulImm
        | Opcode::EqImm
        | Opcode::LtImm
        | Opcode::LeImm
        | Opcode::ListGet
        | Opcode::RecordGet => {
            let a = s.reg()?;
            s.comma()?;
            let b = s.reg()?;
            s.comma()?;
            let imm = s.i32()?;
            Instruction::new(op, a, b, 0, imm)
        }
        Opcode::ListPack | Opcode::RecordPack => {
            let a = s.reg()?;
            s.comma()?;
            let b = s.reg()?;
            s.comma()?;
            s.eat("count=")?;
            let imm = s.i32()?;
            Instruction::new(op, a, b, 0, imm)
        }
        Opcode::MapPack => {
            let a = s.reg()?;
            s.comma()?;
            let b = s.reg()?;
            s.comma()?;
            s.eat("pairs=")?;
            let imm = s.i32()?;
            Instruction::new(op, a, b, 0, imm)
        }
        Opcode::ListLen
        | Opcode::MapLen
        | Opcode::RecordLen
        | Opcode::DictGet
        | Opcode::DictErase => {
            if op == Opcode::DictGet || op == Opcode::DictErase {
                let a = s.reg()?;
                s.comma()?;
                s.eat("key=")?;
                let b = s.reg()?;
                Instruction::abc(op, a, b, 0)
            } else {
                let a = s.reg()?;
                s.comma()?;
                let b = s.reg()?;
                Instruction::abc(op, a, b, 0)
            }
        }
        Opcode::Jump => Instruction::only_imm(op, s.i32()?),
        Opcode::Branch | Opcode::StartSupervisor => {
            if op == Opcode::Branch {
                let a = s.reg()?;
                s.comma()?;
                Instruction::a_imm(op, a, s.i32()?)
            } else {
                s.eat("strategy=")?;
                Instruction::only_imm(op, s.i32()?)
            }
        }
        Opcode::Call => {
            let a = s.reg()?;
            s.comma()?;
            let imm = s.slot("fn")?;
            s.comma()?;
            s.eat("argc=")?;
            let b = s.i32()?;
            let b = u8::try_from(b).map_err(|_| syn(line, "argc"))?;
            Instruction::new(op, a, b, 0, imm)
        }
        Opcode::CallNative => {
            let a = s.reg()?;
            s.comma()?;
            let imm = s.slot("native")?;
            s.comma()?;
            s.eat("argc=")?;
            let b = u8::try_from(s.i32()?).map_err(|_| syn(line, "argc"))?;
            Instruction::new(op, a, b, 0, imm)
        }
        Opcode::CallNativeFrom => {
            let a = s.reg()?;
            s.comma()?;
            let b = s.reg()?;
            s.comma()?;
            let imm = s.slot("native")?;
            Instruction::new(op, a, b, 0, imm)
        }
        Opcode::Return
        | Opcode::Exit
        | Opcode::Sleep
        | Opcode::Receive
        | Opcode::SelfPid
        | Opcode::FreshRequestId
        | Opcode::Demonitor
        | Opcode::Unlink
        | Opcode::RegisterName
        | Opcode::SetTrapExit => {
            let a = s.reg()?;
            Instruction::a_imm(op, a, 0)
        }
        Opcode::Spawn => {
            let a = s.reg()?;
            s.comma()?;
            let imm = s.slot("fn")?;
            s.comma()?;
            s.eat("argc=")?;
            let b = u8::try_from(s.i32()?).map_err(|_| syn(line, "argc"))?;
            s.comma()?;
            s.eat("rights=")?;
            let c = s.hex()?;
            let c = u8::try_from(c).map_err(|_| syn(line, "rights"))?;
            Instruction::new(op, a, b, c, imm)
        }
        Opcode::ReceiveMatchImm | Opcode::ReceiveMatchKind => {
            let a = s.reg()?;
            s.comma()?;
            if op == Opcode::ReceiveMatchImm {
                s.eat("tag=")?;
            } else {
                s.eat("kind=")?;
            }
            Instruction::a_imm(op, a, s.i32()?)
        }
        Opcode::ReceiveMatchTimeout
        | Opcode::ReceiveMatchEq
        | Opcode::ReceiveMatchValue
        | Opcode::ReceiveMatchCorr
        | Opcode::DictPut => {
            let a = s.reg()?;
            s.comma()?;
            let (b_lit, c_lit) = match op {
                Opcode::ReceiveMatchTimeout => ("tag=", "timeout="),
                Opcode::ReceiveMatchEq | Opcode::ReceiveMatchValue => ("tag=", "payload="),
                Opcode::ReceiveMatchCorr => ("tag=", "id="),
                Opcode::DictPut => ("key=", "val="),
                _ => ("", ""),
            };
            s.eat(b_lit)?;
            let b = s.reg()?;
            s.comma()?;
            s.eat(c_lit)?;
            let c = s.reg()?;
            Instruction::abc(op, a, b, c)
        }
        Opcode::ReceiveMatchImmTimeout | Opcode::ReceiveMatchCorrImm => {
            let a = s.reg()?;
            s.comma()?;
            s.eat("tag=")?;
            let imm = s.i32()?;
            s.comma()?;
            if op == Opcode::ReceiveMatchImmTimeout {
                s.eat("timeout=")?;
            } else {
                s.eat("id=")?;
            }
            let b = s.reg()?;
            Instruction::new(op, a, b, 0, imm)
        }
        Opcode::StartChild => {
            let a = s.reg()?;
            s.comma()?;
            let imm = s.slot("fn")?;
            s.comma()?;
            s.eat("policy=")?;
            let b = u8::try_from(s.i32()?).map_err(|_| syn(line, "policy"))?;
            Instruction::new(op, a, b, 0, imm)
        }
        Opcode::SendRemote => {
            s.eat("node=")?;
            let a = s.reg()?;
            s.comma()?;
            s.eat("name=")?;
            let b = s.reg()?;
            s.comma()?;
            s.eat("msg=")?;
            let c = s.reg()?;
            Instruction::abc(op, a, b, c)
        }
        Opcode::AskTimeout => {
            let a = s.reg()?;
            s.comma()?;
            let b = s.reg()?;
            s.comma()?;
            let c = s.reg()?;
            s.comma()?;
            s.eat("timeout=")?;
            let imm = s.reg()?;
            Instruction::new(op, a, b, c, i32::from(imm))
        }
        Opcode::Delegate => {
            let a = s.reg()?;
            s.comma()?;
            let b = s.reg()?;
            s.comma()?;
            s.eat("rights=")?;
            let rights = s.hex()?;
            s.skip();
            if s.rest.is_empty() {
                Instruction::new(op, a, b, 255, rights as i32)
            } else {
                s.eat(",")?;
                s.eat("native=")?;
                let c = s.reg()?;
                Instruction::new(op, a, b, c, rights as i32)
            }
        }
        Opcode::HostAwait => {
            let a = s.reg()?;
            s.comma()?;
            let b = s.reg()?;
            s.comma()?;
            s.eat("op=")?;
            let imm = s.i32()?;
            Instruction::new(op, a, b, 0, imm)
        }
        Opcode::ReceiveMatchCmp => {
            let a = s.reg()?;
            s.comma()?;
            s.eat("tag=")?;
            let b = s.reg()?;
            s.comma()?;
            s.eat("bound=")?;
            let c = s.reg()?;
            s.comma()?;
            s.eat("cmp=")?;
            let imm = s.i32()?;
            Instruction::new(op, a, b, c, imm)
        }
    };
    s.end()?;
    Ok(instr)
}

fn opcode_from_name(name: &str) -> Option<Opcode> {
    Some(match name {
        "Halt" => Opcode::Halt,
        "LoadConst" => Opcode::LoadConst,
        "Move" => Opcode::Move,
        "LoadImm" => Opcode::LoadImm,
        "Add" => Opcode::Add,
        "Sub" => Opcode::Sub,
        "Mul" => Opcode::Mul,
        "Div" => Opcode::Div,
        "Mod" => Opcode::Mod,
        "Neg" => Opcode::Neg,
        "Eq" => Opcode::Eq,
        "Lt" => Opcode::Lt,
        "Le" => Opcode::Le,
        "Jump" => Opcode::Jump,
        "Branch" => Opcode::Branch,
        "Call" => Opcode::Call,
        "Return" => Opcode::Return,
        "CallNative" => Opcode::CallNative,
        "Spawn" => Opcode::Spawn,
        "Yield" => Opcode::Yield,
        "Sleep" => Opcode::Sleep,
        "Exit" => Opcode::Exit,
        "SelfPid" => Opcode::SelfPid,
        "Send" => Opcode::Send,
        "Receive" => Opcode::Receive,
        "ReceiveTimeout" => Opcode::ReceiveTimeout,
        "ReceiveMatch" => Opcode::ReceiveMatch,
        "ReceiveMatchImm" => Opcode::ReceiveMatchImm,
        "Ask" => Opcode::Ask,
        "Monitor" => Opcode::Monitor,
        "Demonitor" => Opcode::Demonitor,
        "Link" => Opcode::Link,
        "Unlink" => Opcode::Unlink,
        "Trap" => Opcode::Trap,
        "Nop" => Opcode::Nop,
        "CallNativeFrom" => Opcode::CallNativeFrom,
        "AddImm" => Opcode::AddImm,
        "SubImm" => Opcode::SubImm,
        "MulImm" => Opcode::MulImm,
        "EqImm" => Opcode::EqImm,
        "LtImm" => Opcode::LtImm,
        "LeImm" => Opcode::LeImm,
        "ListPack" => Opcode::ListPack,
        "ListGet" => Opcode::ListGet,
        "ListLen" => Opcode::ListLen,
        "BitAnd" => Opcode::BitAnd,
        "BitOr" => Opcode::BitOr,
        "BitXor" => Opcode::BitXor,
        "BitNot" => Opcode::BitNot,
        "Shl" => Opcode::Shl,
        "Shr" => Opcode::Shr,
        "ReceiveMatchCmp" => Opcode::ReceiveMatchCmp,
        "MapPack" => Opcode::MapPack,
        "MapGet" => Opcode::MapGet,
        "MapLen" => Opcode::MapLen,
        "TableOpen" => Opcode::TableOpen,
        "TableGet" => Opcode::TableGet,
        "TablePut" => Opcode::TablePut,
        "RecordPack" => Opcode::RecordPack,
        "RecordGet" => Opcode::RecordGet,
        "RecordLen" => Opcode::RecordLen,
        "AskTimeout" => Opcode::AskTimeout,
        "Delegate" => Opcode::Delegate,
        "FreshRequestId" => Opcode::FreshRequestId,
        "ReceiveMatchCorr" => Opcode::ReceiveMatchCorr,
        "ReceiveMatchCorrImm" => Opcode::ReceiveMatchCorrImm,
        "SetTrapExit" => Opcode::SetTrapExit,
        "RegisterName" => Opcode::RegisterName,
        "Whereis" => Opcode::Whereis,
        "ReceiveMatchKind" => Opcode::ReceiveMatchKind,
        "SetRestartPolicy" => Opcode::SetRestartPolicy,
        "HostAwait" => Opcode::HostAwait,
        "ReceiveMatchTimeout" => Opcode::ReceiveMatchTimeout,
        "ReceiveMatchImmTimeout" => Opcode::ReceiveMatchImmTimeout,
        "IsAlive" => Opcode::IsAlive,
        "ExitSignal" => Opcode::ExitSignal,
        "DictPut" => Opcode::DictPut,
        "DictGet" => Opcode::DictGet,
        "DictErase" => Opcode::DictErase,
        "ReceiveMatchEq" => Opcode::ReceiveMatchEq,
        "StartChild" => Opcode::StartChild,
        "ReceiveMatchValue" => Opcode::ReceiveMatchValue,
        "SendRemote" => Opcode::SendRemote,
        "StartSupervisor" => Opcode::StartSupervisor,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::builder::ChunkBuilder;
    use crate::bytecode::{decode, disassemble, encode};

    fn round_trip(chunk: Chunk) -> Result<(), AssembleError> {
        let text = disassemble(&chunk);
        let parsed = assemble(&text)?;
        let again = decode(&encode(&parsed)).map_err(|err| AssembleError::Syntax {
            line: 0,
            message: err.to_string(),
        })?;
        if again != chunk {
            return Err(AssembleError::Syntax {
                line: 0,
                message: format!("round trip mismatch\n{text}"),
            });
        }
        Ok(())
    }

    #[test]
    fn integers_round_trip_as_abi_v5() -> Result<(), AssembleError> {
        let mut b = ChunkBuilder::new("ints");
        b.begin_function("main", 0, 2);
        b.emit_load_imm(0, 41);
        b.emit_load_imm(1, 1);
        b.emit_binop(Opcode::Add, 0, 0, 1);
        b.emit_return(0);
        let chunk = b.finish();
        let text = disassemble(&chunk);
        assert!(text.contains(".abi 5"));
        assert!(!text.contains(".abi 6"));
        round_trip(chunk)
    }

    #[test]
    fn list_round_trip_as_abi_v6() -> Result<(), AssembleError> {
        let list = Value::try_list(vec![Value::Int(1), Value::Int(2)]).ok_or_else(|| {
            AssembleError::Syntax {
                line: 0,
                message: "list".into(),
            }
        })?;
        let mut b = ChunkBuilder::new("lists");
        let index = b.const_(list);
        b.begin_function("main", 0, 1);
        b.emit_load_const(0, index);
        b.emit_return(0);
        let chunk = b.finish();
        let text = disassemble(&chunk);
        assert!(text.contains(".abi 6"), "{text}");
        assert!(
            !text.contains(".abi 5\n") && !text.contains(".abi 5\r"),
            "{text}"
        );
        round_trip(chunk)
    }

    #[test]
    fn map_round_trip_as_abi_v7() -> Result<(), AssembleError> {
        let map = Value::try_map(vec![
            (MapKey::int(1), Value::Int(7)),
            (MapKey::str("a"), Value::Int(2)),
        ])
        .ok_or_else(|| AssembleError::Syntax {
            line: 0,
            message: "map".into(),
        })?;
        let mut b = ChunkBuilder::new("maps");
        let index = b.const_(map);
        b.begin_function("main", 0, 1);
        b.emit_load_const(0, index);
        b.emit_return(0);
        let chunk = b.finish();
        let text = disassemble(&chunk);
        assert!(text.contains(".abi 7"), "{text}");
        round_trip(chunk)
    }

    #[test]
    fn record_round_trip_as_abi_v8() -> Result<(), AssembleError> {
        let record = Value::try_record(vec![Value::Int(4), Value::Int(5)]).ok_or_else(|| {
            AssembleError::Syntax {
                line: 0,
                message: "record".into(),
            }
        })?;
        let mut b = ChunkBuilder::new("recs");
        let index = b.const_(record);
        b.begin_function("main", 0, 1);
        b.emit_load_const(0, index);
        b.emit_return(0);
        let chunk = b.finish();
        let text = disassemble(&chunk);
        assert!(text.contains(".abi 8"), "{text}");
        assert!(text.contains("record ["), "{text}");
        round_trip(chunk)
    }

    #[test]
    fn bad_syntax_is_an_error() {
        let err = assemble(".chunk \"x\"\n.abi 5\n.fn main entry=0 arity=0 regs=1\n0000 Nope\n");
        assert!(matches!(err, Err(AssembleError::Syntax { .. })));
    }

    #[test]
    fn bad_operand_is_an_error() {
        let err = assemble(
            ".chunk \"x\"\n.abi 5\n.fn main entry=0 arity=0 regs=1\n0000 LoadImm r0, no\n",
        );
        assert!(matches!(err, Err(AssembleError::Syntax { .. })));
    }

    #[test]
    fn bad_constant_reference_fails_verify() {
        let err = assemble(
            ".chunk \"x\"\n.abi 5\n.fn main entry=0 arity=0 regs=1\n0000 LoadConst r0, const[3]\n",
        );
        assert!(matches!(
            err,
            Err(AssembleError::Verify(VerifyError::ConstOutOfRange { .. }))
        ));
    }

    #[test]
    fn bad_function_reference_fails_verify() {
        let text = "\
.chunk \"x\"
.abi 5
.fn main entry=0 arity=0 regs=1
0000 Call r0, fn[4], argc=0
";
        let err = assemble(text);
        assert!(matches!(
            err,
            Err(AssembleError::Verify(
                VerifyError::FunctionOutOfRange { .. }
            ))
        ));
    }

    #[test]
    fn declared_abi_must_match_the_constants() {
        let text = "\
.chunk \"x\"
.abi 5
.const list [int 1]
.fn main entry=0 arity=0 regs=1
0000 Return r0
";
        let err = assemble(text);
        assert!(matches!(
            err,
            Err(AssembleError::Abi {
                declared: 5,
                encoded: 6
            })
        ));
    }

    #[test]
    fn empty_input_does_not_panic() {
        assert!(assemble("").is_err());
        assert!(assemble("not a module").is_err());
    }

    #[test]
    fn every_opcode_listing_parses_back() -> Result<(), AssembleError> {
        let samples = [
            Instruction::nullary(Opcode::Halt),
            Instruction::a_imm(Opcode::LoadConst, 1, 2),
            Instruction::abc(Opcode::Move, 1, 2, 0),
            Instruction::a_imm(Opcode::LoadImm, 1, -3),
            Instruction::abc(Opcode::Add, 1, 2, 3),
            Instruction::abc(Opcode::Sub, 1, 2, 3),
            Instruction::abc(Opcode::Mul, 1, 2, 3),
            Instruction::abc(Opcode::Div, 1, 2, 3),
            Instruction::abc(Opcode::Mod, 1, 2, 3),
            Instruction::abc(Opcode::Neg, 1, 2, 0),
            Instruction::abc(Opcode::Eq, 1, 2, 3),
            Instruction::abc(Opcode::Lt, 1, 2, 3),
            Instruction::abc(Opcode::Le, 1, 2, 3),
            Instruction::only_imm(Opcode::Jump, -1),
            Instruction::a_imm(Opcode::Branch, 1, 4),
            Instruction::new(Opcode::Call, 1, 2, 0, 3),
            Instruction::a_imm(Opcode::Return, 1, 0),
            Instruction::new(Opcode::CallNative, 1, 2, 0, 3),
            Instruction::new(Opcode::Spawn, 1, 0, 0, 2),
            Instruction::nullary(Opcode::Yield),
            Instruction::a_imm(Opcode::Sleep, 1, 0),
            Instruction::a_imm(Opcode::Exit, 1, 0),
            Instruction::a_imm(Opcode::SelfPid, 1, 0),
            Instruction::abc(Opcode::Send, 1, 2, 0),
            Instruction::a_imm(Opcode::Receive, 1, 0),
            Instruction::abc(Opcode::ReceiveTimeout, 1, 2, 0),
            Instruction::abc(Opcode::ReceiveMatch, 1, 2, 0),
            Instruction::a_imm(Opcode::ReceiveMatchImm, 1, 7),
            Instruction::abc(Opcode::Ask, 1, 2, 3),
            Instruction::abc(Opcode::Monitor, 1, 2, 0),
            Instruction::a_imm(Opcode::Demonitor, 1, 0),
            Instruction::abc(Opcode::Link, 1, 2, 0),
            Instruction::a_imm(Opcode::Unlink, 1, 0),
            Instruction::only_imm(Opcode::Trap, 9),
            Instruction::nullary(Opcode::Nop),
            Instruction::new(Opcode::CallNativeFrom, 1, 2, 0, 3),
            Instruction::new(Opcode::AddImm, 1, 2, 0, -4),
            Instruction::new(Opcode::SubImm, 1, 2, 0, 4),
            Instruction::new(Opcode::MulImm, 1, 2, 0, 4),
            Instruction::new(Opcode::EqImm, 1, 2, 0, 4),
            Instruction::new(Opcode::LtImm, 1, 2, 0, 4),
            Instruction::new(Opcode::LeImm, 1, 2, 0, 4),
            Instruction::new(Opcode::ListPack, 1, 2, 0, 3),
            Instruction::new(Opcode::ListGet, 1, 2, 0, 3),
            Instruction::abc(Opcode::ListLen, 1, 2, 0),
            Instruction::abc(Opcode::BitAnd, 1, 2, 3),
            Instruction::abc(Opcode::BitOr, 1, 2, 3),
            Instruction::abc(Opcode::BitXor, 1, 2, 3),
            Instruction::abc(Opcode::BitNot, 1, 2, 0),
            Instruction::abc(Opcode::Shl, 1, 2, 3),
            Instruction::abc(Opcode::Shr, 1, 2, 3),
            Instruction::new(Opcode::ReceiveMatchCmp, 1, 2, 3, 0),
            Instruction::new(Opcode::MapPack, 1, 2, 0, 3),
            Instruction::abc(Opcode::MapGet, 1, 2, 3),
            Instruction::abc(Opcode::MapLen, 1, 2, 0),
            Instruction::abc(Opcode::TableOpen, 1, 2, 0),
            Instruction::abc(Opcode::TableGet, 1, 2, 3),
            Instruction::abc(Opcode::TablePut, 1, 2, 3),
            Instruction::new(Opcode::RecordPack, 1, 2, 0, 3),
            Instruction::new(Opcode::RecordGet, 1, 2, 0, 3),
            Instruction::abc(Opcode::RecordLen, 1, 2, 0),
            Instruction::new(Opcode::AskTimeout, 1, 2, 3, 4),
            Instruction::new(Opcode::Delegate, 1, 2, 255, 0),
            Instruction::new(Opcode::Delegate, 1, 2, 3, 1),
            Instruction::a_imm(Opcode::FreshRequestId, 1, 0),
            Instruction::abc(Opcode::ReceiveMatchCorr, 1, 2, 3),
            Instruction::new(Opcode::ReceiveMatchCorrImm, 1, 2, 0, 7),
            Instruction::a_imm(Opcode::SetTrapExit, 1, 0),
            Instruction::a_imm(Opcode::RegisterName, 1, 0),
            Instruction::abc(Opcode::Whereis, 1, 2, 0),
            Instruction::a_imm(Opcode::ReceiveMatchKind, 1, 2),
            Instruction::only_imm(Opcode::SetRestartPolicy, 1),
            Instruction::new(Opcode::HostAwait, 1, 2, 0, 3),
            Instruction::abc(Opcode::ReceiveMatchTimeout, 1, 2, 3),
            Instruction::new(Opcode::ReceiveMatchImmTimeout, 1, 2, 0, 7),
            Instruction::abc(Opcode::IsAlive, 1, 2, 0),
            Instruction::abc(Opcode::ExitSignal, 1, 2, 0),
            Instruction::abc(Opcode::DictPut, 1, 2, 3),
            Instruction::abc(Opcode::DictGet, 1, 2, 0),
            Instruction::abc(Opcode::DictErase, 1, 2, 0),
            Instruction::abc(Opcode::ReceiveMatchEq, 1, 2, 3),
            Instruction::new(Opcode::StartChild, 1, 2, 0, 3),
            Instruction::abc(Opcode::ReceiveMatchValue, 1, 2, 3),
            Instruction::abc(Opcode::SendRemote, 1, 2, 3),
            Instruction::only_imm(Opcode::StartSupervisor, 1),
        ];
        for instr in samples {
            let text = instr.to_string();
            let parsed = parse_instruction(&text, 1)?;
            if parsed != instr {
                return Err(AssembleError::Syntax {
                    line: 1,
                    message: format!("{text} parsed as {parsed:?}, want {instr:?}"),
                });
            }
        }
        Ok(())
    }
}
