use std::fmt;
use std::sync::Arc;

use super::cap::CapId;

/// Reserved Atomic Hop tag for monitor `DOWN` events (not an application tag).
pub const TAG_SYS_DOWN: u16 = 0xFF01;
/// Reserved Atomic Hop tag for linked-exit notices (`Ask` target death or
/// `trap_exit` link signal).
pub const TAG_SYS_EXIT: u16 = 0xFF02;
/// Reserved Atomic Hop tag for a host `HostAwait` completion.
/// `request_id` is the ticket written into the await register.
pub const TAG_SYS_AWAIT: u16 = 0xFF03;

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

/// Most elements a [`Value::List`] may hold. Wider sequences are rejected
/// by the decoder, `ListPack`, and [`Value::try_list`].
pub const MAX_LIST_LEN: usize = 256;

/// Most pairs a [`Value::Map`] may hold. Keys are [`MapKey`] (`Int` or `Str`).
pub const MAX_MAP_LEN: usize = 256;

/// Most fields a [`Value::Record`] may hold. Positional, not a map.
pub const MAX_RECORD_LEN: usize = 256;

/// Key of a [`Value::Map`]. Floats are not keys: equality would not be
/// a useful match, and a NaN key could never be read back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MapKey {
    Int(i64),
    Str(Arc<str>),
}

impl MapKey {
    pub fn int(n: i64) -> Self {
        MapKey::Int(n)
    }

    pub fn str(text: impl AsRef<str>) -> Self {
        MapKey::Str(Arc::from(text.as_ref()))
    }
}

/// Shared stand-in so [`Drop`] can detach a payload without allocating.
fn discarded_payload() -> Arc<Value> {
    static EMPTY: std::sync::OnceLock<Arc<Value>> = std::sync::OnceLock::new();
    Arc::clone(EMPTY.get_or_init(|| Arc::new(Value::Unit)))
}

impl Drop for Message {
    fn drop(&mut self) {
        // Leaf payloads fall through to the field drop. A nested `Message`
        // is a chain: the default drop recurses one frame per hop and
        // overflowed the stack on a long chain (the same failure `push`
        // used to hit while sizing).
        if !matches!(self.payload.as_ref(), Value::Message(_)) {
            return;
        }
        let payload = std::mem::replace(&mut self.payload, discarded_payload());
        let mut current = match Arc::into_inner(payload) {
            Some(value) => value,
            None => return,
        };
        while let Value::Message(mut message) = current {
            let inner = std::mem::replace(&mut message.payload, discarded_payload());
            current = match Arc::into_inner(inner) {
                Some(value) => value,
                None => return,
            };
        }
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
    /// Bounded sequence (ABI v6 wire tag `9`). At most [`MAX_LIST_LEN`]
    /// elements; nesting shares [`super::MAX_VALUE_DEPTH`] with `Message`.
    List(Arc<[Value]>),
    /// Bounded map (ABI v7 wire tag `10`). At most [`MAX_MAP_LEN`] pairs.
    /// Insertion order is kept. A duplicate key is rejected at construction.
    Map(Arc<[(MapKey, Value)]>),
    /// Positional record (ABI v8 wire tag `11`). At most [`MAX_RECORD_LEN`]
    /// fields. Not a map and not a receive pattern.
    Record(Arc<[Value]>),
}

/// A repeated [`MapKey`] is a distinct failure from nesting or length.
/// [`Value::try_map`] rejects both; the decoder reports the duplicate on its
/// own so a hostile module is not described as too deep.
pub(crate) fn map_has_duplicate_key(entries: &[(MapKey, Value)]) -> bool {
    for index in 0..entries.len() {
        for earlier in 0..index {
            if entries[index].0 == entries[earlier].0 {
                return true;
            }
        }
    }
    false
}

impl Value {
    /// BFV0 wire tag for this value (`Unit=0` … `Bytes=8`, `List=9`, `Map=10`).
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
            Value::List(_) => 9,
            Value::Map(_) => 10,
            Value::Record(_) => 11,
        }
    }

    /// Value stored under `key`, if `self` is a map that contains it.
    pub fn map_get(&self, key: &MapKey) -> Option<&Value> {
        match self {
            Value::Map(entries) => entries
                .iter()
                .find(|(existing, _)| existing == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    /// Build a [`Value::Map`]. Fails when there are more than [`MAX_MAP_LEN`]
    /// pairs, a key is repeated, or nesting exceeds the decoder ceiling.
    pub fn try_map(entries: Vec<(MapKey, Value)>) -> Option<Self> {
        if entries.len() > MAX_MAP_LEN || map_has_duplicate_key(&entries) {
            return None;
        }
        let value = Value::Map(Arc::from(entries));
        if value.within_nesting_limit() {
            Some(value)
        } else {
            None
        }
    }

    /// Build a [`Value::List`]. Fails when `items` is longer than
    /// [`MAX_LIST_LEN`] or the nested `Message` / `List` depth exceeds the
    /// decoder ceiling.
    pub fn try_list(items: Vec<Value>) -> Option<Self> {
        if items.len() > MAX_LIST_LEN {
            return None;
        }
        let value = Value::List(Arc::from(items));
        if value.within_nesting_limit() {
            Some(value)
        } else {
            None
        }
    }

    /// Build a [`Value::Record`]. Fails when `fields` is longer than
    /// [`MAX_RECORD_LEN`] or nesting exceeds the decoder ceiling.
    pub fn try_record(fields: Vec<Value>) -> Option<Self> {
        if fields.len() > MAX_RECORD_LEN {
            return None;
        }
        let value = Value::Record(Arc::from(fields));
        if value.within_nesting_limit() {
            Some(value)
        } else {
            None
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

    /// Same ceiling as the `.bf` decoder ([`super::MAX_VALUE_DEPTH`]).
    ///
    /// A deeper `Message` chain would recurse through cap reissue on a
    /// worker stack. The walk stops at the limit instead of descending.
    pub(crate) fn within_nesting_limit(&self) -> bool {
        fn walk(value: &Value, depth: u32) -> bool {
            if depth > super::MAX_VALUE_DEPTH {
                return false;
            }
            match value {
                Value::Message(message) => walk(message.payload.as_ref(), depth + 1),
                Value::List(items) | Value::Record(items) => {
                    items.iter().all(|item| walk(item, depth + 1))
                }
                Value::Map(entries) => entries.iter().all(|(_, value)| walk(value, depth + 1)),
                _ => true,
            }
        }
        walk(self, 0)
    }

    /// Rewrite every live [`CapId`](crate::CapId) stored as [`Value::Cap`].
    ///
    /// [`CapId::NONE`](crate::CapId::NONE) stays put until `on_message`
    /// decides what a nested hop's `reply_cap` becomes. A [`Value::List`] is
    /// walked element by element, so a token cannot hide in a sequence and
    /// cross a flow boundary unchanged. `on_message` runs after the payload
    /// has been rewritten, from the inside out.
    pub(crate) fn map_caps<E: Clone>(
        &self,
        depth: u32,
        too_nested: &E,
        on_cap: &mut impl FnMut(CapId) -> Result<CapId, E>,
        on_message: &mut impl FnMut(&mut Message) -> Result<(), E>,
    ) -> Result<Value, E> {
        if depth > super::MAX_VALUE_DEPTH {
            return Err(too_nested.clone());
        }
        match self {
            Value::Cap(id) if id.is_none() => Ok(Value::Cap(*id)),
            Value::Cap(id) => Ok(Value::Cap(on_cap(*id)?)),
            Value::Message(message) => {
                let payload =
                    message
                        .payload
                        .map_caps(depth + 1, too_nested, on_cap, on_message)?;
                let mut next = message.clone().with_payload(payload);
                on_message(&mut next)?;
                Ok(Value::Message(next))
            }
            Value::List(items) => {
                let mut next = Vec::with_capacity(items.len());
                for item in items.iter() {
                    next.push(item.map_caps(depth + 1, too_nested, on_cap, on_message)?);
                }
                Value::try_list(next).ok_or_else(|| too_nested.clone())
            }
            Value::Map(entries) => {
                let mut next = Vec::with_capacity(entries.len());
                for (key, value) in entries.iter() {
                    next.push((
                        key.clone(),
                        value.map_caps(depth + 1, too_nested, on_cap, on_message)?,
                    ));
                }
                Value::try_map(next).ok_or_else(|| too_nested.clone())
            }
            Value::Record(fields) => {
                let mut next = Vec::with_capacity(fields.len());
                for field in fields.iter() {
                    next.push(field.map_caps(depth + 1, too_nested, on_cap, on_message)?);
                }
                Value::try_record(next).ok_or_else(|| too_nested.clone())
            }
            other => Ok(other.clone()),
        }
    }

    /// Stamp envelopes that `make_msg` left unset (`sender == 0`,
    /// `request_id == 0`). A non-zero sender was already authenticated and
    /// is kept. `reply_cap` is not minted here — the caller decides whether
    /// a reply grant exists on this boundary.
    ///
    /// `None` means the value is nested past the decoder ceiling.
    pub(crate) fn stamp_unset_envelopes(
        &self,
        depth: u32,
        sender_if_unset: u64,
        mint: &mut impl FnMut() -> u64,
    ) -> Option<Value> {
        if depth > super::MAX_VALUE_DEPTH {
            return None;
        }
        match self {
            Value::Message(message) => {
                let payload =
                    message
                        .payload
                        .stamp_unset_envelopes(depth + 1, sender_if_unset, mint)?;
                let mut next = message.clone().with_payload(payload);
                if next.sender == 0 && sender_if_unset != 0 {
                    next.sender = sender_if_unset;
                }
                if next.request_id == 0 {
                    next.request_id = mint();
                }
                Some(Value::Message(next))
            }
            Value::List(items) => {
                let mut next = Vec::with_capacity(items.len());
                for item in items.iter() {
                    next.push(item.stamp_unset_envelopes(depth + 1, sender_if_unset, mint)?);
                }
                Value::try_list(next)
            }
            Value::Map(entries) => {
                let mut next = Vec::with_capacity(entries.len());
                for (key, value) in entries.iter() {
                    next.push((
                        key.clone(),
                        value.stamp_unset_envelopes(depth + 1, sender_if_unset, mint)?,
                    ));
                }
                Value::try_map(next)
            }
            Value::Record(fields) => {
                let mut next = Vec::with_capacity(fields.len());
                for field in fields.iter() {
                    next.push(field.stamp_unset_envelopes(depth + 1, sender_if_unset, mint)?);
                }
                Value::try_record(next)
            }
            other => Some(other.clone()),
        }
    }

    /// A live capability token appears anywhere in this value, including a
    /// nested hop's `reply_cap` and the elements of a list.
    ///
    /// Past the decoder ceiling the answer is `true`: the caller must fail
    /// closed instead of shipping a value it could not inspect.
    pub(crate) fn contains_live_cap(&self, depth: u32) -> bool {
        if depth > super::MAX_VALUE_DEPTH {
            return true;
        }
        match self {
            Value::Cap(id) => !id.is_none(),
            Value::Message(message) => {
                !message.reply_cap.is_none() || message.payload.contains_live_cap(depth + 1)
            }
            Value::List(items) | Value::Record(items) => {
                items.iter().any(|item| item.contains_live_cap(depth + 1))
            }
            Value::Map(entries) => entries
                .iter()
                .any(|(_, value)| value.contains_live_cap(depth + 1)),
            _ => false,
        }
    }

    /// Truthiness used by `Opcode::Branch`: falsy are `Unit`, `Bool(false)`,
    /// `Int(0)`, empty [`Value::Str`], empty [`Value::Bytes`], empty
    /// [`Value::List`], and empty [`Value::Map`].
    #[inline]
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Unit | Value::Bool(false) | Value::Int(0) => false,
            Value::Str(s) if s.is_empty() => false,
            Value::Bytes(b) if b.is_empty() => false,
            Value::List(items) | Value::Record(items) if items.is_empty() => false,
            Value::Map(entries) if entries.is_empty() => false,
            _ => true,
        }
    }

    /// `Int`, or `Bool` as `0` / `1`.
    ///
    /// This coercion is for host natives ([`crate::vm::expect_int`]). Opcode
    /// operands that require an integer (`Sleep`, tags, `request_id`) do not
    /// use it — a `Bool` there is a type error.
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
    ///
    /// A `Message` chain deeper than [`super::MAX_VALUE_DEPTH`] does not
    /// walk the tail. The returned size is [`usize::MAX`], which no mailbox
    /// budget can admit.
    #[inline]
    pub fn memory_size(&self) -> usize {
        match self.heap_size_limited() {
            Some(heap) => std::mem::size_of::<Self>().saturating_add(heap),
            None => usize::MAX,
        }
    }

    /// Heap bytes owned (transitively) by this value, excluding the enum
    /// itself.
    ///
    /// One `Arc` inside this value is counted once. A chain past the decoder
    /// ceiling yields [`usize::MAX`] instead of recursing.
    #[inline]
    pub fn heap_size(&self) -> usize {
        self.heap_size_limited().unwrap_or(usize::MAX)
    }

    /// `None` when the `Message` chain is deeper than the decoder ceiling or
    /// the byte sum does not fit in `usize`.
    ///
    /// Iterative on purpose: `Mailbox::push` asks for this size, and a
    /// recursive walk of a long chain overflowed the caller's stack.
    fn heap_size_limited(&self) -> Option<usize> {
        fn walk(value: &Value, depth: u32) -> Option<usize> {
            let mut extra = 0usize;
            let mut cursor = value;
            let mut depth = depth;
            loop {
                match cursor {
                    Value::Message(message) => {
                        if depth >= super::MAX_VALUE_DEPTH {
                            return None;
                        }
                        extra = extra.checked_add(std::mem::size_of::<Value>())?;
                        cursor = message.payload.as_ref();
                        depth += 1;
                    }
                    Value::List(items) | Value::Record(items) => {
                        if depth >= super::MAX_VALUE_DEPTH {
                            return None;
                        }
                        let slots = items.len().checked_mul(std::mem::size_of::<Value>())?;
                        extra = extra.checked_add(slots)?;
                        for item in items.iter() {
                            extra = extra.checked_add(walk(item, depth + 1)?)?;
                        }
                        return Some(extra);
                    }
                    Value::Map(entries) => {
                        if depth >= super::MAX_VALUE_DEPTH {
                            return None;
                        }
                        let slots = entries.len().checked_mul(std::mem::size_of::<Value>())?;
                        extra = extra.checked_add(slots)?;
                        for (key, value) in entries.iter() {
                            if let MapKey::Str(text) = key {
                                extra = extra.checked_add(text.len())?;
                            }
                            extra = extra.checked_add(walk(value, depth + 1)?)?;
                        }
                        return Some(extra);
                    }
                    Value::Str(s) => return extra.checked_add(s.len()),
                    Value::Bytes(b) => return extra.checked_add(b.len()),
                    Value::Unit
                    | Value::Bool(_)
                    | Value::Int(_)
                    | Value::Float(_)
                    | Value::Pid(_)
                    | Value::Cap(_) => return Some(extra),
                }
            }
        }
        walk(self, 0)
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
            Value::List(_) => "list",
            Value::Map(_) => "map",
            Value::Record(_) => "record",
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
            Value::List(items) => {
                write!(f, "[")?;
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, "]")
            }
            Value::Map(entries) => {
                write!(f, "{{")?;
                for (index, (key, value)) in entries.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    match key {
                        MapKey::Int(n) => write!(f, "{n}")?,
                        MapKey::Str(text) => write!(f, "{text}")?,
                    }
                    write!(f, ": {value}")?;
                }
                write!(f, "}}")
            }
            Value::Record(fields) => {
                write!(f, "rec(")?;
                for (index, field) in fields.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{field}")?;
                }
                write!(f, ")")
            }
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
    /// Fits the `Int` domain. Values above `i64::MAX` saturate instead of
    /// becoming negative.
    fn from(v: u64) -> Self {
        let n = match i64::try_from(v) {
            Ok(n) => n,
            Err(_) => i64::MAX,
        };
        Value::Int(n)
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
    fn nesting_stops_at_the_decoder_ceiling() {
        let mut value = Value::Int(1);
        for _ in 0..16 {
            value = Value::Message(Message::new(0, 1, 1, value));
        }
        assert!(value.within_nesting_limit());
        value = Value::Message(Message::new(0, 1, 1, value));
        assert!(!value.within_nesting_limit());
    }

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
    fn nested_message_heap_counts_each_envelope_until_the_ceiling() {
        let word = std::mem::size_of::<Value>();
        let mut value = Value::str("hello");
        for _ in 0..16 {
            value = Value::Message(Message::new(0, 1, 1, value));
        }
        assert!(value.within_nesting_limit());
        assert_eq!(value.heap_size(), 16 * word + 5);
        assert_eq!(value.memory_size(), word + 16 * word + 5);
        let blob = Value::Message(Message::new(0, 1, 1, Value::bytes(vec![0u8; 50_000])));
        assert_eq!(blob.heap_size(), word + 50_000);
        value = Value::Message(Message::new(0, 1, 1, value));
        assert!(!value.within_nesting_limit());
        assert_eq!(value.heap_size(), usize::MAX);
        assert_eq!(value.memory_size(), usize::MAX);
    }

    #[test]
    fn a_long_message_chain_sizes_without_recursing_off_the_stack() {
        let mut value = Value::Int(1);
        for _ in 0..10_000 {
            value = Value::Message(Message::new(0, 1, 1, value));
        }
        assert!(!value.within_nesting_limit());
        assert_eq!(value.heap_size(), usize::MAX);
        assert_eq!(value.memory_size(), usize::MAX);
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
    fn u64_above_i64_max_saturates() {
        assert_eq!(Value::from(u64::MAX), Value::Int(i64::MAX));
        assert_eq!(Value::from(4u64), Value::Int(4));
    }

    #[test]
    fn str_eq_compares_content() {
        assert_eq!(Value::str("a"), Value::from("a".to_owned()));
        assert_ne!(Value::str("a"), Value::str("b"));
        assert_eq!(Value::bytes([9]), Value::from(vec![9u8]));
    }
}
