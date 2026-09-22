use std::collections::HashMap;

use super::chunk::{Chunk, FunctionDef};
use super::instruction::Instruction;
use super::opcode::Opcode;
use super::value::Value;

/// An unresolved jump target, patched to a relative offset once its address
/// is known (see [`ChunkBuilder::bind_label`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Label(u32);

/// Low-level fluent assembler for [`Chunk`]s (crate-internal).
///
/// External callers should use [`crate::Program`] / [`crate::Fn`]. This type
/// handles label back-patching and raw opcode emission for the public API.
pub(crate) struct ChunkBuilder {
    name: String,
    constants: Vec<Value>,
    code: Vec<Instruction>,
    functions: Vec<FunctionDef>,
    next_label: u32,
    label_targets: HashMap<Label, u32>,
    /// (instruction index, label) pairs awaiting patch.
    pending_jumps: Vec<(usize, Label)>,
    fn_starts: HashMap<String, u32>,
}

impl ChunkBuilder {
    pub fn new(name: impl Into<String>) -> Self {
        ChunkBuilder {
            name: name.into(),
            constants: Vec::new(),
            code: Vec::new(),
            functions: Vec::new(),
            next_label: 0,
            label_targets: HashMap::new(),
            pending_jumps: Vec::new(),
            fn_starts: HashMap::new(),
        }
    }

    pub fn const_(&mut self, v: Value) -> u32 {
        // Constant deduplication keeps hot small-int/bool literals from
        // bloating the pool across a large generated function.
        if let Some(pos) = self.constants.iter().position(|c| c == &v) {
            return pos as u32;
        }
        self.constants.push(v);
        (self.constants.len() - 1) as u32
    }

    pub fn new_label(&mut self) -> Label {
        let l = Label(self.next_label);
        self.next_label += 1;
        l
    }

    /// Bind `label` to the *next* instruction that will be emitted.
    pub fn bind_label(&mut self, label: Label) {
        self.label_targets.insert(label, self.code.len() as u32);
    }

    fn emit(&mut self, instr: Instruction) -> usize {
        self.code.push(instr);
        self.code.len() - 1
    }

    pub fn emit_halt(&mut self) {
        self.emit(Instruction::nullary(Opcode::Halt));
    }

    pub fn emit_load_const(&mut self, dst: u8, konst: u32) {
        self.emit(Instruction::new(Opcode::LoadConst, dst, 0, 0, konst as i32));
    }

    pub fn emit_load_imm(&mut self, dst: u8, imm: i32) {
        self.emit(Instruction::a_imm(Opcode::LoadImm, dst, imm));
    }

    pub fn emit_move(&mut self, dst: u8, src: u8) {
        self.emit(Instruction::abc(Opcode::Move, dst, src, 0));
    }

    pub fn emit_binop(&mut self, op: Opcode, dst: u8, lhs: u8, rhs: u8) {
        debug_assert!(matches!(
            op,
            Opcode::Add
                | Opcode::Sub
                | Opcode::Mul
                | Opcode::Div
                | Opcode::Mod
                | Opcode::Eq
                | Opcode::Lt
                | Opcode::Le
        ));
        self.emit(Instruction::abc(op, dst, lhs, rhs));
    }

    pub fn emit_neg(&mut self, dst: u8, src: u8) {
        self.emit(Instruction::abc(Opcode::Neg, dst, src, 0));
    }

    pub fn emit_jump(&mut self, target: Label) {
        let idx = self.emit(Instruction::only_imm(Opcode::Jump, 0));
        self.pending_jumps.push((idx, target));
    }

    pub fn emit_branch(&mut self, cond: u8, target: Label) {
        let idx = self.emit(Instruction::a_imm(Opcode::Branch, cond, 0));
        self.pending_jumps.push((idx, target));
    }

    pub fn emit_spawn(&mut self, dst: u8, function: u32, argc: u8) {
        self.emit_spawn_with_rights(dst, function, argc, crate::bytecode::CapRights::FLOW);
    }

    /// Bytecode spawn with an explicit rights request (`instr.c`). `NONE` is
    /// confined: the child inherits no power until a later `Delegate`.
    pub fn emit_spawn_with_rights(
        &mut self,
        dst: u8,
        function: u32,
        argc: u8,
        rights: crate::bytecode::CapRights,
    ) {
        self.emit(Instruction::new(
            Opcode::Spawn,
            dst,
            argc,
            rights.bits_u8(),
            function as i32,
        ));
    }

    pub fn emit_delegate(&mut self, dst: u8, src_cap: u8, rights: crate::bytecode::CapRights) {
        self.emit(Instruction::new(
            Opcode::Delegate,
            dst,
            src_cap,
            255,
            rights.bits() as i32,
        ));
    }

    pub fn emit_yield(&mut self) {
        self.emit(Instruction::nullary(Opcode::Yield));
    }

    pub fn emit_sleep(&mut self, millis_reg: u8) {
        self.emit(Instruction::abc(Opcode::Sleep, millis_reg, 0, 0));
    }

    pub fn emit_exit(&mut self, reg: u8) {
        self.emit(Instruction::abc(Opcode::Exit, reg, 0, 0));
    }

    /// Write a **self Cap** ([`CapRights::ADDRESSING`](crate::CapRights::ADDRESSING))
    /// into `dst` (opcode still named `SelfPid`).
    pub fn emit_self_pid(&mut self, dst: u8) {
        self.emit(Instruction::abc(Opcode::SelfPid, dst, 0, 0));
    }

    /// Write the next per-flow correlation id into `dst`.
    pub fn emit_fresh_request_id(&mut self, dst: u8) {
        self.emit(Instruction::abc(Opcode::FreshRequestId, dst, 0, 0));
    }

    /// Fire-and-forget Atomic Hop: `r[target_cap_reg]` must be Cap; `r[msg_reg]` Message.
    pub fn emit_send(&mut self, target_cap_reg: u8, msg_reg: u8) {
        self.emit(Instruction::abc(Opcode::Send, target_cap_reg, msg_reg, 0));
    }

    pub fn emit_receive(&mut self, dst: u8) {
        self.emit(Instruction::abc(Opcode::Receive, dst, 0, 0));
    }

    pub fn emit_receive_timeout(&mut self, dst: u8, millis_reg: u8) {
        self.emit(Instruction::abc(Opcode::ReceiveTimeout, dst, millis_reg, 0));
    }

    /// Selective Atomic Hop: wait for `Message` with `tag == r[tag_reg]`.
    pub fn emit_receive_match(&mut self, dst: u8, tag_reg: u8) {
        self.emit(Instruction::abc(Opcode::ReceiveMatch, dst, tag_reg, 0));
    }

    /// Selective Atomic Hop with an immediate `u16` tag.
    pub fn emit_receive_match_imm(&mut self, dst: u8, tag: u16) {
        self.emit(Instruction::a_imm(
            Opcode::ReceiveMatchImm,
            dst,
            i32::from(tag),
        ));
    }

    /// Selective receive: `tag == r[tag_reg]` and `request_id == r[id_reg]`.
    pub fn emit_receive_match_corr(&mut self, dst: u8, tag_reg: u8, id_reg: u8) {
        self.emit(Instruction::abc(
            Opcode::ReceiveMatchCorr,
            dst,
            tag_reg,
            id_reg,
        ));
    }

    /// Selective receive with immediate tag and `request_id` from `id_reg`.
    pub fn emit_receive_match_corr_imm(&mut self, dst: u8, tag: u16, id_reg: u8) {
        self.emit(Instruction::new(
            Opcode::ReceiveMatchCorrImm,
            dst,
            id_reg,
            0,
            i32::from(tag),
        ));
    }

    /// Selective receive: wait for a hop whose payload wire-tag equals `kind`.
    pub fn emit_receive_match_kind(&mut self, dst: u8, kind: u8) {
        self.emit(Instruction::a_imm(
            Opcode::ReceiveMatchKind,
            dst,
            i32::from(kind),
        ));
    }

    /// Atomic request/reply hop: deliver `r[msg_reg]` to `r[target_cap_reg]` (Cap),
    /// then wait for a correlated reply into `dst`.
    ///
    /// Encoding: `Ask ra, rb, rc` → `a=dest`, `b=target Cap`, `c=request Message`.
    ///
    /// The worker authenticates the request (`sender` + `reply_cap`) before delivery
    /// and completes only when the reply’s `sender` equals the **resolved FlowId**.
    pub fn emit_ask(&mut self, dest: u8, target_cap_reg: u8, msg_reg: u8) {
        self.emit(Instruction::abc(Opcode::Ask, dest, target_cap_reg, msg_reg));
    }

    /// Like [`Self::emit_ask`], with a timeout register in `imm`.
    pub fn emit_ask_timeout(&mut self, dest: u8, target_cap_reg: u8, msg_reg: u8, millis_reg: u8) {
        self.emit(Instruction::new(
            Opcode::AskTimeout,
            dest,
            target_cap_reg,
            msg_reg,
            i32::from(millis_reg),
        ));
    }

    pub fn emit_monitor(&mut self, dest: u8, target_cap_reg: u8) {
        self.emit(Instruction::abc(Opcode::Monitor, dest, target_cap_reg, 0));
    }

    pub fn emit_demonitor(&mut self, monitor_reg: u8) {
        self.emit(Instruction::abc(Opcode::Demonitor, monitor_reg, 0, 0));
    }

    pub fn emit_link(&mut self, dest: u8, target_cap_reg: u8) {
        self.emit(Instruction::abc(Opcode::Link, dest, target_cap_reg, 0));
    }

    pub fn emit_unlink(&mut self, link_reg: u8) {
        self.emit(Instruction::abc(Opcode::Unlink, link_reg, 0, 0));
    }

    pub fn emit_set_trap_exit(&mut self, enabled_reg: u8) {
        self.emit(Instruction::abc(Opcode::SetTrapExit, enabled_reg, 0, 0));
    }

    /// `imm`: 0=Always, 1=OnFailure, 2=Never.
    pub fn emit_set_restart_policy(&mut self, policy: u8) {
        self.emit(Instruction::only_imm(
            Opcode::SetRestartPolicy,
            i32::from(policy),
        ));
    }

    pub fn emit_register_name(&mut self, name_reg: u8) {
        self.emit(Instruction::abc(Opcode::RegisterName, name_reg, 0, 0));
    }

    pub fn emit_whereis(&mut self, dst: u8, name_reg: u8) {
        self.emit(Instruction::abc(Opcode::Whereis, dst, name_reg, 0));
    }

    pub fn emit_trap(&mut self, code: i32) {
        self.emit(Instruction::only_imm(Opcode::Trap, code));
    }

    pub fn emit_call(&mut self, dst: u8, function: u32, argc: u8) {
        self.emit(Instruction::new(
            Opcode::Call,
            dst,
            argc,
            0,
            function as i32,
        ));
    }

    /// Emit a call through the runtime's native (FFI) function table
    /// (design notes §30-31). `native_index` is resolved by name against a
    /// [`crate::NativeTable`] at the call site — the assembler has no
    /// knowledge of what natives exist, on purpose (see
    /// [`crate::verify`]'s note on why `CallNative` targets aren't
    /// range-checked statically).
    pub fn emit_call_native(&mut self, dst: u8, native_index: u32, argc: u8) {
        self.emit(Instruction::new(
            Opcode::CallNative,
            dst,
            argc,
            0,
            native_index as i32,
        ));
    }

    /// Move `src` into `dst`, then `CallNative(dst, native_index, 1)`.
    ///
    /// # The contract this exists to protect: `CallNative` clobbers its argument
    ///
    /// `Opcode::CallNative ra, fb, nc` reads `nc` arguments from
    /// `r[a..a+nc]` and writes the result back into `r[a]`. For `nc == 1`
    /// the argument and result are the same slot — calling a one-arg native
    /// straight on a register you still need destroys it.
    ///
    /// The textbook case is unpacking several fields from one `Message` in
    /// `r0` (`msg_sender`, `msg_tag`, …). `emit_native1_from` always operates
    /// on a **copy** (`dst`), so `src` survives:
    ///
    /// ```text
    /// b.emit_native1_from(1, 0, native_msg_sender);   // r1 = sender(r0)
    /// b.emit_native1_from(2, 0, native_msg_request_id);
    /// ```
    ///
    /// If you don't need `src` afterwards, call `emit_call_native` directly —
    /// the `Move` would be pure overhead. See [`crate::emit_native1_from`] for
    /// the macro-sugar form that forwards here.
    pub fn emit_native1_from(&mut self, dst: u8, src: u8, native_index: u32) {
        self.emit_move(dst, src);
        self.emit_call_native(dst, native_index, 1);
    }

    /// `CallNative(base, native_index, argc)` when `argc` args are **already**
    /// contiguous at `r[base..base+argc]`.
    ///
    /// No behavior beyond [`Self::emit_call_native`] — exists so the call site
    /// reads as "args already packed". See [`crate::emit_native_n`].
    pub fn emit_native_n(&mut self, base: u8, native_index: u32, argc: u8) {
        self.emit_call_native(base, native_index, argc);
    }

    pub fn emit_return(&mut self, reg: u8) {
        self.emit(Instruction::abc(Opcode::Return, reg, 0, 0));
    }

    /// Mark the start of a bytecode function at the current position and
    /// register it in the function table under `name`. Returns the function
    /// index, usable with [`ChunkBuilder::emit_call`]/[`ChunkBuilder::emit_spawn`]
    /// even before the function's body is emitted (functions may call
    /// themselves or each other, forward or backward).
    pub fn begin_function(&mut self, name: impl Into<String>, arity: u8, num_registers: u8) -> u32 {
        let name = name.into();
        let entry = self.code.len() as u32;
        let idx = self.functions.len() as u32;
        self.functions.push(FunctionDef {
            name: name.clone(),
            entry,
            arity,
            num_registers,
        });
        self.fn_starts.insert(name, idx);
        idx
    }

    pub fn function_index(&self, name: &str) -> Option<u32> {
        self.fn_starts.get(name).copied()
    }

    /// Patch the register-file size of an already-`begin_function`'d
    /// function. Exists for assemblers whose register count is only known
    /// *after* emitting the body — `begin_function` must still be called
    /// first so `entry` captures the current code cursor.
    pub fn set_num_registers(&mut self, function_index: u32, num_registers: u8) {
        if let Some(def) = self.functions.get_mut(function_index as usize) {
            def.num_registers = num_registers;
        }
    }

    /// Resolve every pending jump against its bound label and produce the
    /// final immutable [`Chunk`].
    ///
    /// An unbound label is a host assembly bug. This method does **not**
    /// panic: the jump is left with relative offset `0` (falls through).
    /// [`crate::verify`] / [`crate::Runtime::new`] then reject or run a
    /// no-op jump rather than taking the process down at assemble time.
    pub fn finish(mut self) -> Chunk {
        for (idx, label) in self.pending_jumps.drain(..) {
            match self.label_targets.get(&label) {
                Some(target) => {
                    // Relative offset from the instruction *after* this jump.
                    let offset = *target as i64 - (idx as i64 + 1);
                    self.code[idx].imm = offset as i32;
                }
                None => {
                    // Fail-closed without panic: identity jump (offset 0).
                    self.code[idx].imm = 0;
                }
            }
        }
        Chunk {
            name: self.name,
            constants: self.constants,
            code: self.code,
            functions: self.functions,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emit_native1_from_never_clobbers_source_register() {
        let mut b = ChunkBuilder::new("clobber-test");
        b.begin_function("main", 0, 8);
        b.emit_native1_from(1, 0, 10);
        b.emit_native1_from(2, 0, 11);
        b.emit_native1_from(3, 0, 12);
        let chunk = b.finish();

        assert_eq!(chunk.code.len(), 6);
        for (move_idx, call_idx, expected_native) in
            [(0usize, 1usize, 10i32), (2, 3, 11), (4, 5, 12)]
        {
            assert_eq!(chunk.code[move_idx].op, Opcode::Move);
            assert_eq!(chunk.code[move_idx].b, 0);
            assert_eq!(chunk.code[call_idx].op, Opcode::CallNative);
            assert_ne!(chunk.code[call_idx].a, 0);
            assert_eq!(chunk.code[call_idx].imm, expected_native);
        }
    }

    #[test]
    fn emit_native_n_is_a_plain_call_native_with_no_extra_instructions() {
        let mut b = ChunkBuilder::new("native-n-test");
        b.begin_function("main", 0, 8);
        b.emit_load_imm(1, 7);
        b.emit_load_imm(2, 1);
        b.emit_native_n(1, 99, 2);
        let chunk = b.finish();

        assert_eq!(chunk.code.len(), 3);
        assert_eq!(chunk.code[2].op, Opcode::CallNative);
        assert_eq!(chunk.code[2].a, 1);
        assert_eq!(chunk.code[2].b, 2);
        assert_eq!(chunk.code[2].imm, 99);
    }

    #[test]
    fn macro_forms_produce_identical_bytecode_to_the_methods() {
        let mut via_method = ChunkBuilder::new("via-method");
        via_method.begin_function("main", 0, 8);
        via_method.emit_native1_from(1, 0, 10);
        via_method.emit_native_n(1, 99, 2);

        let mut via_macro = ChunkBuilder::new("via-macro");
        via_macro.begin_function("main", 0, 8);
        crate::emit_native1_from!(via_macro, 1, 0, 10);
        crate::emit_native_n!(via_macro, 1, 99, 2);

        assert_eq!(via_method.finish().code, via_macro.finish().code);
    }
}
