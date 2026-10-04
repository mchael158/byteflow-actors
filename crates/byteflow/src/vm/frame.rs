use crate::bytecode::Value;

/// Register operands are `u8`, so a frame's register file cannot exceed this.
pub const MAX_REGISTERS: usize = u8::MAX as usize + 1;

/// One activation record. Register windows are per-frame (not a single
/// global register file), so recursive/re-entrant calls can't clobber a
/// caller's registers — the price is one heap allocation per call, which is
/// acceptable for v0 and is the first thing `byteflow-jit` would optimise
/// away (e.g. via a shared, growable register stack) in design notes §28-29.
#[derive(Debug)]
pub struct Frame {
    function: u32,
    pc: usize,
    registers: Vec<Value>,
    /// Register index in the *caller's* frame that will receive this
    /// frame's return value. `None` for the outermost frame, whose return
    /// value completes the Flow instead.
    dest_reg: Option<u8>,
    /// Bytes currently charged for `Str`/`Bytes` living in this frame's
    /// registers. Released when the frame is popped so FlowQuota /
    /// MemoryBudget cannot leak across `Call`/`Return`.
    heap_charge: usize,
}

impl Frame {
    pub fn new(function: u32, num_registers: u8, dest_reg: Option<u8>) -> Self {
        debug_assert!((num_registers as usize) <= MAX_REGISTERS);
        Frame {
            function,
            pc: 0,
            registers: vec![Value::Unit; num_registers as usize],
            dest_reg,
            heap_charge: 0,
        }
    }

    #[inline]
    pub(crate) fn function(&self) -> u32 {
        self.function
    }

    #[inline]
    pub(crate) fn pc(&self) -> usize {
        self.pc
    }

    #[inline]
    pub(crate) fn set_pc(&mut self, pc: usize) {
        self.pc = pc;
    }

    #[inline]
    pub(crate) fn registers(&self) -> &[Value] {
        &self.registers
    }

    #[inline]
    pub(crate) fn registers_mut(&mut self) -> &mut [Value] {
        &mut self.registers
    }

    #[inline]
    pub(crate) fn dest_reg(&self) -> Option<u8> {
        self.dest_reg
    }

    #[inline]
    pub(crate) fn heap_charge(&self) -> usize {
        self.heap_charge
    }

    #[inline]
    pub(crate) fn set_heap_charge(&mut self, bytes: usize) {
        self.heap_charge = bytes;
    }

    #[inline]
    pub(crate) fn add_heap_charge(&mut self, delta: usize) {
        self.heap_charge = self.heap_charge.saturating_add(delta);
    }

    #[inline]
    pub(crate) fn sub_heap_charge(&mut self, delta: usize) {
        self.heap_charge = self.heap_charge.saturating_sub(delta);
    }
}
