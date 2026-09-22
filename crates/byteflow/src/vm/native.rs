//! Native (FFI) table for `Opcode::CallNative`.
//!
//! # The one rule: never block
//!
//! `CallNative` runs **inline** on the worker thread. A native that blocks
//! stalls every other Flow on that worker. Slow I/O belongs in a dedicated
//! Flow (`Send`/`Receive`), not here.

use std::sync::Arc;

use crate::bytecode::{Cap, CapRights, NativeIdx, NativeMask, RevocationCell, Value};

use super::fault::Fault;

pub use super::fault::NativeCallError;

/// Result type for a native function.
pub type NativeResult = Result<Value, Fault>;

/// A host-side function callable from bytecode via `Opcode::CallNative`.
pub type NativeFn = Arc<dyn Fn(&[Value]) -> NativeResult + Send + Sync>;

/// Immutable, indexable set of natives. Gaps left by [`NativeTableBuilder::register_at`]
/// are `None` — calling them faults with [`Fault::BadNative`].
pub struct NativeTable {
    entries: Vec<Option<(String, NativeFn)>>,
}

impl NativeTable {
    pub fn builder() -> NativeTableBuilder {
        NativeTableBuilder {
            entries: Vec::new(),
        }
    }

    /// Empty table — valid for chunks that never emit `CallNative`.
    pub fn empty() -> Arc<NativeTable> {
        Arc::new(NativeTable {
            entries: Vec::new(),
        })
    }

    #[inline]
    pub fn get(&self, index: u32) -> Option<&NativeFn> {
        self.entries
            .get(index as usize)
            .and_then(|slot| slot.as_ref())
            .map(|(_, f)| f)
    }

    pub fn index_of(&self, name: &str) -> Option<u32> {
        self.entries
            .iter()
            .enumerate()
            .find(|(_, slot)| slot.as_ref().is_some_and(|(n, _)| n == name))
            .map(|(i, _)| i as u32)
    }

    /// Slot count including reserved-but-empty gaps (one past the highest
    /// touched index), not the count of registered functions.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries
            .iter()
            .filter_map(|slot| slot.as_ref().map(|(n, _)| n.as_str()))
    }
}

/// Host error while building a [`NativeTable`] (duplicate name or occupied slot).
///
/// Category A: the embedder misconfigured FFI. Never a panic — `std_native_table`
/// and host tables must surface this as `Result`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeTableError {
    DuplicateName(String),
    SlotOccupied { index: u32, name: String },
}

impl std::fmt::Display for NativeTableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NativeTableError::DuplicateName(name) => {
                write!(f, "duplicate native function registered: '{name}'")
            }
            NativeTableError::SlotOccupied { index, name } => {
                write!(
                    f,
                    "native slot {index} already occupied (registering '{name}')"
                )
            }
        }
    }
}

impl std::error::Error for NativeTableError {}

/// Fluent builder for a [`NativeTable`].
///
/// - [`Self::register`] — next sequential slot (host builds table + chunk together).
/// - [`Self::register_at`] — fixed ABI slot (MCU / separately flashed bytecode).
pub struct NativeTableBuilder {
    entries: Vec<Option<(String, NativeFn)>>,
}

impl NativeTableBuilder {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Register `f` under `name` at the next sequential slot.
    pub fn register<F>(self, name: impl Into<String>, f: F) -> Result<Self, NativeTableError>
    where
        F: Fn(&[Value]) -> NativeResult + Send + Sync + 'static,
    {
        let index = self.entries.len() as u32;
        self.register_at(index, name, f)
    }

    /// Register `f` under `name` at explicit `index`, padding lower gaps as
    /// unregistered (`get` → `None` → [`Fault::BadNative`]).
    pub fn register_at<F>(
        mut self,
        index: u32,
        name: impl Into<String>,
        f: F,
    ) -> Result<Self, NativeTableError>
    where
        F: Fn(&[Value]) -> NativeResult + Send + Sync + 'static,
    {
        let name = name.into();
        for (n, _) in self.entries.iter().flatten() {
            if n == &name {
                return Err(NativeTableError::DuplicateName(name));
            }
        }
        let index_usize = index as usize;
        if index_usize >= self.entries.len() {
            self.entries.resize_with(index_usize + 1, || None);
        }
        if self.entries[index_usize].is_some() {
            return Err(NativeTableError::SlotOccupied { index, name });
        }
        self.entries[index_usize] = Some((name, Arc::new(f)));
        Ok(self)
    }

    pub fn build(self) -> Arc<NativeTable> {
        Arc::new(NativeTable {
            entries: self.entries,
        })
    }
}

impl Default for NativeTableBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Require `args[index]` to exist.
pub fn expect_arg<'a>(args: &'a [Value], index: usize, fn_name: &str) -> Result<&'a Value, Fault> {
    args.get(index).ok_or(Fault::NativeError(format!(
        "{fn_name}: missing argument {index}"
    )))
}

/// Require `args[index]` to coerce to an int (`Value::as_int`).
pub fn expect_int(args: &[Value], index: usize, fn_name: &str) -> Result<i64, Fault> {
    expect_arg(args, index, fn_name)?
        .as_int()
        .ok_or(Fault::NativeError(format!(
            "{fn_name}: argument {index} is not an int"
        )))
}

/// Require `args[index]` to be a bool (ints: nonzero = true).
pub fn expect_bool(args: &[Value], index: usize, fn_name: &str) -> Result<bool, Fault> {
    let v = expect_arg(args, index, fn_name)?;
    match v {
        Value::Bool(_) | Value::Int(_) => Ok(v.is_truthy()),
        other => Err(Fault::NativeError(format!(
            "{fn_name}: argument {index} is not a bool/int (got {})",
            other.type_name()
        ))),
    }
}

/// Require `args[index]` to be a [`crate::Message`].
///
/// Used by the std `msg_*` natives. A wrong type becomes
/// [`Fault::NativeError`] (category B — Flow fault), not a host panic.
pub fn expect_message(
    args: &[Value],
    index: usize,
    fn_name: &str,
) -> Result<crate::Message, Fault> {
    expect_arg(args, index, fn_name)?
        .as_message()
        .cloned()
        .ok_or(Fault::NativeError(format!(
            "{fn_name}: argument {index} is not a message"
        )))
}

/// Coerce `args[index]` to `u64` from `Int` (≥ 0) or `Pid`.
///
/// Used by `make_msg` for unsigned envelope fields (`request_id`, `tag`);
/// accepts a non-negative `Int` or a `Pid` without an extra conversion.
/// Negative ints are rejected — those fields are unsigned on the wire.
pub fn expect_u64(args: &[Value], index: usize, fn_name: &str) -> Result<u64, Fault> {
    let v = expect_arg(args, index, fn_name)?;
    if let Some(p) = v.as_pid() {
        return Ok(p);
    }
    if let Some(i) = v.as_int().filter(|&n| n >= 0) {
        return Ok(i as u64);
    }
    Err(Fault::NativeError(format!(
        "{fn_name}: argument {index} is not a non-negative int/pid (got {})",
        v.type_name()
    )))
}

/// Per-flow native allowlist snapshot, checked before indexing the table.
#[derive(Clone, Debug)]
pub struct NativeGate {
    has_native: bool,
    mask: NativeMask,
    authority_epoch: u64,
    flow_cell: Arc<RevocationCell>,
    native_epoch: u64,
    native_cell: Arc<RevocationCell>,
}

impl NativeGate {
    /// No native right — unit tests and chunks that never call out.
    pub fn deny(native_count: usize) -> Self {
        Self {
            has_native: false,
            mask: NativeMask::empty(native_count),
            authority_epoch: 0,
            flow_cell: Arc::new(RevocationCell::new()),
            native_epoch: 0,
            native_cell: Arc::new(RevocationCell::new()),
        }
    }

    pub fn from_authority(
        cap: &Cap,
        flow_cell: Arc<RevocationCell>,
        native_cell: Arc<RevocationCell>,
        native_count: usize,
    ) -> Self {
        let mask = match &cap.native_mask {
            Some(m) => m.clone(),
            None => NativeMask::empty(native_count),
        };
        Self {
            has_native: cap.rights.contains(CapRights::NATIVE),
            mask,
            authority_epoch: cap.epoch(),
            flow_cell,
            native_epoch: native_cell.epoch(),
            native_cell,
        }
    }

    pub fn is_live(&self) -> bool {
        self.flow_cell.epoch() == self.authority_epoch
            && self.native_cell.epoch() == self.native_epoch
    }
}

/// Host-side check against a [`Cap`] (not the interpreter path).
///
/// The VM uses [`check_native_gate`] on the per-flow snapshot. This helper
/// is for embedders that hold a live `Cap` and want the same verdict.
pub fn check_native_call(
    cap: &Cap,
    table: &NativeTable,
    idx: NativeIdx,
) -> Result<(), NativeCallError> {
    if (idx as usize) >= table.len() {
        return Err(NativeCallError::IndexOutOfRange(idx));
    }
    if !cap.rights.contains(CapRights::NATIVE) {
        return Err(NativeCallError::NoNativeRight);
    }
    match &cap.native_mask {
        Some(mask) if mask.allows(idx) => Ok(()),
        _ => Err(NativeCallError::IndexNotAllowlisted(idx)),
    }
}

pub fn check_native_gate(
    gate: &NativeGate,
    table: &NativeTable,
    idx: NativeIdx,
) -> Result<(), NativeCallError> {
    if (idx as usize) >= table.len() {
        return Err(NativeCallError::IndexOutOfRange(idx));
    }
    if !gate.is_live() {
        return Err(NativeCallError::Revoked);
    }
    if !gate.has_native {
        return Err(NativeCallError::NoNativeRight);
    }
    if gate.mask.allows(idx) {
        Ok(())
    } else {
        Err(NativeCallError::IndexNotAllowlisted(idx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_at_leaves_holes_as_none() -> Result<(), Box<dyn std::error::Error>> {
        let table = NativeTable::builder()
            .register_at(10, "answer", |_| Ok(Value::Int(42)))?
            .build();
        assert_eq!(table.len(), 11);
        assert_eq!(table.index_of("answer"), Some(10));
        let f = table.get(10).ok_or("missing native")?;
        assert!(matches!(f(&[])?, Value::Int(42)));
        assert!(table.get(2).is_none());
        Ok(())
    }

    #[test]
    fn register_at_errors_on_duplicate_slot() {
        let result = NativeTable::builder()
            .register_at(3, "a", |_| Ok(Value::Unit))
            .and_then(|b| b.register_at(3, "b", |_| Ok(Value::Unit)));
        assert!(matches!(
            result,
            Err(NativeTableError::SlotOccupied { index: 3, .. })
        ));
    }

    #[test]
    fn register_errors_on_duplicate_name() {
        let result = NativeTable::builder()
            .register("x", |_| Ok(Value::Unit))
            .and_then(|b| b.register("x", |_| Ok(Value::Unit)));
        assert!(matches!(result, Err(NativeTableError::DuplicateName(_))));
    }

    #[test]
    fn denies_unlisted_index_even_with_native_right() {
        use crate::bytecode::{CapTarget, RevocationCell};

        let cell = RevocationCell::new();
        let mask = NativeMask::from_indices(16, &[2, 4]);
        let cap = Cap::root(CapTarget::Flow(1), CapRights::NATIVE, Some(mask), &cell);
        let table = NativeTable {
            entries: Vec::new(),
        };
        assert!(matches!(
            check_native_call(&cap, &table, 4),
            Err(NativeCallError::IndexOutOfRange(_))
        ));
    }
}
