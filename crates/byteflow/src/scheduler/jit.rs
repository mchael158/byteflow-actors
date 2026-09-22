//! Optional JIT hook for worker threads (`feature = "jit"`).

use std::sync::Arc;

use crate::jit::{run_vm_with_jit_runtime, JitRuntime};
use crate::vm::VmResult;

use super::process::Flow;
use super::runtime::Shared;

/// Run one scheduling quantum, using the shared JIT runtime when enabled.
pub fn run_flow_quantum(flow: &mut Flow, shared: &Shared, budget: u32) -> VmResult {
    let Some(jit) = shared.jit.as_ref() else {
        return flow.vm.run(budget);
    };
    run_vm_with_jit_runtime(&mut flow.vm, budget, jit, Some(&shared.metrics))
}

/// Construct the runtime-wide JIT state when enabled in config.
pub fn new_runtime(chunk: Arc<crate::Chunk>, hot_threshold: u32) -> Arc<JitRuntime> {
    Arc::new(JitRuntime::new(chunk, hot_threshold))
}
