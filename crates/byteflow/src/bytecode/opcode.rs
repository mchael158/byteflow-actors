//! The Byteflow instruction set architecture (ISA v0).
//!
//! Byteflow is register-based (à la Lua 5.x / Dalvik) rather than stack-based
//! (à la JVM/CPython). Register machines need roughly 40-50% fewer dispatched
//! instructions than an equivalent stack machine because they avoid PUSH/POP
//! traffic for every intermediate value, at the cost of slightly larger
//! instruction words. For an interpreter whose steady-state cost is dominated
//! by dispatch (branch prediction + icache misses), fewer instructions per
//! logical operation wins.
//!
//! Every opcode fits in a single byte so a `Vec<Instruction>` is dense and
//! the dispatch table (see `byteflow-vm::interp`) can be a flat jump table
//! indexed directly by discriminant, with no bounds check in release builds
//! (enforced instead at decode/verification time, see [`crate::verify`]).

/// A single Byteflow opcode.
///
/// Numeric values are part of the stable on-disk ABI (`byteflow-bytecode`
/// module format, see [`super::chunk::MAGIC`]) — never renumber an existing
/// variant, only append.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Opcode {
    /// Stop the current Flow's VM loop. Terminal state.
    Halt = 0x00,

    // ---- data movement ----------------------------------------------------
    /// `LoadConst ra, kb`  →  `r[a] = constants[b]`
    LoadConst = 0x01,
    /// `Move ra, rb`  →  `r[a] = r[b]`
    Move = 0x02,
    /// `LoadImm ra, imm`  →  `r[a] = imm as i64` (fast path, skips const pool)
    LoadImm = 0x03,

    // ---- arithmetic (integer; float variants share the same encoding
    // ---- but operate on Value::Float, selected by the operand's runtime tag)
    Add = 0x10,
    Sub = 0x11,
    Mul = 0x12,
    Div = 0x13,
    Mod = 0x14,
    Neg = 0x15,

    // ---- comparison → writes a Value::Bool into ra
    Eq = 0x18,
    Lt = 0x19,
    Le = 0x1A,

    // ---- control flow -------------------------------------------------
    /// `Jump imm` → unconditional relative jump (imm = signed offset in
    /// instructions from the *next* pc).
    Jump = 0x20,
    /// `Branch ra, imm` → jump by `imm` iff `r[a]` is falsy (Bool(false),
    /// Unit, or Int(0)). This is the only conditional branch; `if/else` and
    /// loops both lower to Branch + Jump, keeping the interpreter's branch
    /// predictor state small.
    Branch = 0x21,

    // ---- procedure calls (native Rust functions registered via FFI, or
    // ---- other bytecode functions in the same chunk) ------------------
    /// `Call ra, fb, nc` → call function `fb` with `nc` arguments taken from
    /// `r[a..a+nc]`, result written back into `r[a]`.
    Call = 0x30,
    /// `Return ra` → return `r[a]` to the caller frame (or complete the
    /// Flow if this is the outermost frame).
    Return = 0x31,
    /// `CallNative ra, fb, nc` → like `Call` but `fb` indexes the native
    /// function table instead of the bytecode function table.
    CallNative = 0x32,

    // ---- Flow model --------------------------------------------------
    /// `Spawn ra, fb, nc` → create a new virtual Flow starting at
    /// function `fb`, passing `nc` arguments taken from `r[a+1..a+1+nc]`
    /// (deliberately *not* overlapping `r[a]` itself, which is where the
    /// scheduler writes a **Cap** to the child once created — FlowCap;
    /// see [`crate::VmResult::Spawn`]).
    Spawn = 0x40,
    /// `Yield` → cooperative yield. Control returns to the scheduler, the
    /// Flow is re-enqueued as `Ready` and may resume on any worker.
    Yield = 0x41,
    /// `Sleep ra` → suspend until `r[a]` (interpreted as milliseconds,
    /// Value::Int) has elapsed. Registered on the timer wheel.
    Sleep = 0x42,
    /// `Exit ra` → terminate the Flow, `r[a]` is delivered to `.join()`.
    Exit = 0x43,
    /// `SelfPid ra` → `r[a] =` a **self Cap**
    /// ([`CapRights::ADDRESSING`](crate::CapRights::ADDRESSING) =
    /// `SEND|ASK|LINK|MONITOR`) for this flow.
    /// (Opcode name kept for ABI; the value is `Value::Cap`, not Pid.)
    /// The VM does not store its own id (it has no scheduler state); this
    /// is a scheduler effect, same class as `Spawn`/`Receive`.
    SelfPid = 0x44,

    // ---- messaging --------------------------------------------------------
    /// `Send ra, rb` → Atomic Hop: deliver `r[b]` (`Message`) to the Cap in
    /// `r[a]`. With a full `Reject` inbox the sender parks (`WAITING_SEND`);
    /// otherwise the hop is queued / handed off without blocking the worker
    /// (see [`crate::docs::mailbox`]).
    ///
    /// VM requires Cap + Message; worker resolves Cap (SEND), stamps sender
    /// + `reply_cap`, then pushes to the resolved mailbox.
    Send = 0x50,
    /// `Receive ra` → pop the next message into `r[a]`; if the mailbox is
    /// empty, suspends the Flow in `Waiting` state until a message
    /// arrives.
    Receive = 0x51,
    /// `ReceiveTimeout ra, rb` → like `Receive` but gives up after `r[b]`
    /// milliseconds, writing `Value::Unit` into `r[a]` on timeout.
    ReceiveTimeout = 0x52,
    /// `ReceiveMatch ra, rb` → **Atomic Hop selective receive**: block until
    /// a [`crate::Value::Message`] with `tag == r[b]` (as `u16`) is available.
    /// Non-matching hops stay in the mailbox in FIFO order (skip, don't drop).
    ReceiveMatch = 0x53,
    /// `ReceiveMatchImm ra, imm` → like `ReceiveMatch` with an immediate tag
    /// (`imm` must fit in `u16`).
    ReceiveMatchImm = 0x54,
    /// `Ask ra, rb, rc` → **atomic request/reply hop**.
    ///
    /// 1. Validate `r[b]` as Cap and `r[c]` as Message (VM).
    /// 2. Scheduler resolves Cap (ASK), stamps `sender` + mints `reply_cap`.
    /// 3. Deliver the request to the resolved FlowId (like `Send`).
    /// 4. Suspend until a reply hop matches
    ///    `request_id == request.request_id && sender == resolved_FlowId`.
    /// 5. Write the reply `Message` into `r[a]`.
    ///
    /// Append-only ABI slot (`0x55`).
    Ask = 0x55,
    /// `Monitor ra, rb` → install a one-way watch on Cap `r[b]`; write
    /// [`crate::MonitorRef`] as `Int` into `r[a]`.
    Monitor = 0x56,
    /// `Demonitor ra` → drop the monitor in `r[a]` (`Int` ref).
    Demonitor = 0x57,
    /// `Link ra, rb` → bidirectional link with Cap `r[b]`; write
    /// [`crate::LinkId`] as `Int` into `r[a]`.
    Link = 0x58,
    /// `Unlink ra` → drop the link in `r[a]` (`Int` id).
    Unlink = 0x59,
    /// `AskTimeout ra, rb, rc, rd` → like `Ask`, but give up after
    /// `r[imm]` milliseconds and write `Value::Unit` into `ra`.
    ///
    /// Encoding: `a=dest`, `b=cap`, `c=msg`, `imm=millis_reg`.
    /// Append-only ABI slot (`0x5A`); `Trap` stays `0x60`.
    AskTimeout = 0x5A,
    /// `Delegate ra, rb, rights, rc?` → write an attenuated Cap into `r[a]`.
    ///
    /// `r[b]` is the source Cap; `imm` is the requested rights mask;
    /// `c = 255` means no extra native-mask narrowing. The scheduler
    /// calls `Cap::attenuate` — the only derivation path.
    /// Append-only ABI slot (`0x5B`); `Trap` stays `0x60`.
    Delegate = 0x5B,
    /// `FreshRequestId ra` → `r[a] =` next per-flow correlation id (`Int`).
    ///
    /// Starts at 1; `0` is reserved as “unset” so `Send` / `Ask` can mint.
    /// Not a capability — uniqueness, not unpredictability.
    /// Append-only ABI slot (`0x5C`); `Trap` stays `0x60`.
    FreshRequestId = 0x5C,
    /// `ReceiveMatchCorr ra, rb, rc` → wait for a hop with
    /// `tag == r[b]` and `request_id == r[c]`. Non-matching hops stay queued.
    /// Append-only ABI slot (`0x5D`); `Trap` stays `0x60`.
    ReceiveMatchCorr = 0x5D,
    /// `ReceiveMatchCorrImm ra, rb, imm` → like [`Self::ReceiveMatchCorr`]
    /// with immediate tag (`imm` as `u16`) and `request_id` from `r[b]`.
    /// Append-only ABI slot (`0x5E`); `Trap` stays `0x60`.
    ReceiveMatchCorrImm = 0x5E,
    /// `SetTrapExit ra` → BEAM `process_flag(trap_exit, r[a])`.
    ///
    /// Truthy `r[a]` (same rules as [`Opcode::Branch`]) enables trapping:
    /// this flow receives [`crate::TAG_SYS_EXIT`] when a linked peer exits
    /// (including `Normal`), instead of being killed / silently dropping the
    /// link.
    /// Append-only ABI slot (`0x5F`); `Trap` stays `0x60`.
    SetTrapExit = 0x5F,
    /// `RegisterName ra` → publish `r[a]` (`Str`) as this flow's name.
    /// Only the calling flow is registered. Requires `SEND` on self-authority
    /// (confined spawn cannot squat names). Append-only (`0x62`).
    RegisterName = 0x62,
    /// `Whereis ra, rb` → look up `r[b]` (`Str`); write a **SEND** Cap for
    /// the caller, or `Unit` if missing. Never a raw FlowId. (`0x63`)
    Whereis = 0x63,
    /// `ReceiveMatchKind ra, imm` → block until a [`crate::Value::Message`]
    /// whose `payload.wire_tag() == imm` (BFV0 tag `0..=8`) is available.
    /// Non-matching hops stay queued (FIFO skip). Append-only (`0x64`);
    /// `Trap` stays `0x60`.
    ReceiveMatchKind = 0x64,
    /// `SetRestartPolicy imm` → set this flow's supervisor restart policy:
    /// `0` = Always, `1` = OnFailure, `2` = Never. Invalid `imm` → Trap.
    /// Append-only (`0x65`); `Trap` stays `0x60`.
    SetRestartPolicy = 0x65,
    /// `HostAwait ra, rb, imm` → park this flow and hand `(op=imm, args=r[b])`
    /// to the host [`crate::HostAwaitBridge`]. On completion the host writes
    /// a [`crate::Value`] into `r[a]` (register writeback, like `Receive`).
    /// Requires a bridge on [`crate::RuntimeConfig`]; otherwise the flow fails.
    /// Append-only (`0x66`); not a `CallNative` / std-native slot.
    HostAwait = 0x66,

    // ---- diagnostics / safety ------------------------------------------
    /// `Trap imm` → deliberate fault (assertion failure, div-by-zero, bad
    /// opcode encountered by a corrupt/foreign module, capability
    /// violation). Propagates to the Flow supervisor as [`crate::FlowOutcome::Failed`].
    Trap = 0x60,
    /// `Nop` → no-op, used by the assembler to pad jump targets.
    Nop = 0x61,
}

impl Opcode {
    /// Decode a raw byte into an `Opcode`, used when loading foreign/untrusted
    /// modules. Rejects anything outside the currently defined ISA rather
    /// than transmuting garbage into a jump-table index (which is exactly
    /// the class of bug that turns a VM into a code-execution primitive).
    #[inline]
    pub fn from_u8(byte: u8) -> Option<Opcode> {
        use Opcode::*;
        Some(match byte {
            0x00 => Halt,
            0x01 => LoadConst,
            0x02 => Move,
            0x03 => LoadImm,
            0x10 => Add,
            0x11 => Sub,
            0x12 => Mul,
            0x13 => Div,
            0x14 => Mod,
            0x15 => Neg,
            0x18 => Eq,
            0x19 => Lt,
            0x1A => Le,
            0x20 => Jump,
            0x21 => Branch,
            0x30 => Call,
            0x31 => Return,
            0x32 => CallNative,
            0x40 => Spawn,
            0x41 => Yield,
            0x42 => Sleep,
            0x43 => Exit,
            0x44 => SelfPid,
            0x50 => Send,
            0x51 => Receive,
            0x52 => ReceiveTimeout,
            0x53 => ReceiveMatch,
            0x54 => ReceiveMatchImm,
            0x55 => Ask,
            0x56 => Monitor,
            0x57 => Demonitor,
            0x58 => Link,
            0x59 => Unlink,
            0x5A => AskTimeout,
            0x5B => Delegate,
            0x5C => FreshRequestId,
            0x5D => ReceiveMatchCorr,
            0x5E => ReceiveMatchCorrImm,
            0x5F => SetTrapExit,
            0x60 => Trap,
            0x62 => RegisterName,
            0x63 => Whereis,
            0x64 => ReceiveMatchKind,
            0x65 => SetRestartPolicy,
            0x66 => HostAwait,
            0x61 => Nop,
            _ => return None,
        })
    }

    #[inline]
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

impl std::fmt::Display for Opcode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Opcode::Halt => "Halt",
            Opcode::LoadConst => "LoadConst",
            Opcode::Move => "Move",
            Opcode::LoadImm => "LoadImm",
            Opcode::Add => "Add",
            Opcode::Sub => "Sub",
            Opcode::Mul => "Mul",
            Opcode::Div => "Div",
            Opcode::Mod => "Mod",
            Opcode::Neg => "Neg",
            Opcode::Eq => "Eq",
            Opcode::Lt => "Lt",
            Opcode::Le => "Le",
            Opcode::Jump => "Jump",
            Opcode::Branch => "Branch",
            Opcode::Call => "Call",
            Opcode::Return => "Return",
            Opcode::CallNative => "CallNative",
            Opcode::Spawn => "Spawn",
            Opcode::Yield => "Yield",
            Opcode::Sleep => "Sleep",
            Opcode::Exit => "Exit",
            Opcode::SelfPid => "SelfPid",
            Opcode::Send => "Send",
            Opcode::Receive => "Receive",
            Opcode::ReceiveTimeout => "ReceiveTimeout",
            Opcode::ReceiveMatch => "ReceiveMatch",
            Opcode::ReceiveMatchImm => "ReceiveMatchImm",
            Opcode::Ask => "Ask",
            Opcode::Monitor => "Monitor",
            Opcode::Demonitor => "Demonitor",
            Opcode::Link => "Link",
            Opcode::Unlink => "Unlink",
            Opcode::AskTimeout => "AskTimeout",
            Opcode::Delegate => "Delegate",
            Opcode::FreshRequestId => "FreshRequestId",
            Opcode::ReceiveMatchCorr => "ReceiveMatchCorr",
            Opcode::ReceiveMatchCorrImm => "ReceiveMatchCorrImm",
            Opcode::SetTrapExit => "SetTrapExit",
            Opcode::RegisterName => "RegisterName",
            Opcode::Whereis => "Whereis",
            Opcode::ReceiveMatchKind => "ReceiveMatchKind",
            Opcode::SetRestartPolicy => "SetRestartPolicy",
            Opcode::HostAwait => "HostAwait",
            Opcode::Trap => "Trap",
            Opcode::Nop => "Nop",
        };
        f.write_str(name)
    }
}
