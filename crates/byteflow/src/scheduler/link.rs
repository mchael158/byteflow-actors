//! Bidirectional links (`A <──────────> B`).
//!
//! Distinct from monitors: a monitor delivers `DOWN` to the owner; a link
//! **propagates abnormal exit** to the peer (BEAM: `normal` does not kill)
//! unless the peer has `trap_exit` enabled — then every exit becomes a
//! [`crate::TAG_SYS_EXIT`] hop. Propagation is cooperative — see
//! [`super::finalize`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::error::{LifecycleError, RuntimeError};
use super::process::FlowId;
use super::sync_lock;

/// Opaque link identifier returned by [`LinkTable::link`].
///
/// Inner id is runtime-minted (`pub(crate)`), same rule as [`crate::CapId`].
/// Values fit in `i64` so bytecode can store them as [`crate::Value::Int`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LinkId(pub(crate) u64);

/// Bytecode stores [`LinkId`] as [`crate::Value::Int`]; never mint past this.
const MAX_LINK_ID: u64 = i64::MAX as u64;

static NEXT_LINK: AtomicU64 = AtomicU64::new(1);
static LINK_IDS_EXHAUSTED: AtomicBool = AtomicBool::new(false);

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

fn try_next_link_id() -> Result<LinkId, RuntimeError> {
    if LINK_IDS_EXHAUSTED.load(Ordering::Relaxed) {
        return Err(RuntimeError::LinkIdExhausted);
    }
    let id = NEXT_LINK.fetch_add(1, Ordering::Relaxed);
    if id == 0 || id > MAX_LINK_ID {
        LINK_IDS_EXHAUSTED.store(true, Ordering::Relaxed);
        return Err(RuntimeError::LinkIdExhausted);
    }
    Ok(LinkId(id))
}

#[derive(Debug, Clone, Copy)]
struct Link {
    a: FlowId,
    b: FlowId,
}

/// Bidirectional flow links. Lives on [`super::runtime::Shared`].
///
/// Three maps stay in lockstep: `links` is the record, `pairs` is the
/// uniqueness index (canonical `(min, max)`), `by_flow` is adjacency so
/// finalize is O(degree) instead of a full-table scan.
pub struct LinkTable {
    links: HashMap<LinkId, Link>,
    pairs: HashMap<(FlowId, FlowId), LinkId>,
    by_flow: HashMap<FlowId, Vec<LinkId>>,
}

impl LinkTable {
    pub fn new() -> Self {
        Self {
            links: HashMap::new(),
            pairs: HashMap::new(),
            by_flow: HashMap::new(),
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.links.len()
    }

    #[inline]
    fn pair_key(a: FlowId, b: FlowId) -> (FlowId, FlowId) {
        if a <= b {
            (a, b)
        } else {
            (b, a)
        }
    }

    #[inline]
    pub fn has_pair(&self, a: FlowId, b: FlowId) -> bool {
        a != b && self.pairs.contains_key(&Self::pair_key(a, b))
    }

    fn drop_index(&mut self, flow: FlowId, id: LinkId) {
        if let Some(ids) = self.by_flow.get_mut(&flow) {
            ids.retain(|existing| *existing != id);
            if ids.is_empty() {
                self.by_flow.remove(&flow);
            }
        }
    }

    fn index_flow(&mut self, flow: FlowId, id: LinkId) {
        self.by_flow.entry(flow).or_default().push(id);
    }

    /// Caller already checked self / duplicate. `id` must be unique.
    fn insert(&mut self, id: LinkId, a: FlowId, b: FlowId) {
        self.links.insert(id, Link { a, b });
        self.pairs.insert(Self::pair_key(a, b), id);
        self.index_flow(a, id);
        self.index_flow(b, id);
    }

    /// Drop `id` from every index. `None` if it was already gone.
    fn unlink_id(&mut self, id: LinkId) -> Option<Link> {
        let link = self.links.remove(&id)?;
        self.pairs.remove(&Self::pair_key(link.a, link.b));
        self.drop_index(link.a, id);
        self.drop_index(link.b, id);
        Some(link)
    }

    #[cfg(test)]
    pub fn link(&mut self, a: FlowId, b: FlowId) -> Result<LinkId, LifecycleError> {
        if a == b {
            return Err(LifecycleError::SelfRelation);
        }
        if self.has_pair(a, b) {
            return Err(LifecycleError::AlreadyLinked);
        }
        let id = match try_next_link_id() {
            Ok(id) => id,
            Err(_) => return Err(LifecycleError::Unavailable),
        };
        self.insert(id, a, b);
        Ok(id)
    }

    pub fn unlink_owned(&mut self, owner: FlowId, id: LinkId) -> Result<(), LifecycleError> {
        match self.links.get(&id) {
            Some(link) if link.a == owner || link.b == owner => {
                let _ = self.unlink_id(id);
                Ok(())
            }
            Some(_) => Err(LifecycleError::NotOwner),
            None => Err(LifecycleError::InvalidLink),
        }
    }

    /// Peers of `target` and the link ids, then drop those links.
    ///
    /// Order is by [`LinkId`] so finalize propagation is deterministic
    /// (same discipline as [`super::monitor::MonitorTable::notify_target_exit`]).
    pub fn remove_links_of(&mut self, target: FlowId) -> Vec<(LinkId, FlowId)> {
        let Some(ids) = self.by_flow.remove(&target) else {
            return Vec::new();
        };
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(link) = self.links.remove(&id) else {
                continue;
            };
            self.pairs.remove(&Self::pair_key(link.a, link.b));
            let peer = if link.a == target { link.b } else { link.a };
            self.drop_index(peer, id);
            result.push((id, peer));
        }
        result.sort_unstable_by_key(|(id, _)| id.0);
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

    /// Insert without a liveness check (unit tests).
    #[cfg(test)]
    pub fn link(
        &self,
        a: FlowId,
        b: FlowId,
    ) -> Result<Result<LinkId, LifecycleError>, RuntimeError> {
        self.link_if_live(a, b, |_| Ok(true))
    }

    /// Insert, then confirm both endpoints are still live.
    ///
    /// `is_live` runs **after** the link mutex is released so this never
    /// nests `LinkStore` → `Directory` (finalize takes those locks separately,
    /// never together). If a peer finalized in the window:
    ///
    /// - the link is still here → rollback and [`LifecycleError::NoSuchFlow`]
    ///   (zombie we created after `remove_links_of`)
    /// - the link is already gone → finalize won; return the (now stale) id
    ///   so the caller is not failed a second time on top of link-kill
    pub fn link_if_live<F>(
        &self,
        a: FlowId,
        b: FlowId,
        is_live: F,
    ) -> Result<Result<LinkId, LifecycleError>, RuntimeError>
    where
        F: Fn(FlowId) -> Result<bool, RuntimeError>,
    {
        let id = {
            let mut table = sync_lock::lock(&self.inner, "LinkStore::link")?;
            // Validate before the quota check so LimitReached never masks
            // SelfRelation / AlreadyLinked (same discipline as registry).
            if a == b {
                return Ok(Err(LifecycleError::SelfRelation));
            }
            if table.has_pair(a, b) {
                return Ok(Err(LifecycleError::AlreadyLinked));
            }
            if self.max_links != 0 && table.len() >= self.max_links as usize {
                return Ok(Err(LifecycleError::LinkLimitReached {
                    limit: self.max_links,
                }));
            }
            let id = try_next_link_id()?;
            table.insert(id, a, b);
            id
        };
        self.confirm_live(id, a, b, is_live)
    }

    fn confirm_live<F>(
        &self,
        id: LinkId,
        a: FlowId,
        b: FlowId,
        is_live: F,
    ) -> Result<Result<LinkId, LifecycleError>, RuntimeError>
    where
        F: Fn(FlowId) -> Result<bool, RuntimeError>,
    {
        let a_live = match is_live(a) {
            Ok(v) => v,
            Err(e) => {
                let _ = self.force_remove(id);
                return Err(e);
            }
        };
        let b_live = match is_live(b) {
            Ok(v) => v,
            Err(e) => {
                let _ = self.force_remove(id);
                return Err(e);
            }
        };
        if a_live && b_live {
            return Ok(Ok(id));
        }
        match self.force_remove(id) {
            Ok(true) => {
                let dead = if a_live { b } else { a };
                Ok(Err(LifecycleError::NoSuchFlow(dead)))
            }
            Ok(false) => Ok(Ok(id)),
            Err(e) => Err(e),
        }
    }

    fn force_remove(&self, id: LinkId) -> Result<bool, RuntimeError> {
        Ok(sync_lock::lock(&self.inner, "LinkStore::confirm_live")?
            .unlink_id(id)
            .is_some())
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
        let removed = table.remove_links_of(a);
        let peers: Vec<FlowId> = removed.iter().map(|(_, p)| *p).collect();
        assert_eq!(peers.len(), 2);
        assert!(peers.contains(&b));
        assert!(peers.contains(&c));
        assert!(removed[0].0 < removed[1].0);
        assert!(table.remove_links_of(a).is_empty());
        assert!(table.remove_links_of(b).is_empty());
        assert!(table.remove_links_of(c).is_empty());
        assert_eq!(table.len(), 0);
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
        assert!(table.has_pair(a, b));
        assert!(table.has_pair(b, a));
    }

    #[test]
    fn unlink_owned_rejects_stranger_and_unknown() -> Result<(), String> {
        let mut table = LinkTable::new();
        let a = next_flow_id();
        let b = next_flow_id();
        let stranger = next_flow_id();
        let id = table.link(a, b).map_err(|e| e.to_string())?;
        assert_eq!(
            table.unlink_owned(stranger, id),
            Err(LifecycleError::NotOwner)
        );
        assert_eq!(
            table.unlink_owned(a, LinkId::from_u64(0)),
            Err(LifecycleError::InvalidLink)
        );
        table.unlink_owned(b, id).map_err(|e| e.to_string())?;
        assert_eq!(table.unlink_owned(a, id), Err(LifecycleError::InvalidLink));
        assert_eq!(table.len(), 0);
        assert!(!table.has_pair(a, b));
        Ok(())
    }

    #[test]
    fn unlink_after_remove_is_invalid() -> Result<(), String> {
        let mut table = LinkTable::new();
        let a = next_flow_id();
        let b = next_flow_id();
        let id = table.link(a, b).map_err(|e| e.to_string())?;
        assert_eq!(table.remove_links_of(a), vec![(id, b)]);
        assert_eq!(table.unlink_owned(b, id), Err(LifecycleError::InvalidLink));
        Ok(())
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

    #[test]
    fn unlimited_store_accepts_many() -> Result<(), Box<dyn std::error::Error>> {
        let store = LinkStore::new(0);
        let a = next_flow_id();
        store.link(a, next_flow_id())??;
        store.link(a, next_flow_id())??;
        store.link(a, next_flow_id())??;
        Ok(())
    }

    #[test]
    fn dead_peer_rolls_back_and_releases_quota() -> Result<(), Box<dyn std::error::Error>> {
        let store = LinkStore::new(1);
        let a = next_flow_id();
        let b = next_flow_id();
        let result = store.link_if_live(a, b, |id| Ok(id != b))?;
        assert!(matches!(result, Err(LifecycleError::NoSuchFlow(dead)) if dead == b));
        // Quota released: a fresh pair must fit.
        store.link_if_live(a, next_flow_id(), |_| Ok(true))??;
        Ok(())
    }

    #[test]
    fn finalize_already_removed_is_not_a_second_fault() -> Result<(), Box<dyn std::error::Error>> {
        let store = LinkStore::new(0);
        let a = next_flow_id();
        let b = next_flow_id();
        // Insert as live, tear down as finalize would, then confirm sees a
        // dead peer but the row is already gone → keep Ok(id).
        let id = {
            let mut table = sync_lock::lock(&store.inner, "test")?;
            if table.has_pair(a, b) {
                return Err("pair should be free".into());
            }
            let id = try_next_link_id()?;
            table.insert(id, a, b);
            id
        };
        let _ = store.remove_links_of(b)?;
        let confirmed = store.confirm_live(id, a, b, |id| Ok(id != b))?;
        assert_eq!(confirmed, Ok(id));
        Ok(())
    }
}
