//! SPAWN confinement — children never inherit the parent's raw Cap.
//!
//! The child receives an attenuated Cap, explicitly requested on the SPAWN
//! operand. If bytecode requests nothing (`rights = NONE`), the child is
//! born with no native/send/spawn power and must be granted something later
//! via DELEGATE. Host `Runtime::spawn` mints a trusted root instead.

use crate::bytecode::{Cap, CapRights, CapTarget, NativeMask, RevocationCell};

use super::quota::FlowQuota;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfinedSpawnError {
    RateLimited,
    SourceCapRevoked,
    MissingSpawnRight,
}

impl std::fmt::Display for ConfinedSpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfinedSpawnError::RateLimited => f.write_str("spawn rate exceeded"),
            ConfinedSpawnError::SourceCapRevoked => f.write_str("parent capability revoked"),
            ConfinedSpawnError::MissingSpawnRight => f.write_str("parent lacks SPAWN right"),
        }
    }
}

impl std::error::Error for ConfinedSpawnError {}

impl From<ConfinedSpawnError> for super::error::SpawnError {
    fn from(err: ConfinedSpawnError) -> Self {
        match err {
            ConfinedSpawnError::RateLimited => super::error::SpawnError::SpawnRateExceeded,
            ConfinedSpawnError::SourceCapRevoked => super::error::SpawnError::ParentCapRevoked,
            ConfinedSpawnError::MissingSpawnRight => super::error::SpawnError::MissingSpawnRight,
        }
    }
}

/// `parent_cap` is the parent's self-authority. Child rights come only from
/// [`Cap::attenuate`] — no second grant path. `child_cell` must already be
/// bound in the Cap table for `new_child_id`.
pub fn exec_spawn_authority(
    parent_cap: &Cap,
    parent_cell: &RevocationCell,
    parent_quota: &FlowQuota,
    new_child_id: u64,
    requested_rights: CapRights,
    requested_native: Option<&NativeMask>,
    child_cell: &RevocationCell,
) -> Result<Cap, ConfinedSpawnError> {
    if !parent_cap.is_valid(parent_cell) {
        return Err(ConfinedSpawnError::SourceCapRevoked);
    }
    if !parent_cap.rights.contains(CapRights::SPAWN) {
        return Err(ConfinedSpawnError::MissingSpawnRight);
    }
    parent_quota
        .check_spawn()
        .map_err(|_| ConfinedSpawnError::RateLimited)?;

    let attenuated = parent_cap.attenuate(requested_rights, requested_native);
    Ok(Cap::root(
        CapTarget::Flow(new_child_id),
        attenuated.rights,
        attenuated.native_mask,
        child_cell,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::quota::FlowQuota;

    #[test]
    fn child_cannot_gain_admin() {
        let cell = RevocationCell::new();
        let parent = Cap::root(
            CapTarget::Flow(1),
            CapRights::FLOW,
            Some(NativeMask::full(8)),
            &cell,
        );
        let quota = FlowQuota::new(1000, 1024, 10, 10, 10, 10);
        let child_cell = RevocationCell::new();
        let out = match exec_spawn_authority(
            &parent,
            &cell,
            &quota,
            2,
            CapRights::FLOW.union(CapRights::ADMIN),
            None,
            &child_cell,
        ) {
            Ok(c) => c,
            Err(_) => panic!("spawn authority must succeed"),
        };
        assert!(!out.rights.contains(CapRights::ADMIN));
        assert!(out.rights.contains(CapRights::NATIVE));
    }

    #[test]
    fn confined_request_yields_none() {
        let cell = RevocationCell::new();
        let parent = Cap::root(CapTarget::Flow(1), CapRights::FLOW, None, &cell);
        let quota = FlowQuota::new(1000, 1024, 10, 10, 10, 10);
        let child_cell = RevocationCell::new();
        let out = match exec_spawn_authority(
            &parent,
            &cell,
            &quota,
            2,
            CapRights::NONE,
            None,
            &child_cell,
        ) {
            Ok(c) => c,
            Err(_) => panic!("confined spawn must succeed"),
        };
        assert_eq!(out.rights, CapRights::NONE);
    }
}
