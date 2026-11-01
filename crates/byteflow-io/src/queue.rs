//! Bounded command queue implementing [`IoDriver`] for the fast path.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{
    sync_channel, Receiver, RecvError, RecvTimeoutError, SyncSender, TryRecvError, TrySendError,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use byteflow::{HostAwaitOp, Value};

use crate::metrics::IoMetrics;
use crate::{CancellationToken, IoCommand, IoDriver, IoDriverError};

/// Fast-path command paired with the host-side ID returned to bytecode.
#[derive(Clone, Debug, PartialEq)]
pub struct QueuedCommand {
    /// Identifier previously returned by [`IoDriver::try_submit`].
    ///
    /// IDs are monotonically increasing and start at `1`. Treat them as opaque
    /// correlation tokens.
    pub id: u64,
    /// Command payload to execute on a host-owned device thread.
    pub command: IoCommand,
}

/// Host-specific executor for slow operations reached through `HostAwait`.
///
/// Keeps provider SDKs out of the integration crate while allowing
/// [`QueuedIoDriver`] to delegate the slow path.
pub trait HostAwaitHandler: Send + Sync + std::fmt::Debug + 'static {
    /// Run one HostAwait operation to completion or cooperative cancellation.
    fn execute(
        &self,
        op: HostAwaitOp,
        args: Value,
        cancel: &CancellationToken,
    ) -> Result<Value, IoDriverError>;

    /// Release resources owned by the slow-path handler.
    fn shutdown(&self) -> Result<(), IoDriverError> {
        Ok(())
    }
}

/// Bounded command receiver owned by a host device loop.
pub struct IoCommandReceiver {
    receiver: Mutex<Receiver<QueuedCommand>>,
}

impl std::fmt::Debug for IoCommandReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IoCommandReceiver").finish_non_exhaustive()
    }
}

impl IoCommandReceiver {
    /// Block until the next command arrives.
    ///
    /// Intended for a host-owned driver thread — never a Byteflow worker.
    /// Prefer [`recv_timeout`](Self::recv_timeout) when shutdown must remain
    /// interruptible.
    pub fn recv(&self) -> Result<QueuedCommand, RecvError> {
        match self.receiver.lock() {
            Ok(receiver) => receiver.recv(),
            Err(_) => Err(RecvError),
        }
    }

    /// Wait up to `timeout` for the next command on a host-owned driver thread.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<QueuedCommand, RecvTimeoutError> {
        match self.receiver.lock() {
            Ok(receiver) => receiver.recv_timeout(timeout),
            Err(_) => Err(RecvTimeoutError::Disconnected),
        }
    }

    /// Non-blocking poll for a host-owned driver loop.
    pub fn try_recv(&self) -> Result<QueuedCommand, TryRecvError> {
        match self.receiver.lock() {
            Ok(receiver) => receiver.try_recv(),
            Err(_) => Err(TryRecvError::Disconnected),
        }
    }
}

/// Ready-to-use bounded [`IoDriver`] for the fast command path.
///
/// Provides a non-blocking enqueue API. The application consumes
/// [`IoCommandReceiver`] on its own MQTT/UART/GPIO thread and performs the
/// concrete device operation there. Slow request/response work is delegated to
/// [`HostAwaitHandler`].
///
/// [`QueuedIoDriver::shutdown`] rejects new submissions by taking the sender
/// (fail-closed). It does not join the application’s device consumer thread.
///
/// # Command IDs
///
/// Returned IDs increase monotonically from `1` without skipping on queue-full
/// rejects.
pub struct QueuedIoDriver {
    sender: Mutex<Option<SyncSender<QueuedCommand>>>,
    next_id: AtomicU64,
    closed: AtomicBool,
    host_await: Arc<dyn HostAwaitHandler>,
    metrics: Arc<IoMetrics>,
}

impl std::fmt::Debug for QueuedIoDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueuedIoDriver")
            .field("closed", &self.closed.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl QueuedIoDriver {
    /// Create a bounded driver queue and return its host-side receiver.
    ///
    /// # Errors
    ///
    /// Returns [`IoDriverError`] when `capacity` is zero.
    pub fn new<H: HostAwaitHandler + 'static>(
        capacity: usize,
        host_await: Arc<H>,
    ) -> Result<(Arc<Self>, IoCommandReceiver), IoDriverError> {
        if capacity == 0 {
            return Err(IoDriverError::invalid_args(
                "I/O command queue capacity must be greater than zero",
            ));
        }

        let host_await: Arc<dyn HostAwaitHandler> = host_await;

        let (sender, receiver) = sync_channel(capacity);
        let driver = Arc::new(Self {
            sender: Mutex::new(Some(sender)),
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            host_await,
            metrics: Arc::new(IoMetrics::default()),
        });
        let receiver = IoCommandReceiver {
            receiver: Mutex::new(receiver),
        };
        Ok((driver, receiver))
    }

    /// Shared enqueue / reject counters for this driver.
    pub fn metrics(&self) -> &Arc<IoMetrics> {
        &self.metrics
    }
}

impl IoDriver for QueuedIoDriver {
    fn try_submit(&self, command: IoCommand) -> Result<u64, IoDriverError> {
        if self.closed.load(Ordering::Acquire) {
            self.metrics.record_closed();
            return Err(IoDriverError::closed("I/O command queue is closed"));
        }

        let sender = match self.sender.lock() {
            Ok(guard) => guard.as_ref().cloned(),
            Err(_) => None,
        };
        let Some(sender) = sender else {
            self.metrics.record_closed();
            return Err(IoDriverError::closed("I/O command queue is closed"));
        };

        let id = self.next_id.fetch_add(1, Ordering::AcqRel);
        if id == u64::MAX {
            let _ = self.next_id.fetch_sub(1, Ordering::AcqRel);
            return Err(IoDriverError::exhausted("I/O command ID space exhausted"));
        }

        match sender.try_send(QueuedCommand { id, command }) {
            Ok(()) => {
                self.metrics.record_submitted();
                Ok(id)
            }
            Err(TrySendError::Full(_)) => {
                let _ = self.next_id.fetch_sub(1, Ordering::AcqRel);
                self.metrics.record_full();
                Err(IoDriverError::full("I/O command queue is full"))
            }
            Err(TrySendError::Disconnected(_)) => {
                let _ = self.next_id.fetch_sub(1, Ordering::AcqRel);
                self.metrics.record_closed();
                Err(IoDriverError::closed("I/O command receiver was dropped"))
            }
        }
    }

    fn execute_host_await(
        &self,
        op: HostAwaitOp,
        args: Value,
        cancel: &CancellationToken,
    ) -> Result<Value, IoDriverError> {
        self.host_await.execute(op, args, cancel)
    }

    fn shutdown(&self) -> Result<(), IoDriverError> {
        self.closed.store(true, Ordering::Release);
        // Dropping the last sender closes the channel for the consumer and
        // makes subsequent try_submit fail closed even under races.
        let sender = match self.sender.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        drop(sender);
        self.host_await.shutdown()
    }
}
