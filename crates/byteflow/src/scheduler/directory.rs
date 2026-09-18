//! FlowId → Mailbox directory (sharded).
//!
//! Cap resolution yields a [`FlowId`]; this table is the second hop of
//! delivery — the only globally shared lookup on the Send/Ask hot path.
//! Queueing and park/handoff live inside the [`Mailbox`] once found here.
//!
//! # Concurrency
//!
//! 64 shards (`SHARDS`, power of two) so unrelated FlowIds do not share a
//! mutex. Registration / unregister are rare (once per flow lifetime);
//! lookup is the hot path.
//!
//! # Admission control
//!
//! [`Directory::len`] is an **approximate** metric for dashboards
//! (`Runtime::live_flows`). Live-flow **admission** uses
//! [`super::flow_limit::FlowLimit`], not this count.
//!
//! # Liveness contract with finalize
//!
//! `finalize_flow` calls [`Mailbox::close`] **before** [`Self::unregister`].
//! After close, [`Mailbox::push`] refuses new hops (`MailboxFullReason::Closed`
//! → deliver treats as gone). Holding a stale `Arc<Mailbox>` after unregister
//! cannot resurrect a dead inbox.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::error::{report_fault, RuntimeError};
use super::mailbox::Mailbox;
use super::process::FlowId;
use super::sync_lock;

/// Power of two: shard select is a mask, not a divide.
const SHARDS: usize = 64;
const SHARD_MASK: usize = SHARDS - 1;

/// Runtime-wide registry of live flows' mailboxes.
pub struct Directory {
    shards: Vec<Mutex<HashMap<FlowId, Arc<Mailbox>>>>,
}

impl Directory {
    pub fn new() -> Self {
        let mut shards = Vec::with_capacity(SHARDS);
        for _ in 0..SHARDS {
            shards.push(Mutex::new(HashMap::new()));
        }
        Directory { shards }
    }

    #[inline]
    fn shard_for(&self, id: FlowId) -> &Mutex<HashMap<FlowId, Arc<Mailbox>>> {
        &self.shards[(id.as_u64() as usize) & SHARD_MASK]
    }

    /// Insert a freshly spawned flow. Duplicate id → [`RuntimeError::DuplicateFlowId`].
    pub fn register(&self, id: FlowId, mailbox: Arc<Mailbox>) -> Result<(), RuntimeError> {
        let mut shard = sync_lock::lock(self.shard_for(id), "Directory::register")?;
        if shard.contains_key(&id) {
            return Err(RuntimeError::DuplicateFlowId);
        }
        shard.insert(id, mailbox);
        Ok(())
    }

    /// Remove a flow after finalize. Missing id is a no-op (idempotent).
    pub fn unregister(&self, id: FlowId) -> Result<(), RuntimeError> {
        sync_lock::lock(self.shard_for(id), "Directory::unregister")?.remove(&id);
        Ok(())
    }

    /// Lookup for delivery. `Ok(None)` means the flow is gone (or never lived).
    pub fn lookup(&self, id: FlowId) -> Result<Option<Arc<Mailbox>>, RuntimeError> {
        Ok(sync_lock::lock(self.shard_for(id), "Directory::lookup")?
            .get(&id)
            .cloned())
    }

    /// Approximate live-flow count for metrics — not linearizable across shards.
    ///
    /// On mutex poison, reports the fault and returns the partial sum so far
    /// (lower bound). **Do not** use for `max_flows` admission.
    pub fn len(&self) -> usize {
        let mut n = 0;
        for s in &self.shards {
            match sync_lock::lock(s, "Directory::len") {
                Ok(g) => n += g.len(),
                Err(e) => {
                    report_fault(e);
                    return n;
                }
            }
        }
        n
    }
}

impl Default for Directory {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::mailbox::Mailbox;

    #[test]
    fn register_lookup_unregister() -> Result<(), RuntimeError> {
        let dir = Directory::new();
        let id = FlowId(42);
        let mb = Arc::new(Mailbox::new());
        dir.register(id, Arc::clone(&mb))?;
        assert!(dir.lookup(id)?.is_some());
        assert_eq!(dir.len(), 1);
        dir.unregister(id)?;
        assert!(dir.lookup(id)?.is_none());
        assert_eq!(dir.len(), 0);
        // Idempotent unregister.
        dir.unregister(id)?;
        Ok(())
    }

    #[test]
    fn duplicate_register_is_rejected() -> Result<(), RuntimeError> {
        let dir = Directory::new();
        let id = FlowId(7);
        dir.register(id, Arc::new(Mailbox::new()))?;
        let err = dir.register(id, Arc::new(Mailbox::new()));
        assert!(matches!(err, Err(RuntimeError::DuplicateFlowId)));
        Ok(())
    }

    #[test]
    fn distinct_ids_can_share_shard_without_collision() -> Result<(), RuntimeError> {
        let dir = Directory::new();
        // Same shard: 1 and 1+64.
        let a = FlowId(1);
        let b = FlowId(1 + SHARDS as u64);
        dir.register(a, Arc::new(Mailbox::new()))?;
        dir.register(b, Arc::new(Mailbox::new()))?;
        assert!(dir.lookup(a)?.is_some());
        assert!(dir.lookup(b)?.is_some());
        assert_eq!(dir.len(), 2);
        Ok(())
    }
}
