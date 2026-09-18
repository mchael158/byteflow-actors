use std::sync::Arc;
use std::time::Duration;

use crate::bytecode::{CapId, Chunk, Instruction, Opcode, Value};
use crate::scheduler::FlowQuota;

use super::fault::{Fault, NativeCallError};
use super::frame::Frame;
use super::native::{check_native_gate, NativeGate, NativeTable};
use super::result::VmResult;

/// Hard limit on call nesting. Frames are heap-allocated, so
/// unbounded recursion would grow the Flow's memory instead of crashing
/// the worker thread's native stack — which is worse, not better, without a
/// limit. `4096` comfortably covers real recursive algorithms while keeping
/// a runaway `fn f() { f() }` a `Fault`, not an OOM.
pub const MAX_CALL_DEPTH: usize = 4096;

/// One virtual Flow's execution state: call stack + registers. Cheap
/// enough to construct that spawning a Flow is a handful of small heap
/// allocations, not a native thread/stack (contrast: a `std::thread` reserves
/// megabytes of stack whether it uses them or not).
///
/// `Vm` owns no scheduler, mailbox, or thread handle — see [`VmResult`] for
/// why that separation is the whole point.
pub struct Vm {
    chunk: Arc<Chunk>,
    natives: Arc<NativeTable>,
    native_gate: NativeGate,
    frames: Vec<Frame>,
    /// Lifetime instruction counter, exposed for `FlowMetrics` (design
    /// notes §26).
    instructions_executed: u64,
    /// Next `Message.request_id` for [`Opcode::FreshRequestId`] and for
    /// hops that still carry `0` (“unset”) at the Send/Ask boundary.
    next_request_id: u64,
    /// Per-flow heap charge for `Str`/`Bytes` written into registers.
    quota: Option<Arc<FlowQuota>>,
    /// Process-wide memory ceiling (optional; set by the runtime).
    memory: Option<Arc<crate::MemoryBudget>>,
    /// When true, `Jump`/`Branch` bounds-check targets at runtime (see
    /// [`Self::set_paranoid_jumps`]). Default off: verified chunks trust
    /// `verify`; unverified corrupt jumps fail-open as implicit `return Unit`.
    paranoid_jumps: bool,
}

impl Vm {
    /// Construct a `Vm` ready to run `function` (an index into
    /// `chunk.functions`) with the given arguments loaded into `r0..argc`.
    /// `natives` is the FFI table `Opcode::CallNative` dispatches through —
    /// pass [`NativeTable::empty`] if the chunk never calls out to Rust.
    ///
    /// Native calls are **denied** until [`Self::with_native_gate`] installs
    /// an allowlist (the runtime does this from the flow's attenuated Cap).
    pub fn new(chunk: Arc<Chunk>, natives: Arc<NativeTable>, function: u32, args: &[Value]) -> Result<Self, Fault> {
        let gate = NativeGate::deny(natives.len());
        Self::with_native_gate(chunk, natives, gate, function, args)
    }

    pub fn with_native_gate(
        chunk: Arc<Chunk>,
        natives: Arc<NativeTable>,
        native_gate: NativeGate,
        function: u32,
        args: &[Value],
    ) -> Result<Self, Fault> {
        let def = chunk
            .function(function)
            .ok_or(Fault::BadFunction { index: function, table_size: chunk.functions.len() as u32 })?;
        let mut frame = Frame::new(function, def.num_registers, None);
        frame.set_pc(def.entry as usize);
        load_frame_args(&mut frame, args.iter().cloned(), def.arity)?;
        Ok(Vm {
            chunk,
            natives,
            native_gate,
            frames: vec![frame],
            instructions_executed: 0,
            next_request_id: 1,
            quota: None,
            memory: None,
            paranoid_jumps: false,
        })
    }

    /// Enable runtime bounds checks on `Jump` / `Branch` targets.
    ///
    /// Production paths leave this off and rely on [`crate::verify`]. Turn it
    /// on for hand-built or untrusted chunks that skip verification.
    pub fn set_paranoid_jumps(&mut self, enabled: bool) {
        self.paranoid_jumps = enabled;
    }

    /// Attach the flow's quota so register stores of `Str`/`Bytes` charge heap.
    ///
    /// Overwrites use reserve(new) → release(old) → install (see
    /// [`crate::memory`]). Same `Arc` rewritten into the same slot is not
    /// charged twice.
    pub fn set_quota(&mut self, quota: Arc<FlowQuota>) -> Result<(), Fault> {
        for frame in &self.frames {
            for slot in frame.registers() {
                charge_heap_value(&quota, self.memory.as_deref(), slot)?;
            }
        }
        self.quota = Some(quota);
        Ok(())
    }

    /// Attach the process-wide memory ceiling (call before or with [`Self::set_quota`]).
    pub fn set_memory_budget(&mut self, memory: Arc<crate::MemoryBudget>) {
        self.memory = Some(memory);
    }

    /// Mint a per-flow correlation id. Never returns `0` (`next_request_id`
    /// starts at 1 and saturates, so it cannot wrap to zero).
    pub fn fresh_request_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        id
    }

    pub fn instructions_executed(&self) -> u64 {
        self.instructions_executed
    }

    /// Index into `chunk.functions` for the active (top) call frame.
    /// Useful for diagnostics / supervisor logs when a Flow traps.
    pub fn current_function(&self) -> u32 {
        // Category D: frames must be non-empty while the VM is runnable.
        // We still avoid `.expect` — return 0 as a diagnostic fallback so a
        // broken invariant cannot panic a worker; the next `run` will trap.
        debug_assert!(!self.frames.is_empty(), "frames empty while running");
        match self.frames.last() {
            Some(frame) => frame.function(),
            None => 0,
        }
    }

    /// A cheap `Arc` clone of the chunk this VM is executing. Used by the
    /// scheduler to construct a child `Vm` for `Opcode::Spawn` without
    /// needing to know anything about `Chunk`'s internals — every Flow
    /// spawned (transitively) from the same top-level `spawn()` call shares
    /// one immutable chunk in memory, never copies it.
    pub fn chunk_arc(&self) -> Arc<Chunk> {
        self.chunk.clone()
    }

    /// A cheap `Arc` clone of this VM's native function table, for the same
    /// reason as [`Vm::chunk_arc`]: a `Spawn`-created child must dispatch
    /// `CallNative` through the identical table its parent uses.
    pub fn natives_arc(&self) -> Arc<NativeTable> {
        self.natives.clone()
    }

    /// Deliver a value the scheduler produced on our behalf (a **Cap** from
    /// `Spawn` / `SelfPid`, or a dequeued mailbox message from a `Receive`)
    /// into the register the instruction that suspended us was targeting,
    /// ahead of the next [`Vm::run`] call. A no-op is never valid to skip:
    /// calling `run` without this after a `Spawn`/`Receive` result leaves the
    /// destination register holding its previous (stale) value.
    #[inline]
    pub fn resume_with(&mut self, dest_reg: u8, value: Value) -> Result<(), Fault> {
        self.set_reg(dest_reg, value)
    }

    /// Program counter of the active frame — used by the optional JIT hook.
    pub fn current_pc(&self) -> Option<usize> {
        self.frames.last().map(|f| f.pc())
    }

    /// Number of registers in the active frame.
    pub fn current_num_registers(&self) -> Option<u8> {
        self.frames.last().map(|f| f.registers().len() as u8)
    }

    /// Read-only view of the active register file.
    pub fn top_registers(&self) -> Option<&[Value]> {
        self.frames.last().map(|f| f.registers())
    }

    /// Mutable view of the active register file (JIT sync path).
    pub fn top_registers_mut(&mut self) -> Option<&mut [Value]> {
        self.frames.last_mut().map(|f| f.registers_mut())
    }

    /// Set the active frame's program counter.
    pub fn set_pc(&mut self, pc: usize) {
        if let Some(frame) = self.frames.last_mut() {
            frame.set_pc(pc);
        }
    }

    /// Write one register in the active frame (JIT sync path).
    pub fn set_register(&mut self, reg: u8, value: Value) -> Result<(), Fault> {
        self.set_reg(reg, value)
    }

    /// Deliver a return value through the call stack.
    pub fn return_value(&mut self, value: Value) -> Result<Option<VmResult>, Fault> {
        self.pop_frame(value)
    }

    /// Top call frame. Empty stack is a broken invariant (category D) —
    /// returned as [`Fault::Invariant`], never as `unwrap`/`expect`.
    #[inline]
    fn current(&mut self) -> Result<&mut Frame, Fault> {
        debug_assert!(!self.frames.is_empty(), "frames empty while running");
        self.frames
            .last_mut()
            .ok_or(Fault::Invariant("empty frame stack while running"))
    }

    #[inline]
    fn get_reg(&self, reg: u8) -> Result<Value, Fault> {
        let frame = self
            .frames
            .last()
            .ok_or(Fault::Invariant("empty frame stack while running"))?;
        frame
            .registers()
            .get(reg as usize)
            .cloned()
            .ok_or(Fault::RegisterOutOfRange {
                reg,
                frame_size: frame.registers().len() as u8,
            })
    }

    #[inline]
    fn set_reg(&mut self, reg: u8, value: Value) -> Result<(), Fault> {
        self.charge_register_store(reg, &value)?;
        let frame = self.current()?;
        let len = frame.registers().len() as u8;
        match frame.registers_mut().get_mut(reg as usize) {
            Some(slot) => {
                *slot = value;
                Ok(())
            }
            None => Err(Fault::RegisterOutOfRange { reg, frame_size: len }),
        }
    }

    #[inline]
    fn peek_reg(&self, reg: u8) -> Option<&Value> {
        self.frames.last()?.registers().get(reg as usize)
    }

    /// Heap charge for `Str` / `Bytes` register stores.
    ///
    /// Order: reserve(new) → release(old). Failed reservation leaves the
    /// slot and accounting untouched. Same `Arc` in the same slot is a no-op.
    fn charge_register_store(&self, reg: u8, value: &Value) -> Result<(), Fault> {
        let Some(quota) = self.quota.as_ref() else {
            return Ok(());
        };
        match (value, self.peek_reg(reg)) {
            (Value::Str(s), Some(Value::Str(old))) if Arc::ptr_eq(s, old) => return Ok(()),
            (Value::Bytes(b), Some(Value::Bytes(old))) if Arc::ptr_eq(b, old) => return Ok(()),
            _ => {}
        }

        let new_bytes = heap_charge_of(value);
        let old_bytes = self.peek_reg(reg).map(heap_charge_of).unwrap_or(0);

        // Net accounting: reserve only the growth (or release the shrink).
        // Semantically equal to reserve(new)→release(old) without needing
        // temporary headroom of old+new against a tight mem_limit.
        if new_bytes > old_bytes {
            charge_pair(quota, self.memory.as_deref(), new_bytes - old_bytes)?;
        } else if old_bytes > new_bytes {
            release_pair(quota, self.memory.as_deref(), old_bytes - new_bytes);
        }
        Ok(())
    }

    /// Fetch the next instruction and advance `pc`.
    ///
    /// `Ok(None)` if control fell off the end of the function body without an
    /// explicit `Return`/`Halt` — treated as an implicit `Return Unit`
    /// (friendlier to hand-written bytecode that omits a trailing return).
    /// `Err` if the frame stack is empty (invariant break).
    ///
    /// Borrows are split on purpose: hold `pc`, look up `chunk.code`, then
    /// write `pc+1` — a single `&mut Frame` across `self.chunk` would not
    /// compile.
    #[inline]
    fn fetch(&mut self) -> Result<Option<Instruction>, Fault> {
        let pc = self.current()?.pc();
        let instr = self.chunk.code.get(pc).copied();
        if instr.is_some() {
            self.current()?.set_pc(pc + 1);
        }
        Ok(instr)
    }

    fn numeric_binop(&mut self, op: Opcode, dst: u8, lhs: u8, rhs: u8) -> Result<(), Fault> {
        let a = self.get_reg(lhs)?;
        let b = self.get_reg(rhs)?;
        let result = match (op, &a, &b) {
            (Opcode::Add, Value::Int(x), Value::Int(y)) => Value::Int(x.wrapping_add(*y)),
            (Opcode::Add, _, _) => Value::Float(as_f64(&a)? + as_f64(&b)?),
            (Opcode::Sub, Value::Int(x), Value::Int(y)) => Value::Int(x.wrapping_sub(*y)),
            (Opcode::Sub, _, _) => Value::Float(as_f64(&a)? - as_f64(&b)?),
            (Opcode::Mul, Value::Int(x), Value::Int(y)) => Value::Int(x.wrapping_mul(*y)),
            (Opcode::Mul, _, _) => Value::Float(as_f64(&a)? * as_f64(&b)?),
            (Opcode::Div, Value::Int(x), Value::Int(y)) => {
                if *y == 0 {
                    return Err(Fault::DivideByZero);
                }
                Value::Int(x.wrapping_div(*y))
            }
            (Opcode::Div, _, _) => {
                let denom = as_f64(&b)?;
                Value::Float(as_f64(&a)? / denom)
            }
            (Opcode::Mod, Value::Int(x), Value::Int(y)) => {
                if *y == 0 {
                    return Err(Fault::DivideByZero);
                }
                Value::Int(x.wrapping_rem(*y))
            }
            (Opcode::Mod, _, _) => {
                let got = if !matches!(a, Value::Int(_)) {
                    a.type_name()
                } else {
                    b.type_name()
                };
                return Err(Fault::TypeMismatch {
                    expected: "int",
                    got,
                });
            }
            (Opcode::Eq, _, _) => Value::Bool(a == b),
            (Opcode::Lt, Value::Int(x), Value::Int(y)) => Value::Bool(x < y),
            (Opcode::Lt, _, _) => Value::Bool(as_f64(&a)? < as_f64(&b)?),
            (Opcode::Le, Value::Int(x), Value::Int(y)) => Value::Bool(x <= y),
            (Opcode::Le, _, _) => Value::Bool(as_f64(&a)? <= as_f64(&b)?),
            _ => {
                return Err(Fault::Invariant(
                    "numeric_binop called with a non-arithmetic opcode",
                ))
            }
        };
        self.set_reg(dst, result)
    }

    #[inline]
    fn expect_cap_reg(&self, reg: u8) -> Result<CapId, Fault> {
        let v = self.get_reg(reg)?;
        v.as_cap().ok_or(Fault::TypeMismatch {
            expected: "cap",
            got: v.type_name(),
        })
    }

    #[inline]
    fn expect_message_value(&self, reg: u8) -> Result<Value, Fault> {
        let v = self.get_reg(reg)?;
        if v.as_message().is_none() {
            return Err(Fault::TypeMismatch {
                expected: "message",
                got: v.type_name(),
            });
        }
        Ok(v)
    }

    #[inline]
    fn expect_str_reg(&self, reg: u8) -> Result<Arc<str>, Fault> {
        match self.get_reg(reg)? {
            Value::Str(s) => Ok(s),
            other => Err(Fault::TypeMismatch {
                expected: "str",
                got: other.type_name(),
            }),
        }
    }

    #[inline]
    fn tag_from_reg(&self, reg: u8) -> Result<u16, Fault> {
        tag_from_value(&self.get_reg(reg)?)
    }

    #[inline]
    fn request_id_from_reg(&self, reg: u8) -> Result<u64, Fault> {
        request_id_from_value(&self.get_reg(reg)?)
    }

    fn gather_regs(&self, base: u8, argc: u8, first_offset: u16) -> Result<Vec<Value>, Fault> {
        let mut args = Vec::with_capacity(argc as usize);
        for i in 0..argc {
            let reg = reg_at(base, first_offset + u16::from(i))?;
            args.push(self.get_reg(reg)?);
        }
        Ok(args)
    }

    /// Same policy as `Sleep`: non-negative `Int` only; never silent clamp.
    #[inline]
    fn duration_millis_from_reg(&self, reg: u8) -> Result<Duration, Fault> {
        let millis = self.get_reg(reg)?;
        match millis.as_int() {
            Some(ms) if ms >= 0 => Ok(Duration::from_millis(ms as u64)),
            _ => Err(Fault::TypeMismatch {
                expected: "non-negative int",
                got: millis.type_name(),
            }),
        }
    }

    /// Apply a relative jump from the current (post-fetch) `pc`.
    ///
    /// Valid targets are `0..=code.len()` (one past the end = implicit return).
    /// Paranoid mode traps; otherwise preserves the historical fail-open cast.
    fn apply_relative_jump(&mut self, imm: i32) -> Result<(), Fault> {
        let code_len = self.chunk.code.len();
        let paranoid = self.paranoid_jumps;
        let frame = self.current()?;
        let target_i = frame.pc() as i64 + i64::from(imm);
        if paranoid && (target_i < 0 || target_i as usize > code_len) {
            let target = if target_i < 0 {
                usize::MAX
            } else {
                target_i as usize
            };
            return Err(Fault::BadJump { target, code_len });
        }
        frame.set_pc(target_i as usize);
        Ok(())
    }

    /// Run at most `budget` instructions (cooperative-preemption quantum,
    /// design notes §10-11), or until the Flow completes / needs an
    /// effect the scheduler must perform / faults.
    ///
    /// Every exit path is captured by [`VmResult`] — this function itself
    /// never panics on malformed *verified* bytecode; faults are returned,
    /// not thrown, so a buggy Flow can't take a worker thread down.
    pub fn run(&mut self, budget: u32) -> VmResult {
        macro_rules! trap {
            ($e:expr) => {
                match $e {
                    Ok(v) => v,
                    Err(fault) => return VmResult::Trap(fault),
                }
            };
        }

        for _ in 0..budget {
            self.instructions_executed += 1;
            let instr = match self.fetch() {
                Ok(Some(i)) => i,
                Ok(None) => {
                    // Fell off the end of a function: implicit `return Unit`.
                    match self.pop_frame(Value::Unit) {
                        Ok(Some(result)) => return result,
                        Ok(None) => continue,
                        Err(fault) => return VmResult::Trap(fault),
                    }
                }
                Err(fault) => return VmResult::Trap(fault),
            };

            match instr.op {
                Opcode::Halt => return VmResult::Complete(trap!(self.get_reg(0))),
                Opcode::Nop => {}
                Opcode::LoadConst => {
                    let idx = instr.imm as u32;
                    let konst = match self.chunk.constant(idx) {
                        Some(v) => v.clone(),
                        None => {
                            return VmResult::Trap(Fault::BadConstant {
                                index: idx,
                                pool_size: self.chunk.constants.len() as u32,
                            })
                        }
                    };
                    trap!(self.set_reg(instr.a, konst));
                }
                Opcode::LoadImm => {
                    trap!(self.set_reg(instr.a, Value::Int(instr.imm as i64)));
                }
                Opcode::Move => {
                    let v = trap!(self.get_reg(instr.b));
                    trap!(self.set_reg(instr.a, v));
                }
                Opcode::Add | Opcode::Sub | Opcode::Mul | Opcode::Div | Opcode::Mod
                | Opcode::Eq | Opcode::Lt | Opcode::Le => {
                    trap!(self.numeric_binop(instr.op, instr.a, instr.b, instr.c));
                }
                Opcode::Neg => {
                    let v = trap!(self.get_reg(instr.b));
                    let negated = match v {
                        Value::Int(x) => Value::Int(x.wrapping_neg()),
                        Value::Float(x) => Value::Float(-x),
                        other => {
                            return VmResult::Trap(Fault::TypeMismatch {
                                expected: "int or float",
                                got: other.type_name(),
                            })
                        }
                    };
                    trap!(self.set_reg(instr.a, negated));
                }
                Opcode::Jump => {
                    trap!(self.apply_relative_jump(instr.imm));
                }
                Opcode::Branch => {
                    let cond = trap!(self.get_reg(instr.a));
                    if !cond.is_truthy() {
                        trap!(self.apply_relative_jump(instr.imm));
                    }
                }
                Opcode::Call => {
                    let function = instr.imm as u32;
                    let argc = instr.b;
                    let dst = instr.a;
                    if self.frames.len() >= MAX_CALL_DEPTH {
                        return VmResult::Trap(Fault::CallStackOverflow { depth: self.frames.len() });
                    }
                    let (num_registers, arity, entry) = match self.chunk.function(function) {
                        Some(d) => (d.num_registers, d.arity, d.entry as usize),
                        None => {
                            return VmResult::Trap(Fault::BadFunction {
                                index: function,
                                table_size: self.chunk.functions.len() as u32,
                            })
                        }
                    };
                    let args = trap!(self.gather_regs(dst, argc, 0));
                    let mut new_frame = Frame::new(function, num_registers, Some(dst));
                    new_frame.set_pc(entry);
                    trap!(load_frame_args(&mut new_frame, args, arity));
                    self.frames.push(new_frame);
                }
                Opcode::CallNative => {
                    let native_index = instr.imm as u32;
                    let argc = instr.b;
                    let dst = instr.a;
                    if let Err(err) = check_native_gate(&self.native_gate, &self.natives, native_index)
                    {
                        return VmResult::Trap(match err {
                            NativeCallError::IndexOutOfRange(index) => Fault::BadNative {
                                index,
                                table_size: self.natives.len() as u32,
                            },
                            other => Fault::NativeDenied(other),
                        });
                    }
                    let args = trap!(self.gather_regs(dst, argc, 0));
                    // Runs inline on this worker thread — see
                    // `NativeFn`'s doc comment on why natives must not
                    // block. This is the actual FFI boundary (design
                    // notes §30-31): plain Rust on one side, bytecode
                    // registers on the other, with `Fault::NativeError`
                    // as the only channel for a native-side failure to
                    // become a Flow fault instead of a host panic.
                    let result = match self.natives.get(native_index) {
                        Some(native_fn) => native_fn(&args),
                        None => {
                            return VmResult::Trap(Fault::BadNative {
                                index: native_index,
                                table_size: self.natives.len() as u32,
                            })
                        }
                    };
                    match result {
                        Ok(value) => trap!(self.set_reg(dst, value)),
                        Err(fault) => return VmResult::Trap(fault),
                    }
                }
                Opcode::Return => {
                    let v = trap!(self.get_reg(instr.a));
                    match self.pop_frame(v) {
                        Ok(Some(result)) => return result,
                        Ok(None) => {}
                        Err(fault) => return VmResult::Trap(fault),
                    }
                }
                Opcode::Spawn => {
                    // Args at r[a+1 .. a+1+argc] — `a` itself receives the
                    // child Cap. `a = 255` overflows on the first gather.
                    let args = trap!(self.gather_regs(instr.a, instr.b, 1));
                    return VmResult::Spawn {
                        function: instr.imm as u32,
                        args,
                        dest_reg: instr.a,
                        requested_rights: crate::bytecode::CapRights::from_u8(instr.c),
                    };
                }
                Opcode::Yield => return VmResult::Yield,
                Opcode::Sleep => {
                    return VmResult::Sleep(trap!(self.duration_millis_from_reg(instr.a)));
                }
                Opcode::Exit => {
                    let v = trap!(self.get_reg(instr.a));
                    return VmResult::Complete(v);
                }
                Opcode::SelfPid => {
                    return VmResult::SelfPid { dest_reg: instr.a };
                }
                Opcode::Send => {
                    let target_cap = trap!(self.expect_cap_reg(instr.a));
                    let message = trap!(self.expect_message_value(instr.b));
                    return VmResult::Send {
                        target_cap,
                        message,
                    };
                }
                Opcode::Receive => {
                    return VmResult::Receive {
                        dest_reg: instr.a,
                        timeout: None,
                        match_tag: None,
                        match_request_id: None,
                    };
                }
                Opcode::ReceiveTimeout => {
                    let timeout = trap!(self.duration_millis_from_reg(instr.b));
                    return VmResult::Receive {
                        dest_reg: instr.a,
                        timeout: Some(timeout),
                        match_tag: None,
                        match_request_id: None,
                    };
                }
                Opcode::ReceiveMatch => {
                    let tag = trap!(self.tag_from_reg(instr.b));
                    return VmResult::Receive {
                        dest_reg: instr.a,
                        timeout: None,
                        match_tag: Some(tag),
                        match_request_id: None,
                    };
                }
                Opcode::ReceiveMatchImm => {
                    let tag = trap!(tag_from_imm(instr.imm));
                    return VmResult::Receive {
                        dest_reg: instr.a,
                        timeout: None,
                        match_tag: Some(tag),
                        match_request_id: None,
                    };
                }
                Opcode::FreshRequestId => {
                    let id = self.fresh_request_id();
                    trap!(self.set_reg(instr.a, Value::Int(id as i64)));
                }
                Opcode::ReceiveMatchCorr => {
                    let tag = trap!(self.tag_from_reg(instr.b));
                    let rid = trap!(self.request_id_from_reg(instr.c));
                    return VmResult::Receive {
                        dest_reg: instr.a,
                        timeout: None,
                        match_tag: Some(tag),
                        match_request_id: Some(rid),
                    };
                }
                Opcode::ReceiveMatchCorrImm => {
                    let tag = trap!(tag_from_imm(instr.imm));
                    let rid = trap!(self.request_id_from_reg(instr.b));
                    return VmResult::Receive {
                        dest_reg: instr.a,
                        timeout: None,
                        match_tag: Some(tag),
                        match_request_id: Some(rid),
                    };
                }
                Opcode::Ask => {
                    let target_cap = trap!(self.expect_cap_reg(instr.b));
                    let request = trap!(self.expect_message_value(instr.c));
                    return VmResult::Ask {
                        dest_reg: instr.a,
                        target_cap,
                        request,
                        timeout: None,
                    };
                }
                Opcode::AskTimeout => {
                    let millis_reg = match u8::try_from(instr.imm) {
                        Ok(r) => r,
                        Err(_) => {
                            return VmResult::Trap(Fault::TypeMismatch {
                                expected: "millis register",
                                got: "imm-out-of-range",
                            })
                        }
                    };
                    let timeout = trap!(self.duration_millis_from_reg(millis_reg));
                    let target_cap = trap!(self.expect_cap_reg(instr.b));
                    let request = trap!(self.expect_message_value(instr.c));
                    return VmResult::Ask {
                        dest_reg: instr.a,
                        target_cap,
                        request,
                        timeout: Some(timeout),
                    };
                }
                Opcode::Monitor => {
                    let target_cap = trap!(self.expect_cap_reg(instr.b));
                    return VmResult::Monitor {
                        dest_reg: instr.a,
                        target_cap,
                    };
                }
                Opcode::Demonitor => {
                    return VmResult::Demonitor {
                        monitor_reg: instr.a,
                    };
                }
                Opcode::Link => {
                    let target_cap = trap!(self.expect_cap_reg(instr.b));
                    return VmResult::Link {
                        dest_reg: instr.a,
                        target_cap,
                    };
                }
                Opcode::Unlink => {
                    return VmResult::Unlink {
                        link_reg: instr.a,
                    };
                }
                Opcode::RegisterName => {
                    return VmResult::RegisterName {
                        name: trap!(self.expect_str_reg(instr.a)),
                    };
                }
                Opcode::Whereis => {
                    return VmResult::Whereis {
                        dest_reg: instr.a,
                        name: trap!(self.expect_str_reg(instr.b)),
                    };
                }
                Opcode::Delegate => {
                    let src_cap = trap!(self.expect_cap_reg(instr.b));
                    let want_native_cap = if instr.c == 255 {
                        None
                    } else {
                        Some(trap!(self.expect_cap_reg(instr.c)))
                    };
                    return VmResult::Delegate {
                        dest_reg: instr.a,
                        src_cap,
                        want_rights: crate::bytecode::CapRights::from_bits(instr.imm as u32),
                        want_native_cap,
                    };
                }
                Opcode::Trap => return VmResult::Trap(Fault::Explicit(instr.imm)),
            }
        }
        VmResult::Yield
    }

    /// Pop the current frame, delivering `value` to the caller (or
    /// finishing the Flow if this was the outermost frame). Returns
    /// `Ok(Some(VmResult::Complete(_)))` only in the latter case.
    fn pop_frame(&mut self, value: Value) -> Result<Option<VmResult>, Fault> {
        let finished = self
            .frames
            .pop()
            .ok_or(Fault::Invariant("pop_frame on empty stack"))?;
        match finished.dest_reg() {
            Some(dest) => {
                self.set_reg(dest, value)?;
                Ok(None)
            }
            None => Ok(Some(VmResult::Complete(value))),
        }
    }
}

/// The register index `base + offset`, or [`Fault::RegisterIndexOverflow`]
/// if that sum leaves the register index space.
///
/// Every multi-operand opcode gathers its arguments from consecutive
/// registers. Written as a plain `base + offset` on `u8`, that addition
/// panics in debug and **wraps** in release — so a release build silently
/// reads the wrong register instead of failing, which is the worst of the
/// two outcomes and the one a debug-mode test suite never sees. The sum is
/// therefore computed in a wider type and narrowed explicitly.
#[inline]
fn load_frame_args(
    frame: &mut Frame,
    args: impl IntoIterator<Item = Value>,
    arity: u8,
) -> Result<(), Fault> {
    let frame_size = frame.registers().len() as u8;
    for (i, a) in args.into_iter().enumerate().take(arity as usize) {
        match frame.registers_mut().get_mut(i) {
            Some(slot) => *slot = a,
            None => return Err(Fault::RegisterOutOfRange { reg: i as u8, frame_size }),
        }
    }
    Ok(())
}

fn reg_at(base: u8, offset: u16) -> Result<u8, Fault> {
    match u8::try_from(u32::from(base) + u32::from(offset)) {
        Ok(reg) => Ok(reg),
        Err(_) => Err(Fault::RegisterIndexOverflow {
            base,
            offset: match u8::try_from(offset) {
                Ok(o) => o,
                Err(_) => u8::MAX,
            },
        }),
    }
}

/// Bytes charged for a register-resident heap value (`Str` / `Bytes` len).
#[inline]
fn heap_charge_of(value: &Value) -> usize {
    match value {
        Value::Str(s) => s.len(),
        Value::Bytes(b) => b.len(),
        _ => 0,
    }
}

/// Reserve `bytes` on the flow quota and optional runtime budget.
/// Rolls back the flow charge if the global budget refuses.
fn charge_pair(
    quota: &FlowQuota,
    memory: Option<&crate::MemoryBudget>,
    bytes: usize,
) -> Result<(), Fault> {
    if bytes == 0 {
        return Ok(());
    }
    quota
        .alloc(bytes)
        .map_err(|e| Fault::QuotaExceeded(e.to_string()))?;
    if let Some(mem) = memory {
        if let Err(e) = mem.try_charge(bytes) {
            quota.free(bytes);
            return Err(Fault::QuotaExceeded(e.to_string()));
        }
    }
    Ok(())
}

fn release_pair(quota: &FlowQuota, memory: Option<&crate::MemoryBudget>, bytes: usize) {
    if bytes == 0 {
        return;
    }
    quota.free(bytes);
    if let Some(mem) = memory {
        mem.release(bytes);
    }
}

/// Charge `Str` / `Bytes` length (initial attach / frame scan).
fn charge_heap_value(
    quota: &FlowQuota,
    memory: Option<&crate::MemoryBudget>,
    value: &Value,
) -> Result<(), Fault> {
    charge_pair(quota, memory, heap_charge_of(value))
}

fn as_f64(v: &Value) -> Result<f64, Fault> {
    match v {
        Value::Int(i) => Ok(*i as f64),
        Value::Float(f) => Ok(*f),
        other => Err(Fault::TypeMismatch { expected: "int or float", got: other.type_name() }),
    }
}

/// Decode a Message tag from a register value (`Int` in `0..=u16::MAX`).
#[inline]
fn tag_from_imm(imm: i32) -> Result<u16, Fault> {
    match u16::try_from(imm) {
        Ok(t) if imm >= 0 => Ok(t),
        _ => Err(Fault::TypeMismatch {
            expected: "tag u16",
            got: "imm-out-of-range",
        }),
    }
}

fn tag_from_value(v: &Value) -> Result<u16, Fault> {
    match v.as_int() {
        Some(i) if (0..=i64::from(u16::MAX)).contains(&i) => Ok(i as u16),
        Some(_) => Err(Fault::TypeMismatch {
            expected: "tag u16",
            got: "int-out-of-range",
        }),
        None => Err(Fault::TypeMismatch {
            expected: "int",
            got: v.type_name(),
        }),
    }
}

/// Decode a `request_id` from a register (`Int` in `0..=i64::MAX`).
#[inline]
fn request_id_from_value(v: &Value) -> Result<u64, Fault> {
    match v.as_int() {
        Some(i) if i >= 0 => Ok(i as u64),
        Some(_) => Err(Fault::TypeMismatch {
            expected: "request_id u64",
            got: "negative-int",
        }),
        None => Err(Fault::TypeMismatch {
            expected: "int",
            got: v.type_name(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::builder::ChunkBuilder;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// `Spawn a=255` reads its arguments from `a+1`, so the first index
    /// already leaves the register space. This used to panic in debug and —
    /// far worse — wrap around to `r0` in release, silently spawning with the
    /// wrong argument.
    #[test]
    fn spawn_from_the_last_register_traps_instead_of_wrapping() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        b.begin_function("main", 0, 2);
        b.emit_spawn(255, 0, 1);
        b.emit_return(0);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        match vm.run(10) {
            VmResult::Trap(Fault::RegisterIndexOverflow {
                base: 255,
                offset: 1,
            }) => Ok(()),
            other => Err(format!("expected RegisterIndexOverflow, got {other:?}").into()),
        }
    }

    /// `Vm::new` is public and reachable without `verify`, and its panic
    /// landed on the caller's thread — the embedder's on `Runtime::spawn`, or
    /// a worker's on bytecode `Spawn`, outside the `catch_unwind`.
    #[test]
    fn entering_a_function_with_too_few_registers_faults() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        b.begin_function("main", 3, 1);
        b.emit_return(0);
        let args = [Value::Int(1), Value::Int(2), Value::Int(3)];
        match Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &args) {
            Err(Fault::RegisterOutOfRange {
                reg: 1,
                frame_size: 1,
            }) => Ok(()),
            Err(e) => Err(format!("unexpected fault: {e}").into()),
            Ok(_) => Err("three arguments cannot be loaded into one register".into()),
        }
    }

    #[test]
    fn calling_a_function_with_too_few_registers_traps() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        let callee = b.begin_function("callee", 3, 1);
        b.emit_return(0);
        let main = b.begin_function("main", 0, 4);
        b.emit_load_imm(0, 7);
        b.emit_load_imm(1, 8);
        b.emit_load_imm(2, 9);
        b.emit_call(0, callee, 3);
        b.emit_return(0);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), main, &[])?;
        match vm.run(50) {
            VmResult::Trap(Fault::RegisterOutOfRange {
                reg: 1,
                frame_size: 1,
            }) => Ok(()),
            other => Err(format!("expected RegisterOutOfRange, got {other:?}").into()),
        }
    }

    /// The boundary case that must keep working: gathering right up to the
    /// last register is legal, it is only going *past* it that faults.
    #[test]
    fn gathering_up_to_the_last_register_still_works() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        let callee = b.begin_function("callee", 1, 1);
        b.emit_return(0);
        let main = b.begin_function("main", 0, 255);
        b.emit_load_imm(254, 5);
        // Argument at r254 — the highest index a 255-register frame has.
        b.emit_call(254, callee, 1);
        b.emit_return(254);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), main, &[])?;
        match vm.run(100) {
            VmResult::Complete(_) => Ok(()),
            other => Err(format!("expected completion, got {other:?}").into()),
        }
    }

    #[test]
    fn neg_i64_min_wrapping() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        b.begin_function("main", 0, 1);
        let k = b.const_(Value::Int(i64::MIN));
        b.emit_load_const(0, k);
        b.emit_neg(0, 0);
        b.emit_return(0);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        match vm.run(10) {
            VmResult::Complete(Value::Int(v)) if v == i64::MIN => Ok(()),
            other => Err(format!("expected Complete(Int(i64::MIN)), got {other:?}").into()),
        }
    }

    #[test]
    fn mod_non_int_type_mismatch() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        b.begin_function("main", 0, 3);
        b.emit_load_imm(0, 10);
        let kf = b.const_(Value::Float(3.0));
        b.emit_load_const(1, kf);
        b.emit_binop(Opcode::Mod, 2, 0, 1);
        b.emit_return(2);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        match vm.run(20) {
            VmResult::Trap(Fault::TypeMismatch {
                expected: "int",
                got: "float",
            }) => Ok(()),
            other => Err(format!("expected TypeMismatch, got {other:?}").into()),
        }
    }

    #[test]
    fn receive_timeout_negative_traps() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        b.begin_function("main", 0, 2);
        b.emit_load_imm(1, -1);
        b.emit_receive_timeout(0, 1);
        b.emit_return(0);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        match vm.run(10) {
            VmResult::Trap(Fault::TypeMismatch {
                expected: "non-negative int",
                ..
            }) => Ok(()),
            other => Err(format!("expected TypeMismatch, got {other:?}").into()),
        }
    }

    #[test]
    fn ask_timeout_negative_traps() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        b.begin_function("main", 0, 4);
        b.emit_load_imm(3, -5);
        // Cap/message regs are garbage; millis is checked first.
        b.emit_ask_timeout(0, 1, 2, 3);
        b.emit_return(0);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        match vm.run(10) {
            VmResult::Trap(Fault::TypeMismatch {
                expected: "non-negative int",
                ..
            }) => Ok(()),
            other => Err(format!("expected TypeMismatch, got {other:?}").into()),
        }
    }

    #[test]
    fn paranoid_jump_out_of_range_traps() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        b.begin_function("main", 0, 1);
        b.emit_load_imm(0, 0);
        b.emit_return(0);
        let mut chunk = b.finish();
        let code_len = chunk.code.len();
        chunk.code[0] = Instruction::only_imm(Opcode::Jump, 100);
        let mut vm = Vm::new(Arc::new(chunk), NativeTable::empty(), 0, &[])?;
        vm.set_paranoid_jumps(true);
        match vm.run(10) {
            VmResult::Trap(Fault::BadJump {
                target: 101,
                code_len: len,
            }) if len == code_len => Ok(()),
            other => Err(format!("expected BadJump(101), got {other:?}").into()),
        }
    }

    #[test]
    fn overwrite_releases_previous_heap_charge() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        let k1 = b.const_(Value::Str("a".repeat(100).into()));
        let k2 = b.const_(Value::Str("b".repeat(100).into()));
        b.begin_function("main", 0, 1);
        b.emit_load_const(0, k1);
        b.emit_load_const(0, k2);
        b.emit_return(0);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        // Limit 150: first 100 fits; without release the second 100 would fail.
        let quota = Arc::new(crate::scheduler::FlowQuota::from_config(
            crate::QuotaConfig {
                mem_limit: 150,
                ..crate::QuotaConfig::permissive()
            },
        ));
        let memory = Arc::new(crate::MemoryBudget::new(10_000));
        vm.set_memory_budget(Arc::clone(&memory));
        vm.set_quota(Arc::clone(&quota))?;
        match vm.run(20) {
            VmResult::Complete(Value::Str(s)) if s.len() == 100 => {
                assert_eq!(quota.mem_used(), 100);
                assert_eq!(memory.used(), 100);
                Ok(())
            }
            other => Err(format!("expected Complete(100-byte Str), got {other:?}").into()),
        }
    }

    #[test]
    fn returning_over_quota_keeps_quota_fault() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        let k = b.const_(Value::Str("x".repeat(200).into()));
        let callee = b.begin_function("callee", 0, 1);
        b.emit_load_const(0, k);
        b.emit_return(0);
        let main = b.begin_function("main", 0, 1);
        b.emit_call(0, callee, 0);
        b.emit_return(0);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), main, &[])?;
        // 200 fits the callee store; writeback into the caller charges again.
        let quota = Arc::new(crate::scheduler::FlowQuota::new(10_000, 250, 1, 1, 1, 1));
        vm.set_quota(quota)?;
        match vm.run(50) {
            VmResult::Trap(Fault::QuotaExceeded(_)) => Ok(()),
            other => Err(format!("expected QuotaExceeded from return writeback, got {other:?}").into()),
        }
    }

    #[test]
    fn call_native_without_gate_is_typed_denied() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        b.begin_function("main", 0, 1);
        b.emit_call_native(0, 0, 0);
        b.emit_return(0);
        let table = NativeTable::builder()
            .register("noop", |_| Ok(Value::Unit))?;
        let mut vm = Vm::new(Arc::new(b.finish()), table.build(), 0, &[])?;
        match vm.run(10) {
            VmResult::Trap(Fault::NativeDenied(NativeCallError::NoNativeRight)) => Ok(()),
            other => Err(format!("expected NativeDenied(NoNativeRight), got {other:?}").into()),
        }
    }

    #[test]
    fn non_paranoid_bad_jump_still_fail_opens() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        b.begin_function("main", 0, 1);
        b.emit_load_imm(0, 7);
        b.emit_return(0);
        let mut chunk = b.finish();
        chunk.code[0] = Instruction::only_imm(Opcode::Jump, 100);
        let mut vm = Vm::new(Arc::new(chunk), NativeTable::empty(), 0, &[])?;
        match vm.run(10) {
            // Fall off the end → implicit return Unit from outermost frame.
            VmResult::Complete(Value::Unit) => Ok(()),
            other => Err(format!("expected Complete(Unit), got {other:?}").into()),
        }
    }
}