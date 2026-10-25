//! Host-side scheduler diagnostics on stderr, gated by `BYTEFLOW_LOG`.
//!
//! # Why this is separate from the `print` native
//!
//! Actor-visible logging (`CallNative print`) writes to **stdout** and is part
//! of the program's observable behaviour — samples and demos use it to show
//! envelopes crossing the mailbox. Scheduler diagnostics (spawn / send /
//! park / finish) are an **operator** concern: they must not pollute stdout
//! when an embedder pipes Flow output, and they must stay off by default
//! so a production run is silent unless asked.
//!
//! ```text
//! BYTEFLOW_LOG unset / 0 / off / false  → silent
//! BYTEFLOW_LOG=1 / info / true          → spawn, send, receive, finish
//! BYTEFLOW_LOG=debug / 2                → also park, deliver queued/handoff
//! ```
//!
//! # Lazy init
//!
//! The level is read from the environment once (first call) and cached in
//! atomics. That avoids re-parsing `std::env` on every send in a hot worker
//! loop. Tests can override via [`set_level_for_tests`].

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const OFF: u8 = 0;
const INFO: u8 = 1;
const DEBUG: u8 = 2;

static LEVEL: AtomicU8 = AtomicU8::new(OFF);
static INIT: AtomicU8 = AtomicU8::new(0);

fn level() -> u8 {
    if INIT.load(Ordering::Acquire) == 0 {
        let parsed = match std::env::var("BYTEFLOW_LOG") {
            Ok(v) => {
                let v = v.trim().to_ascii_lowercase();
                match v.as_str() {
                    "" | "0" | "off" | "false" | "no" => OFF,
                    "debug" | "trace" | "2" => DEBUG,
                    // "1", "info", "true", "yes", or any other non-empty token
                    _ => INFO,
                }
            }
            Err(_) => OFF,
        };
        LEVEL.store(parsed, Ordering::Release);
        INIT.store(1, Ordering::Release);
    }
    LEVEL.load(Ordering::Acquire)
}

fn stamp_ms() -> u64 {
    // Logging must never panic: a broken clock just prints epoch-ish 0.
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as u64,
        Err(_) => 0,
    }
}

/// Override the cached level (tests only). `0` off, `1` info, `2` debug.
pub fn set_level_for_tests(level: u8) {
    LEVEL.store(level.min(DEBUG), Ordering::Release);
    INIT.store(1, Ordering::Release);
}

/// INFO-level line when [`BYTEFLOW_LOG`](crate::log) is at least `info`.
#[inline]
pub fn info(msg: impl std::fmt::Display) {
    if level() >= INFO {
        eprintln!("[{}] byteflow INFO  {}", stamp_ms(), msg);
    }
}

/// DEBUG-level line when [`BYTEFLOW_LOG`](crate::log) is `debug`.
#[inline]
pub fn debug(msg: impl std::fmt::Display) {
    if level() >= DEBUG {
        eprintln!("[{}] byteflow DEBUG {}", stamp_ms(), msg);
    }
}

/// A collector refused or dropped one diagnostic line.
///
/// The runtime ignores this. The flow keeps its original outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogRecordError;

impl std::fmt::Display for LogRecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("log sink rejected the line")
    }
}

impl std::error::Error for LogRecordError {}

/// Receives the diagnostic line that would be written to stderr.
///
/// `try_record` runs on the worker that emitted the event. It must return
/// promptly and must not panic. [`Err`] is ignored: a collector failure does
/// not fail the flow. There is no queue inside the runtime.
pub trait LogSink: Send + Sync + std::fmt::Debug + 'static {
    fn try_record(&self, line: &str) -> Result<(), LogRecordError>;
}

pub(crate) fn info_sink(sink: Option<&std::sync::Arc<dyn LogSink>>, msg: impl std::fmt::Display) {
    forward(sink, INFO, "INFO ", msg);
}

pub(crate) fn debug_sink(sink: Option<&std::sync::Arc<dyn LogSink>>, msg: impl std::fmt::Display) {
    forward(sink, DEBUG, "DEBUG", msg);
}

fn forward(
    sink: Option<&std::sync::Arc<dyn LogSink>>,
    min: u8,
    label: &str,
    msg: impl std::fmt::Display,
) {
    let show = level() >= min;
    if !show && sink.is_none() {
        return;
    }
    let line = format!("[{}] byteflow {label} {msg}", stamp_ms());
    if show {
        eprintln!("{line}");
    }
    if let Some(sink) = sink {
        let _ = sink.try_record(&line);
    }
}
