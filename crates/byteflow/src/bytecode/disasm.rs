use std::fmt::Write as _;

use super::asm::write_constant;
use super::chunk::Chunk;
use super::format::module_abi;

/// Assemblable listing of a [`Chunk`]. [`super::assemble`] reads it back.
///
/// The `.abi` line is the version [`super::encode`] would write, not a
/// hardcoded v5.
pub fn disassemble(chunk: &Chunk) -> String {
    let mut out = String::new();
    let _ = writeln!(out, ".chunk {}", quote(&chunk.name));
    let _ = writeln!(out, ".abi {}", module_abi(&chunk.constants));
    for value in &chunk.constants {
        write_constant(&mut out, value);
    }
    for def in &chunk.functions {
        let _ = writeln!(
            out,
            ".fn {} entry={} arity={} regs={}",
            def.name, def.entry, def.arity, def.num_registers
        );
    }
    for (i, instr) in chunk.code.iter().enumerate() {
        let label = match chunk.functions.iter().find(|f| f.entry as usize == i) {
            Some(f) => format!("  ; {}", f.name),
            None => String::new(),
        };
        let _ = writeln!(out, "{i:04}  {instr}{label}");
    }
    out
}

fn quote(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use crate::bytecode::builder::ChunkBuilder;

    #[test]
    fn lists_main() {
        let mut b = ChunkBuilder::new("d");
        b.begin_function("main", 0, 1);
        b.emit_load_imm(0, 1);
        b.emit_return(0);
        let text = super::disassemble(&b.finish());
        assert!(text.contains("LoadImm r0, 1"));
        assert!(text.contains("main"));
        assert!(text.contains(".abi 5"));
    }

    #[test]
    fn call_native_names_the_native_table() {
        let mut b = ChunkBuilder::new("n");
        b.begin_function("main", 0, 2);
        b.emit_call_native(0, 6, 1);
        b.emit_native1_from(1, 0, 7);
        b.emit_return(1);
        let text = super::disassemble(&b.finish());
        assert!(text.contains("CallNative r0, native[6], argc=1"));
        assert!(text.contains("CallNativeFrom r1, r0, native[7]"));
        assert!(!text.contains("CallNative r0, fn["));
    }
}
