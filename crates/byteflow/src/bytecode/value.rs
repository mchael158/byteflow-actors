use std::fmt;
use std::sync::Arc;

use super::cap::CapId;

/// Reserved Atomic Hop tag for monitor `DOWN` events (not an application tag).
pub const TAG_SYS_DOWN: u16 = 0xFF01;
/// Reserved Atomic Hop tag for linked-exit notices (`Ask` target death or
/// `trap_exit` link signal).
pub const TAG_SYS_EXIT: u16 = 0xFF02;

/// Envelope carried in mailboxes and registers (**Atomic Hop**).
///
/// `payload` is a [`Value`] behind [`Arc`] so hops can carry `Int`, `Str`,
/// `Bytes`, or nested messages. Mailbox byte budgets charge
/// [`Value::memory_size`] of the whole hop, including the payload tree.
///
/// # Security
///
/// - **`sender`**: FlowId stamped by the scheduler on bytecode `Send` / `Ask`
///   and host [`crate::Runtime::send`] (`0` = [`crate::FlowId::HOST`])
///   (invariant **S1**). Not a capability.
/// - **`reply_cap`**: stable SEND-only [`CapId`] per `(recipient, sender)`
///   pair (holder = recipient, reused across hops). Correlation is
///   [`Self::request_id`]. [`CapId::NONE`] means “no reply grant”.
///
/// See `docs/security.md`.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    /// Authenticated origin FlowId (`0` = trusted host / non-flow).
    pub sender: u64,
    /// Capability granting **SEND** back to [`Self::sender`], or [`CapId::NONE`].
    pub reply_cap: CapId,
    /// Client correlation token; echoed on replies (`Ask` / **S2**).
    pub request_id: u64,
    /// Protocol discriminator (opaque to the VM).
    pub tag: u16,
    /// Application body (shared so hop clones are cheap).
    pub payload: Arc<Value>,
}

impl Message {
    /// Build an envelope. `sender` / `reply_cap` are placeholders until a
    /// bytecode hop is authenticated by the scheduler.
    pub fn new(sender: u64, request_id: u64, tag: u16, payload: impl Into<Value>) -> Self {
        Self {
            sender,
            reply_cap: CapId::NONE,
            request_id,
            tag,
            payload: Arc::new(payload.into()),
        }
    }

    /// Outgoing hop from host Rust (`sender` / `reply_cap` filled on delivery).
    pub fn request(request_id: u64, tag: u16, payload: impl Into<Value>) -> Self {
        Self::new(0, request_id, tag, payload)
    }

    /// Reply envelope echoing `request_id` from a received hop (host path).
    pub fn reply_to(req: &Self, tag: u16, payload: impl Into<Value>) -> Self {
        Self::new(0, req.request_id, tag, payload)
    }

    /// Runtime lifecycle hop: monitor `DOWN` (`tag == `[`TAG_SYS_DOWN`]).
    pub fn down(monitor: u64, target_flow: u64, reason: u64) -> Self {
        Self::new(
            target_flow,
            monitor,
            TAG_SYS_DOWN,
            Value::Int(reason as i64),
        )
    }

    /// Runtime lifecycle hop: Ask target exited, or a linked peer exited
    /// while this flow has `trap_exit` enabled (`tag == `[`TAG_SYS_EXIT`]).
    pub fn linked_exit(target_flow: u64, reason: u64) -> Self {
        Self::new(target_flow, 0, TAG_SYS_EXIT, Value::Int(reason as i64))
    }

    #[inline]
    pub fn is_down(&self) -> bool {
        self.tag == TAG_SYS_DOWN
    }

    #[inline]
    pub fn is_exit(&self) -> bool {
        self.tag == TAG_SYS_EXIT
    }

    /// Stamp origin FlowId and attach a reply capability (scheduler only).
    #[inline]
    pub(crate) fn authenticate(mut self, sender: u64, reply_cap: CapId) -> Self {
        self.sender = sender;
        self.reply_cap = reply_cap;
        self
    }

    pub(crate) fn with_payload(mut self, payload: Value) -> Self {
        self.payload = Arc::new(payload);
        self
    }
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "msg{{from=flow#{}, reply={}, id={}, tag={}, payload={}}}",
            self.sender, self.reply_cap, self.request_id, self.tag, self.payload
        )
    }
}

/// A dynamically-tagged runtime value.
///
/// [`Value::Cap`] is an opaque [`CapId`] for `Send` / `Ask`. Authority lives
/// in the runtime [`crate::CapTable`], keyed by holder — not in this tag.
/// [`Value::Pid`] remains for **identity** inside authenticated messages.
///
/// [`Value::Str`] / [`Value::Bytes`] are heap payloads shared via [`Arc`].
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// Internal / message identity (FlowId as `u64`). **Not** a Send target.
    Pid(u64),
    /// Atomic Hop envelope. See [`Message`].
    Message(Message),
    /// Opaque capability token. Required for `Send` / `Ask`.
    Cap(CapId),
    /// UTF-8 text (constant pool, natives, host).
    Str(Arc<str>),
    /// Opaque byte buffer (constant pool, natives, host).
    Bytes(Arc<[u8]>),
}

impl Value {
    /// BFV0 wire tag for this value (`Unit=0` … `Bytes=8`).
    ///
    /// Used by [`crate::Opcode::ReceiveMatchKind`] selective receive.
    #[inline]
    pub fn wire_tag(&self) -> u8 {
        match self {
            Value::Unit => 0,
            Value::Bool(_) => 1,
            Value::Int(_) => 2,
            Value::Float(_) => 3,
            Value::Pid(_) => 4,
            Value::Message(_) => 5,
            Value::Cap(_) => 6,
            Value::Str(_) => 7,
            Value::Bytes(_) => 8,
        }
    }

    /// Build a [`Value::Str`] from anything string-like.
    #[inline]
    pub fn str(s: impl AsRef<str>) -> Self {
        Value::Str(Arc::from(s.as_ref()))
    }

    /// Build a [`Value::Bytes`] from a byte slice.
    #[inline]
    pub fn bytes(b: impl AsRef<[u8]>) -> Self {
        Value::Bytes(Arc::from(b.as_ref()))
    }

    /// Truthiness used by `Opcode::Branch`: falsy are `Unit`, `Bool(false)`,
    /// `Int(0)`, empty [`Value::Str`], and empty [`Value::Bytes`].
    #[inline]
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Unit | Value::Bool(false) | Value::Int(0) => false,
            Value::Str(s) if s.is_empty() => false,
            Value::Bytes(b) if b.is_empty() => false,
            _ => true,
        }
    }

    #[inline]
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::Bool(b) => Some(*b as i64),
            _ => None,
        }
    }

    #[inline]
    pub fn as_pid(&self) -> Option<u64> {
        match self {
            Value::Pid(p) => Some(*p),
            _ => None,
        }
    }

    #[inline]
    pub fn as_cap(&self) -> Option<CapId> {
        match self {
            Value::Cap(c) => Some(*c),
            _ => None,
        }
    }

    #[inline]
    pub fn as_message(&self) -> Option<&Message> {
        match self {
            Value::Message(m) => Some(m),
            _ => None,
        }
    }

    #[inline]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s.as_ref()),
            _ => None,
        }
    }

    #[inline]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b.as_ref()),
            Value::Str(s) => Some(s.as_bytes()),
            _ => None,
        }
    }

    /// Bytes this value is **charged** for against a mailbox byte budget
    /// (see [`crate::MailboxBytes`]).
    ///
    /// [`Value::Str`] / [`Value::Bytes`] / hop payloads are `Arc`-shared: the
    /// same buffer cloned into N mailboxes exists once in memory, but each
    /// mailbox is charged the full length. That over-counts on purpose.
    #[inline]
    pub fn memory_size(&self) -> usize {
        std::mem::size_of::<Self>() + self.heap_size()
    }

    /// Heap bytes owned (transitively) by this value, excluding the enum
    /// itself.
    #[inline]
    pub fn heap_size(&self) -> usize {
        match self {
            Value::Str(s) => s.len(),
            Value::Bytes(b) => b.len(),
            Value::Message(m) => m.payload.memory_size(),
            Value::Unit
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Pid(_)
            | Value::Cap(_) => 0,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Unit => "unit",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Pid(_) => "pid",
            Value::Message(_) => "message",
            Value::Cap(_) => "cap",
            Value::Str(_) => "str",
            Value::Bytes(_) => "bytes",
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Unit => write!(f, "()"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::Pid(p) => write!(f, "flow#{p}"),
            Value::Message(m) => write!(f, "{m}"),
            Value::Cap(c) => write!(f, "{c}"),
            Value::Str(s) => write!(f, "{s}"),
            Value::Bytes(b) => write!(f, "bytes[{}]", b.len()),
        }
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}
impl From<i32> for Value {
    fn from(v: i32) -> Self {
        Value::Int(i64::from(v))
    }
}
impl From<u64> for Value {
    fn from(v: u64) -> Self {
        Value::Int(v as i64)
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Float(v)
    }
}
impl From<Message> for Value {
    fn from(m: Message) -> Self {
        Value::Message(m)
    }
}
impl From<CapId> for Value {
    fn from(c: CapId) -> Self {
        Value::Cap(c)
    }
}
impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::str(s)
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(Arc::from(s))
    }
}
impl From<&[u8]> for Value {
    fn from(b: &[u8]) -> Self {
        Value::bytes(b)
    }
}
impl From<Vec<u8>> for Value {
    fn from(b: Vec<u8>) -> Self {
        Value::Bytes(Arc::from(b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_is_truthy_and_round_trips_helpers() {
        let m = Message::new(7, 99, 10, 1u64);
        let v = Value::Message(m.clone());
        assert!(v.is_truthy());
        assert_eq!(v.as_message(), Some(&m));
        assert_eq!(v.type_name(), "message");
        assert_eq!(m.payload.as_ref(), &Value::Int(1));
    }

    #[test]
    fn authenticate_stamps_sender_and_reply_cap() {
        let reply = CapId::from_raw(7);
        let m = Message::new(999, 1, 2, 3u64).authenticate(42, reply);
        assert_eq!(m.sender, 42);
        assert_eq!(m.reply_cap, reply);
        assert_eq!(m.request_id, 1);
    }

    #[test]
    fn str_payload_is_charged_on_the_hop() {
        let hop = Value::Message(Message::new(1, 1, 1, Value::str("hello")));
        assert!(hop.heap_size() >= 5);
    }

    #[test]
    fn cap_is_truthy() {
        let cap = CapId::from_raw(3);
        assert!(Value::Cap(cap).is_truthy());
        assert_eq!(Value::Cap(cap).as_cap(), Some(cap));
    }

    #[test]
    fn str_and_bytes_helpers() {
        let s = Value::str("hi");
        assert_eq!(s.as_str(), Some("hi"));
        assert_eq!(s.type_name(), "str");
        assert!(s.is_truthy());
        assert!(!Value::str("").is_truthy());

        let b = Value::bytes([1u8, 2, 3]);
        assert_eq!(b.as_bytes(), Some(&[1, 2, 3][..]));
        assert_eq!(b.type_name(), "bytes");
        assert!(b.is_truthy());
        assert!(!Value::bytes([]).is_truthy());

        assert_eq!(s.as_bytes(), Some(b"hi".as_slice()));
    }

    #[test]
    fn str_eq_compares_content() {
        assert_eq!(Value::str("a"), Value::from("a".to_owned()));
        assert_ne!(Value::str("a"), Value::str("b"));
        assert_eq!(Value::bytes([9]), Value::from(vec![9u8]));
    }
}
