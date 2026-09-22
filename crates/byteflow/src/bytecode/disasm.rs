use std::fmt::Write as _;

use super::chunk::{Chunk, ABI_VERSION};

/// Human-readable listing of a [`Chunk`], used by `byteflow-cli disasm`.
pub fn disassemble(chunk: &Chunk) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "; chunk {:?} abi={} constants={} functions={} code={}",
        chunk.name,
        ABI_VERSION,
        chunk.constants.len(),
        chunk.functions.len(),
        chunk.code.len()
    );
    for (i, c) in chunk.constants.iter().enumerate() {
        let _ = writeln!(out, ";   const[{i}] = {c}");
    }
    for (i, def) in chunk.functions.iter().enumerate() {
        let _ = writeln!(
            out,
            "; fn[{i}] {} entry={} arity={} regs={}",
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
    }
}
