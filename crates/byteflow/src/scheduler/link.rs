//! Bidirectional links (`A <──────────> B`).
//!
//! Distinct from monitors: a monitor delivers `DOWN` to the owner; a link
//! **propagates abnormal exit** to the peer (BEAM: `normal` does not kill)
//! unless the peer has `trap_exit` enabled — then every exit becomes a
//! [`crate::TAG_SYS_EXIT`] hop. Propagation is cooperative — see
//! [`super::finalize`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use super::error::{LifecycleError, RuntimeError};
use super::process::FlowId;
use super::sync_lock;

/// Opaque link identifier returned by [`LinkTable::link`].
///
/// Inner id is runtime-minted (`pub(crate)`), same rule as [`crate::CapId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LinkId(pub(crate) u64);

static NEXT_LINK: AtomicU64 = AtomicU64::new(1);

impl LinkId {
    #[inline]
    pub fn as_u64(self) -> u64 {
        self.0
    }

    #[inline]
    pub(crate) fn from_u64(raw: u64) -> Self {
        Self(raw)
    }
}

impl std::fmt::Display for LinkId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "link#{}", self.0)
    }
}

#[derive(Debug, Clone, Copy)]
struct Link {
    a: FlowId,
    b: FlowId,
}

/// Bidirectional flow links. Lives on [`super::runtime::Shared`].
pub struct LinkTable {
    links: HashMap<LinkId, Link>,
}

impl LinkTable {
    pub fn new() -> Self {
        Self {
            links: HashMap::new(),
        }
    }

    pub fn link(&mut self, a: FlowId, b: FlowId) -> Result<LinkId, LifecycleError> {
        if a == b {
            return Err(LifecycleError::SelfRelation);
        }
        if self
            .links
            .values()
            .any(|link| (link.a == a && link.b == b) || (link.a == b && link.b == a))
        {
            return Err(LifecycleError::AlreadyLinked);
        }
        let id = LinkId(NEXT_LINK.fetch_add(1, Ordering::Relaxed));
        self.links.insert(id, Link { a, b });
        Ok(id)
    }

    pub fn unlink_owned(&mut self, owner: FlowId, id: LinkId) -> Result<(), LifecycleError> {
        match self.links.get(&id) {
            Some(link) if link.a == owner || link.b == owner => {
                self.links.remove(&id);
                Ok(())
            }
            Some(_) => Err(LifecycleError::NotOwner),
            None => Err(LifecycleError::InvalidLink),
        }
    }

    /// Peers of `target` and the link ids, then drop those links.
    pub fn remove_links_of(&mut self, target: FlowId) -> Vec<(LinkId, FlowId)> {
        let mut result = Vec::new();
        self.links.retain(|id, link| {
            if link.a == target {
                result.push((*id, link.b));
                false
            } else if link.b == target {
                result.push((*id, link.a));
                false
            } else {
                true
            }
        });
        result
    }
}

impl Default for LinkTable {
    fn default() -> Self {
        Self::new()
    }
}

pub struct LinkStore {
    inner: std::sync::Mutex<LinkTable>,
}

impl LinkStore {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(LinkTable::new()),
        }
    }

    pub fn link(
        &self,
        a: FlowId,
        b: FlowId,
    ) -> Result<Result<LinkId, LifecycleError>, RuntimeError> {
        Ok(sync_lock::lock(&self.inner, "LinkStore::link")?.link(a, b))
    }

    pub fn unlink_owned(
        &self,
        owner: FlowId,
        id: LinkId,
    ) -> Result<Result<(), LifecycleError>, RuntimeError> {
        Ok(sync_lock::lock(&self.inner, "LinkStore::unlink_owned")?.unlink_owned(owner, id))
    }

    pub fn remove_links_of(&self, target: FlowId) -> Result<Vec<(LinkId, FlowId)>, RuntimeError> {
        Ok(sync_lock::lock(&self.inner, "LinkStore::remove_links_of")?.remove_links_of(target))
    }
}

impl Default for LinkStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::process::next_flow_id;

    #[test]
    fn remove_links_returns_both_directions() -> Result<(), String> {
        let mut table = LinkTable::new();
        let a = next_flow_id();
        let b = next_flow_id();
        let c = next_flow_id();
        table.link(a, b).map_err(|e| e.to_string())?;
        table.link(c, a).map_err(|e| e.to_string())?;
        let peers: Vec<FlowId> = table
            .remove_links_of(a)
            .into_iter()
            .map(|(_, p)| p)
            .collect();
        assert_eq!(peers.len(), 2);
        assert!(peers.contains(&b));
        assert!(peers.contains(&c));
        assert!(table.remove_links_of(a).is_empty());
        Ok(())
    }

    #[test]
    fn rejects_self_and_duplicate() {
        let mut table = LinkTable::new();
        let a = next_flow_id();
        let b = next_flow_id();
        assert_eq!(table.link(a, a), Err(LifecycleError::SelfRelation));
        assert!(table.link(a, b).is_ok());
        assert_eq!(table.link(b, a), Err(LifecycleError::AlreadyLinked));
    }
}
