//! Per-thread Cranelift [`JITModule`] — compilation does not contend on a global lock.

use std::cell::RefCell;

use super::error::CompileError;

thread_local! {
    static TLS_MODULE: RefCell<Option<cranelift_jit::JITModule>> = const { RefCell::new(None) };
}

fn create_module() -> Result<cranelift_jit::JITModule, CompileError> {
    let flags = super::trace::cranelift_flags();
    let isa = cranelift_native::builder()
        .map_err(|e| CompileError::Backend(e.to_string()))?
        .finish(flags)
        .map_err(|e| CompileError::Backend(e.to_string()))?;
    let builder =
        cranelift_jit::JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    Ok(cranelift_jit::JITModule::new(builder))
}

/// Run `f` against this worker thread's JIT module (lazy init).
pub fn with_jit_module<R>(
    f: impl FnOnce(&mut cranelift_jit::JITModule) -> Result<R, CompileError>,
) -> Result<R, CompileError> {
    TLS_MODULE.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(create_module()?);
        }
        let module = slot.as_mut().ok_or_else(|| {
            CompileError::Backend("thread-local JIT module failed to initialize".into())
        })?;
        f(module)
    })
}
