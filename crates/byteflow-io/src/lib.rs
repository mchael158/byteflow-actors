#![deny(unsafe_code)]

//! Host-side I/O integration for the Byteflow runtime.
//!
//! # Role
//!
//! This crate does **not** extend the Byteflow ISA with I/O opcodes. It
//! provides:
//!
//! - bounded host command queues for fast `CallNative` adapters;
//! - a dedicated [`HostAwaitBridge`] worker pool for slow external work;
//! - helpers that inject inbound events as Atomic Hop [`byteflow::Value::Message`] values;
//! - generic [`crate::fleet`] site registry + optional [`laundry`] washer profile;
//! - typed driver errors via [`IoErrorKind`] / [`IoDriverError`].
//!
//! Concrete MQTT, UART, GPIO, and payment clients remain in the host
//! application. Implement [`IoDriver`] so that [`IoDriver::try_submit`] is
//! bounded and non-blocking.
//!
//! # Dependency direction
//!
//! ```text
//! byteflow-io ──► byteflow
//! ```
//!
//! The core never depends on this crate.
//!
//! # Shutdown
//!
//! 1. Stop external callbacks and host-owned device consumer loops.
//! 2. Call [`byteflow::Runtime::shutdown`].
//! 3. Call [`ByteflowIo::shutdown`].

mod bridge;
mod driver;
mod error;
mod events;
pub mod fleet;
pub mod laundry;
mod metrics;
mod native_table;
mod queue;
pub mod samples;

pub use bridge::{CancellationToken, WorkerHostAwaitBridge};
pub use driver::{IoCommand, IoDriver};
pub use error::{IoDriverError, IoErrorKind, IoInitError, NativeTableBuildError};
pub use events::EventRouter;
pub use metrics::{IoMetrics, IoMetricsSnapshot};
pub use native_table::{build_native_table, native_index};
pub use queue::{HostAwaitHandler, IoCommandReceiver, QueuedCommand, QueuedIoDriver};

use std::sync::Arc;

use byteflow::{HostAwaitBridge, OutputSink, RuntimeConfig};

/// Default process-wide HostAwait ticket limit applied by [`IoConfig::default`].
pub const DEFAULT_MAX_HOST_AWAITS: u32 = 128;

/// Configuration for the host-owned I/O bridge.
#[derive(Clone, Debug)]
pub struct IoConfig {
    /// Number of dedicated host I/O worker threads (not Byteflow workers).
    ///
    /// Workers share an MPMC bounded queue; values greater than `1` run true
    /// parallel HostAwait execution.
    pub worker_threads: usize,
    /// Maximum number of HostAwait jobs waiting for a free worker.
    pub queue_capacity: usize,
    /// Process-wide outstanding HostAwait ticket limit forwarded to Byteflow
    /// (`0` = unlimited).
    pub max_host_awaits: u32,
    /// Thread-name prefix for host I/O workers (`{prefix}-{index}`).
    pub thread_name: String,
}

impl Default for IoConfig {
    fn default() -> Self {
        Self {
            worker_threads: 2,
            queue_capacity: 128,
            max_host_awaits: DEFAULT_MAX_HOST_AWAITS,
            thread_name: "byteflow-io".to_owned(),
        }
    }
}

/// Host integration handle. Does not own the Byteflow [`byteflow::Runtime`].
///
/// Keep this value alive for as long as the runtime may execute `HostAwait`.
/// Preferred shutdown sequence: stop external driver input, call
/// `Runtime::shutdown()`, then [`ByteflowIo::shutdown`].
pub struct ByteflowIo {
    driver: Arc<dyn IoDriver>,
    bridge: Arc<WorkerHostAwaitBridge>,
    config: IoConfig,
}

impl std::fmt::Debug for ByteflowIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ByteflowIo")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl ByteflowIo {
    /// Create the host I/O bridge and its bounded worker pool.
    ///
    /// # Errors
    ///
    /// Returns [`IoInitError`] when `worker_threads` or `queue_capacity` is
    /// zero, or when a worker thread cannot be spawned.
    pub fn new(driver: Arc<dyn IoDriver>, config: IoConfig) -> Result<Self, IoInitError> {
        let bridge = Arc::new(WorkerHostAwaitBridge::new(
            Arc::clone(&driver),
            config.worker_threads,
            config.queue_capacity,
            config.thread_name.clone(),
        )?);

        Ok(Self {
            driver,
            bridge,
            config,
        })
    }

    /// Return the bridge as Byteflow’s [`HostAwaitBridge`] trait object.
    pub fn host_await_bridge(&self) -> Arc<dyn HostAwaitBridge> {
        self.bridge.clone()
    }

    /// Shared HostAwait bridge metrics (submit / full / settle).
    pub fn metrics(&self) -> &Arc<IoMetrics> {
        self.bridge.metrics()
    }

    /// Build a [`RuntimeConfig`] wired to this bridge and the given output sink.
    ///
    /// Pass the same sink to [`build_native_table`] so the standard `print`
    /// native and the runtime share one destination.
    pub fn runtime_config(&self, output: Arc<dyn OutputSink>) -> RuntimeConfig {
        RuntimeConfig {
            host_await: Some(self.host_await_bridge()),
            max_host_awaits: self.config.max_host_awaits,
            output,
            ..RuntimeConfig::default()
        }
    }

    /// Shared driver used by both fast natives and HostAwait workers.
    pub fn driver(&self) -> &Arc<dyn IoDriver> {
        &self.driver
    }

    /// Refuse new bridge work, drain accepted jobs, join workers, then shut
    /// down the driver.
    ///
    /// Call only after [`byteflow::Runtime::shutdown`] has completed. The
    /// driver is shared with native functions and must not be stopped earlier.
    ///
    /// # Errors
    ///
    /// Propagates [`IoDriverError`] from [`IoDriver::shutdown`].
    pub fn shutdown(&self) -> Result<(), IoDriverError> {
        self.bridge.shutdown();
        self.driver.shutdown()
    }
}

impl Drop for ByteflowIo {
    fn drop(&mut self) {
        // Best-effort safety net. Prefer an explicit `shutdown` so errors and
        // runtime/driver ordering remain observable.
        self.bridge.shutdown();
        let _ = self.driver.shutdown();
    }
}
