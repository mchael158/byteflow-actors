//! One scheduling slice.
//!
//! The worker asks for a quantum here. When the `jit` feature is on and the
//! runtime enabled it, a hot integer or float trace may run first. Every
//! other instruction stays in the interpreter.

use crate::vm::VmResult;

use super::process::Flow;
use super::runtime::Shared;

pub(crate) fn run_slice(flow: &mut Flow, shared: &Shared, budget: u32) -> VmResult {
    #[cfg(feature = "jit")]
    {
        super::jit::run_flow_quantum(flow, shared, budget)
    }
    #[cfg(not(feature = "jit"))]
    {
        let _ = shared;
        flow.vm.run(budget)
    }
}
