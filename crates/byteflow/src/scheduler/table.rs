//! One named table per flow.
//!
//! The owner opens it and receives a Cap whose target is [`CapTarget::Table`].
//! `RECV` reads, `SEND` writes. Delegate narrows those bits. Finalize drops
//! the rows and revokes the Cap. There is no process-wide table that any
//! bytecode can write.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::bytecode::{Cap, CapId, CapRights, CapTarget, MapKey, RevocationCell, Value};

use super::capability::CapError;
use super::process::FlowId;
use super::runtime::Shared;
use super::sync_lock;

const MAX_ROWS: usize = 256;
const MAX_BYTES: usize = 64 * 1024;
const MAX_NAME: usize = 256;

struct OwnedTable {
    owner: FlowId,
    rows: Vec<(MapKey, Value)>,
    bytes: usize,
}

struct TablesInner {
    by_owner: std::collections::HashMap<u64, u64>,
    tables: std::collections::HashMap<u64, OwnedTable>,
}

pub(crate) struct FlowTables {
    inner: Mutex<TablesInner>,
    next_id: AtomicU64,
}

impl FlowTables {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(TablesInner {
                by_owner: std::collections::HashMap::new(),
                tables: std::collections::HashMap::new(),
            }),
            next_id: AtomicU64::new(1),
        }
    }

    pub(crate) fn open(&self, shared: &Shared, owner: FlowId, name: &str) -> Result<CapId, String> {
        if name.is_empty() || name.len() > MAX_NAME {
            return Err("table name must be 1..=256 bytes".into());
        }
        let mut inner =
            sync_lock::lock(&self.inner, "FlowTables::open").map_err(|e| e.to_string())?;
        if inner.by_owner.contains_key(&owner.as_u64()) {
            return Err("flow already owns a table".into());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if id == 0 {
            // The add already wrapped the counter to 1. Pin it at 0 so the
            // next open cannot mint table id 1 again while an old table lives.
            self.next_id.store(0, Ordering::Relaxed);
            return Err("table ids exhausted".into());
        }
        let cell = Arc::new(RevocationCell::new());
        shared
            .caps
            .bind_table(id, Arc::clone(&cell))
            .map_err(|e| e.to_string())?;
        let cap = Cap::root(
            CapTarget::Table(id),
            CapRights::SEND.union(CapRights::RECV),
            None,
            cell.as_ref(),
        );
        let cap_id = match shared.caps.grant(owner, cap) {
            Ok(cap_id) => cap_id,
            Err(err) => {
                let _ = shared.caps.revoke_table(id);
                return Err(err.to_string());
            }
        };
        inner.by_owner.insert(owner.as_u64(), id);
        inner.tables.insert(
            id,
            OwnedTable {
                owner,
                rows: Vec::new(),
                bytes: 0,
            },
        );
        Ok(cap_id)
    }

    pub(crate) fn get(
        &self,
        shared: &Shared,
        holder: FlowId,
        cap: CapId,
        key: &Value,
    ) -> Result<Value, String> {
        let id = table_id(shared, holder, cap, CapRights::RECV)?;
        let key = map_key(key)?;
        let inner = sync_lock::lock(&self.inner, "FlowTables::get").map_err(|e| e.to_string())?;
        let Some(table) = inner.tables.get(&id) else {
            return Err("table is closed".into());
        };
        Ok(
            match table.rows.iter().find(|(existing, _)| existing == &key) {
                Some((_, value)) => value.clone(),
                None => Value::Unit,
            },
        )
    }

    pub(crate) fn put(
        &self,
        shared: &Shared,
        holder: FlowId,
        cap: CapId,
        key: &Value,
        value: Value,
    ) -> Result<(), String> {
        let id = table_id(shared, holder, cap, CapRights::SEND)?;
        let key = map_key(key)?;
        if !value_allowed(&value, 0) {
            return Err("table values cannot hold cap, pid, or message".into());
        }
        let add = row_bytes(&key, &value)?;
        let mut inner =
            sync_lock::lock(&self.inner, "FlowTables::put").map_err(|e| e.to_string())?;
        let (owner, old, next) = {
            let Some(table) = inner.tables.get_mut(&id) else {
                return Err("table is closed".into());
            };
            let owner = table.owner;
            let old_at = table.rows.iter().position(|(existing, _)| existing == &key);
            let old = match old_at {
                Some(index) => row_bytes(&table.rows[index].0, &table.rows[index].1)?,
                None => 0,
            };
            // `old == 0` is also a stored `Int` key with a scalar value.
            // The row cap applies only when the key is new.
            if old_at.is_none() && table.rows.len() >= MAX_ROWS {
                return Err("table row limit".into());
            }
            let next = match (table.bytes - old).checked_add(add) {
                Some(sum) => sum,
                None => return Err("table byte limit".into()),
            };
            if next > MAX_BYTES {
                return Err("table byte limit".into());
            }
            (owner, old, next)
        };
        account(shared, owner, old, add)?;
        let Some(table) = inner.tables.get_mut(&id) else {
            release_account(shared, owner, add.saturating_sub(old));
            return Err("table is closed".into());
        };
        if let Some(index) = table.rows.iter().position(|(existing, _)| existing == &key) {
            table.rows[index].1 = value;
        } else {
            table.rows.push((key, value));
        }
        table.bytes = next;
        Ok(())
    }

    pub(crate) fn close_owner(&self, shared: &Shared, owner: FlowId) {
        let removed = match sync_lock::lock(&self.inner, "FlowTables::close_owner") {
            Ok(mut inner) => {
                let Some(id) = inner.by_owner.remove(&owner.as_u64()) else {
                    return;
                };
                let table = inner.tables.remove(&id);
                (id, table)
            }
            Err(err) => {
                super::error::report_fault(err);
                return;
            }
        };
        let (id, table) = removed;
        if let Some(table) = table {
            if table.bytes > 0 {
                release_account(shared, owner, table.bytes);
            }
        }
        if let Err(err) = shared.caps.revoke_table(id) {
            super::error::report_fault(err);
        }
    }
}

fn table_id(shared: &Shared, holder: FlowId, cap: CapId, need: CapRights) -> Result<u64, String> {
    let entry = shared
        .caps
        .resolve(cap, holder, need)
        .map_err(cap_message)?;
    match entry.cap.target() {
        CapTarget::Table(id) => Ok(id),
        _ => Err("capability does not address a table".into()),
    }
}

fn cap_message(err: CapError) -> String {
    err.to_string()
}

fn map_key(value: &Value) -> Result<MapKey, String> {
    match value {
        Value::Int(n) => Ok(MapKey::Int(*n)),
        Value::Str(text) => Ok(MapKey::Str(Arc::clone(text))),
        other => Err(format!(
            "table key must be int or str, got {}",
            other.type_name()
        )),
    }
}

fn value_allowed(value: &Value, depth: u32) -> bool {
    if depth > crate::bytecode::MAX_VALUE_DEPTH {
        return false;
    }
    match value {
        Value::Cap(_) | Value::Pid(_) | Value::Message(_) => false,
        Value::List(items) => items.iter().all(|item| value_allowed(item, depth + 1)),
        Value::Map(entries) => entries
            .iter()
            .all(|(_, item)| value_allowed(item, depth + 1)),
        Value::Unit
        | Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::Str(_)
        | Value::Bytes(_) => true,
    }
}

fn row_bytes(key: &MapKey, value: &Value) -> Result<usize, String> {
    let key_bytes = match key {
        MapKey::Int(_) => 0,
        MapKey::Str(text) => text.len(),
    };
    let heap = value.heap_size();
    if heap == usize::MAX {
        return Err("table value is too nested".into());
    }
    key_bytes
        .checked_add(heap)
        .ok_or_else(|| "table row does not fit".to_string())
}

fn account(shared: &Shared, owner: FlowId, old: usize, new: usize) -> Result<(), String> {
    if new > old {
        let delta = new - old;
        shared
            .memory
            .try_charge(delta)
            .map_err(|err| err.to_string())?;
        if let Some(quota) = shared.quotas.get(owner).map_err(|err| err.to_string())? {
            if let Err(err) = quota.alloc(delta) {
                shared.memory.release(delta);
                return Err(err.to_string());
            }
        }
        Ok(())
    } else if old > new {
        release_account(shared, owner, old - new);
        Ok(())
    } else {
        Ok(())
    }
}

fn release_account(shared: &Shared, owner: FlowId, bytes: usize) {
    if bytes == 0 {
        return;
    }
    shared.memory.release(bytes);
    if let Ok(Some(quota)) = shared.quotas.get(owner) {
        quota.free(bytes);
    }
}
