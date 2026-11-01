//! Error types for bridge initialization, native-table composition, and drivers.

use std::fmt;

use byteflow::NativeTableError;

/// Stable classification for [`IoDriverError`] (hosts / tests can `match`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IoErrorKind {
    /// Cooperative cancellation via [`crate::CancellationToken`].
    Cancelled,
    /// Host or transport deadline elapsed.
    DeadlineExceeded,
    /// Bounded command queue rejected a submit.
    QueueFull,
    /// Queue / driver already shut down.
    QueueClosed,
    /// Arguments failed validation / parsing.
    InvalidArgs,
    /// Operation is not handled by this driver.
    Unsupported,
    /// HostAwait (or similar) path not implemented.
    NotImplemented,
    /// Internal mutex poisoned.
    Poisoned,
    /// Monotonic ID space exhausted.
    Exhausted,
    /// Catch-all with a free-form message.
    Other,
}

impl fmt::Display for IoErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "cancelled",
            Self::DeadlineExceeded => "deadline exceeded",
            Self::QueueFull => "queue full",
            Self::QueueClosed => "queue closed",
            Self::InvalidArgs => "invalid arguments",
            Self::Unsupported => "unsupported",
            Self::NotImplemented => "not implemented",
            Self::Poisoned => "poisoned",
            Self::Exhausted => "exhausted",
            Self::Other => "error",
        })
    }
}

/// Error returned by a host I/O adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IoDriverError {
    kind: IoErrorKind,
    message: String,
}

impl IoDriverError {
    /// Create an error with an explicit kind and human-readable message.
    pub fn new(kind: IoErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Borrow the stable kind.
    pub fn kind(&self) -> IoErrorKind {
        self.kind
    }

    /// Borrow the error message.
    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn cancelled(message: impl Into<String>) -> Self {
        Self::new(IoErrorKind::Cancelled, message)
    }

    pub fn deadline(message: impl Into<String>) -> Self {
        Self::new(IoErrorKind::DeadlineExceeded, message)
    }

    pub fn full(message: impl Into<String>) -> Self {
        Self::new(IoErrorKind::QueueFull, message)
    }

    pub fn closed(message: impl Into<String>) -> Self {
        Self::new(IoErrorKind::QueueClosed, message)
    }

    pub fn invalid_args(message: impl Into<String>) -> Self {
        Self::new(IoErrorKind::InvalidArgs, message)
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(IoErrorKind::Unsupported, message)
    }

    pub fn not_implemented(message: impl Into<String>) -> Self {
        Self::new(IoErrorKind::NotImplemented, message)
    }

    pub fn poisoned(message: impl Into<String>) -> Self {
        Self::new(IoErrorKind::Poisoned, message)
    }

    pub fn exhausted(message: impl Into<String>) -> Self {
        Self::new(IoErrorKind::Exhausted, message)
    }

    pub fn other(message: impl Into<String>) -> Self {
        Self::new(IoErrorKind::Other, message)
    }
}

impl fmt::Display for IoDriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for IoDriverError {}

/// Failure while creating the host I/O worker pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IoInitError {
    /// `worker_threads` was zero.
    ZeroWorkers,
    /// `queue_capacity` was zero.
    ZeroQueueCapacity,
    /// An OS thread could not be spawned.
    ThreadSpawn(String),
}

impl fmt::Display for IoInitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroWorkers => write!(f, "byteflow-io requires at least one worker thread"),
            Self::ZeroQueueCapacity => {
                write!(f, "byteflow-io queue capacity must be greater than zero")
            }
            Self::ThreadSpawn(message) => {
                write!(f, "failed to start byteflow-io worker: {message}")
            }
        }
    }
}

impl std::error::Error for IoInitError {}

/// Failure while composing Byteflow’s standard natives with I/O slots.
#[derive(Debug)]
pub enum NativeTableBuildError {
    /// A required standard native was missing from the core table.
    MissingStandardNative { index: u32, name: &'static str },
    /// [`byteflow::NativeTableBuilder`] rejected a registration.
    NativeTable(NativeTableError),
}

impl fmt::Display for NativeTableBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingStandardNative { index, name } => {
                write!(
                    f,
                    "Byteflow standard native {name} is missing at slot {index}"
                )
            }
            Self::NativeTable(error) => write!(f, "failed to build native table: {error}"),
        }
    }
}

impl std::error::Error for NativeTableBuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::MissingStandardNative { .. } => None,
            Self::NativeTable(error) => Some(error),
        }
    }
}

impl From<NativeTableError> for NativeTableBuildError {
    fn from(value: NativeTableError) -> Self {
        Self::NativeTable(value)
    }
}
