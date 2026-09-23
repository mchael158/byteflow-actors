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

    #[inline]
    pub fn len(&self) -> usize {
        self.links.len()
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

/// Fail-closed wrapper; process-wide link quota lives here (`table.len()`).
pub struct LinkStore {
    inner: std::sync::Mutex<LinkTable>,
    max_links: u32,
}

impl LinkStore {
    /// `max_links == 0` → unlimited.
    pub fn new(max_links: u32) -> Self {
        Self {
            inner: std::sync::Mutex::new(LinkTable::new()),
            max_links,
        }
    }

    pub fn link(
        &self,
        a: FlowId,
        b: FlowId,
    ) -> Result<Result<LinkId, LifecycleError>, RuntimeError> {
        let mut table = sync_lock::lock(&self.inner, "LinkStore::link")?;
        // Validate before the quota check so LimitReached never masks
        // SelfRelation / AlreadyLinked (same discipline as registry).
        if a == b {
            return Ok(Err(LifecycleError::SelfRelation));
        }
        if table
            .links
            .values()
            .any(|link| (link.a == a && link.b == b) || (link.a == b && link.b == a))
        {
            return Ok(Err(LifecycleError::AlreadyLinked));
        }
        if self.max_links != 0 && table.len() as u32 >= self.max_links {
            return Ok(Err(LifecycleError::LinkLimitReached {
                limit: self.max_links,
            }));
        }
        Ok(table.link(a, b))
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
        Self::new(0)
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

    #[test]
    fn link_limit_is_process_wide_and_released() -> Result<(), Box<dyn std::error::Error>> {
        let store = LinkStore::new(1);
        let a = next_flow_id();
        let b = next_flow_id();
        let c = next_flow_id();
        let first = store.link(a, b)??;
        let second = store.link(b, c)?;
        assert!(matches!(
            second,
            Err(LifecycleError::LinkLimitReached { limit: 1 })
        ));
        store.unlink_owned(a, first)??;
        store.link(b, c)??;
        Ok(())
    }

    #[test]
    fn link_limit_does_not_mask_self_or_duplicate() -> Result<(), Box<dyn std::error::Error>> {
        let store = LinkStore::new(1);
        let a = next_flow_id();
        let b = next_flow_id();
        store.link(a, b)??;
        // At capacity: self / duplicate must still win over LimitReached.
        let self_err = store.link(a, a)?;
        assert!(matches!(self_err, Err(LifecycleError::SelfRelation)));
        let dup = store.link(b, a)?;
        assert!(matches!(dup, Err(LifecycleError::AlreadyLinked)));
        let other = store.link(a, next_flow_id())?;
        assert!(matches!(
            other,
            Err(LifecycleError::LinkLimitReached { limit: 1 })
        ));
        Ok(())
    }
}
