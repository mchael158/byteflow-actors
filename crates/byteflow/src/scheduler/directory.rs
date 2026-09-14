use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::error::{report_fault, RuntimeError};
use super::mailbox::Mailbox;
use super::process::FlowId;
use super::sync_lock;

/// Number of shards in the directory's hash map. Sharding turns "look up a
/// mailbox by FlowId" (which happens on every resolved `Send`, potentially
/// millions of times per second across dozens of worker threads) from one
/// runtime-global lock into `SHARDS` independent ones, so unrelated
/// flows sending messages concurrently don't serialize against each
/// other. A power of two keeps the shard-select modulo a cheap mask.
const SHARDS: usize = 64;

/// Runtime-wide registry mapping a still-alive flow's [`FlowId`] to
/// its [`Mailbox`]. Cap resolution happens first (bytecode); this table is
/// the second hop — the *only* globally-shared, non-thread-local state a
/// delivery touches after Cap→FlowId. Everything else about message delivery
/// (queueing vs. waking a parked receiver) happens inside the `Mailbox`
/// itself once it's been found here. See design notes' mailbox
/// architecture (§13-14).
///
/// A production-grade version would replace the `Mutex<HashMap<..>>` per
/// shard with a lock-free structure (e.g. `flurry`/`dashmap`'s approach) —
/// left as-is here because registration/removal are one-time-per-flow
/// events (not per-message), so the lock is held only briefly and rarely.
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
        &self.shards[(id.as_u64() as usize) % SHARDS]
    }

    pub fn register(&self, id: FlowId, mailbox: Arc<Mailbox>) -> Result<(), RuntimeError> {
        let mut shard = sync_lock::lock(self.shard_for(id), "Directory::register")?;
        if shard.contains_key(&id) {
            return Err(RuntimeError::DuplicateFlowId);
        }
        shard.insert(id, mailbox);
        Ok(())
    }

    pub fn unregister(&self, id: FlowId) -> Result<(), RuntimeError> {
        sync_lock::lock(self.shard_for(id), "Directory::unregister")?.remove(&id);
        Ok(())
    }

    pub fn lookup(&self, id: FlowId) -> Result<Option<Arc<Mailbox>>, RuntimeError> {
        Ok(sync_lock::lock(self.shard_for(id), "Directory::lookup")?
            .get(&id)
            .cloned())
    }

    /// Approximate live-flow count, for `runtime.metrics()`. "Approximate"
    /// because it's a sum across shards taken without a global snapshot lock
    /// — exactly right for a dashboard counter, not for anything requiring
    /// linearizability.
    ///
    /// On mutex poison, reports the fault and returns the partial sum so far
    /// (fail-closed for that shard, still useful as a lower bound).
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
