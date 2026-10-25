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
    /// `LoadConst ra, imm`  →  `r[a] = constants[imm]`
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
    /// [`crate::LinkId`] as `Int` into `r[a]`. `0` means finalize already
    /// consumed the row and propagated the exit; that value is not a link.
    Link = 0x58,
    /// `Unlink ra` → drop the link in `r[a]` (`Int` id). `0` does nothing.
    Unlink = 0x59,
    /// `AskTimeout ra, rb, rc, imm` → like `Ask`, but give up after
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
    /// whose `payload.wire_tag() == imm` (BFV0 tag `0..=10`) is available.
    /// Non-matching hops stay queued (FIFO skip). Append-only (`0x64`);
    /// `Trap` stays `0x60`.
    ReceiveMatchKind = 0x64,
    /// `SetRestartPolicy imm` → set this flow's supervisor restart policy:
    /// `0` = Always, `1` = OnFailure, `2` = Never. Invalid `imm` → Trap.
    /// Append-only (`0x65`); `Trap` stays `0x60`.
    SetRestartPolicy = 0x65,
    /// `HostAwait ra, rb, imm` → register a host operation `(op=imm, args=r[b])`
    /// and write its ticket id (`Int`) into `r[a]`. The flow keeps running.
    /// The host completes with a [`crate::TAG_SYS_AWAIT`] hop (`request_id`
    /// = ticket) that bytecode reads with `Receive`. Requires a bridge on
    /// [`crate::RuntimeConfig`]; otherwise the flow fails.
    /// Append-only (`0x66`); not a `CallNative` / std-native slot.
    HostAwait = 0x66,
    /// `ReceiveMatchTimeout ra, rb, rc` → like [`Self::ReceiveMatch`] but
    /// give up after `r[c]` milliseconds and write `Value::Unit` (BEAM
    /// `receive … after`). Non-matching hops stay queued. Append-only (`0x67`).
    ReceiveMatchTimeout = 0x67,
    /// `ReceiveMatchImmTimeout ra, rb, imm` → like [`Self::ReceiveMatchImm`]
    /// with timeout register `r[b]` (imm = tag). Writes `Unit` on expiry.
    /// Append-only (`0x68`).
    ReceiveMatchImmTimeout = 0x68,
    /// `IsAlive ra, rb` → `r[a] = Bool`: the Cap in `r[b]` is held by this
    /// flow and still names a live flow (BEAM `is_process_alive/1`).
    /// Unknown / not-held / revoked / non-flow target → `false` (no trap).
    /// Append-only (`0x69`).
    IsAlive = 0x69,
    /// `ExitSignal ra, rb` → BEAM `exit/2`: signal the flow addressed by
    /// Cap `r[a]` (needs `LINK`) with reason `r[b]` (`FlowExitReason` as `Int`).
    /// `Normal` is ignored. `Killed` cannot be trapped. Other reasons become
    /// a [`crate::TAG_SYS_EXIT`] hop when the target has `trap_exit`.
    /// Append-only (`0x6A`).
    ExitSignal = 0x6A,
    /// `DictPut ra, rb, rc` → process dictionary: `r[a] = old`, key `r[b]`
    /// (`Int`), value `r[c]` (scalar, `Str`, or `Bytes`). `Str` / `Bytes`
    /// are charged on the flow quota for as long as the dictionary holds
    /// them. Missing old → `Unit`. Append-only (`0x6B`).
    DictPut = 0x6B,
    /// `DictGet ra, rb` → `r[a] = dict[r[b]]` or `Unit`. Append-only (`0x6C`).
    DictGet = 0x6C,
    /// `DictErase ra, rb` → remove key `r[b]`, write the old value or `Unit`
    /// into `r[a]`. Append-only (`0x6D`).
    DictErase = 0x6D,
    /// `ReceiveMatchEq ra, rb, rc` → selective receive: `tag == r[b]` and
    /// payload `Int == r[c]`. Non-matching hops stay queued. Append-only (`0x6E`).
    ReceiveMatchEq = 0x6E,
    /// `StartChild ra, policy, fn` → spawn function `imm` as a supervised
    /// child (`b` = restart policy `0..=2`). Writes an addressing Cap into
    /// `r[a]`. Attaches to the supervisor this flow started with
    /// [`Self::StartSupervisor`], otherwise to the host supervisor that
    /// started this flow. With neither, the flow fails. Append-only (`0x6F`).
    StartChild = 0x6F,
    /// `ReceiveMatchValue ra, rb, rc` → selective receive: `tag == r[b]` and
    /// payload structurally equal to `r[c]` (any [`crate::Value`], not only
    /// `Int`). Non-matching hops stay queued. Append-only (`0x70`).
    ReceiveMatchValue = 0x70,
    /// `SendRemote ra, rb, rc` → deliver hop `r[c]` to node `r[a]` (`Int`)
    /// service `r[b]` (`Str`). The host drains [`crate::DistributionBridge`].
    /// Byteflow does not open a socket. Append-only (`0x71`).
    SendRemote = 0x71,
    /// `StartSupervisor imm` → start a supervisor owned by this flow.
    /// `imm`: `0` one-for-one, `1` one-for-all, `2` rest-for-one.
    /// Later `StartChild` attaches to it. Append-only (`0x72`).
    StartSupervisor = 0x72,
    /// `AddImm ra, rb, imm` → `r[a] = r[b] + imm` (`imm` sign-extended to
    /// `i64`). Integer add wraps; a float in `r[b]` adds in `f64`. One
    /// instruction instead of `LoadImm` plus `Add`, and no scratch register.
    /// Append-only (`0x73`).
    AddImm = 0x73,
    /// `CallNativeFrom ra, rb, native` → `r[a] = native_1(r[b])`.
    ///
    /// Same FFI slot as [`Self::CallNative`] (`imm` is the native index) but
    /// the argument is `r[b]` and the result lands in `r[a]`. `r[b]` survives
    /// when `a != b`, so unpacking a `Message` does not need a preceding
    /// `Move`. Append-only (`0x74`).
    CallNativeFrom = 0x74,
    /// `SubImm ra, rb, imm` → `r[a] = r[b] - imm` (`imm` sign-extended to
    /// `i64`). Integer subtraction wraps; a float in `r[b]` subtracts in
    /// `f64`. Append-only (`0x75`).
    SubImm = 0x75,
    /// `LtImm ra, rb, imm` → `r[a] = r[b] < imm` as `Bool`. Integers compare
    /// as signed `i64`. Floats compare as `f64` (NaN is not less). `Bool`
    /// is not a number here. Append-only (`0x76`).
    LtImm = 0x76,
    /// `ListPack ra, rb, imm` → `r[a]` becomes a list of `imm` values
    /// starting at `r[b]`. `imm` is `0..=256`. Append-only (`0x77`).
    ListPack = 0x77,
    /// `ListGet ra, rb, imm` → `r[a] = r[b][imm]`. `r[b]` must be a list
    /// and `imm` a valid index. Append-only (`0x78`).
    ListGet = 0x78,
    /// `ListLen ra, rb` → `r[a] = len(r[b])` as `Int`. Append-only (`0x79`).
    ListLen = 0x79,
    /// `MulImm ra, rb, imm` → `r[a] = r[b] * imm` (`imm` sign-extended to
    /// `i64`). Integer multiply wraps; a float in `r[b]` multiplies in
    /// `f64`. Append-only (`0x7A`).
    MulImm = 0x7A,
    /// `EqImm ra, rb, imm` → `r[a] = Bool(r[b] == Int(imm))`.
    ///
    /// The immediate is an integer. A float, bool, or string in `r[b]`
    /// compares unequal, the same as [`Self::Eq`] against a `LoadImm`.
    /// Append-only (`0x7B`).
    EqImm = 0x7B,
    /// `LeImm ra, rb, imm` → `r[a] = r[b] <= imm` as `Bool`. Integers
    /// compare as signed `i64`. Floats compare as `f64` (NaN is not less
    /// or equal). `Bool` is not a number here. Append-only (`0x7C`).
    LeImm = 0x7C,
    /// `BitAnd ra, rb, rc` → `r[a] = r[b] & r[c]`. Both operands must be
    /// `Int`. Append-only (`0x7D`).
    BitAnd = 0x7D,
    /// `BitOr ra, rb, rc` → `r[a] = r[b] | r[c]`. Both operands must be
    /// `Int`. Append-only (`0x7E`).
    BitOr = 0x7E,
    /// `BitXor ra, rb, rc` → `r[a] = r[b] ^ r[c]`. Both operands must be
    /// `Int`. Append-only (`0x7F`).
    BitXor = 0x7F,
    /// `BitNot ra, rb` → `r[a] = !r[b]`. `r[b]` must be `Int`.
    /// Append-only (`0x80`).
    BitNot = 0x80,
    /// `Shl ra, rb, rc` → `r[a] = r[b] << (r[c] & 63)`. Both operands must
    /// be `Int`. The shift count keeps its low 6 bits, so a negative count
    /// still names a shift in `0..64`. Append-only (`0x81`).
    Shl = 0x81,
    /// `Shr ra, rb, rc` → arithmetic `r[a] = r[b] >> (r[c] & 63)`. Both
    /// operands must be `Int`. The sign bit fills the vacated bits.
    /// Append-only (`0x82`).
    Shr = 0x82,
    /// `ReceiveMatchCmp ra, rb, rc, imm` → wait for tag `r[b]` and an `Int`
    /// payload compared with `r[c]` by `imm` (`0` eq, `1` lt, `2` le).
    /// A hop that fails the comparison stays queued. Append-only (`0x83`).
    ReceiveMatchCmp = 0x83,
    /// `MapPack ra, rb, imm` → `r[a]` becomes a map of `imm` pairs starting
    /// at `r[b]` (`key`, `value`, `key`, `value`, …). `imm` is `0..=256`.
    /// Keys are `Int` or `Str`. A duplicate key traps. Append-only (`0x84`).
    MapPack = 0x84,
    /// `MapGet ra, rb, rc` → `r[a] = r[b][r[c]]`. Missing key writes `Unit`.
    /// Append-only (`0x85`).
    MapGet = 0x85,
    /// `MapLen ra, rb` → `r[a] = len(r[b])` as `Int`. Append-only (`0x86`).
    MapLen = 0x86,
    /// `TableOpen ra, rb` → this flow's table, named by the `Str` in `r[b]`.
    /// Writes a Cap (`SEND|RECV`, target = that table) into `r[a]`.
    /// A second open traps. Append-only (`0x87`).
    TableOpen = 0x87,
    /// `TableGet ra, rb, rc` → `r[a]` is the row of cap `r[b]` at key `r[c]`.
    /// Requires `RECV`. Missing key writes `Unit`. Append-only (`0x88`).
    TableGet = 0x88,
    /// `TablePut ra, rb, rc` → store `r[c]` in cap `r[a]` at key `r[b]`.
    /// Requires `SEND`. Append-only (`0x89`).
    TablePut = 0x89,
    /// `RecordPack ra, rb, imm` → `r[a]` becomes a positional record of
    /// `imm` fields starting at `r[b]`. `imm` is `0..=256`. Append-only (`0x8A`).
    RecordPack = 0x8A,
    /// `RecordGet ra, rb, imm` → `r[a] = r[b][imm]`. `r[b]` must be a record.
    /// Append-only (`0x8B`).
    RecordGet = 0x8B,
    /// `RecordLen ra, rb` → `r[a] = len(r[b])` as `Int`. Append-only (`0x8C`).
    RecordLen = 0x8C,

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
            0x61 => Nop,
            0x62 => RegisterName,
            0x63 => Whereis,
            0x64 => ReceiveMatchKind,
            0x65 => SetRestartPolicy,
            0x66 => HostAwait,
            0x67 => ReceiveMatchTimeout,
            0x68 => ReceiveMatchImmTimeout,
            0x69 => IsAlive,
            0x6A => ExitSignal,
            0x6B => DictPut,
            0x6C => DictGet,
            0x6D => DictErase,
            0x6E => ReceiveMatchEq,
            0x6F => StartChild,
            0x70 => ReceiveMatchValue,
            0x71 => SendRemote,
            0x72 => StartSupervisor,
            0x73 => AddImm,
            0x74 => CallNativeFrom,
            0x75 => SubImm,
            0x76 => LtImm,
            0x77 => ListPack,
            0x78 => ListGet,
            0x79 => ListLen,
            0x7A => MulImm,
            0x7B => EqImm,
            0x7C => LeImm,
            0x7D => BitAnd,
            0x7E => BitOr,
            0x7F => BitXor,
            0x80 => BitNot,
            0x81 => Shl,
            0x82 => Shr,
            0x83 => ReceiveMatchCmp,
            0x84 => MapPack,
            0x85 => MapGet,
            0x86 => MapLen,
            0x87 => TableOpen,
            0x88 => TableGet,
            0x89 => TablePut,
            0x8A => RecordPack,
            0x8B => RecordGet,
            0x8C => RecordLen,
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
            Opcode::AddImm => "AddImm",
            Opcode::SubImm => "SubImm",
            Opcode::LtImm => "LtImm",
            Opcode::ListPack => "ListPack",
            Opcode::ListGet => "ListGet",
            Opcode::ListLen => "ListLen",
            Opcode::MulImm => "MulImm",
            Opcode::EqImm => "EqImm",
            Opcode::LeImm => "LeImm",
            Opcode::BitAnd => "BitAnd",
            Opcode::BitOr => "BitOr",
            Opcode::BitXor => "BitXor",
            Opcode::BitNot => "BitNot",
            Opcode::Shl => "Shl",
            Opcode::Shr => "Shr",
            Opcode::ReceiveMatchCmp => "ReceiveMatchCmp",
            Opcode::MapPack => "MapPack",
            Opcode::MapGet => "MapGet",
            Opcode::MapLen => "MapLen",
            Opcode::TableOpen => "TableOpen",
            Opcode::TableGet => "TableGet",
            Opcode::TablePut => "TablePut",
            Opcode::RecordPack => "RecordPack",
            Opcode::RecordGet => "RecordGet",
            Opcode::RecordLen => "RecordLen",
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
            Opcode::CallNativeFrom => "CallNativeFrom",
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
            Opcode::ReceiveMatchTimeout => "ReceiveMatchTimeout",
            Opcode::ReceiveMatchImmTimeout => "ReceiveMatchImmTimeout",
            Opcode::IsAlive => "IsAlive",
            Opcode::ExitSignal => "ExitSignal",
            Opcode::DictPut => "DictPut",
            Opcode::DictGet => "DictGet",
            Opcode::DictErase => "DictErase",
            Opcode::ReceiveMatchEq => "ReceiveMatchEq",
            Opcode::StartChild => "StartChild",
            Opcode::ReceiveMatchValue => "ReceiveMatchValue",
            Opcode::SendRemote => "SendRemote",
            Opcode::StartSupervisor => "StartSupervisor",
            Opcode::Trap => "Trap",
            Opcode::Nop => "Nop",
        };
        f.write_str(name)
    }
}

/// Integer comparison used by [`Opcode::ReceiveMatchCmp`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntCmp {
    Eq,
    Lt,
    Le,
}

impl IntCmp {
    pub const fn from_imm(imm: i32) -> Option<Self> {
        match imm {
            0 => Some(Self::Eq),
            1 => Some(Self::Lt),
            2 => Some(Self::Le),
            _ => None,
        }
    }

    pub const fn as_imm(self) -> i32 {
        match self {
            Self::Eq => 0,
            Self::Lt => 1,
            Self::Le => 2,
        }
    }

    pub fn holds(self, payload: i64, bound: i64) -> bool {
        match self {
            Self::Eq => payload == bound,
            Self::Lt => payload < bound,
            Self::Le => payload <= bound,
        }
    }
}
