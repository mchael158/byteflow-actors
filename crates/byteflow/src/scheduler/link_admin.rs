//! LINK / MONITOR / ADMIN as real capabilities, not ambient authority.
//!
//! Before: LINK/MONITOR took a bare FlowId (or a Cap resolved with empty
//! rights) and succeeded if the target existed — knowing the id was enough.
//! Now: the Cap must target that exact flow and carry the matching right.

use crate::bytecode::{Cap, CapRights, CapTarget, RevocationCell};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkError {
    WrongTarget,
    MissingRight,
    CapRevoked,
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkError::WrongTarget => f.write_str("cap does not target this flow"),
            LinkError::MissingRight => f.write_str("cap lacks LINK/MONITOR right"),
            LinkError::CapRevoked => f.write_str("capability revoked"),
        }
    }
}

impl std::error::Error for LinkError {}

pub fn check_link(cap: &Cap, cell: &RevocationCell, target: u64) -> Result<(), LinkError> {
    if !cap.is_valid(cell) {
        return Err(LinkError::CapRevoked);
    }
    if cap.target != CapTarget::Flow(target) {
        return Err(LinkError::WrongTarget);
    }
    if !cap.rights.contains(CapRights::LINK) {
        return Err(LinkError::MissingRight);
    }
    Ok(())
}

pub fn check_monitor(cap: &Cap, cell: &RevocationCell, target: u64) -> Result<(), LinkError> {
    if !cap.is_valid(cell) {
        return Err(LinkError::CapRevoked);
    }
    if cap.target != CapTarget::Flow(target) {
        return Err(LinkError::WrongTarget);
    }
    if !cap.rights.contains(CapRights::MONITOR) {
        return Err(LinkError::MissingRight);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminError {
    MissingRight,
    CapRevoked,
    WrongTarget,
}

impl std::fmt::Display for AdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdminError::MissingRight => f.write_str("cap lacks ADMIN right"),
            AdminError::CapRevoked => f.write_str("capability revoked"),
            AdminError::WrongTarget => f.write_str("ADMIN requires CapTarget::Scheduler"),
        }
    }
}

impl std::error::Error for AdminError {}

/// Scheduler-level ops (kill, inspect, quota top-up) all pass this check.
pub fn check_admin(cap: &Cap, cell: &RevocationCell) -> Result<(), AdminError> {
    if !cap.is_valid(cell) {
        return Err(AdminError::CapRevoked);
    }
    if cap.target != CapTarget::Scheduler {
        return Err(AdminError::WrongTarget);
    }
    if !cap.rights.contains(CapRights::ADMIN) {
        return Err(AdminError::MissingRight);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knowing_flow_id_alone_is_not_enough_to_link() {
        let cell = RevocationCell::new();
        let cap = Cap::root(CapTarget::Flow(1), CapRights::SEND, None, &cell);
        assert!(matches!(
            check_link(&cap, &cell, 1),
            Err(LinkError::MissingRight)
        ));
        let cap2 = Cap::root(CapTarget::Flow(2), CapRights::LINK, None, &cell);
        assert!(matches!(
            check_link(&cap2, &cell, 1),
            Err(LinkError::WrongTarget)
        ));
    }

    #[test]
    fn admin_requires_scheduler_target_and_right() {
        let cell = RevocationCell::new();
        let cap = Cap::root(CapTarget::Scheduler, CapRights::ADMIN, None, &cell);
        assert!(check_admin(&cap, &cell).is_ok());
        let cap2 = Cap::root(CapTarget::Flow(1), CapRights::ADMIN, None, &cell);
        assert!(matches!(
            check_admin(&cap2, &cell),
            Err(AdminError::WrongTarget)
        ));
    }
}
