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
use std::sync::{Arc, Condvar, Mutex};

use crate::bytecode::{CapId, Message, Value};

use super::error::RuntimeError;
use super::process::FlowId;
use super::sync_lock;

/// How many outbound hops a bridge holds before `SendRemote` fails the flow.
const MAX_OUTBOUND: usize = 1024;

/// One authenticated hop waiting for the host to deliver it off-node.
#[derive(Clone, Debug)]
pub struct OutboundHop {
    pub sender: FlowId,
    /// Host-defined node number. `0` is not special.
    pub node: u64,
    pub name: Arc<str>,
    pub message: Message,
}

/// Queue the host drains. Share one [`Arc`] via [`crate::RuntimeConfig::distribution`].
#[derive(Debug)]
pub struct DistributionBridge {
    queue: Mutex<VecDeque<OutboundHop>>,
    ready: Condvar,
}

impl DistributionBridge {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
        })
    }

    pub(crate) fn push(&self, hop: OutboundHop) -> Result<(), String> {
        let mut queue =
            sync_lock::lock(&self.queue, "DistributionBridge::push").map_err(|e| e.to_string())?;
        if queue.len() >= MAX_OUTBOUND {
            return Err("distribution bridge full".into());
        }
        queue.push_back(hop);
        self.ready.notify_one();
        Ok(())
    }

    /// Next hop, if the host has not fallen behind.
    pub fn try_recv(&self) -> Result<Option<OutboundHop>, RuntimeError> {
        let mut queue = sync_lock::lock(&self.queue, "DistributionBridge::try_recv")?;
        Ok(queue.pop_front())
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
    bridge.push(OutboundHop {
        sender: flow.id,
        node,
        name,
        message: stamped,
    })
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
    Ok(OutboundHop {
        sender: FlowId(sender),
        node,
        name: Arc::from(name),
        message,
    })
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
        let hop = OutboundHop {
            sender: FlowId(9),
            node: 3,
            name: std::sync::Arc::from("svc"),
            message: Message::new(9, 4, 1, Value::Int(5)).authenticate(9, CapId::NONE),
        };
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
    fn frame_rejects_a_live_cap() {
        use crate::CapId;
        let hop = OutboundHop {
            sender: FlowId(9),
            node: 1,
            name: std::sync::Arc::from("svc"),
            message: Message::new(9, 1, 1, Value::Cap(CapId::from_raw(1)))
                .authenticate(9, CapId::NONE),
        };
        assert!(matches!(
            encode_frame(&hop),
            Err(RemoteFrameError::LiveCapability)
        ));
    }

    #[test]
    fn frame_rejects_a_list_past_the_cap() {
        let hop = OutboundHop {
            sender: FlowId(9),
            node: 1,
            name: std::sync::Arc::from("svc"),
            message: Message::new(
                9,
                1,
                1,
                Value::List(std::sync::Arc::from(vec![Value::Int(1); 257])),
            )
            .authenticate(9, crate::CapId::NONE),
        };
        assert!(matches!(
            encode_frame(&hop),
            Err(RemoteFrameError::Unsupported)
        ));
    }

    #[test]
    fn frame_accepts_a_list_at_the_cap() -> Result<(), RemoteFrameError> {
        let list =
            Value::try_list(vec![Value::Int(1); 256]).ok_or(RemoteFrameError::Unsupported)?;
        let hop = OutboundHop {
            sender: FlowId(9),
            node: 1,
            name: std::sync::Arc::from("svc"),
            message: Message::new(9, 1, 1, list).authenticate(9, crate::CapId::NONE),
        };
        encode_frame(&hop).map(|_| ())
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
        let frame = encode_frame(&OutboundHop {
            sender: FlowId(44),
            node: 2,
            name: std::sync::Arc::from("inbox"),
            message: Message::new(44, 1, 7, Value::Int(8)).authenticate(44, CapId::NONE),
        })?;
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
        let agreed = OutboundHop {
            sender: FlowId(9),
            node: 1,
            name: std::sync::Arc::from("svc"),
            message: Message::new(9, 1, 1, Value::Int(1)).authenticate(9, CapId::NONE),
        };
        let bytes = encode_frame(&agreed)?;
        let host = OutboundHop {
            sender: FlowId(9),
            node: 1,
            name: std::sync::Arc::from("svc"),
            message: Message::new(0, 1, 1, Value::Int(1)).authenticate(0, CapId::NONE),
        };
        if !matches!(encode_frame(&host), Err(RemoteFrameError::SenderZero)) {
            return Err(RemoteFrameError::Unsupported);
        }
        let other = OutboundHop {
            sender: FlowId(9),
            node: 1,
            name: std::sync::Arc::from("svc"),
            message: Message::new(4, 1, 1, Value::Int(1)).authenticate(4, CapId::NONE),
        };
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
