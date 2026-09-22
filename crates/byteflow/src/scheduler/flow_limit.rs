//! Atomic live-flow reservation for [`crate::RuntimeConfig::max_flows`].
//!
//! `directory.len()` is an observation, not a reservation — two workers can
//! both pass a len check and overshoot the cap. This counter turns the limit
//! into a CAS-backed resource released exactly once in finalize (or on spawn
//! rollback).

use std::sync::atomic::{AtomicUsize, Ordering};

use super::error::SpawnError;

/// Process-wide live-flow budget shared by every `spawn_on`.
pub(crate) struct FlowLimit {
    live: AtomicUsize,
    /// `0` = unlimited (still tracks `live` for diagnostics / symmetry).
    max: usize,
}

impl FlowLimit {
    pub fn new(max_flows: u32) -> Self {
        FlowLimit {
            live: AtomicUsize::new(0),
            max: max_flows as usize,
        }
    }

    /// Reserve one live slot. Fail closed if `max != 0` and already at cap.
    pub fn try_reserve(&self) -> Result<(), SpawnError> {
        loop {
            let current = self.live.load(Ordering::Acquire);
            if self.max != 0 && current >= self.max {
                return Err(SpawnError::FlowLimit {
                    current,
                    max: self.max as u32,
                });
            }
            match self.live.compare_exchange_weak(
                current,
                current.saturating_add(1),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(_) => continue,
            }
        }
    }

    /// Release one reserved slot (finalize or aborted spawn).
    pub fn release(&self) {
        let _ = self
            .live
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |c| {
                Some(c.saturating_sub(1))
            });
    }

    #[cfg(test)]
    pub fn live(&self) -> usize {
        self.live.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_hits_cap() {
        let lim = FlowLimit::new(2);
        assert!(lim.try_reserve().is_ok());
        assert!(lim.try_reserve().is_ok());
        assert!(matches!(
            lim.try_reserve(),
            Err(SpawnError::FlowLimit { current: 2, max: 2 })
        ));
        lim.release();
        assert!(lim.try_reserve().is_ok());
        assert_eq!(lim.live(), 2);
    }

    #[test]
    fn unlimited_always_reserves() {
        let lim = FlowLimit::new(0);
        for _ in 0..8 {
            assert!(lim.try_reserve().is_ok());
        }
        assert_eq!(lim.live(), 8);
        for _ in 0..8 {
            lim.release();
        }
        assert_eq!(lim.live(), 0);
    }
}
