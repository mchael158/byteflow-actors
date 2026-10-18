//! Outbound hops for another node.
//!
//! Byteflow does not open sockets and does not speak a cluster protocol.
//! The host owns I/O. Bytecode [`crate::Opcode::SendRemote`] stamps the
//! sender here and queues the hop on [`DistributionBridge`]; the embedder
//! drains that queue and delivers it however it wants.
//!
//! Payload `Cap` tokens are not re-granted to the remote node. They stay
//! opaque bytes the host must not treat as local authority on another
//! process. `reply_cap` is [`crate::CapId::NONE`] — there is no local flow
//! to mint a reply address for.

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
    if !flow.authority.rights.contains(crate::CapRights::SEND) {
        return Err("send_remote denied: flow lacks SEND right".into());
    }
    let Some(bridge) = shared.distribution.as_ref() else {
        return Err("send_remote: no distribution bridge".into());
    };
    let Some(mut msg) = message.as_message().cloned() else {
        return Err("send_remote: value is not a hop".into());
    };
    if let Err(e) = flow.quota.check_send() {
        return Err(e.to_string());
    }
    if msg.request_id == 0 {
        msg.request_id = flow.vm.fresh_request_id();
    }
    let stamped = stamp(msg, flow.id);
    bridge.push(OutboundHop {
        sender: flow.id,
        node,
        name,
        message: stamped,
    })
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
}
