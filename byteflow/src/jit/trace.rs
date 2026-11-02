use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use cranelift_codegen::settings::Configurable;

use crate::Chunk;

use super::frame::JitEntry;

/// Maximum instructions recorded into a single trace.
pub const MAX_TRACE_LENGTH: usize = 256;

/// How many times `(function, pc)` must run before compilation is attempted.
pub const HOT_THRESHOLD: u32 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TraceKey {
    pub function: u32,
    pub entry_pc: u32,
    /// `true` when every live register at entry was `Float`, `Bool`, or
    /// `Unit`. Arithmetic in the trace is IEEE `f64`, not wrapping `i64`.
    pub floats: bool,
}

#[derive(Debug, Clone)]
pub struct TraceSpan {
    pub function: u32,
    pub range: Range<u32>,
}

#[derive(Debug)]
pub struct CompiledTrace {
    pub span: TraceSpan,
    pub entry: JitEntry,
}

pub struct TraceCache {
    traces: HashMap<TraceKey, CompiledTrace>,
    failed: HashSet<TraceKey>,
}

impl TraceCache {
    pub fn new() -> Self {
        Self {
            traces: HashMap::new(),
            failed: HashSet::new(),
        }
    }

    pub fn get(&self, key: &TraceKey) -> Option<&CompiledTrace> {
        self.traces.get(key)
    }

    pub fn is_failed(&self, key: &TraceKey) -> bool {
        self.failed.contains(key)
    }

    pub fn mark_failed(&mut self, key: TraceKey) {
        self.failed.insert(key);
    }

    pub fn insert(&mut self, key: TraceKey, trace: CompiledTrace) {
        self.traces.insert(key, trace);
    }

    pub fn clear(&mut self) {
        self.traces.clear();
        self.failed.clear();
    }

    #[cfg(test)]
    pub(crate) fn has_float_trace(&self) -> bool {
        self.traces.keys().any(|key| key.floats)
    }

    /// `pc` lies in a trace compiled with `TraceKey::floats`.
    #[cfg(test)]
    pub(crate) fn float_trace_covers(&self, pc: u32) -> bool {
        self.traces.iter().any(|(key, trace)| {
            key.floats && trace.span.function == key.function && trace.span.range.contains(&pc)
        })
    }
}

impl Default for TraceCache {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
pub struct HotCounter {
    counters: HashMap<TraceKey, u32>,
}

impl HotCounter {
    pub fn hit(&mut self, key: TraceKey, threshold: u32) -> bool {
        let counter = self.counters.entry(key).or_insert(0);
        *counter = counter.saturating_add(1);
        *counter >= threshold
    }

    pub fn reset(&mut self, key: TraceKey) {
        self.counters.remove(&key);
    }

    pub fn clear(&mut self) {
        self.counters.clear();
    }
}

pub struct JitContext {
    pub chunk: Arc<Chunk>,
    pub cache: TraceCache,
    pub hot: HotCounter,
    pub hot_threshold: u32,
    module: cranelift_jit::JITModule,
}

pub(crate) fn cranelift_flags() -> cranelift_codegen::settings::Flags {
    let mut flag_builder = cranelift_codegen::settings::builder();
    if !cfg!(debug_assertions) {
        // Debug compiles stay at Cranelift's default (`none`). Release
        // traces pay for the speed pipeline once per hot region.
        let _ = flag_builder.set("opt_level", "speed");
    }
    cranelift_codegen::settings::Flags::new(flag_builder)
}

impl JitContext {
    pub fn new(chunk: Arc<Chunk>) -> Result<Self, super::error::CompileError> {
        let flags = cranelift_flags();
        let isa = cranelift_native::builder()
            .map_err(|e| super::error::CompileError::Backend(e.to_string()))?
            .finish(flags)
            .map_err(|e| super::error::CompileError::Backend(e.to_string()))?;
        let builder =
            cranelift_jit::JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        let module = cranelift_jit::JITModule::new(builder);
        Ok(Self {
            chunk,
            cache: TraceCache::new(),
            hot: HotCounter::default(),
            hot_threshold: HOT_THRESHOLD,
            module,
        })
    }

    pub fn module_mut(&mut self) -> &mut cranelift_jit::JITModule {
        &mut self.module
    }

    /// Drop compiled traces and hot counters after the bytecode image changes.
    pub fn reload(&mut self, chunk: Arc<Chunk>) {
        self.chunk = chunk;
        self.cache.clear();
        self.hot.clear();
    }
}
