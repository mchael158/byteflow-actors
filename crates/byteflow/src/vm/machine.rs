use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::bytecode::{CapId, Chunk, Instruction, Opcode, Value};
use crate::scheduler::FlowQuota;

use super::fault::{Fault, NativeCallError};
use super::frame::Frame;
use super::native::{check_native_gate, NativeGate, NativeTable};
use super::result::VmResult;

/// Bytes a dictionary slot owns. Scalars are free. `Str` / `Bytes` charge
/// their length even when a register still holds the same `Arc`: the
/// dictionary keeps the buffer after that register is overwritten.
fn dict_owned_bytes(value: &Value) -> Result<usize, Fault> {
    match value {
        Value::Unit | Value::Bool(_) | Value::Int(_) | Value::Float(_) => Ok(0),
        Value::Str(s) => Ok(s.len()),
        Value::Bytes(b) => Ok(b.len()),
        other => Err(Fault::TypeMismatch {
            expected: "dict value (scalar, str, or bytes)",
            got: other.type_name(),
        }),
    }
}

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
    /// Retained for API / `RuntimeConfig` wiring. Out-of-range jumps always
    /// trap with [`Fault::BadJump`] (fail-closed); this flag no longer gates
    /// the check.
    #[allow(dead_code)]
    paranoid_jumps: bool,
    /// BEAM process dictionary. Allocated on first `DictPut` so the common
    /// flow that never touches the dict pays nothing.
    dict: Option<HashMap<i64, Value>>,
    /// Heap bytes charged for `Str` / `Bytes` stored in [`Self::dict`].
    /// Separate from frame charges: overwriting the source register must not
    /// free a buffer the dictionary still owns.
    dict_bytes: usize,
}

impl Vm {
    /// Construct a `Vm` ready to run `function` (an index into
    /// `chunk.functions`) with the given arguments loaded into `r0..argc`.
    /// `natives` is the FFI table `Opcode::CallNative` dispatches through —
    /// pass [`NativeTable::empty`] if the chunk never calls out to Rust.
    ///
    /// Native calls are **denied** until [`Self::with_native_gate`] installs
    /// an allowlist (the runtime does this from the flow's attenuated Cap).
    pub fn new(
        chunk: Arc<Chunk>,
        natives: Arc<NativeTable>,
        function: u32,
        args: &[Value],
    ) -> Result<Self, Fault> {
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
        let def = chunk.function(function).ok_or(Fault::BadFunction {
            index: function,
            table_size: chunk.functions.len() as u32,
        })?;
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
            dict: None,
            dict_bytes: 0,
        })
    }

    /// Historical API: out-of-range `Jump` / `Branch` always trap with
    /// [`Fault::BadJump`]. The flag is retained for `RuntimeConfig` compatibility
    /// but does not change jump semantics.
    pub fn set_paranoid_jumps(&mut self, enabled: bool) {
        self.paranoid_jumps = enabled;
    }

    /// Attach the flow's quota so register stores of `Str`/`Bytes` charge heap.
    ///
    /// Charges the **total** current register heap in one reservation, then
    /// installs the quota. A failed reservation leaves `self.quota` unset and
    /// does not partially inflate the budget. Same `Arc` rewritten into the
    /// same slot is not charged twice on later stores.
    pub fn set_quota(&mut self, quota: Arc<FlowQuota>) -> Result<(), Fault> {
        let mut per_frame: Vec<usize> = Vec::with_capacity(self.frames.len());
        let mut total = 0usize;
        for frame in &self.frames {
            let mut bytes = 0usize;
            for value in frame.registers() {
                let part = heap_charge_of(value)?;
                bytes = match bytes.checked_add(part) {
                    Some(sum) => sum,
                    None => {
                        return Err(Fault::QuotaExceeded("heap charge does not fit".into()));
                    }
                };
            }
            total = match total.checked_add(bytes) {
                Some(sum) => sum,
                None => return Err(Fault::QuotaExceeded("heap charge does not fit".into())),
            };
            per_frame.push(bytes);
        }
        total = match total.checked_add(self.dict_bytes) {
            Some(sum) => sum,
            None => return Err(Fault::QuotaExceeded("heap charge does not fit".into())),
        };
        charge_pair(&quota, self.memory.as_deref(), total)?;
        for (frame, bytes) in self.frames.iter_mut().zip(per_frame) {
            frame.set_heap_charge(bytes);
        }
        self.quota = Some(quota);
        Ok(())
    }

    /// Attach the process-wide memory ceiling (call before or with [`Self::set_quota`]).
    pub fn set_memory_budget(&mut self, memory: Arc<crate::MemoryBudget>) {
        self.memory = Some(memory);
    }

    /// Mint a per-flow correlation id.
    ///
    /// Stays in the `Value::Int` (`i64`) domain so
    /// `FreshRequestId` → `Int` → [`request_id_from_value`] never produces a
    /// negative id. Never returns `0` (`next_request_id` starts at 1).
    pub fn fresh_request_id(&mut self) -> u64 {
        let id = self.next_request_id;
        if self.next_request_id < i64::MAX as u64 {
            self.next_request_id += 1;
        }
        id
    }

    #[cfg(test)]
    fn set_next_request_id_for_test(&mut self, id: u64) {
        self.next_request_id = id;
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

    /// Write a register. Bounds-check the slot **before** any heap charge so
    /// a failed store cannot inflate FlowQuota / MemoryBudget.
    #[inline]
    fn set_reg(&mut self, reg: u8, value: Value) -> Result<(), Fault> {
        let frame = self
            .frames
            .last()
            .ok_or(Fault::Invariant("empty frame stack while running"))?;
        let frame_size = frame.registers().len() as u8;
        let old = frame
            .registers()
            .get(reg as usize)
            .ok_or(Fault::RegisterOutOfRange { reg, frame_size })?;

        // `old` is `&Value`; keep both sides as references for the helper.
        let old_bytes = heap_charge_of(old)?;
        let new_bytes = heap_charge_of(&value)?;

        match (&value, old) {
            (Value::Str(s), Value::Str(prev)) if Arc::ptr_eq(s, prev) => return Ok(()),
            (Value::Bytes(b), Value::Bytes(prev)) if Arc::ptr_eq(b, prev) => return Ok(()),
            _ => {}
        }

        if let Some(quota) = self.quota.as_ref() {
            if new_bytes > old_bytes {
                charge_pair(quota, self.memory.as_deref(), new_bytes - old_bytes)?;
            } else if old_bytes > new_bytes {
                release_pair(quota, self.memory.as_deref(), old_bytes - new_bytes);
            }
        }

        let frame = self.current()?;
        if new_bytes > old_bytes {
            let delta = new_bytes - old_bytes;
            if !frame.try_add_heap_charge(delta) {
                if let Some(quota) = self.quota.as_ref() {
                    release_pair(quota, self.memory.as_deref(), delta);
                }
                return Err(Fault::QuotaExceeded("heap charge does not fit".into()));
            }
        } else if old_bytes > new_bytes {
            frame.sub_heap_charge(old_bytes - new_bytes);
        }
        match frame.registers_mut().get_mut(reg as usize) {
            Some(slot) => {
                *slot = value;
                Ok(())
            }
            None => Err(Fault::RegisterOutOfRange {
                reg,
                frame_size: frame.registers().len() as u8,
            }),
        }
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

    fn add_imm(&mut self, dst: u8, src: u8, imm: i32) -> Result<(), Fault> {
        let src_val = self.get_reg(src)?;
        let addend = i64::from(imm);
        let result = match src_val {
            Value::Int(x) => Value::Int(x.wrapping_add(addend)),
            Value::Float(x) => Value::Float(x + (addend as f64)),
            other => {
                return Err(Fault::TypeMismatch {
                    expected: "int or float",
                    got: other.type_name(),
                })
            }
        };
        self.set_reg(dst, result)
    }

    fn sub_imm(&mut self, dst: u8, src: u8, imm: i32) -> Result<(), Fault> {
        let src_val = self.get_reg(src)?;
        let subtrahend = i64::from(imm);
        let result = match src_val {
            Value::Int(x) => Value::Int(x.wrapping_sub(subtrahend)),
            Value::Float(x) => Value::Float(x - (subtrahend as f64)),
            other => {
                return Err(Fault::TypeMismatch {
                    expected: "int or float",
                    got: other.type_name(),
                })
            }
        };
        self.set_reg(dst, result)
    }

    fn lt_imm(&mut self, dst: u8, src: u8, imm: i32) -> Result<(), Fault> {
        let src_val = self.get_reg(src)?;
        let rhs = i64::from(imm);
        let result = match src_val {
            Value::Int(x) => Value::Bool(x < rhs),
            Value::Float(x) => Value::Bool(x < (rhs as f64)),
            other => {
                return Err(Fault::TypeMismatch {
                    expected: "int or float",
                    got: other.type_name(),
                })
            }
        };
        self.set_reg(dst, result)
    }

    fn list_pack(&mut self, dst: u8, base: u8, count: i32) -> Result<(), Fault> {
        if !(0..=crate::bytecode::MAX_LIST_LEN as i32).contains(&count) {
            return Err(Fault::TypeMismatch {
                expected: "list length 0..=32",
                got: "imm-out-of-range",
            });
        }
        let mut items = Vec::with_capacity(count as usize);
        for offset in 0..count {
            let reg = reg_at(base, offset as u16)?;
            items.push(self.get_reg(reg)?);
        }
        let list = match Value::try_list(items) {
            Some(list) => list,
            None => return Err(Fault::QuotaExceeded("value nesting exceeds limit".into())),
        };
        self.set_reg(dst, list)
    }

    fn list_get(&mut self, dst: u8, src: u8, index: i32) -> Result<(), Fault> {
        let value = self.get_reg(src)?;
        let Value::List(items) = value else {
            return Err(Fault::TypeMismatch {
                expected: "list",
                got: value.type_name(),
            });
        };
        if index < 0 {
            return Err(Fault::TypeMismatch {
                expected: "list index",
                got: "negative-int",
            });
        }
        match items.get(index as usize) {
            Some(item) => self.set_reg(dst, item.clone()),
            None => Err(Fault::TypeMismatch {
                expected: "list index",
                got: "out-of-range",
            }),
        }
    }

    fn list_len(&mut self, dst: u8, src: u8) -> Result<(), Fault> {
        let value = self.get_reg(src)?;
        let Value::List(items) = value else {
            return Err(Fault::TypeMismatch {
                expected: "list",
                got: value.type_name(),
            });
        };
        self.set_reg(dst, Value::Int(items.len() as i64))
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
    fn int_from_reg(&self, reg: u8) -> Result<i64, Fault> {
        match self.get_reg(reg)? {
            Value::Int(n) => Ok(n),
            other => Err(Fault::TypeMismatch {
                expected: "int",
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

    /// Move `old_bytes` of dictionary charge to `new_bytes`.
    ///
    /// The counter is not the register frame. `pop_frame` must not release it.
    fn adjust_dict_quota(&mut self, old_bytes: usize, new_bytes: usize) -> Result<(), Fault> {
        if new_bytes > old_bytes {
            let delta = new_bytes - old_bytes;
            if let Some(quota) = self.quota.as_ref() {
                charge_pair(quota, self.memory.as_deref(), delta)?;
            }
            self.dict_bytes = match self.dict_bytes.checked_add(delta) {
                Some(next) => next,
                None => {
                    if let Some(quota) = self.quota.as_ref() {
                        release_pair(quota, self.memory.as_deref(), delta);
                    }
                    return Err(Fault::QuotaExceeded("heap charge does not fit".into()));
                }
            };
        } else if old_bytes > new_bytes {
            let delta = old_bytes - new_bytes;
            if let Some(quota) = self.quota.as_ref() {
                release_pair(quota, self.memory.as_deref(), delta);
            }
            self.dict_bytes = self.dict_bytes.saturating_sub(delta);
        }
        Ok(())
    }

    /// Non-negative `Int` only. `Bool` is not a duration.
    fn duration_millis_from_reg(&self, reg: u8) -> Result<Duration, Fault> {
        let millis = self.get_reg(reg)?;
        match require_int(&millis) {
            Ok(ms) if ms >= 0 => Ok(Duration::from_millis(ms as u64)),
            Ok(_) => Err(Fault::TypeMismatch {
                expected: "non-negative int",
                got: "negative-int",
            }),
            Err(fault) => Err(fault),
        }
    }

    /// Apply a relative jump from the current (post-fetch) `pc`.
    ///
    /// Valid targets are `0..=code.len()` (one past the end = implicit return).
    /// Out-of-range targets always trap with [`Fault::BadJump`] (fail-closed).
    fn apply_relative_jump(&mut self, imm: i32) -> Result<(), Fault> {
        let code_len = self.chunk.code.len();
        let frame = self.current()?;
        let target_i = frame.pc() as i64 + i64::from(imm);
        if target_i < 0 || target_i as usize > code_len {
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
                Opcode::AddImm => {
                    trap!(self.add_imm(instr.a, instr.b, instr.imm));
                }
                Opcode::SubImm => {
                    trap!(self.sub_imm(instr.a, instr.b, instr.imm));
                }
                Opcode::LtImm => {
                    trap!(self.lt_imm(instr.a, instr.b, instr.imm));
                }
                Opcode::ListPack => {
                    trap!(self.list_pack(instr.a, instr.b, instr.imm));
                }
                Opcode::ListGet => {
                    trap!(self.list_get(instr.a, instr.b, instr.imm));
                }
                Opcode::ListLen => {
                    trap!(self.list_len(instr.a, instr.b));
                }
                Opcode::Move => {
                    let v = trap!(self.get_reg(instr.b));
                    trap!(self.set_reg(instr.a, v));
                }
                Opcode::Add
                | Opcode::Sub
                | Opcode::Mul
                | Opcode::Div
                | Opcode::Mod
                | Opcode::Eq
                | Opcode::Lt
                | Opcode::Le => {
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
                        return VmResult::Trap(Fault::CallStackOverflow {
                            depth: self.frames.len(),
                        });
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
                    // `verify` rejects this statically. Direct `Vm::run` on an
                    // unverified chunk must not drop extras or fill holes with
                    // `Unit` — both hide a wrong call.
                    if argc != arity {
                        return VmResult::Trap(Fault::BadArity {
                            passed: usize::from(argc),
                            expected: arity,
                        });
                    }
                    let args = trap!(self.gather_regs(dst, argc, 0));
                    let mut new_frame = Frame::new(function, num_registers, Some(dst));
                    new_frame.set_pc(entry);
                    trap!(self.load_frame_args_charged(&mut new_frame, args, arity));
                    self.frames.push(new_frame);
                }
                Opcode::CallNative | Opcode::CallNativeFrom => {
                    let native_index = instr.imm as u32;
                    // `CallNative` reads and writes `r[a]`. `CallNativeFrom`
                    // reads `r[b]` and writes `r[a]`, so a one-arg unpack does
                    // not destroy the source.
                    let (arg_base, argc) = if instr.op == Opcode::CallNativeFrom {
                        (instr.b, 1)
                    } else {
                        (instr.a, instr.b)
                    };
                    let dst = instr.a;
                    if let Err(err) =
                        check_native_gate(&self.native_gate, &self.natives, native_index)
                    {
                        return VmResult::Trap(match err {
                            NativeCallError::IndexOutOfRange(index) => Fault::BadNative {
                                index,
                                table_size: self.natives.len() as u32,
                            },
                            other => Fault::NativeDenied(other),
                        });
                    }
                    let args = trap!(self.gather_regs(arg_base, argc, 0));
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
                        match_payload_kind: None,
                        match_payload_eq: None,
                        match_payload_value: None,
                    };
                }
                Opcode::ReceiveTimeout => {
                    let timeout = trap!(self.duration_millis_from_reg(instr.b));
                    return VmResult::Receive {
                        dest_reg: instr.a,
                        timeout: Some(timeout),
                        match_tag: None,
                        match_request_id: None,
                        match_payload_kind: None,
                        match_payload_eq: None,
                        match_payload_value: None,
                    };
                }
                Opcode::ReceiveMatch => {
                    let tag = trap!(self.tag_from_reg(instr.b));
                    return VmResult::Receive {
                        dest_reg: instr.a,
                        timeout: None,
                        match_tag: Some(tag),
                        match_request_id: None,
                        match_payload_kind: None,
                        match_payload_eq: None,
                        match_payload_value: None,
                    };
                }
                Opcode::ReceiveMatchImm => {
                    let tag = trap!(tag_from_imm(instr.imm));
                    return VmResult::Receive {
                        dest_reg: instr.a,
                        timeout: None,
                        match_tag: Some(tag),
                        match_request_id: None,
                        match_payload_kind: None,
                        match_payload_eq: None,
                        match_payload_value: None,
                    };
                }
                Opcode::ReceiveMatchTimeout => {
                    let tag = trap!(self.tag_from_reg(instr.b));
                    let timeout = trap!(self.duration_millis_from_reg(instr.c));
                    return VmResult::Receive {
                        dest_reg: instr.a,
                        timeout: Some(timeout),
                        match_tag: Some(tag),
                        match_request_id: None,
                        match_payload_kind: None,
                        match_payload_eq: None,
                        match_payload_value: None,
                    };
                }
                Opcode::ReceiveMatchImmTimeout => {
                    let tag = trap!(tag_from_imm(instr.imm));
                    let timeout = trap!(self.duration_millis_from_reg(instr.b));
                    return VmResult::Receive {
                        dest_reg: instr.a,
                        timeout: Some(timeout),
                        match_tag: Some(tag),
                        match_request_id: None,
                        match_payload_kind: None,
                        match_payload_eq: None,
                        match_payload_value: None,
                    };
                }
                Opcode::IsAlive => {
                    let target_cap = trap!(self.expect_cap_reg(instr.b));
                    return VmResult::IsAlive {
                        dest_reg: instr.a,
                        target_cap,
                    };
                }
                Opcode::ExitSignal => {
                    let target_cap = trap!(self.expect_cap_reg(instr.a));
                    let reason = trap!(self.int_from_reg(instr.b));
                    let Ok(reason_u) = u64::try_from(reason) else {
                        return VmResult::Trap(Fault::TypeMismatch {
                            expected: "FlowExitReason 0..=6",
                            got: "imm-out-of-range",
                        });
                    };
                    if crate::FlowExitReason::from_u64(reason_u).is_none() {
                        return VmResult::Trap(Fault::TypeMismatch {
                            expected: "FlowExitReason 0..=6",
                            got: "imm-out-of-range",
                        });
                    }
                    return VmResult::ExitSignal {
                        target_cap,
                        reason: reason_u,
                    };
                }
                Opcode::DictPut => {
                    let key = trap!(self.int_from_reg(instr.b));
                    let value = trap!(self.get_reg(instr.c));
                    let new_bytes = trap!(dict_owned_bytes(&value));
                    let previous = match self.dict.as_ref().and_then(|d| d.get(&key).cloned()) {
                        Some(prev) => prev,
                        None => Value::Unit,
                    };
                    let old_bytes = trap!(dict_owned_bytes(&previous));
                    // Charge first, write the register second, and only then
                    // replace the slot. `set_reg` of the previous value can
                    // refuse (the old buffer moves into `ra` while `rc` and
                    // the dictionary both still hold the new one). Rolling
                    // the charge back leaves the slot untouched.
                    if let Err(fault) = self.adjust_dict_quota(old_bytes, new_bytes) {
                        return VmResult::Trap(fault);
                    }
                    if let Err(fault) = self.set_reg(instr.a, previous) {
                        if let Err(rollback) = self.adjust_dict_quota(new_bytes, old_bytes) {
                            return VmResult::Trap(rollback);
                        }
                        return VmResult::Trap(fault);
                    }
                    self.dict
                        .get_or_insert_with(HashMap::new)
                        .insert(key, value);
                }
                Opcode::DictGet => {
                    let key = trap!(self.int_from_reg(instr.b));
                    let value = match self.dict.as_ref().and_then(|d| d.get(&key).cloned()) {
                        Some(v) => v,
                        None => Value::Unit,
                    };
                    trap!(self.set_reg(instr.a, value));
                }
                Opcode::DictErase => {
                    let key = trap!(self.int_from_reg(instr.b));
                    // A bad destination must not drop the entry on the way out.
                    trap!(self.get_reg(instr.a));
                    let old = match self.dict.as_mut().and_then(|d| d.remove(&key)) {
                        Some(v) => v,
                        None => Value::Unit,
                    };
                    let old_bytes = trap!(dict_owned_bytes(&old));
                    trap!(self.adjust_dict_quota(old_bytes, 0));
                    trap!(self.set_reg(instr.a, old));
                }
                Opcode::ReceiveMatchEq => {
                    let tag = trap!(self.tag_from_reg(instr.b));
                    let payload = trap!(self.int_from_reg(instr.c));
                    return VmResult::Receive {
                        dest_reg: instr.a,
                        timeout: None,
                        match_tag: Some(tag),
                        match_request_id: None,
                        match_payload_kind: None,
                        match_payload_eq: Some(payload),
                        match_payload_value: None,
                    };
                }
                Opcode::ReceiveMatchValue => {
                    let tag = trap!(self.tag_from_reg(instr.b));
                    let payload = trap!(self.get_reg(instr.c));
                    return VmResult::Receive {
                        dest_reg: instr.a,
                        timeout: None,
                        match_tag: Some(tag),
                        match_request_id: None,
                        match_payload_kind: None,
                        match_payload_eq: None,
                        match_payload_value: Some(Arc::new(payload)),
                    };
                }
                Opcode::SendRemote => {
                    let node = trap!(self.int_from_reg(instr.a));
                    if node < 0 {
                        return VmResult::Trap(Fault::TypeMismatch {
                            expected: "node id >= 0",
                            got: "negative",
                        });
                    }
                    let name = match trap!(self.get_reg(instr.b)) {
                        Value::Str(name) => name,
                        _ => {
                            return VmResult::Trap(Fault::TypeMismatch {
                                expected: "str",
                                got: "not-str",
                            })
                        }
                    };
                    let message = trap!(self.expect_message_value(instr.c));
                    let Ok(node) = u64::try_from(node) else {
                        return VmResult::Trap(Fault::TypeMismatch {
                            expected: "node id",
                            got: "imm-out-of-range",
                        });
                    };
                    return VmResult::SendRemote {
                        node,
                        name,
                        message,
                    };
                }
                Opcode::StartSupervisor => {
                    let Ok(strategy) = u8::try_from(instr.imm) else {
                        return VmResult::Trap(Fault::TypeMismatch {
                            expected: "strategy 0..=2",
                            got: "imm-out-of-range",
                        });
                    };
                    if strategy > 2 {
                        return VmResult::Trap(Fault::TypeMismatch {
                            expected: "strategy 0..=2",
                            got: "imm-out-of-range",
                        });
                    }
                    return VmResult::StartSupervisor { strategy };
                }
                Opcode::StartChild => {
                    let function = match u32::try_from(instr.imm) {
                        Ok(f) => f,
                        Err(_) => u32::MAX,
                    };
                    if self.chunk.function(function).is_none() {
                        return VmResult::Trap(Fault::BadFunction {
                            index: function,
                            table_size: self.chunk.functions.len() as u32,
                        });
                    }
                    if crate::RestartPolicy::from_u8(instr.b).is_none() {
                        return VmResult::Trap(Fault::TypeMismatch {
                            expected: "restart policy 0..=2",
                            got: "imm-out-of-range",
                        });
                    }
                    return VmResult::StartChild {
                        dest_reg: instr.a,
                        function,
                        policy: instr.b,
                    };
                }
                Opcode::ReceiveMatchKind => {
                    // Selective receive by payload wire-tag (`Value::wire_tag`,
                    // 0..=8). Non-matching hops stay queued (FIFO skip).
                    let kind = match u8::try_from(instr.imm) {
                        Ok(k) if k <= 9 => k,
                        _ => {
                            return VmResult::Trap(Fault::TypeMismatch {
                                expected: "payload kind 0..=9",
                                got: "imm-out-of-range",
                            })
                        }
                    };
                    return VmResult::Receive {
                        dest_reg: instr.a,
                        timeout: None,
                        match_tag: None,
                        match_request_id: None,
                        match_payload_kind: Some(kind),
                        match_payload_eq: None,
                        match_payload_value: None,
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
                        match_payload_kind: None,
                        match_payload_eq: None,
                        match_payload_value: None,
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
                        match_payload_kind: None,
                        match_payload_eq: None,
                        match_payload_value: None,
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
                    return VmResult::Unlink { link_reg: instr.a };
                }
                Opcode::SetTrapExit => {
                    let enabled = trap!(self.get_reg(instr.a)).is_truthy();
                    return VmResult::SetTrapExit { enabled };
                }
                Opcode::SetRestartPolicy => {
                    // Flow-local OTP restart policy for the host Supervisor
                    // (`0` Always / `1` OnFailure / `2` Never).
                    let policy = match instr.imm {
                        0..=2 => instr.imm as u8,
                        _ => {
                            return VmResult::Trap(Fault::TypeMismatch {
                                expected: "restart policy 0..=2",
                                got: "imm-out-of-range",
                            })
                        }
                    };
                    return VmResult::SetRestartPolicy { policy };
                }
                Opcode::HostAwait => {
                    let args = trap!(self.get_reg(instr.b));
                    let op = if instr.imm < 0 {
                        return VmResult::Trap(Fault::TypeMismatch {
                            expected: "HostAwait op >= 0",
                            got: "imm-negative",
                        });
                    } else {
                        instr.imm as u32
                    };
                    return VmResult::HostAwait {
                        dest_reg: instr.a,
                        op,
                        args,
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
    ///
    /// Releases the finished frame's heap charge (except the escaping
    /// return value on the outermost `Complete` path, which stays charged
    /// until flow finalize — matching prior MemoryBudget semantics).
    fn pop_frame(&mut self, value: Value) -> Result<Option<VmResult>, Fault> {
        let escaping = self
            .frames
            .last()
            .ok_or(Fault::Invariant("pop_frame on empty stack"))?
            .dest_reg()
            .is_none();
        // Charge the escaping value before the pop, so a chain past the
        // decoder ceiling fails without dropping the frame first.
        let keep = if escaping { heap_charge_of(&value)? } else { 0 };
        let finished = self
            .frames
            .pop()
            .ok_or(Fault::Invariant("pop_frame on empty stack"))?;
        match finished.dest_reg() {
            Some(dest) => {
                if let Some(quota) = self.quota.as_ref() {
                    release_pair(quota, self.memory.as_deref(), finished.heap_charge());
                }
                self.set_reg(dest, value)?;
                Ok(None)
            }
            None => {
                if let Some(quota) = self.quota.as_ref() {
                    let release = finished.heap_charge().saturating_sub(keep);
                    release_pair(quota, self.memory.as_deref(), release);
                }
                Ok(Some(VmResult::Complete(value)))
            }
        }
    }

    /// Load call arguments into a not-yet-pushed frame, charging heap for
    /// any `Str`/`Bytes`. Rolls back a partial charge if a later arg fails.
    fn load_frame_args_charged(
        &self,
        frame: &mut Frame,
        args: impl IntoIterator<Item = Value>,
        arity: u8,
    ) -> Result<(), Fault> {
        let frame_size = frame.registers().len() as u8;
        let mut charged = 0usize;
        for (i, a) in args.into_iter().enumerate().take(arity as usize) {
            let bytes = heap_charge_of(&a)?;
            if bytes > 0 {
                if let Some(quota) = self.quota.as_ref() {
                    if let Err(e) = charge_pair(quota, self.memory.as_deref(), bytes) {
                        if charged > 0 {
                            release_pair(quota, self.memory.as_deref(), charged);
                        }
                        frame.set_heap_charge(0);
                        return Err(e);
                    }
                }
                let Some(next) = charged.checked_add(bytes) else {
                    if let Some(quota) = self.quota.as_ref() {
                        release_pair(quota, self.memory.as_deref(), charged);
                        release_pair(quota, self.memory.as_deref(), bytes);
                    }
                    frame.set_heap_charge(0);
                    return Err(Fault::QuotaExceeded("heap charge does not fit".into()));
                };
                if !frame.try_add_heap_charge(bytes) {
                    if let Some(quota) = self.quota.as_ref() {
                        release_pair(quota, self.memory.as_deref(), next);
                    }
                    frame.set_heap_charge(0);
                    return Err(Fault::QuotaExceeded("heap charge does not fit".into()));
                }
                charged = next;
            }
            match frame.registers_mut().get_mut(i) {
                Some(slot) => *slot = a,
                None => {
                    if charged > 0 {
                        if let Some(quota) = self.quota.as_ref() {
                            release_pair(quota, self.memory.as_deref(), charged);
                        }
                        frame.set_heap_charge(0);
                    }
                    return Err(Fault::RegisterOutOfRange {
                        reg: i as u8,
                        frame_size,
                    });
                }
            }
        }
        Ok(())
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
            None => {
                return Err(Fault::RegisterOutOfRange {
                    reg: i as u8,
                    frame_size,
                })
            }
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

/// Bytes charged for a register-resident heap value.
///
/// `Str` / `Bytes` charge their length. A [`Value::Message`] charges the
/// payload, down to the same ceiling as the `.bf` decoder. Stopping at that
/// ceiling and still storing the value would leave the tail unpaid.
#[inline]
fn heap_charge_of(value: &Value) -> Result<usize, Fault> {
    fn walk(value: &Value, depth: u32) -> Result<usize, Fault> {
        if depth > crate::bytecode::MAX_VALUE_DEPTH {
            return Err(Fault::QuotaExceeded("value nesting exceeds limit".into()));
        }
        match value {
            Value::Str(s) => Ok(s.len()),
            Value::Bytes(b) => Ok(b.len()),
            Value::Message(message) => walk(message.payload.as_ref(), depth + 1),
            Value::List(items) => {
                let mut sum = items
                    .len()
                    .checked_mul(std::mem::size_of::<Value>())
                    .ok_or_else(|| Fault::QuotaExceeded("heap charge does not fit".into()))?;
                for item in items.iter() {
                    let part = walk(item, depth + 1)?;
                    sum = match sum.checked_add(part) {
                        Some(total) => total,
                        None => {
                            return Err(Fault::QuotaExceeded("heap charge does not fit".into()));
                        }
                    };
                }
                Ok(sum)
            }
            Value::Unit
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Pid(_)
            | Value::Cap(_) => Ok(0),
        }
    }
    walk(value, 0)
}

/// Bytes to reserve on the sender quota while `value` is in flight.
///
/// The hop is still in a register, and that register already pays for the
/// `Str` / `Bytes` inside it. Charging [`Value::memory_size`] on top of that
/// counts the same buffers twice and rejects a send the quota can hold.
#[inline]
pub(crate) fn in_flight_hop_charge(value: &Value) -> usize {
    match heap_charge_of(value) {
        Ok(held) => value.memory_size().saturating_sub(held),
        Err(_) => value.memory_size(),
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

fn as_f64(v: &Value) -> Result<f64, Fault> {
    match v {
        Value::Int(i) => Ok(*i as f64),
        Value::Float(f) => Ok(*f),
        other => Err(Fault::TypeMismatch {
            expected: "int or float",
            got: other.type_name(),
        }),
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

/// `Int` only. [`Value::as_int`] still coerces `Bool` for natives.
fn require_int(v: &Value) -> Result<i64, Fault> {
    match v {
        Value::Int(n) => Ok(*n),
        other => Err(Fault::TypeMismatch {
            expected: "int",
            got: other.type_name(),
        }),
    }
}

fn tag_from_value(v: &Value) -> Result<u16, Fault> {
    match require_int(v) {
        Ok(i) if (0..=i64::from(u16::MAX)).contains(&i) => Ok(i as u16),
        Ok(_) => Err(Fault::TypeMismatch {
            expected: "tag u16",
            got: "int-out-of-range",
        }),
        Err(fault) => Err(fault),
    }
}

/// Decode a `request_id` from a register (`Int` in `0..=i64::MAX`).
#[inline]
fn request_id_from_value(v: &Value) -> Result<u64, Fault> {
    match require_int(v) {
        Ok(i) if i >= 0 => Ok(i as u64),
        Ok(_) => Err(Fault::TypeMismatch {
            expected: "request_id u64",
            got: "negative-int",
        }),
        Err(fault) => Err(fault),
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
    fn call_with_the_wrong_argc_traps_instead_of_padding() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        let callee = b.begin_function("callee", 0, 1);
        b.emit_load_imm(0, 1);
        b.emit_return(0);
        let main = b.begin_function("main", 0, 2);
        b.emit_load_imm(0, 7);
        b.emit_call(0, callee, 1);
        b.emit_return(0);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), main, &[])?;
        match vm.run(20) {
            VmResult::Trap(Fault::BadArity {
                passed: 1,
                expected: 0,
            }) => Ok(()),
            other => Err(format!("expected BadArity, got {other:?}").into()),
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
    fn return_releases_callee_heap_before_caller_writeback() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        let k = b.const_(Value::Str("x".repeat(200).into()));
        let callee = b.begin_function("callee", 0, 1);
        b.emit_load_const(0, k);
        b.emit_return(0);
        let main = b.begin_function("main", 0, 1);
        b.emit_call(0, callee, 0);
        b.emit_return(0);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), main, &[])?;
        // Old bug: callee charged 200 and return writeback charged again without
        // release → QuotaExceeded at 250. Correct path releases then recharges.
        let quota = Arc::new(crate::scheduler::FlowQuota::new(10_000, 250, 1, 1, 1, 1));
        vm.set_quota(Arc::clone(&quota))?;
        match vm.run(50) {
            VmResult::Complete(Value::Str(s)) if s.len() == 200 => {
                assert_eq!(quota.mem_used(), 200);
                Ok(())
            }
            other => Err(format!("expected Complete(200-byte Str), got {other:?}").into()),
        }
    }

    #[test]
    fn set_reg_out_of_range_does_not_charge() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        b.begin_function("main", 0, 1);
        b.emit_return(0);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        let quota = Arc::new(crate::scheduler::FlowQuota::from_config(
            crate::QuotaConfig {
                mem_limit: 10_000,
                ..crate::QuotaConfig::permissive()
            },
        ));
        vm.set_quota(Arc::clone(&quota))?;
        let big = Value::Str("z".repeat(500).into());
        match vm.set_register(255, big) {
            Err(Fault::RegisterOutOfRange {
                reg: 255,
                frame_size: 1,
            }) => {
                assert_eq!(quota.mem_used(), 0);
                Ok(())
            }
            other => Err(format!("expected RegisterOutOfRange, got {other:?}").into()),
        }
    }

    #[test]
    fn set_quota_is_transactional() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        b.begin_function("main", 0, 2);
        b.emit_return(0);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        // Two 100-byte Strs already in registers; limit 150 must refuse the
        // whole attach without leaving a partial charge on the quota object.
        vm.set_register(0, Value::Str("a".repeat(100).into()))?;
        vm.set_register(1, Value::Str("b".repeat(100).into()))?;
        let quota = Arc::new(crate::scheduler::FlowQuota::from_config(
            crate::QuotaConfig {
                mem_limit: 150,
                ..crate::QuotaConfig::permissive()
            },
        ));
        match vm.set_quota(Arc::clone(&quota)) {
            Err(Fault::QuotaExceeded(_)) => {
                assert_eq!(
                    quota.mem_used(),
                    0,
                    "failed set_quota must not leave a partial charge"
                );
                Ok(())
            }
            other => Err(format!("expected QuotaExceeded, got {other:?}").into()),
        }
    }

    #[test]
    fn message_payload_counts_against_the_heap_quota() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        b.begin_function("main", 0, 1);
        b.emit_return(0);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        let quota = Arc::new(FlowQuota::new(10_000, 64, 1, 1, 1, 1));
        vm.set_quota(Arc::clone(&quota))?;
        let hop = Value::Message(crate::bytecode::Message::new(
            0,
            1,
            1,
            Value::str("x".repeat(200)),
        ));
        match vm.set_register(0, hop) {
            Err(Fault::QuotaExceeded(_)) => {
                assert_eq!(quota.mem_used(), 0);
                Ok(())
            }
            other => Err(format!("expected QuotaExceeded, got {other:?}").into()),
        }
    }

    #[test]
    fn overwriting_a_message_releases_the_payload_charge() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        b.begin_function("main", 0, 1);
        b.emit_return(0);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        let quota = Arc::new(FlowQuota::new(10_000, 10_000, 1, 1, 1, 1));
        vm.set_quota(Arc::clone(&quota))?;
        vm.set_register(
            0,
            Value::Message(crate::bytecode::Message::new(
                0,
                1,
                1,
                Value::str("x".repeat(200)),
            )),
        )?;
        assert_eq!(quota.mem_used(), 200);
        vm.set_register(0, Value::Int(1))?;
        assert_eq!(quota.mem_used(), 0);
        Ok(())
    }

    #[test]
    fn a_message_past_the_decoder_ceiling_is_not_stored() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        b.begin_function("main", 0, 1);
        b.emit_return(0);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        let quota = Arc::new(FlowQuota::new(10_000, 10_000, 1, 1, 1, 1));
        vm.set_quota(Arc::clone(&quota))?;
        let mut value = Value::str("x".repeat(200));
        for _ in 0..17 {
            value = Value::Message(crate::bytecode::Message::new(0, 1, 1, value));
        }
        match vm.set_register(0, value) {
            Err(Fault::QuotaExceeded(_)) => {
                assert_eq!(quota.mem_used(), 0);
                Ok(())
            }
            other => Err(format!("expected QuotaExceeded, got {other:?}").into()),
        }
    }

    #[test]
    fn in_flight_hop_does_not_recount_the_held_payload() {
        let hop = Value::Message(crate::bytecode::Message::new(
            0,
            1,
            1,
            Value::str("x".repeat(200)),
        ));
        let inflight = in_flight_hop_charge(&hop);
        assert_eq!(inflight + 200, hop.memory_size());
        assert!(inflight < hop.memory_size());
    }

    #[test]
    fn sleep_rejects_a_bool() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        let flag = b.const_(Value::Bool(true));
        b.begin_function("main", 0, 1);
        b.emit_load_const(0, flag);
        b.emit_sleep(0);
        b.emit_return(0);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        match vm.run(10) {
            VmResult::Trap(Fault::TypeMismatch {
                expected: "int",
                got: "bool",
            }) => Ok(()),
            other => Err(format!("expected TypeMismatch(bool), got {other:?}").into()),
        }
    }

    #[test]
    fn tag_and_request_id_reject_a_bool() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        let flag = b.const_(Value::Bool(false));
        b.begin_function("main", 0, 3);
        b.emit_load_const(0, flag);
        b.emit_receive_match(1, 0);
        b.emit_return(0);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        match vm.run(10) {
            VmResult::Trap(Fault::TypeMismatch {
                expected: "int",
                got: "bool",
            }) => {}
            other => return Err(format!("tag bool: {other:?}").into()),
        }
        let mut b = ChunkBuilder::new("t");
        let flag = b.const_(Value::Bool(true));
        b.begin_function("main", 0, 3);
        b.emit_load_imm(0, 1);
        b.emit_load_const(1, flag);
        b.emit_receive_match_corr(2, 0, 1);
        b.emit_return(0);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        match vm.run(10) {
            VmResult::Trap(Fault::TypeMismatch {
                expected: "int",
                got: "bool",
            }) => Ok(()),
            other => Err(format!("request_id bool: {other:?}").into()),
        }
    }

    #[test]
    fn sleep_accepts_zero_and_rejects_a_float() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        b.begin_function("main", 0, 1);
        b.emit_load_imm(0, 0);
        b.emit_sleep(0);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        match vm.run(10) {
            VmResult::Sleep(d) if d.is_zero() => {}
            other => return Err(format!("expected Sleep(0), got {other:?}").into()),
        }
        let mut b = ChunkBuilder::new("t");
        let n = b.const_(Value::Float(1.0));
        b.begin_function("main", 0, 1);
        b.emit_load_const(0, n);
        b.emit_sleep(0);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        match vm.run(10) {
            VmResult::Trap(Fault::TypeMismatch {
                expected: "int",
                got: "float",
            }) => Ok(()),
            other => Err(format!("expected float rejection, got {other:?}").into()),
        }
    }

    #[test]
    fn dict_str_stays_charged_after_the_register_is_cleared() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        let text = b.const_(Value::Str("x".repeat(100).into()));
        b.begin_function("main", 0, 3);
        b.emit_load_const(0, text);
        b.emit_load_imm(1, 7);
        b.emit_dict_put(2, 1, 0);
        b.emit_load_imm(0, 0);
        b.emit_halt();
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        let quota = Arc::new(FlowQuota::new(10_000, 500, 1, 1, 1, 1));
        vm.set_quota(Arc::clone(&quota))?;
        match vm.run(20) {
            VmResult::Complete(_) => {
                assert_eq!(quota.mem_used(), 100);
                Ok(())
            }
            other => Err(format!("expected Complete, got {other:?}").into()),
        }
    }

    #[test]
    fn dict_bytes_round_trip_charges_the_buffer() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        let blob = b.const_(Value::bytes(vec![1u8; 60]));
        b.begin_function("main", 0, 3);
        b.emit_load_const(0, blob);
        b.emit_load_imm(1, 4);
        b.emit_dict_put(2, 1, 0);
        b.emit_load_imm(0, 0);
        b.emit_dict_get(0, 1);
        b.emit_halt();
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        let quota = Arc::new(FlowQuota::new(10_000, 500, 1, 1, 1, 1));
        vm.set_quota(Arc::clone(&quota))?;
        match vm.run(30) {
            VmResult::Complete(Value::Bytes(buf)) if buf.len() == 60 => {
                // Register and dictionary each hold the buffer.
                assert_eq!(quota.mem_used(), 120);
                Ok(())
            }
            other => Err(format!("expected bytes, got {other:?}").into()),
        }
    }

    #[test]
    fn dict_put_rejects_a_second_copy_that_does_not_fit() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        let text = b.const_(Value::Str("x".repeat(100).into()));
        b.begin_function("main", 0, 3);
        b.emit_load_const(0, text);
        b.emit_load_imm(1, 1);
        b.emit_dict_put(2, 1, 0);
        b.emit_halt();
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        let quota = Arc::new(FlowQuota::new(10_000, 150, 1, 1, 1, 1));
        vm.set_quota(Arc::clone(&quota))?;
        match vm.run(20) {
            VmResult::Trap(Fault::QuotaExceeded(_)) => {
                assert_eq!(quota.mem_used(), 100);
                Ok(())
            }
            other => Err(format!("expected QuotaExceeded, got {other:?}").into()),
        }
    }

    #[test]
    fn dict_replace_that_cannot_park_the_old_string_keeps_it() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        let first = b.const_(Value::Str("a".repeat(100).into()));
        let second = b.const_(Value::Str("b".repeat(80).into()));
        b.begin_function("main", 0, 4);
        b.emit_load_const(0, first);
        b.emit_load_imm(1, 1);
        b.emit_dict_put(2, 1, 0);
        b.emit_load_imm(0, 0);
        b.emit_load_const(0, second);
        b.emit_dict_put(3, 1, 0);
        b.emit_halt();
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        // 200 admits the original string in both the register and the
        // dictionary. Returning that string to r3 while r0 and the
        // dictionary both hold the 80-byte replacement needs 260.
        let quota = Arc::new(FlowQuota::new(10_000, 200, 1, 1, 1, 1));
        vm.set_quota(Arc::clone(&quota))?;
        match vm.run(30) {
            VmResult::Trap(Fault::QuotaExceeded(_)) => {
                // r0 still holds 80 and the dictionary still holds 100.
                assert_eq!(quota.mem_used(), 180);
                Ok(())
            }
            other => Err(format!("expected QuotaExceeded, got {other:?}").into()),
        }
    }

    #[test]
    fn dict_erase_moves_the_charge_back_to_the_register() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        let text = b.const_(Value::Str("y".repeat(80).into()));
        b.begin_function("main", 0, 4);
        b.emit_load_const(0, text);
        b.emit_load_imm(1, 3);
        b.emit_dict_put(2, 1, 0);
        b.emit_load_imm(0, 0);
        b.emit_dict_erase(0, 1);
        b.emit_halt();
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        let quota = Arc::new(FlowQuota::new(10_000, 1_000, 1, 1, 1, 1));
        vm.set_quota(Arc::clone(&quota))?;
        match vm.run(30) {
            VmResult::Complete(Value::Str(s)) if s.len() == 80 => {
                assert_eq!(quota.mem_used(), 80);
                Ok(())
            }
            other => Err(format!("expected the erased string, got {other:?}").into()),
        }
    }

    #[test]
    fn dict_rejects_a_message_and_an_empty_string_is_free() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        let hop = b.const_(Value::Message(crate::bytecode::Message::new(0, 1, 1, 1i64)));
        b.begin_function("main", 0, 3);
        b.emit_load_const(0, hop);
        b.emit_load_imm(1, 1);
        b.emit_dict_put(2, 1, 0);
        b.emit_halt();
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        match vm.run(10) {
            VmResult::Trap(Fault::TypeMismatch {
                expected: "dict value (scalar, str, or bytes)",
                got: "message",
            }) => {}
            other => return Err(format!("message dict: {other:?}").into()),
        }
        let mut b = ChunkBuilder::new("t");
        let empty = b.const_(Value::Str("".into()));
        b.begin_function("main", 0, 3);
        b.emit_load_const(0, empty);
        b.emit_load_imm(1, 1);
        b.emit_dict_put(2, 1, 0);
        b.emit_halt();
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        let quota = Arc::new(FlowQuota::new(10_000, 64, 1, 1, 1, 1));
        vm.set_quota(Arc::clone(&quota))?;
        match vm.run(20) {
            VmResult::Complete(_) => {
                assert_eq!(quota.mem_used(), 0);
                Ok(())
            }
            other => Err(format!("empty str: {other:?}").into()),
        }
    }

    #[test]
    fn dict_replace_releases_the_previous_string() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        let first = b.const_(Value::Str("a".repeat(100).into()));
        let second = b.const_(Value::Str("b".repeat(40).into()));
        b.begin_function("main", 0, 4);
        b.emit_load_const(0, first);
        b.emit_load_imm(1, 1);
        b.emit_dict_put(2, 1, 0);
        b.emit_load_imm(0, 0);
        b.emit_load_const(0, second);
        b.emit_dict_put(2, 1, 0);
        b.emit_load_imm(2, 0);
        b.emit_halt();
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        let quota = Arc::new(FlowQuota::new(10_000, 1_000, 1, 1, 1, 1));
        vm.set_quota(Arc::clone(&quota))?;
        match vm.run(40) {
            VmResult::Complete(_) => {
                // r0 still holds the 40-byte replacement; the dict holds the
                // same buffer; the 100-byte previous value was overwritten.
                assert_eq!(quota.mem_used(), 80);
                Ok(())
            }
            other => Err(format!("expected Complete, got {other:?}").into()),
        }
    }

    #[test]
    fn dictionaries_are_isolated_between_vms() -> TestResult {
        let mut writer = ChunkBuilder::new("t");
        writer.begin_function("main", 0, 3);
        writer.emit_load_imm(0, 9);
        writer.emit_load_imm(1, 1);
        writer.emit_dict_put(2, 1, 0);
        writer.emit_halt();
        let mut vm = Vm::new(Arc::new(writer.finish()), NativeTable::empty(), 0, &[])?;
        match vm.run(20) {
            VmResult::Complete(_) => {}
            other => return Err(format!("writer: {other:?}").into()),
        }
        let mut reader = ChunkBuilder::new("t");
        reader.begin_function("main", 0, 2);
        reader.emit_load_imm(1, 1);
        reader.emit_dict_get(0, 1);
        reader.emit_halt();
        let mut other_vm = Vm::new(Arc::new(reader.finish()), NativeTable::empty(), 0, &[])?;
        match other_vm.run(20) {
            VmResult::Complete(Value::Unit) => Ok(()),
            other => Err(format!("reader saw {other:?}").into()),
        }
    }

    #[test]
    fn tag_bounds_and_non_integers_stay_strict() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        b.begin_function("main", 0, 2);
        b.emit_load_imm(0, i32::from(u16::MAX));
        b.emit_receive_match(1, 0);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        match vm.run(10) {
            VmResult::Receive {
                match_tag: Some(tag),
                ..
            } if tag == u16::MAX => {}
            other => return Err(format!("u16::MAX tag: {other:?}").into()),
        }
        let mut b = ChunkBuilder::new("t");
        b.begin_function("main", 0, 2);
        b.emit_load_imm(0, i32::from(u16::MAX) + 1);
        b.emit_receive_match(1, 0);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        match vm.run(10) {
            VmResult::Trap(Fault::TypeMismatch {
                expected: "tag u16",
                got: "int-out-of-range",
            }) => {}
            other => return Err(format!("tag overflow: {other:?}").into()),
        }
        let mut b = ChunkBuilder::new("t");
        let text = b.const_(Value::str("nope"));
        b.begin_function("main", 0, 1);
        b.emit_load_const(0, text);
        b.emit_sleep(0);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        match vm.run(10) {
            VmResult::Trap(Fault::TypeMismatch {
                expected: "int",
                got: "str",
            }) => Ok(()),
            other => Err(format!("str sleep: {other:?}").into()),
        }
    }

    #[test]
    fn sub_imm_and_lt_imm_follow_integer_and_float_rules() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        b.begin_function("main", 0, 2);
        b.emit_load_imm(0, 10);
        b.emit_sub_imm(1, 0, 3);
        b.emit_return(1);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        match vm.run(10) {
            VmResult::Complete(Value::Int(7)) => {}
            other => return Err(format!("10 - 3: {other:?}").into()),
        }
        let mut b = ChunkBuilder::new("t");
        let big = b.const_(Value::Int(i64::MAX));
        b.begin_function("main", 0, 2);
        b.emit_load_const(0, big);
        b.emit_sub_imm(1, 0, -1);
        b.emit_return(1);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        match vm.run(10) {
            VmResult::Complete(Value::Int(n)) if n == i64::MIN => {}
            other => return Err(format!("wrap: {other:?}").into()),
        }
        let mut b = ChunkBuilder::new("t");
        let flag = b.const_(Value::Bool(true));
        b.begin_function("main", 0, 2);
        b.emit_load_const(0, flag);
        b.emit_sub_imm(1, 0, 1);
        b.emit_return(1);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        match vm.run(10) {
            VmResult::Trap(Fault::TypeMismatch {
                expected: "int or float",
                got: "bool",
            }) => {}
            other => return Err(format!("bool sub: {other:?}").into()),
        }
        let mut b = ChunkBuilder::new("t");
        let num = b.const_(Value::Float(1.5));
        b.begin_function("main", 0, 2);
        b.emit_load_const(0, num);
        b.emit_lt_imm(1, 0, 2);
        b.emit_return(1);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        match vm.run(10) {
            VmResult::Complete(Value::Bool(true)) => {}
            other => return Err(format!("1.5 < 2: {other:?}").into()),
        }
        let mut b = ChunkBuilder::new("t");
        let nan = b.const_(Value::Float(f64::NAN));
        b.begin_function("main", 0, 2);
        b.emit_load_const(0, nan);
        b.emit_lt_imm(1, 0, 0);
        b.emit_return(1);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        match vm.run(10) {
            VmResult::Complete(Value::Bool(false)) => Ok(()),
            other => Err(format!("nan < 0: {other:?}").into()),
        }
    }

    #[test]
    fn list_pack_get_len_and_quota() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        b.begin_function("main", 0, 4);
        b.emit_load_imm(0, 7);
        b.emit_load_imm(1, 8);
        b.emit_list_pack(2, 0, 2);
        b.emit_list_get(3, 2, 1);
        b.emit_return(3);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        match vm.run(20) {
            VmResult::Complete(Value::Int(8)) => {}
            other => return Err(format!("list get: {other:?}").into()),
        }
        let mut b = ChunkBuilder::new("t");
        let text = b.const_(Value::Str("z".repeat(100).into()));
        b.begin_function("main", 0, 2);
        b.emit_load_const(0, text);
        b.emit_list_pack(1, 0, 1);
        b.emit_halt();
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        let quota = Arc::new(FlowQuota::new(10_000, 1_000, 1, 1, 1, 1));
        vm.set_quota(Arc::clone(&quota))?;
        let slot = std::mem::size_of::<Value>();
        match vm.run(10) {
            VmResult::Complete(_) => {
                assert_eq!(quota.mem_used(), 100 + slot + 100);
                Ok(())
            }
            other => Err(format!("list quota: {other:?}").into()),
        }
    }

    #[test]
    fn fresh_request_id_stays_in_i64_domain() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        b.begin_function("main", 0, 1);
        b.emit_return(0);
        let mut vm = Vm::new(Arc::new(b.finish()), NativeTable::empty(), 0, &[])?;
        vm.set_next_request_id_for_test(i64::MAX as u64);
        let id = vm.fresh_request_id();
        assert_eq!(id, i64::MAX as u64);
        assert_eq!(vm.fresh_request_id(), i64::MAX as u64);
        assert!((id as i64) >= 0);
        Ok(())
    }

    #[test]
    fn call_native_without_gate_is_typed_denied() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        b.begin_function("main", 0, 1);
        b.emit_call_native(0, 0, 0);
        b.emit_return(0);
        let table = NativeTable::builder().register("noop", |_| Ok(Value::Unit))?;
        let mut vm = Vm::new(Arc::new(b.finish()), table.build(), 0, &[])?;
        match vm.run(10) {
            VmResult::Trap(Fault::NativeDenied(NativeCallError::NoNativeRight)) => Ok(()),
            other => Err(format!("expected NativeDenied(NoNativeRight), got {other:?}").into()),
        }
    }

    #[test]
    fn bad_jump_fail_closes_even_without_paranoid_flag() -> TestResult {
        let mut b = ChunkBuilder::new("t");
        b.begin_function("main", 0, 1);
        b.emit_load_imm(0, 7);
        b.emit_return(0);
        let mut chunk = b.finish();
        let code_len = chunk.code.len();
        chunk.code[0] = Instruction::only_imm(Opcode::Jump, 100);
        let mut vm = Vm::new(Arc::new(chunk), NativeTable::empty(), 0, &[])?;
        // Flag retained for API compat; jumps always fail closed.
        vm.set_paranoid_jumps(false);
        match vm.run(10) {
            VmResult::Trap(Fault::BadJump {
                target: 101,
                code_len: len,
            }) if len == code_len => Ok(()),
            other => Err(format!("expected BadJump(101), got {other:?}").into()),
        }
    }
}
