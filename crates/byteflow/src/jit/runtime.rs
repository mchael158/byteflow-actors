//! Shared JIT state for the M:N runtime (cache + hot counters, lock-free execution path).

use std::sync::{Arc, Mutex, RwLock};

use crate::Chunk;

use super::trace::{HotCounter, TraceCache, TraceKey, HOT_THRESHOLD};
use super::CompiledTrace;

/// Runtime-wide JIT state: compiled traces are shared; each worker thread owns
/// its own Cranelift module for compilation ([`super::module_local`]).
pub struct JitRuntime {
    chunk: RwLock<Arc<Chunk>>,
    pub hot_threshold: u32,
    cache: RwLock<TraceCache>,
    hot: Mutex<HotCounter>,
}

impl JitRuntime {
    pub fn new(chunk: Arc<Chunk>, hot_threshold: u32) -> Self {
        Self {
            chunk: RwLock::new(chunk),
            hot_threshold,
            cache: RwLock::new(TraceCache::new()),
            hot: Mutex::new(HotCounter::default()),
        }
    }

    pub fn with_default_threshold(chunk: Arc<Chunk>) -> Self {
        Self::new(chunk, HOT_THRESHOLD)
    }

    /// Current bytecode image this cache was built for.
    pub fn chunk(&self) -> Option<Arc<Chunk>> {
        self.chunk.read().ok().map(|g| Arc::clone(&g))
    }

    /// True when `vm_chunk` is the same image as the JIT cache.
    pub fn matches_chunk(&self, vm_chunk: &Arc<Chunk>) -> bool {
        match self.chunk.read() {
            Ok(guard) => Arc::ptr_eq(&guard, vm_chunk),
            Err(_) => false,
        }
    }

    /// Replace the bytecode image and discard stale compiled traces.
    pub fn reload(&self, chunk: Arc<Chunk>) {
        if let Ok(mut current) = self.chunk.write() {
            *current = chunk;
        }
        if let Ok(mut cache) = self.cache.write() {
            cache.clear();
        }
        if let Ok(mut hot) = self.hot.lock() {
            hot.clear();
        }
    }

    pub fn get_trace(&self, key: &TraceKey) -> Option<JitEntryCopy> {
        let cache = self.cache.read().ok()?;
        cache.get(key).map(|t| JitEntryCopy { entry: t.entry })
    }

    pub fn insert_trace(&self, key: TraceKey, trace: CompiledTrace) {
        if let Ok(mut cache) = self.cache.write() {
            cache.insert(key, trace);
        }
    }

    pub fn record_hot_hit(&self, key: TraceKey) -> bool {
        let Ok(mut hot) = self.hot.lock() else {
            return false;
        };
        hot.hit(key, self.hot_threshold)
    }
}

/// [`JitEntry`] is a function pointer and safe to copy out of the cache briefly.
#[derive(Clone, Copy)]
pub struct JitEntryCopy {
    pub entry: super::frame::JitEntry,
}
