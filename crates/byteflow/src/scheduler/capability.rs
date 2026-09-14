//! Runtime-local capability table (Phase 3 — FlowCap + attenuate).
//!
//! A [`CapId`] is an opaque 128-bit token, **not** an authorization.
//! Authorization lives in [`Cap`], owned by one [`crate::Runtime`]:
//!
//! ```text
//! CapId  →  { holder, Cap { target, rights, native_mask, epoch } }
//! ```
//!
//! Every derived grant goes through [`Cap::attenuate`]. `mint` is the
//! trusted-runtime root path (self Cap, spawn addressing). Hop `reply_cap`
//! uses [`CapTable::mint_or_reuse`] — one live SEND token per
//! `(holder, target)` pair.
//!
//! See `docs/security.md` (S6 / S7) and `docs/atomic-hop.md`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::bytecode::{Cap, CapId, CapTarget, NativeMask, RevocationCell};

pub use crate::bytecode::CapRights;

use super::error::RuntimeError;
use super::process::FlowId;
use super::sync_lock;

const MINT_ATTEMPTS: u32 = 32;

/// One live capability entry. The [`CapId`] is the map key, not a field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capability {
    /// Flow allowed to *use* this token.
    pub holder: FlowId,
    pub cap: Cap,
}

impl Capability {
    /// Flow this Cap addresses, if the target is a flow.
    pub fn target(&self) -> Option<FlowId> {
        self.cap.target.flow_id().map(FlowId)
    }

    pub fn rights(&self) -> CapRights {
        self.cap.rights
    }
}

/// Why [`CapTable::resolve`] / [`CapTable::delegate`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapError {
    Unknown,
    NotHolder,
    InsufficientRights,
    WrongTarget,
    /// Mutex poison — fail closed (never `into_inner`).
    Unavailable,
}

impl std::fmt::Display for CapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CapError::Unknown => f.write_str("unknown or revoked capability"),
            CapError::NotHolder => f.write_str("calling flow does not hold this capability"),
            CapError::InsufficientRights => f.write_str("capability lacks required rights"),
            CapError::WrongTarget => f.write_str("capability target is not a flow"),
            CapError::Unavailable => f.write_str("capability table unavailable (poisoned lock)"),
        }
    }
}

impl std::error::Error for CapError {}

impl From<RuntimeError> for CapError {
    fn from(_err: RuntimeError) -> Self {
        CapError::Unavailable
    }
}

struct CapTableInner {
    entries: HashMap<CapId, Capability>,
    flow_cells: HashMap<u64, Arc<RevocationCell>>,
    /// Stable reply address: `(holder, target) → CapId`.
    ///
    /// Hop `reply_cap` is an *address* (SEND back to the sender), not a
    /// per-message ticket. Correlation stays on `Message.request_id`.
    reply_index: HashMap<(FlowId, FlowId), CapId>,
}

/// Registry `CapId → Capability`, owned by one runtime's [`super::runtime::Shared`].
pub struct CapTable {
    inner: Mutex<CapTableInner>,
    native_cell: Arc<RevocationCell>,
    scheduler_cell: Arc<RevocationCell>,
}

impl CapTable {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(CapTableInner {
                entries: HashMap::new(),
                flow_cells: HashMap::new(),
                reply_index: HashMap::new(),
            }),
            native_cell: Arc::new(RevocationCell::new()),
            scheduler_cell: Arc::new(RevocationCell::new()),
        }
    }

    pub fn native_cell(&self) -> Arc<RevocationCell> {
        Arc::clone(&self.native_cell)
    }

    fn lock(&self, where_: &'static str) -> Result<std::sync::MutexGuard<'_, CapTableInner>, RuntimeError> {
        sync_lock::lock(&self.inner, where_)
    }

    /// Get-or-create the revocation cell for `flow`.
    pub fn bind_flow(&self, flow: FlowId) -> Result<Arc<RevocationCell>, RuntimeError> {
        let mut g = self.lock("CapTable::bind_flow")?;
        Ok(g.flow_cells
            .entry(flow.as_u64())
            .or_insert_with(|| Arc::new(RevocationCell::new()))
            .clone())
    }

    pub fn flow_cell(&self, flow: FlowId) -> Result<Option<Arc<RevocationCell>>, RuntimeError> {
        Ok(self.lock("CapTable::flow_cell")?.flow_cells.get(&flow.as_u64()).cloned())
    }

    pub fn scheduler_cell(&self) -> Arc<RevocationCell> {
        Arc::clone(&self.scheduler_cell)
    }

    fn insert_fresh(
        table: &mut HashMap<CapId, Capability>,
        entry: Capability,
    ) -> Result<CapId, RuntimeError> {
        for _ in 0..MINT_ATTEMPTS {
            let id = CapId::random().map_err(|_| RuntimeError::EntropyFailed)?;
            if id.is_none() || table.contains_key(&id) {
                continue;
            }
            table.insert(id, entry);
            return Ok(id);
        }
        Err(RuntimeError::CapIdCollision)
    }

    fn insert(&self, holder: FlowId, cap: Cap) -> Result<CapId, RuntimeError> {
        let mut table = self.lock("CapTable::insert")?;
        Self::insert_fresh(
            &mut table.entries,
            Capability { holder, cap },
        )
    }

    /// Trusted root mint: always a **new** token.
    ///
    /// Used for self Cap (`SelfPid`) and child addressing (`Spawn`). Hop
    /// `reply_cap` must go through [`Self::mint_or_reuse`] so the table
    /// stays O(pairs), not O(hops).
    pub fn mint(
        &self,
        holder: FlowId,
        target: FlowId,
        rights: CapRights,
    ) -> Result<CapId, RuntimeError> {
        let cell = self.bind_flow(target)?;
        let cap = Cap::root(CapTarget::Flow(target.as_u64()), rights, None, cell.as_ref());
        self.grant(holder, cap)
    }

    /// Reply-address mint: reuse the live SEND Cap for `(holder, target)`.
    ///
    /// [`FlowId`] is never reused (`next_flow_id` is monotonic), so a cached
    /// pair cannot alias a later incarnation. Stale index entries are dropped
    /// after an epoch / holder / target check — never trusted blindly.
    ///
    /// One lock for lookup + insert so reuse is atomic on the existing
    /// global mutex (no CAS / retry).
    pub fn mint_or_reuse(
        &self,
        holder: FlowId,
        target: FlowId,
        rights: CapRights,
    ) -> Result<CapId, RuntimeError> {
        let mut g = self.lock("CapTable::mint_or_reuse")?;
        if let Some(&id) = g.reply_index.get(&(holder, target)) {
            if Self::reply_cap_is_live(&g, id, holder, target, rights) {
                return Ok(id);
            }
            g.reply_index.remove(&(holder, target));
        }
        let cell = g
            .flow_cells
            .entry(target.as_u64())
            .or_insert_with(|| Arc::new(RevocationCell::new()))
            .clone();
        let cap = Cap::root(CapTarget::Flow(target.as_u64()), rights, None, cell.as_ref());
        let id = Self::insert_fresh(&mut g.entries, Capability { holder, cap })?;
        g.reply_index.insert((holder, target), id);
        Ok(id)
    }

    fn reply_cap_is_live(
        table: &CapTableInner,
        id: CapId,
        holder: FlowId,
        target: FlowId,
        rights: CapRights,
    ) -> bool {
        let Some(entry) = table.entries.get(&id) else {
            return false;
        };
        if entry.holder != holder {
            return false;
        }
        if entry.cap.target != CapTarget::Flow(target.as_u64()) {
            return false;
        }
        if !entry.cap.rights.contains(rights) {
            return false;
        }
        match table.flow_cells.get(&target.as_u64()) {
            Some(cell) => entry.cap.is_valid(cell.as_ref()),
            None => false,
        }
    }

    /// Insert a Cap that was already produced by [`Cap::attenuate`] or
    /// [`Cap::root`] (trusted).
    pub fn grant(&self, holder: FlowId, cap: Cap) -> Result<CapId, RuntimeError> {
        self.insert(holder, cap)
    }

    /// Host / registry lookup: existence only, no holder check.
    pub fn lookup(&self, id: CapId) -> Result<Option<Capability>, RuntimeError> {
        if id.is_none() {
            return Ok(None);
        }
        Ok(self.lock("CapTable::lookup")?.entries.get(&id).cloned())
    }

    /// Bytecode path: token + holder + rights + live epoch.
    pub fn resolve(
        &self,
        id: CapId,
        holder: FlowId,
        required: CapRights,
    ) -> Result<Capability, CapError> {
        if id.is_none() {
            return Err(CapError::Unknown);
        }
        let table = self.lock("CapTable::resolve")?;
        let entry = table.entries.get(&id).cloned().ok_or(CapError::Unknown)?;
        if entry.holder != holder {
            return Err(CapError::NotHolder);
        }
        if !entry.cap.rights.contains(required) {
            return Err(CapError::InsufficientRights);
        }
        let valid = match entry.cap.target {
            CapTarget::Flow(fid) => match table.flow_cells.get(&fid) {
                Some(cell) => entry.cap.is_valid(cell.as_ref()),
                None => false,
            },
            CapTarget::NativeTable => entry.cap.is_valid(self.native_cell.as_ref()),
            CapTarget::Scheduler => entry.cap.is_valid(self.scheduler_cell.as_ref()),
        };
        if !valid {
            return Err(CapError::Unknown);
        }
        Ok(entry)
    }

    /// New token, attenuated rights, `to_holder` becomes the holder.
    /// Sole bytecode derivation path — calls [`Cap::attenuate`].
    pub fn attenuate(
        &self,
        id: CapId,
        from_holder: FlowId,
        to_holder: FlowId,
        want_rights: CapRights,
        want_native: Option<&NativeMask>,
    ) -> Result<CapId, CapError> {
        let src = self.resolve(id, from_holder, CapRights::empty())?;
        let cell = match src.cap.target {
            CapTarget::Flow(fid) => self
                .lock("CapTable::attenuate")?
                .flow_cells
                .get(&fid)
                .cloned()
                .ok_or(CapError::Unknown)?,
            CapTarget::NativeTable | CapTarget::Scheduler => {
                return Err(CapError::WrongTarget);
            }
        };
        let narrowed = match super::delegate::exec_delegate(
            &src.cap,
            cell.as_ref(),
            want_rights,
            want_native,
        ) {
            Ok(c) => c,
            Err(super::delegate::DelegateError::SourceRevoked) => {
                return Err(CapError::Unknown)
            }
            Err(super::delegate::DelegateError::SourceLacksNative) => {
                return Err(CapError::InsufficientRights)
            }
        };
        if !narrowed.is_valid(cell.as_ref()) {
            return Err(CapError::Unknown);
        }
        self.insert(to_holder, narrowed).map_err(CapError::from)
    }

    /// New token, same target/rights (attenuation with `want = src.rights`).
    pub fn delegate(
        &self,
        id: CapId,
        from_holder: FlowId,
        to_holder: FlowId,
    ) -> Result<CapId, CapError> {
        let src = self.resolve(id, from_holder, CapRights::empty())?;
        self.attenuate(id, from_holder, to_holder, src.cap.rights, src.cap.native_mask.as_ref())
    }

    /// Host spawn: re-issue a live Cap so `new_holder` can use it.
    /// Does not require the caller to be the current holder (trusted host).
    /// Still goes through [`Cap::attenuate`] (identity attenuation).
    pub fn reissue_for(&self, id: CapId, new_holder: FlowId) -> Result<CapId, CapError> {
        let cap = self.lookup(id)?.ok_or(CapError::Unknown)?;
        let table = self.lock("CapTable::reissue_for")?;
        let valid = match cap.cap.target {
            CapTarget::Flow(fid) => match table.flow_cells.get(&fid) {
                Some(cell) => cap.cap.is_valid(cell.as_ref()),
                None => false,
            },
            CapTarget::NativeTable => cap.cap.is_valid(self.native_cell.as_ref()),
            CapTarget::Scheduler => cap.cap.is_valid(self.scheduler_cell.as_ref()),
        };
        drop(table);
        if !valid {
            return Err(CapError::Unknown);
        }
        let narrowed = cap.cap.attenuate(cap.cap.rights, cap.cap.native_mask.as_ref());
        self.insert(new_holder, narrowed).map_err(CapError::from)
    }

    /// Drop every capability held by or targeting `flow` (flow exited).
    /// Bumps the flow's epoch first so leaked tokens fail in O(1).
    pub fn revoke_flow(&self, flow: FlowId) -> Result<usize, RuntimeError> {
        let mut g = self.lock("CapTable::revoke_flow")?;
        if let Some(cell) = g.flow_cells.get(&flow.as_u64()) {
            cell.revoke();
        }
        g.flow_cells.remove(&flow.as_u64());
        let before = g.entries.len();
        let fid = flow.as_u64();
        g.entries.retain(|_, e| {
            e.holder != flow && e.cap.target != CapTarget::Flow(fid)
        });
        g.reply_index.retain(|(h, t), _| *h != flow && *t != flow);
        Ok(before - g.entries.len())
    }

    #[cfg(test)]
    fn entry_count(&self) -> Result<usize, RuntimeError> {
        Ok(self.lock("CapTable::entry_count")?.entries.len())
    }

    #[cfg(test)]
    fn reply_index_len(&self) -> Result<usize, RuntimeError> {
        Ok(self.lock("CapTable::reply_index_len")?.reply_index.len())
    }

    #[cfg(test)]
    fn reply_index_mentions(&self, flow: FlowId) -> Result<bool, RuntimeError> {
        Ok(self
            .lock("CapTable::reply_index_mentions")?
            .reply_index
            .keys()
            .any(|(h, t)| *h == flow || *t == flow))
    }
}

impl Default for CapTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::process::next_flow_id;

    #[test]
    fn random_ids_are_not_sequential() -> Result<(), Box<dyn std::error::Error>> {
        let table = CapTable::new();
        let h = next_flow_id();
        let t = next_flow_id();
        let a = table.mint(h, t, CapRights::SEND)?;
        let b = table.mint(h, t, CapRights::SEND)?;
        assert_ne!(a, b);
        assert!(!a.is_none());
        assert_eq!(
            table.resolve(CapId::from_raw(1), h, CapRights::SEND),
            Err(CapError::Unknown)
        );
        Ok(())
    }

    #[test]
    fn resolve_requires_holder_and_rights() -> Result<(), Box<dyn std::error::Error>> {
        let table = CapTable::new();
        let holder = next_flow_id();
        let other = next_flow_id();
        let target = next_flow_id();
        let cap = table.mint(holder, target, CapRights::SEND)?;
        assert_eq!(
            table.resolve(cap, holder, CapRights::SEND)?.target(),
            Some(target)
        );
        assert_eq!(
            table.resolve(cap, other, CapRights::SEND),
            Err(CapError::NotHolder)
        );
        assert_eq!(
            table.resolve(cap, holder, CapRights::ASK),
            Err(CapError::InsufficientRights)
        );
        Ok(())
    }

    #[test]
    fn capability_isolation_is_per_table() -> Result<(), Box<dyn std::error::Error>> {
        let a = CapTable::new();
        let b = CapTable::new();
        let holder = next_flow_id();
        let target = next_flow_id();
        let cap = a.mint(holder, target, CapRights::SEND)?;
        assert_eq!(
            b.resolve(cap, holder, CapRights::SEND),
            Err(CapError::Unknown)
        );
        Ok(())
    }

    #[test]
    fn finalizing_flow_revokes_held_and_targeted_caps() -> Result<(), Box<dyn std::error::Error>> {
        let table = CapTable::new();
        let dead = next_flow_id();
        let alive = next_flow_id();
        let target = next_flow_id();

        let held_by_dead = table.mint(dead, target, CapRights::SEND)?;
        let targeting_dead = table.mint(alive, dead, CapRights::SEND)?;
        let unrelated = table.mint(alive, target, CapRights::SEND)?;

        let removed = table.revoke_flow(dead)?;
        assert_eq!(removed, 2);
        assert_eq!(
            table.resolve(held_by_dead, dead, CapRights::SEND),
            Err(CapError::Unknown)
        );
        assert_eq!(
            table.resolve(targeting_dead, alive, CapRights::SEND),
            Err(CapError::Unknown)
        );
        assert!(table.resolve(unrelated, alive, CapRights::SEND).is_ok());
        Ok(())
    }

    #[test]
    fn delegate_issues_a_new_id_for_the_child() -> Result<(), Box<dyn std::error::Error>> {
        let table = CapTable::new();
        let parent = next_flow_id();
        let child = next_flow_id();
        let target = next_flow_id();
        let original = table.mint(parent, target, CapRights::SEND_ASK)?;
        let granted = table.delegate(original, parent, child)?;
        assert_ne!(granted, original);
        assert_eq!(
            table.resolve(granted, child, CapRights::SEND)?.target(),
            Some(target)
        );
        assert_eq!(
            table.resolve(original, child, CapRights::SEND),
            Err(CapError::NotHolder)
        );
        assert!(table.resolve(original, parent, CapRights::ASK).is_ok());
        Ok(())
    }

    #[test]
    fn attenuate_cannot_escalate() -> Result<(), Box<dyn std::error::Error>> {
        let table = CapTable::new();
        let parent = next_flow_id();
        let child = next_flow_id();
        let target = next_flow_id();
        let original = table.mint(parent, target, CapRights::SEND)?;
        let granted = table.attenuate(
            original,
            parent,
            child,
            CapRights::SEND.union(CapRights::ADMIN),
            None,
        )?;
        let got = table.resolve(granted, child, CapRights::SEND)?;
        assert!(!got.rights().contains(CapRights::ADMIN));
        assert!(got.rights().contains(CapRights::SEND));
        Ok(())
    }

    #[test]
    fn mint_or_reuse_is_stable_per_pair() -> Result<(), Box<dyn std::error::Error>> {
        let table = CapTable::new();
        let holder = next_flow_id();
        let target = next_flow_id();
        let first = table.mint_or_reuse(holder, target, CapRights::SEND)?;
        for _ in 0..1_000 {
            let again = table.mint_or_reuse(holder, target, CapRights::SEND)?;
            assert_eq!(again, first);
        }
        assert_eq!(table.entry_count()?, 1);
        assert_eq!(table.reply_index_len()?, 1);
        assert!(table.resolve(first, holder, CapRights::SEND).is_ok());
        Ok(())
    }

    #[test]
    fn mint_or_reuse_distinct_pairs_are_independent() -> Result<(), Box<dyn std::error::Error>> {
        let table = CapTable::new();
        let a = next_flow_id();
        let b = next_flow_id();
        let ab = table.mint_or_reuse(b, a, CapRights::SEND)?;
        let ba = table.mint_or_reuse(a, b, CapRights::SEND)?;
        assert_ne!(ab, ba);
        assert_eq!(table.entry_count()?, 2);
        assert_eq!(table.reply_index_len()?, 2);
        Ok(())
    }

    #[test]
    fn revoke_flow_clears_reply_index_for_holder_and_target() -> Result<(), Box<dyn std::error::Error>> {
        let table = CapTable::new();
        let a = next_flow_id();
        let b = next_flow_id();
        let alive = next_flow_id();
        let ab = table.mint_or_reuse(b, a, CapRights::SEND)?;
        let ba = table.mint_or_reuse(a, b, CapRights::SEND)?;
        let kept = table.mint_or_reuse(alive, b, CapRights::SEND)?;

        table.revoke_flow(a)?;
        assert!(!table.reply_index_mentions(a)?);
        assert_eq!(
            table.resolve(ab, b, CapRights::SEND),
            Err(CapError::Unknown)
        );
        assert_eq!(
            table.resolve(ba, a, CapRights::SEND),
            Err(CapError::Unknown)
        );
        assert!(table.resolve(kept, alive, CapRights::SEND).is_ok());
        assert!(!table.reply_index_mentions(a)?);
        assert!(table.reply_index_mentions(b)?);

        table.revoke_flow(b)?;
        assert!(!table.reply_index_mentions(a)?);
        assert!(!table.reply_index_mentions(b)?);
        Ok(())
    }

    #[test]
    fn mint_or_reuse_after_revoke_issues_a_fresh_id() -> Result<(), Box<dyn std::error::Error>> {
        let table = CapTable::new();
        let holder = next_flow_id();
        let target = next_flow_id();
        let old = table.mint_or_reuse(holder, target, CapRights::SEND)?;
        table.revoke_flow(target)?;
        assert!(!table.reply_index_mentions(target)?);
        let fresh = table.mint_or_reuse(holder, target, CapRights::SEND)?;
        assert_ne!(fresh, old);
        assert_eq!(
            table.resolve(old, holder, CapRights::SEND),
            Err(CapError::Unknown)
        );
        assert!(table.resolve(fresh, holder, CapRights::SEND).is_ok());
        Ok(())
    }
}
