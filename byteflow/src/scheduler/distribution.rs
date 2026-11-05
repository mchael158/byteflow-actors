//! Outbound hops for another node.
//!
//! Byteflow does not open sockets and does not speak a cluster protocol.
//! The host owns I/O. Bytecode [`crate::Opcode::SendRemote`] stamps the
//! sender here and queues the hop on [`DistributionBridge`]; the embedder
//! drains that queue and delivers it however it wants.
//!
//! Payload `Cap` tokens do not leave the node. A live cap, including one
//! nested in a list or in another hop's `reply_cap`, fails the send before
//! a send-quota token is spent. A nested envelope whose `sender` is still
//! `0` is stamped with this flow. `reply_cap` on the outbound hop itself
//! is [`crate::CapId::NONE`] — there is no local flow to mint a reply
//! address for.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::bytecode::{CapId, Message, Value};

use super::error::RuntimeError;
use super::process::FlowId;
use super::sync_lock;

/// Default hop count before [`DistributionBridge`] refuses `SendRemote`.
pub const DEFAULT_MAX_PENDING_HOPS: usize = 1024;
/// Default retained-byte budget for pending outbound hops (64 MiB).
pub const DEFAULT_MAX_PENDING_BYTES: usize = 64 * 1024 * 1024;

/// Limits for [`DistributionBridge`] pending hops (count + bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DistributionConfig {
    /// Max hops waiting for the host (and held by the host after `try_recv`).
    pub max_pending_hops: usize,
    /// Max accounted bytes for those hops (see [`OutboundHop`] charge).
    pub max_pending_bytes: usize,
}

impl Default for DistributionConfig {
    fn default() -> Self {
        Self {
            max_pending_hops: DEFAULT_MAX_PENDING_HOPS,
            max_pending_bytes: DEFAULT_MAX_PENDING_BYTES,
        }
    }
}

/// RAII reservation against a bridge byte/hop budget.
///
/// Lives on [`OutboundHop`] from [`DistributionBridge::push`] until the hop
/// is dropped (still on the bridge, or after the host finished sending).
#[derive(Debug)]
struct BridgeCharge {
    accounts: Arc<BridgeAccounts>,
    bytes: usize,
}

impl Drop for BridgeCharge {
    fn drop(&mut self) {
        self.accounts.release(self.bytes);
    }
}

#[derive(Debug)]
struct BridgeAccounts {
    hops: AtomicUsize,
    bytes: AtomicUsize,
    max_hops: usize,
    max_bytes: usize,
}

impl BridgeAccounts {
    fn try_charge(self: &Arc<Self>, bytes: usize) -> Result<BridgeCharge, ()> {
        loop {
            let hops = self.hops.load(Ordering::Acquire);
            if hops >= self.max_hops {
                return Err(());
            }
            let used = self.bytes.load(Ordering::Acquire);
            let Some(next_bytes) = used.checked_add(bytes) else {
                return Err(());
            };
            if next_bytes > self.max_bytes {
                return Err(());
            }
            if self
                .hops
                .compare_exchange_weak(hops, hops + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue;
            }
            if self
                .bytes
                .compare_exchange_weak(used, next_bytes, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                self.hops.fetch_sub(1, Ordering::AcqRel);
                continue;
            }
            return Ok(BridgeCharge {
                accounts: Arc::clone(self),
                bytes,
            });
        }
    }

    fn release(&self, bytes: usize) {
        self.hops.fetch_sub(1, Ordering::AcqRel);
        // Saturating: a bug should not underflow into a huge budget.
        loop {
            let cur = self.bytes.load(Ordering::Acquire);
            let next = cur.saturating_sub(bytes);
            if self
                .bytes
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }
    }

    fn pending_hops(&self) -> usize {
        self.hops.load(Ordering::Relaxed)
    }

    fn pending_bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }
}

/// One authenticated hop waiting for the host to deliver it off-node.
///
/// Not [`Clone`]: a bridge memory charge must have a single owner. Use
/// [`OutboundHop::new`] for encode/decode paths without a charge.
#[derive(Debug)]
pub struct OutboundHop {
    pub sender: FlowId,
    /// Host-defined node number. `0` is not special.
    pub node: u64,
    pub name: Arc<str>,
    pub message: Message,
    charge: Option<BridgeCharge>,
}

impl OutboundHop {
    /// Build a hop with no bridge reservation (encode/decode / tests).
    pub fn new(sender: FlowId, node: u64, name: Arc<str>, message: Message) -> Self {
        Self {
            sender,
            node,
            name,
            message,
            charge: None,
        }
    }
}

fn hop_byte_cost(hop: &OutboundHop) -> usize {
    std::mem::size_of::<OutboundHop>()
        .saturating_add(hop.name.len())
        .saturating_add(Value::Message(hop.message.clone()).memory_size())
}

/// Queue the host drains. Share one [`Arc`] via [`crate::RuntimeConfig::distribution`].
///
/// Pending hops consume both a hop slot and a byte reservation until the
/// host drops the [`OutboundHop`] (after encode/send or discard). This is
/// independent of per-flow [`crate::QuotaConfig`] memory — a finalized flow
/// must not erase bridge retention.
#[derive(Debug)]
pub struct DistributionBridge {
    queue: Mutex<VecDeque<OutboundHop>>,
    ready: Condvar,
    accounts: Arc<BridgeAccounts>,
}

impl DistributionBridge {
    pub fn new() -> Arc<Self> {
        Self::with_config(DistributionConfig::default())
    }

    pub fn with_config(cfg: DistributionConfig) -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
            accounts: Arc::new(BridgeAccounts {
                hops: AtomicUsize::new(0),
                bytes: AtomicUsize::new(0),
                max_hops: cfg.max_pending_hops.max(1),
                max_bytes: cfg.max_pending_bytes.max(1),
            }),
        })
    }

    pub(crate) fn push(&self, mut hop: OutboundHop) -> Result<(), String> {
        let bytes = hop_byte_cost(&hop);
        let charge = self
            .accounts
            .try_charge(bytes)
            .map_err(|()| "distribution bridge full".to_string())?;
        hop.charge = Some(charge);
        let mut queue =
            sync_lock::lock(&self.queue, "DistributionBridge::push").map_err(|e| e.to_string())?;
        queue.push_back(hop);
        self.ready.notify_one();
        Ok(())
    }

    /// Next hop, if the host has not fallen behind.
    ///
    /// The returned hop still holds its byte reservation until dropped.
    pub fn try_recv(&self) -> Result<Option<OutboundHop>, RuntimeError> {
        let mut queue = sync_lock::lock(&self.queue, "DistributionBridge::try_recv")?;
        Ok(queue.pop_front())
    }

    /// Hops waiting on the bridge queue (not yet `try_recv`'d by the host).
    pub fn outbound_pending(&self) -> Result<usize, RuntimeError> {
        let queue = sync_lock::lock(&self.queue, "DistributionBridge::outbound_pending")?;
        Ok(queue.len())
    }

    /// Hop slots charged (queue + hops held by the host after recv).
    pub fn pending_charged_hops(&self) -> usize {
        self.accounts.pending_hops()
    }

    /// Bytes charged (queue + hops held by the host after recv).
    pub fn pending_charged_bytes(&self) -> usize {
        self.accounts.pending_bytes()
    }

    /// Block until a hop arrives or `timeout` elapses.
    pub fn recv_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<Option<OutboundHop>, RuntimeError> {
        let mut queue = sync_lock::lock(&self.queue, "DistributionBridge::recv_timeout")?;
        let start = std::time::Instant::now();
        loop {
            if let Some(hop) = queue.pop_front() {
                return Ok(Some(hop));
            }
            let left = match timeout.checked_sub(start.elapsed()) {
                Some(left) => left,
                None => return Ok(None),
            };
            let (guard, status) =
                sync_lock::wait_timeout(&self.ready, queue, left, "DistributionBridge::wait")?;
            queue = guard;
            if status.timed_out() && queue.is_empty() {
                return Ok(None);
            }
        }
    }
}

/// Outcome of a transactional remote-frame reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedupState {
    /// Delivery in progress — concurrent peers wait for commit/rollback.
    Pending,
    /// Successfully delivered; retransmits are duplicates.
    Accepted,
}

/// Result of [`RemoteFrameDedup::reserve`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedupTicket {
    /// Caller must [`RemoteFrameDedup::commit`] or [`RemoteFrameDedup::rollback`].
    Fresh { sender: u64, request_id: u64 },
    /// Already accepted (or peer committed while waiting).
    Duplicate,
}

/// Sliding-window replay filter for [`crate::Runtime::deliver_frame`].
///
/// Reservations are transactional: the key is marked [`DedupState::Pending`]
/// before delivery and only becomes [`DedupState::Accepted`] after the
/// mailbox accepts the hop. A failed delivery rolls back so the host may
/// retransmit.
#[derive(Debug)]
pub struct RemoteFrameDedup {
    capacity: usize,
    seen: Mutex<VecDeque<(u64, u64, DedupState)>>,
    wait: Condvar,
}

impl RemoteFrameDedup {
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(RemoteFrameDedup {
            capacity: capacity.max(1),
            seen: Mutex::new(VecDeque::new()),
            wait: Condvar::new(),
        })
    }

    /// Atomically reserve `(sender, request_id)` for delivery.
    ///
    /// If another worker holds [`DedupState::Pending`] on the same key, this
    /// waits until that peer commits or rolls back.
    pub fn reserve(&self, sender: u64, request_id: u64) -> Result<DedupTicket, RuntimeError> {
        let mut queue = sync_lock::lock(&self.seen, "RemoteFrameDedup::reserve")?;
        loop {
            if let Some(idx) = queue
                .iter()
                .position(|&(s, id, _)| s == sender && id == request_id)
            {
                match queue[idx].2 {
                    DedupState::Accepted => return Ok(DedupTicket::Duplicate),
                    DedupState::Pending => {
                        queue = sync_lock::wait(&self.wait, queue, "RemoteFrameDedup::wait")?;
                        continue;
                    }
                }
            }
            queue.push_back((sender, request_id, DedupState::Pending));
            Self::evict_accepted(&mut queue, self.capacity);
            return Ok(DedupTicket::Fresh { sender, request_id });
        }
    }

    /// Mark a reserved key as accepted (idempotent retransmits become duplicates).
    pub fn commit(&self, sender: u64, request_id: u64) -> Result<(), RuntimeError> {
        let mut queue = sync_lock::lock(&self.seen, "RemoteFrameDedup::commit")?;
        if let Some(entry) = queue
            .iter_mut()
            .find(|&&mut (s, id, _)| s == sender && id == request_id)
        {
            entry.2 = DedupState::Accepted;
        }
        Self::evict_accepted(&mut queue, self.capacity);
        self.wait.notify_all();
        Ok(())
    }

    /// Drop a pending reservation so a later retransmit may reserve again.
    pub fn rollback(&self, sender: u64, request_id: u64) -> Result<(), RuntimeError> {
        let mut queue = sync_lock::lock(&self.seen, "RemoteFrameDedup::rollback")?;
        queue.retain(|&(s, id, state)| {
            !(s == sender && id == request_id && state == DedupState::Pending)
        });
        self.wait.notify_all();
        Ok(())
    }

    /// Drop oldest [`DedupState::Accepted`] entries while over capacity.
    /// Pending slots are kept (may briefly exceed capacity under load).
    fn evict_accepted(queue: &mut VecDeque<(u64, u64, DedupState)>, capacity: usize) {
        while queue.len() > capacity {
            let Some(front) = queue.front() else {
                break;
            };
            if front.2 == DedupState::Pending {
                break;
            }
            queue.pop_front();
        }
    }
}

/// Stamp `sender` and clear `reply_cap`. Does not mint a local reply Cap.
pub(crate) fn stamp(message: Message, sender: FlowId) -> Message {
    message.authenticate(sender.as_u64(), CapId::NONE)
}

/// `SendRemote` effect. `message` must already be a hop.
pub(crate) fn deliver(
    shared: &super::runtime::Shared,
    flow: &mut super::process::Flow,
    node: u64,
    name: Arc<str>,
    message: Value,
) -> Result<(), String> {
    if !flow.authority.rights().contains(crate::CapRights::SEND) {
        return Err("send_remote denied: flow lacks SEND right".into());
    }
    let Some(bridge) = shared.distribution.as_ref() else {
        return Err("send_remote: no distribution bridge".into());
    };
    let Some(mut msg) = message.as_message().cloned() else {
        return Err("send_remote: value is not a hop".into());
    };
    if !msg.payload.within_nesting_limit() {
        return Err("send_remote: payload too nested".into());
    }
    if msg.payload.contains_live_cap(0) {
        return Err("send_remote: payload contains a local capability".into());
    }
    if let Err(e) = flow.quota.check_send() {
        return Err(e.to_string());
    }
    if msg.request_id == 0 {
        msg.request_id = flow.vm.fresh_request_id();
    }
    let Some(payload) = msg
        .payload
        .stamp_unset_envelopes(0, flow.id.as_u64(), &mut || flow.vm.fresh_request_id())
    else {
        return Err("send_remote: payload too nested".into());
    };
    let stamped = stamp(msg.with_payload(payload), flow.id);
    bridge.push(OutboundHop::new(flow.id, node, name, stamped))
}

const FRAME_MAGIC: [u8; 4] = *b"BFH1";
const FRAME_VERSION: u8 = 1;
const MAX_FRAME_NAME: usize = 256;

/// Why a remote hop frame was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteFrameError {
    BadMagic,
    Truncated,
    BadVersion,
    BadUtf8,
    /// A `Cap` or `Pid` was on the wire, or a nested hop still had a reply cap.
    LiveCapability,
    /// Sender `0` is the local host. A remote frame must name another id.
    SenderZero,
    /// The frame header and the envelope name different senders.
    SenderMismatch,
    Name,
    Unsupported,
    NoSuchName,
    Mailbox(super::mailbox::MailboxFullReason),
    Unavailable,
}

impl std::fmt::Display for RemoteFrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadMagic => f.write_str("remote frame magic"),
            Self::Truncated => f.write_str("remote frame truncated"),
            Self::BadVersion => f.write_str("remote frame version"),
            Self::BadUtf8 => f.write_str("remote frame name"),
            Self::LiveCapability => f.write_str("remote frame carried a local capability"),
            Self::SenderZero => f.write_str("remote frame sender is the local host"),
            Self::SenderMismatch => f.write_str("remote frame sender does not match the envelope"),
            Self::Name => f.write_str("remote frame name"),
            Self::Unsupported => f.write_str("remote frame value"),
            Self::NoSuchName => f.write_str("remote frame name is not registered"),
            Self::Mailbox(reason) => write!(f, "remote frame mailbox ({reason})"),
            Self::Unavailable => f.write_str("remote frame unavailable"),
        }
    }
}

impl std::error::Error for RemoteFrameError {}

/// Bytes the host can write on its own socket. No capability leaves the node.
pub fn encode_frame(hop: &OutboundHop) -> Result<Vec<u8>, RemoteFrameError> {
    if hop.sender.is_host() {
        return Err(RemoteFrameError::SenderZero);
    }
    if hop.name.is_empty() || hop.name.len() > MAX_FRAME_NAME {
        return Err(RemoteFrameError::Name);
    }
    if hop.message.sender != hop.sender.as_u64() {
        return Err(frame_sender_error(hop.message.sender));
    }
    screen_message(&hop.message, 0)?;
    let mut out = Vec::with_capacity(32 + hop.name.len());
    out.extend_from_slice(&FRAME_MAGIC);
    out.push(FRAME_VERSION);
    out.extend_from_slice(&hop.node.to_le_bytes());
    out.extend_from_slice(&(hop.name.len() as u16).to_le_bytes());
    out.extend_from_slice(hop.name.as_bytes());
    out.extend_from_slice(&hop.sender.as_u64().to_le_bytes());
    out.extend_from_slice(&crate::bytecode::encode_value(&Value::Message(
        hop.message.clone(),
    )));
    Ok(out)
}

/// Inverse of [`encode_frame`]. The host still decides which bytes are trusted.
pub fn decode_frame(bytes: &[u8]) -> Result<OutboundHop, RemoteFrameError> {
    let mut pos = 0usize;
    let magic: [u8; 4] = take(bytes, &mut pos)?;
    if magic != FRAME_MAGIC {
        return Err(RemoteFrameError::BadMagic);
    }
    let version: [u8; 1] = take(bytes, &mut pos)?;
    if version[0] != FRAME_VERSION {
        return Err(RemoteFrameError::BadVersion);
    }
    let node = u64::from_le_bytes(take(bytes, &mut pos)?);
    let name_len = u16::from_le_bytes(take(bytes, &mut pos)?) as usize;
    if name_len == 0 || name_len > MAX_FRAME_NAME {
        return Err(RemoteFrameError::Name);
    }
    let name_bytes = take_slice(bytes, &mut pos, name_len)?;
    let name = std::str::from_utf8(name_bytes).map_err(|_| RemoteFrameError::BadUtf8)?;
    let sender = u64::from_le_bytes(take(bytes, &mut pos)?);
    if sender == 0 {
        return Err(RemoteFrameError::SenderZero);
    }
    let (value, consumed) =
        crate::bytecode::decode_value(&bytes[pos..]).map_err(frame_format_error)?;
    if consumed != bytes.len() - pos {
        return Err(RemoteFrameError::Unsupported);
    }
    let Value::Message(message) = value else {
        return Err(RemoteFrameError::Unsupported);
    };
    if message.sender != sender {
        return Err(frame_sender_error(message.sender));
    }
    screen_message(&message, 0)?;
    Ok(OutboundHop::new(
        FlowId(sender),
        node,
        Arc::from(name),
        message,
    ))
}

fn frame_sender_error(message_sender: u64) -> RemoteFrameError {
    if message_sender == 0 {
        RemoteFrameError::SenderZero
    } else {
        RemoteFrameError::SenderMismatch
    }
}

fn take<const N: usize>(data: &[u8], pos: &mut usize) -> Result<[u8; N], RemoteFrameError> {
    let slice = take_slice(data, pos, N)?;
    let mut out = [0u8; N];
    out.copy_from_slice(slice);
    Ok(out)
}

fn take_slice<'a>(data: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8], RemoteFrameError> {
    let end = pos.checked_add(n).ok_or(RemoteFrameError::Truncated)?;
    if end > data.len() {
        return Err(RemoteFrameError::Truncated);
    }
    let slice = &data[*pos..end];
    *pos = end;
    Ok(slice)
}

fn frame_format_error(err: crate::bytecode::FormatError) -> RemoteFrameError {
    match err {
        crate::bytecode::FormatError::Truncated => RemoteFrameError::Truncated,
        crate::bytecode::FormatError::BadUtf8 => RemoteFrameError::BadUtf8,
        crate::bytecode::FormatError::UnknownValueTag(4 | 6) => RemoteFrameError::LiveCapability,
        _ => RemoteFrameError::Unsupported,
    }
}

fn screen_message(message: &Message, depth: u32) -> Result<(), RemoteFrameError> {
    if depth > crate::bytecode::MAX_VALUE_DEPTH {
        return Err(RemoteFrameError::Unsupported);
    }
    if !message.reply_cap.is_none() {
        return Err(RemoteFrameError::LiveCapability);
    }
    screen_value(message.payload.as_ref(), depth + 1)
}

fn screen_value(value: &Value, depth: u32) -> Result<(), RemoteFrameError> {
    if depth > crate::bytecode::MAX_VALUE_DEPTH {
        return Err(RemoteFrameError::Unsupported);
    }
    match value {
        Value::Pid(_) | Value::Cap(_) => Err(RemoteFrameError::LiveCapability),
        Value::Message(message) => screen_message(message, depth),
        Value::List(items) => {
            if items.len() > crate::bytecode::MAX_LIST_LEN {
                return Err(RemoteFrameError::Unsupported);
            }
            items
                .iter()
                .try_for_each(|item| screen_value(item, depth + 1))
        }
        Value::Record(fields) => {
            if fields.len() > crate::bytecode::MAX_RECORD_LEN {
                return Err(RemoteFrameError::Unsupported);
            }
            fields
                .iter()
                .try_for_each(|field| screen_value(field, depth + 1))
        }
        Value::Map(entries) => {
            if entries.len() > crate::bytecode::MAX_MAP_LEN {
                return Err(RemoteFrameError::Unsupported);
            }
            entries
                .iter()
                .try_for_each(|(_, item)| screen_value(item, depth + 1))
        }
        Value::Unit
        | Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::Str(_)
        | Value::Bytes(_) => Ok(()),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::time::Duration;

    use crate::{FlowOutcome, Program, Runtime, RuntimeConfig, Value};

    use super::*;

    #[test]
    fn host_drains_a_remote_hop() -> Result<(), Box<dyn std::error::Error>> {
        let bridge = DistributionBridge::new();
        let mut program = Program::new("remote");
        program.function("main", 0, |f| {
            let node = f.load_i32(7);
            let name = f.load_str("svc");
            let payload = f.load_i32(41);
            let msg = f.hop_fresh(3, payload);
            f.send_remote(node, name, msg);
            f.return_(payload);
        });
        let rt = Runtime::with_natives_and_config(
            program.build(),
            crate::std_native_table(),
            RuntimeConfig {
                distribution: Some(Arc::clone(&bridge)),
                workers: 1,
                ..RuntimeConfig::default()
            },
        )?;
        let outcome = rt.spawn(0, &[])?.join();
        rt.shutdown();
        match outcome {
            FlowOutcome::Completed(Value::Int(41)) => {}
            other => return Err(format!("remote send failed: {other:?}").into()),
        }
        let hop = match bridge.recv_timeout(Duration::from_secs(1))? {
            Some(hop) => hop,
            None => return Err("bridge empty".into()),
        };
        if hop.node != 7 || hop.name.as_ref() != "svc" {
            return Err(format!("unexpected route {hop:?}").into());
        }
        if hop.message.reply_cap != CapId::NONE {
            return Err("remote hop minted a local reply cap".into());
        }
        if !matches!(hop.message.payload.as_ref(), Value::Int(41)) {
            return Err(format!("payload {:?}", hop.message.payload).into());
        }
        Ok(())
    }

    #[test]
    fn a_live_cap_in_the_payload_does_not_leave_the_node() -> Result<(), Box<dyn std::error::Error>>
    {
        let bridge = DistributionBridge::new();
        let mut program = Program::new("remote-cap");
        program.function("main", 0, |f| {
            let node = f.load_i32(7);
            let name = f.load_str("svc");
            let me = f.self_cap();
            let msg = f.hop_fresh(3, me);
            f.send_remote(node, name, msg);
            f.return_(node);
        });
        let rt = Runtime::with_natives_and_config(
            program.build(),
            crate::std_native_table(),
            RuntimeConfig {
                distribution: Some(Arc::clone(&bridge)),
                workers: 1,
                ..RuntimeConfig::default()
            },
        )?;
        let outcome = rt.spawn(0, &[])?.join();
        rt.shutdown();
        match outcome {
            FlowOutcome::Failed(reason) if reason.contains("local capability") => {}
            other => return Err(format!("expected a refused remote cap, got {other:?}").into()),
        }
        if bridge.try_recv()?.is_some() {
            return Err("bridge accepted a hop that carried a capability".into());
        }
        Ok(())
    }

    #[test]
    fn remote_nested_envelope_is_stamped_with_the_flow() -> Result<(), Box<dyn std::error::Error>> {
        let bridge = DistributionBridge::new();
        let mut program = Program::new("remote-nested");
        program.function("main", 0, |f| {
            let node = f.load_i32(7);
            let name = f.load_str("svc");
            let body = f.load_i32(1);
            let inner = f.hop_fresh(1, body);
            let outer = f.hop_fresh(2, inner);
            f.send_remote(node, name, outer);
            f.return_(node);
        });
        let rt = Runtime::with_natives_and_config(
            program.build(),
            crate::std_native_table(),
            RuntimeConfig {
                distribution: Some(Arc::clone(&bridge)),
                workers: 1,
                ..RuntimeConfig::default()
            },
        )?;
        let outcome = rt.spawn(0, &[])?.join();
        rt.shutdown();
        if !matches!(outcome, FlowOutcome::Completed(Value::Int(7))) {
            return Err(format!("remote nested send failed: {outcome:?}").into());
        }
        let hop = match bridge.recv_timeout(Duration::from_secs(1))? {
            Some(hop) => hop,
            None => return Err("bridge empty".into()),
        };
        let Some(inner) = hop.message.payload.as_message() else {
            return Err(format!("payload {:?}", hop.message.payload).into());
        };
        if inner.sender != hop.sender.as_u64() {
            return Err(format!("nested sender {} hop {}", inner.sender, hop.sender).into());
        }
        if !inner.reply_cap.is_none() {
            return Err("remote nested hop minted a local reply cap".into());
        }
        Ok(())
    }

    #[test]
    fn frame_keeps_the_declared_sender() -> Result<(), RemoteFrameError> {
        use crate::CapId;
        let hop = OutboundHop::new(
            FlowId(9),
            3,
            std::sync::Arc::from("svc"),
            Message::new(9, 4, 1, Value::Int(5)).authenticate(9, CapId::NONE),
        );
        let back = decode_frame(&encode_frame(&hop)?)?;
        if back.sender.as_u64() != 9 || back.node != 3 || back.message.tag != 1 {
            return Err(RemoteFrameError::Unsupported);
        }
        if !matches!(back.message.payload.as_ref(), Value::Int(5)) {
            return Err(RemoteFrameError::Unsupported);
        }
        Ok(())
    }

    #[test]
    fn frame_round_trips_a_record_without_a_cap() -> Result<(), RemoteFrameError> {
        use crate::CapId;
        let record = Value::try_record(vec![Value::Int(1), Value::Int(2)])
            .ok_or(RemoteFrameError::Unsupported)?;
        let hop = OutboundHop::new(
            FlowId(9),
            3,
            std::sync::Arc::from("svc"),
            Message::new(9, 4, 1, record).authenticate(9, CapId::NONE),
        );
        let back = decode_frame(&encode_frame(&hop)?)?;
        if back.sender.as_u64() != 9 || !back.message.reply_cap.is_none() {
            return Err(RemoteFrameError::Unsupported);
        }
        match back.message.payload.as_ref() {
            Value::Record(fields) if fields.len() == 2 => Ok(()),
            _ => Err(RemoteFrameError::Unsupported),
        }
    }

    #[test]
    fn frame_rejects_a_live_cap() {
        use crate::CapId;
        let hop = OutboundHop::new(
            FlowId(9),
            1,
            std::sync::Arc::from("svc"),
            Message::new(9, 1, 1, Value::Cap(CapId::from_raw(1))).authenticate(9, CapId::NONE),
        );
        assert!(matches!(
            encode_frame(&hop),
            Err(RemoteFrameError::LiveCapability)
        ));
    }

    #[test]
    fn frame_rejects_a_list_past_the_cap() {
        let hop = OutboundHop::new(
            FlowId(9),
            1,
            std::sync::Arc::from("svc"),
            Message::new(
                9,
                1,
                1,
                Value::List(std::sync::Arc::from(vec![Value::Int(1); 257])),
            )
            .authenticate(9, crate::CapId::NONE),
        );
        assert!(matches!(
            encode_frame(&hop),
            Err(RemoteFrameError::Unsupported)
        ));
    }

    #[test]
    fn frame_accepts_a_list_at_the_cap() -> Result<(), RemoteFrameError> {
        let list =
            Value::try_list(vec![Value::Int(1); 256]).ok_or(RemoteFrameError::Unsupported)?;
        let hop = OutboundHop::new(
            FlowId(9),
            1,
            std::sync::Arc::from("svc"),
            Message::new(9, 1, 1, list).authenticate(9, crate::CapId::NONE),
        );
        encode_frame(&hop).map(|_| ())
    }

    #[test]
    fn bridge_rejects_when_hop_count_exhausted() -> Result<(), Box<dyn std::error::Error>> {
        let bridge = DistributionBridge::with_config(DistributionConfig {
            max_pending_hops: 2,
            max_pending_bytes: DEFAULT_MAX_PENDING_BYTES,
        });
        let mk = |rid| {
            OutboundHop::new(
                FlowId(1),
                1,
                Arc::from("svc"),
                Message::new(1, rid, 1, Value::Int(1)).authenticate(1, CapId::NONE),
            )
        };
        bridge.push(mk(1))?;
        bridge.push(mk(2))?;
        match bridge.push(mk(3)) {
            Err(msg) if msg.contains("full") => {}
            other => return Err(format!("expected full, got {other:?}").into()),
        }
        assert_eq!(bridge.pending_charged_hops(), 2);
        let _ = bridge.try_recv()?;
        assert_eq!(bridge.pending_charged_hops(), 1);
        bridge.push(mk(4))?;
        Ok(())
    }

    #[test]
    fn bridge_rejects_when_byte_budget_exhausted() -> Result<(), Box<dyn std::error::Error>> {
        let probe = OutboundHop::new(
            FlowId(1),
            1,
            Arc::from("svc"),
            Message::new(1, 1, 1, Value::Bytes(Arc::<[u8]>::from(vec![0u8; 400])))
                .authenticate(1, CapId::NONE),
        );
        let one = hop_byte_cost(&probe);
        let bridge = DistributionBridge::with_config(DistributionConfig {
            max_pending_hops: 64,
            // Room for exactly one large hop.
            max_pending_bytes: one,
        });
        bridge.push(probe)?;
        let again = OutboundHop::new(
            FlowId(1),
            1,
            Arc::from("svc"),
            Message::new(1, 2, 1, Value::Bytes(Arc::<[u8]>::from(vec![0u8; 400])))
                .authenticate(1, CapId::NONE),
        );
        match bridge.push(again) {
            Err(msg) if msg.contains("full") => {}
            other => return Err(format!("expected byte full, got {other:?}").into()),
        }
        let held = bridge.try_recv()?.ok_or("empty")?;
        assert!(bridge.pending_charged_bytes() > 0);
        drop(held);
        assert_eq!(bridge.pending_charged_bytes(), 0);
        assert_eq!(bridge.pending_charged_hops(), 0);
        Ok(())
    }

    #[test]
    fn bridge_charge_survives_after_recv_until_drop() -> Result<(), Box<dyn std::error::Error>> {
        let bridge = DistributionBridge::with_config(DistributionConfig {
            max_pending_hops: 1,
            max_pending_bytes: DEFAULT_MAX_PENDING_BYTES,
        });
        bridge.push(OutboundHop::new(
            FlowId(1),
            1,
            Arc::from("svc"),
            Message::new(1, 1, 1, Value::Int(1)).authenticate(1, CapId::NONE),
        ))?;
        let hop = bridge.try_recv()?.ok_or("empty")?;
        assert_eq!(bridge.outbound_pending()?, 0);
        assert_eq!(bridge.pending_charged_hops(), 1);
        match bridge.push(OutboundHop::new(
            FlowId(1),
            1,
            Arc::from("svc"),
            Message::new(1, 2, 1, Value::Int(2)).authenticate(1, CapId::NONE),
        )) {
            Err(msg) if msg.contains("full") => {}
            other => return Err(format!("host-held hop should keep slot, got {other:?}").into()),
        }
        drop(hop);
        bridge.push(OutboundHop::new(
            FlowId(1),
            1,
            Arc::from("svc"),
            Message::new(1, 3, 1, Value::Int(3)).authenticate(1, CapId::NONE),
        ))?;
        Ok(())
    }

    #[test]
    fn remote_frame_dedup_commit_makes_replay_duplicate() -> Result<(), RuntimeError> {
        let dedup = RemoteFrameDedup::new(4);
        let DedupTicket::Fresh { sender, request_id } = dedup.reserve(1, 100)? else {
            panic!("expected Fresh");
        };
        dedup.commit(sender, request_id)?;
        assert_eq!(dedup.reserve(1, 100)?, DedupTicket::Duplicate);
        assert!(matches!(dedup.reserve(1, 101)?, DedupTicket::Fresh { .. }));
        Ok(())
    }

    #[test]
    fn remote_frame_dedup_rollback_allows_retry() -> Result<(), RuntimeError> {
        let dedup = RemoteFrameDedup::new(4);
        let DedupTicket::Fresh { sender, request_id } = dedup.reserve(1, 100)? else {
            panic!("expected Fresh");
        };
        dedup.rollback(sender, request_id)?;
        assert!(matches!(dedup.reserve(1, 100)?, DedupTicket::Fresh { .. }));
        Ok(())
    }

    #[test]
    fn remote_frame_dedup_concurrent_pending_serializes() -> Result<(), RuntimeError> {
        use std::sync::Barrier;
        use std::thread;
        let dedup = RemoteFrameDedup::new(8);
        let barrier = Arc::new(Barrier::new(2));
        let d1 = Arc::clone(&dedup);
        let b1 = Arc::clone(&barrier);
        let t1 = thread::spawn(move || -> Result<DedupTicket, RuntimeError> {
            let ticket = d1.reserve(9, 42)?;
            b1.wait();
            thread::sleep(Duration::from_millis(30));
            if let DedupTicket::Fresh { sender, request_id } = ticket {
                d1.commit(sender, request_id)?;
            }
            Ok(ticket)
        });
        let d2 = Arc::clone(&dedup);
        let b2 = Arc::clone(&barrier);
        let t2 = thread::spawn(move || -> Result<DedupTicket, RuntimeError> {
            b2.wait();
            d2.reserve(9, 42)
        });
        let a = t1.join().expect("t1")?;
        let b = t2.join().expect("t2")?;
        let fresh = matches!(a, DedupTicket::Fresh { .. }) as u8
            + matches!(b, DedupTicket::Fresh { .. }) as u8;
        let dups =
            matches!(a, DedupTicket::Duplicate) as u8 + matches!(b, DedupTicket::Duplicate) as u8;
        assert_eq!(fresh, 1, "exactly one fresh reservation");
        assert_eq!(dups, 1, "peer sees duplicate after commit");
        Ok(())
    }

    #[test]
    fn deliver_frame_retries_after_unknown_name() -> Result<(), Box<dyn std::error::Error>> {
        use crate::CapId;
        let mut program = Program::new("late");
        program.function("main", 0, |f| {
            let msg = f.receive();
            let n = f.hop_payload(msg);
            f.return_(n);
        });
        let rt = Runtime::with_natives_and_config(
            program.build(),
            crate::std_native_table(),
            RuntimeConfig {
                workers: 1,
                remote_dedup_window: 32,
                ..RuntimeConfig::default()
            },
        )?;
        let handle = rt.spawn(0, &[])?;
        let frame = encode_frame(&OutboundHop::new(
            FlowId(44),
            2,
            Arc::from("late"),
            Message::new(44, 1, 99, Value::Int(8)).authenticate(44, CapId::NONE),
        ))?;
        match rt.deliver_frame(&frame) {
            Err(RemoteFrameError::NoSuchName) => {}
            other => return Err(format!("expected NoSuchName, got {other:?}").into()),
        }
        let cap = rt.mint_cap(handle.id())?;
        rt.register_name("late", cap)?;
        rt.deliver_frame(&frame).map_err(|err| format!("{err:?}"))?;
        let outcome = handle
            .join_timeout(Duration::from_secs(2))
            .ok_or("retry after register timed out")?;
        rt.shutdown();
        match outcome {
            FlowOutcome::Completed(Value::Int(8)) => Ok(()),
            other => Err(format!("retry deliver: {other:?}").into()),
        }
    }

    #[test]
    fn deliver_frame_retries_after_mailbox_full() -> Result<(), Box<dyn std::error::Error>> {
        use crate::scheduler::{MailboxCapacity, MailboxConfig, OverflowPolicy};
        use crate::CapId;
        let mut program = Program::new("full");
        // Long sleep so both remote frames hit a non-parked inbox (queue path).
        program.function("main", 0, |f| {
            let delay = f.load_i32(500);
            f.sleep(delay);
            let _filler = f.receive();
            let msg = f.receive();
            let n = f.hop_payload(msg);
            f.return_(n);
        });
        let capacity = MailboxCapacity::new(1).ok_or("capacity")?;
        let rt = Runtime::with_natives_and_config(
            program.build(),
            crate::std_native_table(),
            RuntimeConfig {
                workers: 1,
                remote_dedup_window: 32,
                mailbox: MailboxConfig::new(capacity, OverflowPolicy::Reject),
                ..RuntimeConfig::default()
            },
        )?;
        let handle = rt.spawn(0, &[])?;
        let cap = rt.mint_cap(handle.id())?;
        rt.register_name("full", cap)?;
        std::thread::sleep(Duration::from_millis(50));
        // Message::new(sender, request_id, tag, payload)
        let filler = encode_frame(&OutboundHop::new(
            FlowId(50),
            2,
            Arc::from("full"),
            Message::new(50, 1, 1, Value::Int(1)).authenticate(50, CapId::NONE),
        ))?;
        let target = encode_frame(&OutboundHop::new(
            FlowId(50),
            2,
            Arc::from("full"),
            Message::new(50, 99, 1, Value::Int(7)).authenticate(50, CapId::NONE),
        ))?;
        rt.deliver_frame(&filler)
            .map_err(|err| format!("filler: {err:?}"))?;
        match rt.deliver_frame(&target) {
            Err(RemoteFrameError::Mailbox(_)) => {}
            other => return Err(format!("expected Mailbox full, got {other:?}").into()),
        }
        // After the sleeper wakes and consumes the filler, retransmit succeeds.
        std::thread::sleep(Duration::from_millis(600));
        rt.deliver_frame(&target)
            .map_err(|err| format!("retransmit: {err:?}"))?;
        let outcome = handle
            .join_timeout(Duration::from_secs(2))
            .ok_or("mailbox-full retry timed out")?;
        rt.shutdown();
        match outcome {
            FlowOutcome::Completed(Value::Int(7)) => Ok(()),
            other => Err(format!("mailbox-full retry: {other:?}").into()),
        }
    }

    #[test]
    fn deliver_frame_accepted_replay_is_silent() -> Result<(), Box<dyn std::error::Error>> {
        use crate::CapId;
        let mut program = Program::new("once");
        program.function("main", 0, |f| {
            let msg = f.receive();
            let n = f.hop_payload(msg);
            f.return_(n);
        });
        let rt = Runtime::with_natives_and_config(
            program.build(),
            crate::std_native_table(),
            RuntimeConfig {
                workers: 1,
                remote_dedup_window: 32,
                ..RuntimeConfig::default()
            },
        )?;
        let handle = rt.spawn(0, &[])?;
        let cap = rt.mint_cap(handle.id())?;
        rt.register_name("once", cap)?;
        let frame = encode_frame(&OutboundHop::new(
            FlowId(44),
            2,
            Arc::from("once"),
            Message::new(44, 1, 7, Value::Int(8)).authenticate(44, CapId::NONE),
        ))?;
        rt.deliver_frame(&frame).map_err(|err| format!("{err:?}"))?;
        rt.deliver_frame(&frame)
            .map_err(|err| format!("replay: {err:?}"))?;
        let outcome = handle
            .join_timeout(Duration::from_secs(2))
            .ok_or("accepted replay timed out")?;
        rt.shutdown();
        match outcome {
            FlowOutcome::Completed(Value::Int(8)) => Ok(()),
            other => Err(format!("accepted replay: {other:?}").into()),
        }
    }

    #[test]
    fn deliver_frame_lands_in_the_named_mailbox() -> Result<(), Box<dyn std::error::Error>> {
        use crate::CapId;
        let mut program = Program::new("inbox");
        program.function("main", 0, |f| {
            let msg = f.receive();
            let n = f.hop_payload(msg);
            f.return_(n);
        });
        let rt = Runtime::with_natives_and_config(
            program.build(),
            crate::std_native_table(),
            RuntimeConfig {
                workers: 1,
                ..RuntimeConfig::default()
            },
        )?;
        let handle = rt.spawn(0, &[])?;
        let cap = rt.mint_cap(handle.id())?;
        rt.register_name("inbox", cap)?;
        let frame = encode_frame(&OutboundHop::new(
            FlowId(44),
            2,
            std::sync::Arc::from("inbox"),
            Message::new(44, 1, 7, Value::Int(8)).authenticate(44, CapId::NONE),
        ))?;
        rt.deliver_frame(&frame).map_err(|err| format!("{err:?}"))?;
        let outcome = handle
            .join_timeout(Duration::from_secs(2))
            .ok_or("deliver frame timed out")?;
        rt.shutdown();
        match outcome {
            FlowOutcome::Completed(Value::Int(8)) => Ok(()),
            other => Err(format!("deliver frame: {other:?}").into()),
        }
    }

    #[test]
    fn receive_match_cmp_keeps_the_failing_int_queued() -> Result<(), Box<dyn std::error::Error>> {
        use crate::bytecode::IntCmp;
        use crate::bytecode::Message;
        let mut program = Program::new("guard");
        program.function("main", 0, |f| {
            let tag = f.load_i32(1);
            let bound = f.load_i32(10);
            let msg = f.receive_match_cmp(tag, bound, IntCmp::Lt);
            let n = f.hop_payload(msg);
            f.return_(n);
        });
        let rt = Runtime::with_natives_and_config(
            program.build(),
            crate::std_native_table(),
            RuntimeConfig {
                workers: 1,
                ..RuntimeConfig::default()
            },
        )?;
        let handle = rt.spawn(0, &[])?;
        rt.send(
            handle.id(),
            Value::Message(Message::request(1, 1, Value::Int(20))),
        )?;
        rt.send(
            handle.id(),
            Value::Message(Message::request(2, 1, Value::Int(3))),
        )?;
        let outcome = handle
            .join_timeout(Duration::from_secs(2))
            .ok_or("guard timed out")?;
        rt.shutdown();
        match outcome {
            FlowOutcome::Completed(Value::Int(3)) => Ok(()),
            other => Err(format!("guard: {other:?}").into()),
        }
    }

    #[test]
    fn table_put_get_and_read_only_delegate() -> Result<(), Box<dyn std::error::Error>> {
        use crate::bytecode::CapRights;
        let mut program = Program::new("table");
        program.function("main", 0, |f| {
            let name = f.load_str("box");
            let table = f.table_open(name);
            let key = f.load_i32(1);
            let value = f.load_i32(42);
            f.table_put(table, key, value);
            let got = f.table_get(table, key);
            f.return_(got);
        });
        let rt = Runtime::with_natives_and_config(
            program.build(),
            crate::std_native_table(),
            RuntimeConfig {
                workers: 1,
                ..RuntimeConfig::default()
            },
        )?;
        let handle = rt.spawn(0, &[])?;
        let outcome = handle
            .join_timeout(Duration::from_secs(2))
            .ok_or("table get timed out")?;
        rt.shutdown();
        if !matches!(outcome, FlowOutcome::Completed(Value::Int(42))) {
            return Err(format!("table get: {outcome:?}").into());
        }

        let mut program = Program::new("table-ro");
        program.function("main", 0, |f| {
            let name = f.load_str("box");
            let table = f.table_open(name);
            let key = f.load_i32(1);
            let value = f.load_i32(42);
            f.table_put(table, key, value);
            let read_only = f.delegate(table, CapRights::RECV);
            f.table_put(read_only, key, value);
            f.return_(value);
        });
        let rt = Runtime::with_natives_and_config(
            program.build(),
            crate::std_native_table(),
            RuntimeConfig {
                workers: 1,
                ..RuntimeConfig::default()
            },
        )?;
        let handle = rt.spawn(0, &[])?;
        let outcome = handle
            .join_timeout(Duration::from_secs(2))
            .ok_or("read-only put timed out")?;
        rt.shutdown();
        match outcome {
            FlowOutcome::Failed(text) if text.contains("rights") => Ok(()),
            other => Err(format!("read-only put: {other:?}").into()),
        }
    }

    #[test]
    fn frame_rejects_a_sender_that_is_not_the_envelope() -> Result<(), RemoteFrameError> {
        use crate::CapId;
        let agreed = OutboundHop::new(
            FlowId(9),
            1,
            std::sync::Arc::from("svc"),
            Message::new(9, 1, 1, Value::Int(1)).authenticate(9, CapId::NONE),
        );
        let bytes = encode_frame(&agreed)?;
        let host = OutboundHop::new(
            FlowId(9),
            1,
            std::sync::Arc::from("svc"),
            Message::new(0, 1, 1, Value::Int(1)).authenticate(0, CapId::NONE),
        );
        if !matches!(encode_frame(&host), Err(RemoteFrameError::SenderZero)) {
            return Err(RemoteFrameError::Unsupported);
        }
        let other = OutboundHop::new(
            FlowId(9),
            1,
            std::sync::Arc::from("svc"),
            Message::new(4, 1, 1, Value::Int(1)).authenticate(4, CapId::NONE),
        );
        if !matches!(encode_frame(&other), Err(RemoteFrameError::SenderMismatch)) {
            return Err(RemoteFrameError::Unsupported);
        }
        // Header, then the `.bf` message tag, then the envelope sender.
        let envelope_at = 4 + 1 + 8 + 2 + agreed.name.len() + 8 + 1;
        let mut forged = bytes.clone();
        forged[envelope_at..envelope_at + 8].copy_from_slice(&0u64.to_le_bytes());
        if !matches!(decode_frame(&forged), Err(RemoteFrameError::SenderZero)) {
            return Err(RemoteFrameError::Unsupported);
        }
        forged[envelope_at..envelope_at + 8].copy_from_slice(&4u64.to_le_bytes());
        if !matches!(decode_frame(&forged), Err(RemoteFrameError::SenderMismatch)) {
            return Err(RemoteFrameError::Unsupported);
        }
        Ok(())
    }

    #[test]
    fn table_overwrite_at_the_row_cap_keeps_the_row() -> Result<(), Box<dyn std::error::Error>> {
        use crate::bytecode::builder::ChunkBuilder;
        let mut b = ChunkBuilder::new("full");
        b.begin_function("main", 0, 6);
        let name = b.const_(Value::str("box"));
        b.emit_load_const(0, name);
        b.emit_table_open(1, 0);
        b.emit_load_imm(2, 0);
        b.emit_load_imm(3, 0);
        let again = b.new_label();
        let done = b.new_label();
        b.bind_label(again);
        b.emit_table_put(1, 2, 3);
        b.emit_add_imm(2, 2, 1);
        b.emit_lt_imm(4, 2, 256);
        b.emit_branch(4, done);
        b.emit_jump(again);
        b.bind_label(done);
        b.emit_load_imm(2, 0);
        b.emit_load_imm(3, 7);
        b.emit_table_put(1, 2, 3);
        b.emit_table_get(5, 1, 2);
        b.emit_return(5);
        let rt = Runtime::with_natives_and_config(
            b.finish(),
            crate::std_native_table(),
            RuntimeConfig {
                workers: 1,
                ..RuntimeConfig::default()
            },
        )?;
        let handle = rt.spawn(0, &[])?;
        let outcome = handle
            .join_timeout(Duration::from_secs(2))
            .ok_or("table fill timed out")?;
        rt.shutdown();
        match outcome {
            FlowOutcome::Completed(Value::Int(7)) => Ok(()),
            other => Err(format!("overwrite at cap: {other:?}").into()),
        }
    }
}
