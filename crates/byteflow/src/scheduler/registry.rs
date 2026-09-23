//! Named registry: `name → CapId` (address), never `name → FlowId`.
//!
//! Host `whereis` returns the **stored** Cap (the token passed to
//! `register_name`). Bytecode `Whereis` remints a SEND Cap for the
//! *caller* (`CapTable::mint_or_reuse`) so knowing a name is not an
//! ambient grant of the registered token.
//!
//! Entries are swept when the **target** flow exits ([`Registry::unregister_flow`]).
//! A revoked Cap cannot be registered; lookup after exit returns `None`.

use std::collections::HashMap;

use super::error::{LifecycleError, RuntimeError};
use super::process::FlowId;
use super::sync_lock;
use crate::bytecode::CapId;

/// Registry key. Interned as `Box<str>` so lookups do not allocate a `String`
/// on the happy path when the caller already has a `&str`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RegistryName(Box<str>);

impl RegistryName {
    pub fn new(name: impl Into<Box<str>>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for RegistryName {
    fn from(name: &str) -> Self {
        Self(name.into())
    }
}

struct Entry {
    cap: CapId,
    flow: FlowId,
}

pub struct Registry {
    by_name: HashMap<RegistryName, Entry>,
    by_flow: HashMap<FlowId, Vec<RegistryName>>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            by_name: HashMap::new(),
            by_flow: HashMap::new(),
        }
    }

    pub fn register(
        &mut self,
        name: RegistryName,
        cap: CapId,
        flow: FlowId,
    ) -> Result<(), LifecycleError> {
        if name.as_str().is_empty() {
            return Err(LifecycleError::EmptyName);
        }
        if self.by_name.contains_key(&name) {
            return Err(LifecycleError::AlreadyRegistered);
        }
        self.by_flow.entry(flow).or_default().push(name.clone());
        self.by_name.insert(name, Entry { cap, flow });
        Ok(())
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn whereis(&self, name: &str) -> Option<CapId> {
        self.by_name.get(&RegistryName::from(name)).map(|e| e.cap)
    }

    pub fn target(&self, name: &str) -> Option<FlowId> {
        self.by_name.get(&RegistryName::from(name)).map(|e| e.flow)
    }

    pub fn unregister(&mut self, name: &str) -> bool {
        let key = RegistryName::from(name);
        match self.by_name.remove(&key) {
            Some(entry) => {
                if let Some(names) = self.by_flow.get_mut(&entry.flow) {
                    names.retain(|n| n != &key);
                    if names.is_empty() {
                        self.by_flow.remove(&entry.flow);
                    }
                }
                true
            }
            None => false,
        }
    }

    /// Drop every name that pointed at `flow` (called from finalize).
    pub fn unregister_flow(&mut self, flow: FlowId) {
        if let Some(names) = self.by_flow.remove(&flow) {
            for name in names {
                self.by_name.remove(&name);
            }
        }
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

pub struct RegistryStore {
    inner: std::sync::Mutex<Registry>,
    max_names: u32,
}

impl RegistryStore {
    /// `max_registry_names == 0` → unlimited.
    pub fn new(max_registry_names: u32) -> Self {
        Self {
            inner: std::sync::Mutex::new(Registry::new()),
            max_names: max_registry_names,
        }
    }

    pub fn register(
        &self,
        name: RegistryName,
        cap: CapId,
        flow: FlowId,
    ) -> Result<Result<(), LifecycleError>, RuntimeError> {
        let mut table = sync_lock::lock(&self.inner, "RegistryStore::register")?;
        if name.as_str().is_empty() {
            return Ok(Err(LifecycleError::EmptyName));
        }
        if table.by_name.contains_key(&name) {
            return Ok(Err(LifecycleError::AlreadyRegistered));
        }
        if self.max_names != 0 && table.len() as u32 >= self.max_names {
            return Ok(Err(LifecycleError::RegistryLimitReached {
                limit: self.max_names,
            }));
        }
        Ok(table.register(name, cap, flow))
    }

    pub fn whereis(&self, name: &str) -> Result<Option<CapId>, RuntimeError> {
        Ok(sync_lock::lock(&self.inner, "RegistryStore::whereis")?.whereis(name))
    }

    pub fn target(&self, name: &str) -> Result<Option<FlowId>, RuntimeError> {
        Ok(sync_lock::lock(&self.inner, "RegistryStore::target")?.target(name))
    }

    pub fn unregister(&self, name: &str) -> Result<bool, RuntimeError> {
        Ok(sync_lock::lock(&self.inner, "RegistryStore::unregister")?.unregister(name))
    }

    pub fn unregister_flow(&self, flow: FlowId) -> Result<(), RuntimeError> {
        sync_lock::lock(&self.inner, "RegistryStore::unregister_flow")?.unregister_flow(flow);
        Ok(())
    }
}

impl Default for RegistryStore {
    fn default() -> Self {
        Self::new(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::CapId;
    use crate::scheduler::process::next_flow_id;

    #[test]
    fn unregister_flow_clears_names() {
        let mut reg = Registry::new();
        let flow = next_flow_id();
        let cap = CapId::from_raw(42);
        assert!(reg.register(RegistryName::from("svc"), cap, flow).is_ok());
        assert_eq!(reg.whereis("svc"), Some(cap));
        reg.unregister_flow(flow);
        assert_eq!(reg.whereis("svc"), None);
    }

    #[test]
    fn duplicate_name_is_rejected() {
        let mut reg = Registry::new();
        let flow = next_flow_id();
        assert!(reg
            .register(RegistryName::from("svc"), CapId::from_raw(1), flow)
            .is_ok());
        assert_eq!(
            reg.register(RegistryName::from("svc"), CapId::from_raw(2), flow),
            Err(LifecycleError::AlreadyRegistered)
        );
    }

    #[test]
    fn empty_name_is_rejected() {
        let mut reg = Registry::new();
        let flow = next_flow_id();
        assert_eq!(
            reg.register(RegistryName::from(""), CapId::from_raw(1), flow),
            Err(LifecycleError::EmptyName)
        );
    }

    #[test]
    fn registry_limit_distinct_from_already_registered() -> Result<(), Box<dyn std::error::Error>>
    {
        let store = RegistryStore::new(1);
        let a = next_flow_id();
        let b = next_flow_id();
        store.register(RegistryName::from("a"), CapId::from_raw(1), a)??;
        let dup = store.register(RegistryName::from("a"), CapId::from_raw(2), b)?;
        assert!(matches!(dup, Err(LifecycleError::AlreadyRegistered)));
        let limited = store.register(RegistryName::from("b"), CapId::from_raw(3), b)?;
        assert!(matches!(
            limited,
            Err(LifecycleError::RegistryLimitReached { limit: 1 })
        ));
        store.unregister("a")?;
        store.register(RegistryName::from("b"), CapId::from_raw(3), b)??;
        Ok(())
    }
}
