//! Primary API for assembling [`Chunk`] programs in host Rust.
//!
//! Use [`Program`] to define a module and [`Fn`] to emit each function with
//! named registers instead of manual `r0` / `emit_*` bookkeeping.
//!
//! ```rust
//! use byteflow::Program;
//!
//! let mut program = Program::new("count");
//! program.function("main", 0, |f| {
//!     let limit = f.load_int(100);
//!     let counter = f.load_i32(0);
//!     f.while_lt(counter, limit, |f| f.add_imm(counter, 1));
//!     f.return_(counter);
//! });
//! let chunk = program.build();
//! ```

use super::builder::ChunkBuilder;

pub use super::builder::Label;
use super::chunk::Chunk;
use super::opcode::Opcode;
use super::value::Value;

/// Function index returned by [`Program::function`].
pub type FuncId = u32;

/// A virtual register in the current function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Reg(u8);

impl Reg {
    /// Underlying register index in the bytecode function frame.
    pub const fn index(self) -> u8 {
        self.0
    }
}

impl From<Reg> for u8 {
    fn from(r: Reg) -> u8 {
        r.0
    }
}

/// Contiguous register window (e.g. four slots for `make_msg` natives).
#[derive(Clone, Copy, Debug)]
pub struct RegWindow {
    base: Reg,
    len: u8,
}

impl RegWindow {
    /// First register in the window (native result lands here for `make_msg`).
    pub fn base(self) -> Reg {
        self.base
    }

    /// Register at offset `i` within the window (`0 <= i < len`).
    pub fn at(self, i: u8) -> Reg {
        debug_assert!(i < self.len, "RegWindow index out of bounds");
        Reg(self.base.0.saturating_add(i))
    }
}

/// Assemble one bytecode module.
pub struct Program {
    inner: ChunkBuilder,
}

impl Program {
    /// Start a new module named `name`.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            inner: ChunkBuilder::new(name),
        }
    }

    /// Define a function and run `body` against a fresh [`Fn`] context.
    ///
    /// Returns the function index for [`Fn::spawn`] / [`Fn::call`].
    pub fn function(
        &mut self,
        name: impl Into<String>,
        arity: u8,
        body: impl FnOnce(&mut Fn<'_>),
    ) -> FuncId {
        let mut f = Fn::open(&mut self.inner, name, arity);
        let id = f.function;
        body(&mut f);
        id
    }

    /// Define a function with an explicit register-file size.
    ///
    /// Useful for fixed register layouts (`reg(255)`) or verification demos
    /// where `num_registers < arity` must fail static checks.
    pub fn function_raw(
        &mut self,
        name: impl Into<String>,
        arity: u8,
        num_registers: u8,
        body: impl FnOnce(&mut Fn<'_>),
    ) -> FuncId {
        let mut f = Fn::open_with(&mut self.inner, name, arity, num_registers);
        let id = f.function;
        body(&mut f);
        id
    }

    /// Look up a function index by name (available before [`Self::build`]).
    pub fn function_index(&self, name: &str) -> Option<FuncId> {
        self.inner.function_index(name)
    }

    /// Finish assembly and produce an immutable [`Chunk`].
    pub fn build(self) -> Chunk {
        self.inner.finish()
    }
}

/// Emit instructions for one bytecode function.
pub struct Fn<'a> {
    b: &'a mut ChunkBuilder,
    function: FuncId,
    next_reg: u8,
    scratch: Option<Reg>,
}

impl<'a> Fn<'a> {
    fn open(b: &'a mut ChunkBuilder, name: impl Into<String>, arity: u8) -> Self {
        Self::open_with(b, name, arity, arity.max(4))
    }

    fn open_with(
        b: &'a mut ChunkBuilder,
        name: impl Into<String>,
        arity: u8,
        num_registers: u8,
    ) -> Self {
        let function = b.begin_function(name, arity, num_registers);
        Self {
            b,
            function,
            next_reg: num_registers,
            scratch: None,
        }
    }

    /// Allocate a fresh local register.
    pub fn local(&mut self) -> Reg {
        let reg = Reg(self.next_reg);
        self.next_reg = self.next_reg.saturating_add(1);
        self.b.set_num_registers(self.function, self.next_reg);
        reg
    }

    /// Bind to an explicit register index without growing the declared frame.
    pub fn reg(&mut self, index: u8) -> Reg {
        Reg(index)
    }

    /// Ensure the register file is at least `count` slots wide.
    pub fn reserve(&mut self, count: u8) {
        if count > self.next_reg {
            self.next_reg = count;
            self.b.set_num_registers(self.function, self.next_reg);
        }
    }

    /// Allocate `count` contiguous locals; useful before [`Self::native_n`].
    pub fn window(&mut self, count: u8) -> RegWindow {
        let base_idx = self.next_reg;
        for _ in 0..count {
            let _ = self.local();
        }
        RegWindow {
            base: Reg(base_idx),
            len: count,
        }
    }

    /// Load a small immediate into a new local.
    pub fn load_i32(&mut self, n: i32) -> Reg {
        let reg = self.local();
        self.b.emit_load_imm(reg.0, n);
        reg
    }

    /// Load a constant-pool integer into a new local.
    pub fn load_int(&mut self, n: i64) -> Reg {
        let konst = self.b.const_(Value::Int(n));
        let reg = self.local();
        self.b.emit_load_const(reg.0, konst);
        reg
    }

    /// Store an immediate into an existing register.
    pub fn set(&mut self, reg: Reg, imm: i32) {
        self.b.emit_load_imm(reg.0, imm);
    }

    /// `dst = src`
    pub fn mov(&mut self, dst: Reg, src: Reg) {
        self.b.emit_move(dst.0, src.0);
    }

    fn binop(&mut self, op: Opcode, lhs: Reg, rhs: Reg) -> Reg {
        let dst = self.local();
        self.b.emit_binop(op, dst.0, lhs.0, rhs.0);
        dst
    }

    fn binop_imm(&mut self, op: Opcode, lhs: Reg, imm: i32) -> Reg {
        let rhs = self.temp();
        self.b.emit_load_imm(rhs.0, imm);
        self.binop(op, lhs, rhs)
    }

    /// `dst = lhs + rhs` into a new local.
    pub fn add(&mut self, lhs: Reg, rhs: Reg) -> Reg {
        self.binop(Opcode::Add, lhs, rhs)
    }

    /// `dst += imm` in place.
    pub fn add_imm(&mut self, dst: Reg, imm: i32) {
        let tmp = self.temp();
        self.b.emit_load_imm(tmp.0, imm);
        self.b.emit_binop(Opcode::Add, dst.0, dst.0, tmp.0);
    }

    pub fn sub(&mut self, lhs: Reg, rhs: Reg) -> Reg {
        self.binop(Opcode::Sub, lhs, rhs)
    }

    pub fn mul(&mut self, lhs: Reg, rhs: Reg) -> Reg {
        self.binop(Opcode::Mul, lhs, rhs)
    }

    pub fn div(&mut self, lhs: Reg, rhs: Reg) -> Reg {
        self.binop(Opcode::Div, lhs, rhs)
    }

    pub fn modulo(&mut self, lhs: Reg, rhs: Reg) -> Reg {
        self.binop(Opcode::Mod, lhs, rhs)
    }

    pub fn neg(&mut self, src: Reg) -> Reg {
        let dst = self.local();
        self.b.emit_neg(dst.0, src.0);
        dst
    }

    pub fn eq(&mut self, lhs: Reg, rhs: Reg) -> Reg {
        self.binop(Opcode::Eq, lhs, rhs)
    }

    pub fn eq_imm(&mut self, lhs: Reg, imm: i32) -> Reg {
        self.binop_imm(Opcode::Eq, lhs, imm)
    }

    pub fn lt(&mut self, lhs: Reg, rhs: Reg) -> Reg {
        self.binop(Opcode::Lt, lhs, rhs)
    }

    pub fn le(&mut self, lhs: Reg, rhs: Reg) -> Reg {
        self.binop(Opcode::Le, lhs, rhs)
    }

    /// While `counter < limit` (interpreter `Branch` skips on falsy).
    pub fn while_lt<F>(&mut self, counter: Reg, limit: Reg, body: F)
    where
        F: FnOnce(&mut Fn<'_>),
    {
        let head = self.b.new_label();
        let done = self.b.new_label();
        let cond = self.local();
        self.b.bind_label(head);
        self.b.emit_binop(Opcode::Lt, cond.0, counter.0, limit.0);
        self.b.emit_branch(cond.0, done);
        body(self);
        self.b.emit_jump(head);
        self.b.bind_label(done);
    }

    pub fn label(&mut self) -> Label {
        self.b.new_label()
    }

    pub fn bind(&mut self, label: Label) {
        self.b.bind_label(label);
    }

    pub fn jump(&mut self, label: Label) {
        self.b.emit_jump(label);
    }

    /// Branch when `cond` is falsy (`0`).
    pub fn branch_if_falsy(&mut self, cond: Reg, target: Label) {
        self.b.emit_branch(cond.0, target);
    }

    pub fn return_(&mut self, value: Reg) {
        self.b.emit_return(value.0);
    }

    pub fn call(&mut self, function: FuncId, argc: u8) -> Reg {
        let dst = self.local();
        self.b.emit_call(dst.0, function, argc);
        dst
    }

    pub fn halt(&mut self) {
        self.b.emit_halt();
    }

    pub fn yield_(&mut self) {
        self.b.emit_yield();
    }

    pub fn sleep(&mut self, millis: Reg) {
        self.b.emit_sleep(millis.0);
    }

    pub fn exit(&mut self, reg: Reg) {
        self.b.emit_exit(reg.0);
    }

    /// Self Cap ([`CapRights::ADDRESSING`](crate::CapRights::ADDRESSING):
    /// `SEND|ASK|LINK|MONITOR` for this flow).
    ///
    /// Despite the opcode name (`SelfPid`), the value is a [`crate::Value::Cap`],
    /// not a Pid. In BEAM terms this is closer to a **reply address** grant
    /// than to `self()` — use [`Self::hop_sender`] on a *delivered* hop for
    /// authenticated origin identity.
    pub fn self_cap(&mut self) -> Reg {
        let cap = self.local();
        self.b.emit_self_pid(cap.0);
        cap
    }

    /// Alias for [`Self::self_cap`] — preferred name when coming from BEAM.
    pub fn self_address(&mut self) -> Reg {
        self.self_cap()
    }

    pub fn spawn(&mut self, function: FuncId, argc: u8) -> Reg {
        self.spawn_with_rights(function, argc, crate::bytecode::CapRights::FLOW)
    }

    /// Spawn a child that receives only `rights` ⊆ parent authority.
    pub fn spawn_with_rights(
        &mut self,
        function: FuncId,
        argc: u8,
        rights: crate::bytecode::CapRights,
    ) -> Reg {
        let cap = self.local();
        self.b.emit_spawn_with_rights(cap.0, function, argc, rights);
        cap
    }

    /// Spawn with `rights = NONE` (confined by default).
    pub fn spawn_confined(&mut self, function: FuncId, argc: u8) -> Reg {
        self.spawn_with_rights(function, argc, crate::bytecode::CapRights::NONE)
    }

    /// Spawn into an explicit destination register (e.g. `reg(255)`).
    pub fn spawn_at(&mut self, dst: Reg, function: FuncId, argc: u8) {
        self.b.emit_spawn(dst.0, function, argc);
    }

    /// Attenuate `src` into a new Cap (rights mask is an immediate).
    pub fn delegate(&mut self, src: Reg, rights: crate::bytecode::CapRights) -> Reg {
        let dst = self.local();
        self.b.emit_delegate(dst.0, src.0, rights);
        dst
    }

    pub fn send(&mut self, target_cap: Reg, msg: Reg) {
        self.b.emit_send(target_cap.0, msg.0);
    }

    pub fn receive(&mut self) -> Reg {
        let msg = self.local();
        self.b.emit_receive(msg.0);
        msg
    }

    pub fn receive_timeout(&mut self, millis: Reg) -> Reg {
        let msg = self.local();
        self.b.emit_receive_timeout(msg.0, millis.0);
        msg
    }

    pub fn receive_match(&mut self, tag: Reg) -> Reg {
        let msg = self.local();
        self.b.emit_receive_match(msg.0, tag.0);
        msg
    }

    pub fn receive_match_imm(&mut self, tag: u16) -> Reg {
        let msg = self.local();
        self.b.emit_receive_match_imm(msg.0, tag);
        msg
    }

    /// Next per-flow correlation id (`Int`, starts at 1).
    pub fn fresh_request_id(&mut self) -> Reg {
        let dst = self.local();
        self.b.emit_fresh_request_id(dst.0);
        dst
    }

    /// Wait for `tag == tag_reg` and `request_id == id_reg` (FIFO skip).
    pub fn receive_match_corr(&mut self, tag: Reg, request_id: Reg) -> Reg {
        let msg = self.local();
        self.b.emit_receive_match_corr(msg.0, tag.0, request_id.0);
        msg
    }

    /// Wait for immediate `tag` and `request_id == id_reg` (FIFO skip).
    pub fn receive_match_corr_imm(&mut self, tag: u16, request_id: Reg) -> Reg {
        let msg = self.local();
        self.b.emit_receive_match_corr_imm(msg.0, tag, request_id.0);
        msg
    }

    /// Wait for a hop whose `payload.wire_tag() == kind` (`0..=8`, FIFO skip).
    pub fn receive_match_kind(&mut self, kind: u8) -> Reg {
        let msg = self.local();
        self.b.emit_receive_match_kind(msg.0, kind);
        msg
    }

    /// Build a hop with a freshly minted `request_id`.
    pub fn hop_fresh(&mut self, tag: i32, payload: Reg) -> Reg {
        let req_id = self.fresh_request_id();
        self.hop(req_id, tag, payload)
    }

    /// RPC hop: deliver `msg` to `target_cap`, wait for correlated reply.
    ///
    /// If the target exits first, dest is a [`crate::TAG_SYS_EXIT`] hop
    /// (not a hang).
    pub fn ask(&mut self, target_cap: Reg, msg: Reg) -> Reg {
        let reply = self.local();
        self.b.emit_ask(reply.0, target_cap.0, msg.0);
        reply
    }

    /// Like [`Self::ask`], but writes `Unit` into the dest if `millis` elapses.
    pub fn ask_timeout(&mut self, target_cap: Reg, msg: Reg, millis: Reg) -> Reg {
        let reply = self.local();
        self.b
            .emit_ask_timeout(reply.0, target_cap.0, msg.0, millis.0);
        reply
    }

    /// One-way watch: when the flow addressed by `target_cap` exits, this
    /// flow receives a [`crate::TAG_SYS_DOWN`] hop. Returns a monitor ref (`Int`).
    pub fn monitor(&mut self, target_cap: Reg) -> Reg {
        let dst = self.local();
        self.b.emit_monitor(dst.0, target_cap.0);
        dst
    }

    pub fn demonitor(&mut self, monitor: Reg) {
        self.b.emit_demonitor(monitor.0);
    }

    /// Bidirectional link: without `trap_exit`, abnormal exit of either side
    /// kills the peer. With [`Self::set_trap_exit`] on a peer, that peer gets
    /// [`crate::TAG_SYS_EXIT`] hops instead (including `Normal`).
    pub fn link(&mut self, target_cap: Reg) -> Reg {
        let dst = self.local();
        self.b.emit_link(dst.0, target_cap.0);
        dst
    }

    pub fn unlink(&mut self, link: Reg) {
        self.b.emit_unlink(link.0);
    }

    /// BEAM `process_flag(trap_exit, enabled)`. Truthy `enabled` converts
    /// linked exits into [`crate::TAG_SYS_EXIT`] hops (including `Normal`)
    /// instead of killing this flow / silently dropping the link.
    pub fn set_trap_exit(&mut self, enabled: Reg) {
        self.b.emit_set_trap_exit(enabled.0);
    }

    /// Set this flow's supervisor restart policy (bytecode slice of OTP
    /// `child_spec` restart). Takes effect for the **current** incarnation;
    /// the dying flow's policy is what the supervisor consults on exit.
    pub fn set_restart_policy(&mut self, policy: super::RestartPolicy) {
        self.b.emit_set_restart_policy(policy.as_u8());
    }

    /// Park until the host [`crate::HostAwaitBridge`] completes.
    ///
    /// `op` is an opaque host discriminator (`imm`). `args` is a single
    /// [`crate::Value`] handed to the bridge. On success the result is
    /// written into the returned register (Receive-style writeback).
    pub fn host_await(&mut self, op: u32, args: Reg) -> Reg {
        let dst = self.local();
        self.b.emit_host_await(dst.0, args.0, op);
        dst
    }

    /// Load a UTF-8 constant into a new local.
    pub fn load_str(&mut self, s: impl AsRef<str>) -> Reg {
        let konst = self.b.const_(Value::str(s));
        let reg = self.local();
        self.b.emit_load_const(reg.0, konst);
        reg
    }

    /// Load a byte-buffer constant into a new local.
    pub fn load_bytes(&mut self, bytes: impl AsRef<[u8]>) -> Reg {
        let konst = self.b.const_(Value::bytes(bytes.as_ref()));
        let reg = self.local();
        self.b.emit_load_const(reg.0, konst);
        reg
    }

    /// Publish `name` (`Str`) as this flow. Requires `SEND` on self-authority.
    pub fn register_name(&mut self, name: Reg) {
        self.b.emit_register_name(name.0);
    }

    /// Look up `name` (`Str`): SEND Cap for the caller, or `Unit`.
    pub fn whereis(&mut self, name: Reg) -> Reg {
        let dst = self.local();
        self.b.emit_whereis(dst.0, name.0);
        dst
    }

    pub fn trap(&mut self, code: i32) {
        self.b.emit_trap(code);
    }

    /// Copy `src`, call a one-arg native, return the result in a new local.
    ///
    /// The source register is preserved (`CallNative` clobbers its argument slot).
    pub fn native1_from(&mut self, src: Reg, native: u32) -> Reg {
        let dst = self.local();
        self.b.emit_native1_from(dst.0, src.0, native);
        dst
    }

    /// Side-effect native on a copy of `src` (e.g. `print`).
    pub fn native1_on(&mut self, src: Reg, native: u32) {
        let tmp = self.local();
        self.b.emit_native1_from(tmp.0, src.0, native);
    }

    /// `CallNative` with `argc` args already at `base..base+argc`.
    pub fn native_n(&mut self, base: Reg, native: u32, argc: u8) {
        self.b.emit_native_n(base.0, native, argc);
    }

    /// Call a native with `argc` args already in `base..base+argc`.
    pub fn call_native(&mut self, base: Reg, native: u32, argc: u8) {
        self.native_n(base, native, argc);
    }

    /// Call a zero-arg native; returns the result register.
    pub fn call_native0(&mut self, native: u32) -> Reg {
        let dst = self.local();
        self.b.emit_call_native(dst.0, native, 0);
        dst
    }

    /// Build an outgoing Atomic Hop (`request_id`, `tag`, `payload`).
    ///
    /// The scheduler overwrites `sender` and attaches `reply_cap` on [`Self::send`]
    /// / [`Self::ask`]. Prefer [`Self::hop_fresh`] so `request_id` is unique.
    /// Do not forge a sender (see [`crate::docs::security`]).
    pub fn hop(&mut self, request_id: Reg, tag: i32, payload: Reg) -> Reg {
        self.make_msg(
            crate::natives::std_native::MAKE_MSG,
            request_id,
            tag,
            payload,
        )
    }

    /// Extract authenticated origin from a delivered hop (`Value::Pid`).
    pub fn hop_sender(&mut self, msg: Reg) -> Reg {
        self.native1_from(msg, crate::natives::std_native::MSG_SENDER)
    }

    pub fn hop_request_id(&mut self, msg: Reg) -> Reg {
        self.native1_from(msg, crate::natives::std_native::MSG_REQUEST_ID)
    }

    pub fn hop_tag(&mut self, msg: Reg) -> Reg {
        self.native1_from(msg, crate::natives::std_native::MSG_TAG)
    }

    pub fn hop_payload(&mut self, msg: Reg) -> Reg {
        self.native1_from(msg, crate::natives::std_native::MSG_PAYLOAD)
    }

    /// Reply address grant minted for the original sender (`Value::Cap`).
    pub fn hop_reply_cap(&mut self, msg: Reg) -> Reg {
        self.native1_from(msg, crate::natives::std_native::MSG_REPLY_CAP)
    }

    /// Build a reply hop echoing `request_id` from `req`.
    pub fn reply_to(&mut self, req: Reg, tag: i32, payload: Reg) -> Reg {
        let req_id = self.hop_request_id(req);
        self.hop(req_id, tag, payload)
    }

    /// Reply to `req` via its `reply_cap` (typical server pattern).
    pub fn send_reply(&mut self, req: Reg, tag: i32, payload: Reg) {
        let reply_cap = self.hop_reply_cap(req);
        let reply = self.reply_to(req, tag, payload);
        self.send(reply_cap, reply);
    }

    /// Build a [`Value::Message`] via `make_msg` at `native_index`.
    ///
    /// There is no sender operand — the scheduler stamps identity on `Send`.
    /// A 4-arg legacy encoding is still accepted by the native (first arg
    /// discarded) so forged-sender regressions keep compiling.
    pub fn make_msg(&mut self, native_index: u32, request_id: Reg, tag: i32, payload: Reg) -> Reg {
        let w = self.window(3);
        self.mov(w.at(0), request_id);
        self.set(w.at(1), tag);
        self.mov(w.at(2), payload);
        self.native_n(w.base(), native_index, 3);
        w.base()
    }

    /// Legacy 4-arg `make_msg` (sender slot is ignored by the native).
    pub fn make_msg_legacy_sender(
        &mut self,
        native_index: u32,
        sender: Reg,
        request_id: Reg,
        tag: i32,
        payload: Reg,
    ) -> Reg {
        let w = self.window(4);
        self.mov(w.at(0), sender);
        self.mov(w.at(1), request_id);
        self.set(w.at(2), tag);
        self.mov(w.at(3), payload);
        self.native_n(w.base(), native_index, 4);
        w.base()
    }

    fn temp(&mut self) -> Reg {
        if let Some(scratch) = self.scratch {
            return scratch;
        }
        let scratch = self.local();
        self.scratch = Some(scratch);
        scratch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NativeTable, Value, Vm, VmResult};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn count_to(n: i64) -> Chunk {
        let mut program = Program::new("count");
        program.function("main", 0, |f| {
            let limit = f.load_int(n);
            let counter = f.load_i32(0);
            f.while_lt(counter, limit, |f| f.add_imm(counter, 1));
            f.return_(counter);
        });
        program.build()
    }

    #[test]
    fn function_raw_keeps_declared_register_file_for_verify() -> TestResult {
        let mut program = Program::new("malformed");
        program.function_raw("main", 3, 1, |f| {
            let r0 = f.reg(0);
            f.return_(r0);
        });
        let chunk = program.build();
        assert_eq!(chunk.functions[0].arity, 3);
        assert_eq!(chunk.functions[0].num_registers, 1);
        assert!(matches!(
            crate::verify(&chunk),
            Err(crate::VerifyError::ArityExceedsRegisters {
                function: 0,
                arity: 3,
                num_registers: 1,
            })
        ));
        Ok(())
    }

    #[test]
    fn count_loop_returns_n() -> TestResult {
        let chunk = count_to(100);
        let mut vm = Vm::new(std::sync::Arc::new(chunk), NativeTable::empty(), 0, &[])?;
        assert!(matches!(
            vm.run(10_000),
            VmResult::Complete(Value::Int(100))
        ));
        Ok(())
    }

    #[test]
    fn ping_pong_shape() -> TestResult {
        let chunk = crate::samples::ping_pong();
        crate::verify(&chunk)?;
        assert!(chunk.functions.iter().any(|f| f.name == "main"));
        assert!(chunk.functions.iter().any(|f| f.name == "pong"));
        Ok(())
    }
}
