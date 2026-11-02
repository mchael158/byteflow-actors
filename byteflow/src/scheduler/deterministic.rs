//! Recorded order for external events in tests.
//!
//! A single worker still races with host threads that complete I/O.
//! [`ReplayQueue`] delivers events by the sequence number a test script or
//! an earlier trace assigned. It does not number events by arrival, and it
//! does not replace the production scheduler. The gate stays off unless a
//! test installs it.

use std::collections::{BTreeMap, HashMap};

use crate::bytecode::Value;

use super::process::FlowId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplayError {
    SequenceExhausted,
    DuplicateSequence,
    StaleSequence,
}

pub(crate) struct ReplayQueue<E> {
    next_sequence: u64,
    pending: BTreeMap<u64, E>,
}

impl<E> Default for ReplayQueue<E> {
    fn default() -> Self {
        Self {
            next_sequence: 0,
            pending: BTreeMap::new(),
        }
    }
}

impl<E> ReplayQueue<E> {
    /// Inserts an event with its recorded sequence number.
    pub(crate) fn insert(&mut self, sequence: u64, event: E) -> Result<(), ReplayError> {
        self.validate(sequence)?;
        self.pending.insert(sequence, event);
        Ok(())
    }

    /// Delivers only the next expected event.
    ///
    /// Missing sequence numbers intentionally block progress.
    pub(crate) fn pop_next(&mut self) -> Option<(u64, E)> {
        let sequence = self.next_sequence;
        let event = self.pending.remove(&sequence)?;
        match sequence.checked_add(1) {
            Some(next) => {
                self.next_sequence = next;
                Some((sequence, event))
            }
            None => {
                self.pending.insert(sequence, event);
                None
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn pending_len(&self) -> usize {
        self.pending.len()
    }

    #[cfg(test)]
    pub(crate) fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    fn validate(&self, sequence: u64) -> Result<(), ReplayError> {
        if sequence == u64::MAX {
            return Err(ReplayError::SequenceExhausted);
        }
        if sequence < self.next_sequence {
            return Err(ReplayError::StaleSequence);
        }
        if self.pending.contains_key(&sequence) {
            return Err(ReplayError::DuplicateSequence);
        }
        Ok(())
    }
}

/// One authenticated host completion waiting for its recorded turn.
pub(crate) struct StagedHostHop {
    pub flow: FlowId,
    pub value: Value,
}

/// Test-only gate. Production leaves this unset.
#[derive(Default)]
pub(crate) struct HostReplayGate {
    queue: ReplayQueue<StagedHostHop>,
    sequences: HashMap<u64, u64>,
    waiting: HashMap<u64, StagedHostHop>,
}

impl HostReplayGate {
    /// Records the sequence a ticket must use. A completion that arrived
    /// earlier is inserted now.
    #[cfg(test)]
    pub(crate) fn bind(&mut self, ticket: u64, sequence: u64) -> Result<(), ReplayError> {
        if self.sequences.contains_key(&ticket)
            || self
                .sequences
                .values()
                .any(|existing| *existing == sequence)
        {
            return Err(ReplayError::DuplicateSequence);
        }
        self.queue.validate(sequence)?;
        if let Some(hop) = self.waiting.remove(&ticket) {
            self.queue.insert(sequence, hop)?;
        }
        self.sequences.insert(ticket, sequence);
        Ok(())
    }

    /// Holds `hop` until [`Self::bind`] has named `ticket`, then until every
    /// earlier sequence has been delivered.
    pub(crate) fn stage(&mut self, ticket: u64, hop: StagedHostHop) -> Result<(), ReplayError> {
        if let Some(sequence) = self.sequences.get(&ticket).copied() {
            self.queue.validate(sequence)?;
            self.queue.insert(sequence, hop)?;
        } else {
            self.waiting.insert(ticket, hop);
        }
        Ok(())
    }

    pub(crate) fn drain_ready(&mut self) -> Vec<StagedHostHop> {
        let mut ready = Vec::new();
        while let Some((_, hop)) = self.queue.pop_next() {
            ready.push(hop);
        }
        ready
    }
}

pub(crate) fn replay_label(err: ReplayError) -> &'static str {
    match err {
        ReplayError::SequenceExhausted => "sequence exhausted",
        ReplayError::DuplicateSequence => "duplicate sequence",
        ReplayError::StaleSequence => "stale sequence",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivers_events_in_recorded_order() -> Result<(), ReplayError> {
        let mut queue = ReplayQueue::default();

        queue.insert(2, "third")?;
        queue.insert(0, "first")?;
        queue.insert(1, "second")?;

        assert_eq!(queue.pop_next(), Some((0, "first")));
        assert_eq!(queue.pop_next(), Some((1, "second")));
        assert_eq!(queue.pop_next(), Some((2, "third")));
        assert_eq!(queue.pop_next(), None);
        assert!(queue.is_empty());

        Ok(())
    }

    #[test]
    fn does_not_skip_a_missing_sequence() -> Result<(), ReplayError> {
        let mut queue = ReplayQueue::default();

        queue.insert(1, "later")?;

        assert_eq!(queue.pop_next(), None);
        assert_eq!(queue.next_sequence(), 0);
        assert_eq!(queue.pending_len(), 1);

        queue.insert(0, "first")?;

        assert_eq!(queue.pop_next(), Some((0, "first")));
        assert_eq!(queue.pop_next(), Some((1, "later")));

        Ok(())
    }

    #[test]
    fn rejects_duplicate_and_stale_sequences() -> Result<(), ReplayError> {
        let mut queue = ReplayQueue::default();

        queue.insert(0, "first")?;

        assert_eq!(
            queue.insert(0, "duplicate"),
            Err(ReplayError::DuplicateSequence)
        );

        assert_eq!(queue.pop_next(), Some((0, "first")));

        assert_eq!(queue.insert(0, "stale"), Err(ReplayError::StaleSequence));

        Ok(())
    }

    #[test]
    fn rejects_the_sequence_that_cannot_advance() {
        let mut queue = ReplayQueue::default();
        assert_eq!(
            queue.insert(u64::MAX, "nope"),
            Err(ReplayError::SequenceExhausted)
        );
        assert_eq!(queue.pop_next(), None);
        assert_eq!(queue.next_sequence(), 0);
    }
}
