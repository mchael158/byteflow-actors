//! Reference host I/O for remote Atomic Hop frames — **not** a cluster protocol.
//!
//! Byteflow already defines [`encode_frame`] / [`decode_frame`] and
//! [`Runtime::deliver_frame`]. This module adds a copy-friendly **length-prefixed
//! wire envelope** and small TCP/UDP helpers with **bounded outbound queues**
//! so a slow socket applies backpressure to [`DistributionBridge`] drains.
//!
//! The embedder still owns routing (`OutboundHop::node`), TLS, auth, and
//! retries. See [`crate::docs::remote_transport`].

pub mod router;

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::encode_frame;
use crate::scheduler::{DistributionBridge, RemoteFrameError};

fn transport_info(msg: impl std::fmt::Display) {
    crate::log::info(format!("transport: {msg}"));
}

fn transport_debug(msg: impl std::fmt::Display) {
    crate::log::debug(format!("transport: {msg}"));
}

/// Default cap on one length-prefixed payload (typical mailbox byte budget).
pub const DEFAULT_MAX_WIRE_BYTES: u32 = 4 * 1024 * 1024;

const WIRE_MAGIC: [u8; 4] = *b"BFT1";
const WIRE_VERSION: u8 = 1;

/// Why host transport refused or failed an operation.
#[derive(Debug)]
pub enum TransportError {
    Io(std::io::Error),
    Truncated,
    BadMagic,
    BadVersion,
    FrameTooLarge { len: u32, max: u32 },
    TrailingData,
    InvalidConfig(&'static str),
    Deliver(RemoteFrameError),
    ThreadJoin,
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "transport io: {e}"),
            Self::Truncated => f.write_str("transport frame truncated"),
            Self::BadMagic => f.write_str("transport bad magic"),
            Self::BadVersion => f.write_str("transport bad version"),
            Self::FrameTooLarge { len, max } => {
                write!(f, "transport frame {len} bytes exceeds max {max}")
            }
            Self::TrailingData => f.write_str("transport frame trailing data"),
            Self::InvalidConfig(msg) => write!(f, "transport config: {msg}"),
            Self::Deliver(e) => write!(f, "deliver_frame: {e}"),
            Self::ThreadJoin => f.write_str("transport thread panicked"),
        }
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Deliver(e) => Some(e),
            _ => None,
        }
    }
}

fn check_max_wire(max_wire_bytes: u32) -> Result<(), TransportError> {
    if max_wire_bytes == 0 {
        return Err(TransportError::InvalidConfig(
            "max_wire_bytes must be at least 1",
        ));
    }
    Ok(())
}

impl From<std::io::Error> for TransportError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Configuration for [`spawn_tcp_outbound`].
#[derive(Clone, Debug)]
pub struct TcpOutboundConfig {
    /// Bounded queue between bridge drain and the TCP writer (backpressure).
    pub queue_capacity: usize,
    pub max_wire_bytes: u32,
    /// How long [`DistributionBridge::recv_timeout`] waits when the queue is empty.
    pub bridge_poll: Duration,
    pub connect_timeout: Duration,
}

impl Default for TcpOutboundConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 256,
            max_wire_bytes: DEFAULT_MAX_WIRE_BYTES,
            bridge_poll: Duration::from_millis(50),
            connect_timeout: Duration::from_secs(5),
        }
    }
}

/// Stops [`spawn_tcp_outbound`] and joins its worker threads.
pub struct TcpOutboundHandle {
    stop: Arc<AtomicBool>,
    joins: Vec<JoinHandle<()>>,
    wire_pending: Arc<AtomicUsize>,
}

impl TcpOutboundHandle {
    /// Frames queued for the TCP writer but not yet written.
    pub fn wire_pending(&self) -> usize {
        self.wire_pending.load(Ordering::Acquire)
    }

    /// Poll until [`Self::wire_pending`] is zero or `timeout` elapses.
    pub fn wait_wire_drained(&self, timeout: Duration) -> Result<(), TransportError> {
        let deadline = Instant::now() + timeout;
        while self.wire_pending.load(Ordering::Acquire) > 0 {
            if Instant::now() >= deadline {
                return Err(TransportError::InvalidConfig("wire queue drain timed out"));
            }
            thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    pub fn stop(self) -> Result<(), TransportError> {
        self.stop.store(true, Ordering::Relaxed);
        for join in self.joins {
            join.join().map_err(|_| TransportError::ThreadJoin)?;
        }
        Ok(())
    }
}

const SESSION_DRAIN: Duration = Duration::from_secs(5);

/// Drains `bridge`, encodes each [`crate::OutboundHop`], and writes length-prefixed
/// frames to `peer`. When the outbound queue is full, the pump **blocks**, so
/// [`DistributionBridge`] fills and bytecode `SendRemote` eventually fails closed.
pub fn spawn_tcp_outbound(
    bridge: Arc<DistributionBridge>,
    peer: SocketAddr,
    config: TcpOutboundConfig,
) -> Result<TcpOutboundHandle, TransportError> {
    check_max_wire(config.max_wire_bytes)?;
    if config.queue_capacity == 0 {
        return Err(TransportError::InvalidConfig(
            "queue_capacity must be at least 1",
        ));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let stop_writer = Arc::clone(&stop);
    let stop_pump = Arc::clone(&stop);
    let (wire_tx, wire_rx) = sync_channel(config.queue_capacity);
    let wire_pending = Arc::new(AtomicUsize::new(0));
    let pending_pump = Arc::clone(&wire_pending);
    let pending_writer = Arc::clone(&wire_pending);
    let max_wire = config.max_wire_bytes;
    let connect_timeout = config.connect_timeout;

    let writer = thread::Builder::new()
        .name("byteflow-tcp-out".into())
        .spawn(move || {
            let _ = run_tcp_writer(
                peer,
                connect_timeout,
                max_wire,
                wire_rx,
                stop_writer,
                pending_writer,
            );
        })
        .map_err(TransportError::Io)?;

    let bridge_poll = config.bridge_poll;
    let pump = thread::Builder::new()
        .name("byteflow-bridge-pump".into())
        .spawn(move || {
            pump_bridge_to_channel(&bridge, wire_tx, bridge_poll, stop_pump, pending_pump);
        })
        .map_err(TransportError::Io)?;

    Ok(TcpOutboundHandle {
        stop,
        joins: vec![pump, writer],
        wire_pending,
    })
}

fn pump_bridge_to_channel(
    bridge: &DistributionBridge,
    wire_tx: SyncSender<Vec<u8>>,
    poll: Duration,
    stop: Arc<AtomicBool>,
    wire_pending: Arc<AtomicUsize>,
) {
    while !stop.load(Ordering::Relaxed) {
        match bridge.recv_timeout(poll) {
            Ok(Some(hop)) => match encode_frame(&hop) {
                Ok(bytes) => match wire_tx.send(bytes) {
                    Ok(()) => {
                        wire_pending.fetch_add(1, Ordering::Release);
                    }
                    Err(_) => break,
                },
                Err(e) => {
                    transport_info(format!("encode_frame failed; stopping outbound pump: {e}"));
                    break;
                }
            },
            Ok(None) => {}
            Err(e) => {
                transport_info(format!("bridge recv failed: {e}"));
                break;
            }
        }
    }
}

fn run_tcp_writer(
    peer: SocketAddr,
    connect_timeout: Duration,
    max_wire: u32,
    wire_rx: std::sync::mpsc::Receiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
    wire_pending: Arc<AtomicUsize>,
) -> Result<(), TransportError> {
    let mut retry_frame: Option<Vec<u8>> = None;
    while !stop.load(Ordering::Relaxed) {
        let mut stream = match TcpStream::connect_timeout(&peer, connect_timeout) {
            Ok(s) => s,
            Err(e) => {
                transport_debug(format!("connect {peer} failed: {e}"));
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        if stream.set_nodelay(true).is_err() {
            continue;
        }
        while !stop.load(Ordering::Relaxed) {
            let bytes = if let Some(frame) = retry_frame.take() {
                frame
            } else {
                match wire_rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(b) => b,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                }
            };
            if let Err(e) = write_wire_frame(&mut stream, &bytes, max_wire) {
                transport_info(format!("write to {peer} failed: {e}"));
                retry_frame = Some(bytes);
                break;
            }
            wire_pending.fetch_sub(1, Ordering::Release);
        }
    }
    Ok(())
}

/// Poll until [`DistributionBridge::try_recv`] returns `None` or `timeout` elapses.
///
/// Use after the sending runtime has finished bytecode work so the outbound
/// pump can encode and queue wire frames before [`TcpOutboundHandle::stop`].
pub fn wait_bridge_drained(
    bridge: &DistributionBridge,
    timeout: Duration,
) -> Result<(), TransportError> {
    let deadline = Instant::now() + timeout;
    loop {
        match bridge.try_recv() {
            Ok(None) => return Ok(()),
            Ok(Some(_)) => {}
            Err(_) => {
                return Err(TransportError::InvalidConfig(
                    "distribution bridge unavailable",
                ));
            }
        }
        if Instant::now() >= deadline {
            return Err(TransportError::InvalidConfig("bridge drain timed out"));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Configuration for [`spawn_tcp_inbound`].
#[derive(Clone, Debug)]
pub struct TcpInboundConfig {
    pub max_wire_bytes: u32,
    /// Concurrent TCP session threads. `0` means no limit.
    pub max_sessions: usize,
}

impl Default for TcpInboundConfig {
    fn default() -> Self {
        Self {
            max_wire_bytes: DEFAULT_MAX_WIRE_BYTES,
            max_sessions: 64,
        }
    }
}

/// Accepts TCP connections and delivers each wire frame through `deliver`.
pub struct TcpInboundHandle {
    stop: Arc<AtomicBool>,
    join: JoinHandle<()>,
    active_sessions: Arc<AtomicUsize>,
}

impl TcpInboundHandle {
    pub fn stop(self) -> Result<(), TransportError> {
        self.stop.store(true, Ordering::Relaxed);
        self.join.join().map_err(|_| TransportError::ThreadJoin)?;
        let deadline = Instant::now() + SESSION_DRAIN;
        while self.active_sessions.load(Ordering::Acquire) > 0 {
            if Instant::now() >= deadline {
                return Err(TransportError::InvalidConfig(
                    "inbound sessions did not finish",
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
}

pub fn spawn_tcp_inbound<F>(
    listener: TcpListener,
    deliver: Arc<F>,
    config: TcpInboundConfig,
) -> Result<TcpInboundHandle, TransportError>
where
    F: Fn(&[u8]) -> Result<(), RemoteFrameError> + Send + Sync + 'static,
{
    check_max_wire(config.max_wire_bytes)?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_accept = Arc::clone(&stop);
    let active_sessions = Arc::new(AtomicUsize::new(0));
    let sessions_accept = Arc::clone(&active_sessions);
    let max_wire = config.max_wire_bytes;
    let max_sessions = config.max_sessions;
    let join = thread::Builder::new()
        .name("byteflow-tcp-in".into())
        .spawn(move || {
            accept_inbound_loop(
                listener,
                deliver,
                max_wire,
                max_sessions,
                stop_accept,
                sessions_accept,
            );
        })
        .map_err(TransportError::Io)?;
    Ok(TcpInboundHandle {
        stop,
        join,
        active_sessions,
    })
}

fn accept_inbound_loop<F>(
    listener: TcpListener,
    deliver: Arc<F>,
    max_wire: u32,
    max_sessions: usize,
    stop: Arc<AtomicBool>,
    active_sessions: Arc<AtomicUsize>,
) where
    F: Fn(&[u8]) -> Result<(), RemoteFrameError> + Send + Sync + 'static,
{
    let _ = listener.set_nonblocking(true);
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, peer)) => {
                let deliver = Arc::clone(&deliver);
                let sessions = Arc::clone(&active_sessions);
                let Ok(guard) = SessionGuard::try_start(sessions, max_sessions) else {
                    transport_info(format!(
                        "inbound session limit ({max_sessions}) reached; refusing {peer}"
                    ));
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                };
                if thread::Builder::new()
                    .name("byteflow-tcp-session".into())
                    .spawn(move || {
                        let _guard = guard;
                        if let Err(e) = serve_tcp_connection(stream, &*deliver, max_wire) {
                            transport_info(format!("session from {peer} ended: {e}"));
                        }
                    })
                    .is_err()
                {
                    transport_info("inbound session thread spawn failed");
                    continue;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

struct SessionGuard(Arc<AtomicUsize>);

impl SessionGuard {
    fn try_start(active: Arc<AtomicUsize>, max_sessions: usize) -> Result<Self, ()> {
        if max_sessions == 0 {
            active.fetch_add(1, Ordering::Relaxed);
            return Ok(Self(active));
        }
        loop {
            let current = active.load(Ordering::Acquire);
            if current >= max_sessions {
                return Err(());
            }
            if active
                .compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(Self(active));
            }
        }
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}

fn serve_tcp_connection<F>(
    mut stream: TcpStream,
    deliver: &F,
    max_wire: u32,
) -> Result<(), TransportError>
where
    F: Fn(&[u8]) -> Result<(), RemoteFrameError>,
{
    loop {
        let frame = match read_wire_frame(&mut stream, max_wire) {
            Ok(f) => f,
            Err(TransportError::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
                ) =>
            {
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        if let Err(e) = deliver(&frame) {
            transport_info(format!("deliver_frame failed: {e}"));
            return Err(TransportError::Deliver(e));
        }
    }
}

/// One datagram carries exactly one wire frame (`BFT1` header + BFH1 body).
pub fn send_wire_frame_udp(
    sock: &UdpSocket,
    addr: SocketAddr,
    bfh1_payload: &[u8],
    max_wire_bytes: u32,
) -> Result<(), TransportError> {
    let wire = encode_wire_bytes(bfh1_payload, max_wire_bytes)?;
    sock.send_to(&wire, addr)?;
    Ok(())
}

/// Reads one datagram wire frame. Returns the inner BFH1 bytes and the sender.
pub fn recv_wire_frame_udp(
    sock: &UdpSocket,
    buf: &mut [u8],
    max_wire_bytes: u32,
) -> Result<(Vec<u8>, SocketAddr), TransportError> {
    let (n, from) = sock.recv_from(buf)?;
    decode_wire_bytes(&buf[..n], max_wire_bytes).map(|inner| (inner.to_vec(), from))
}

/// Write `BFT1 | u8 ver | u32 le len | payload` where `payload` is an [`encode_frame`] blob.
pub fn write_wire_frame(
    writer: &mut impl Write,
    bfh1_payload: &[u8],
    max_wire_bytes: u32,
) -> Result<(), TransportError> {
    let wire = encode_wire_bytes(bfh1_payload, max_wire_bytes)?;
    writer.write_all(&wire)?;
    Ok(())
}

/// Read one length-prefixed frame and return the inner BFH1 bytes.
pub fn read_wire_frame(
    reader: &mut impl Read,
    max_wire_bytes: u32,
) -> Result<Vec<u8>, TransportError> {
    let mut header = [0u8; 9];
    read_exact(reader, &mut header)?;
    let len = wire_payload_len(&header, max_wire_bytes)?;
    if len == 0 {
        return Err(TransportError::InvalidConfig("empty remote hop payload"));
    }
    let mut body = vec![0u8; len as usize];
    read_exact(reader, &mut body)?;
    Ok(body)
}

pub use router::{resolve_hop_peer, NodeRouter, StaticNodeRouter};

fn encode_wire_bytes(bfh1_payload: &[u8], max_wire_bytes: u32) -> Result<Vec<u8>, TransportError> {
    let len = u32::try_from(bfh1_payload.len()).map_err(|_| TransportError::FrameTooLarge {
        len: u32::MAX,
        max: max_wire_bytes,
    })?;
    if len > max_wire_bytes {
        return Err(TransportError::FrameTooLarge {
            len,
            max: max_wire_bytes,
        });
    }
    let mut out = Vec::with_capacity(9 + bfh1_payload.len());
    out.extend_from_slice(&WIRE_MAGIC);
    out.push(WIRE_VERSION);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(bfh1_payload);
    Ok(out)
}

fn decode_wire_bytes(wire: &[u8], max_wire_bytes: u32) -> Result<&[u8], TransportError> {
    let len = wire_payload_len(wire, max_wire_bytes)?;
    let body_start = 9usize;
    let body_end = body_start
        .checked_add(len as usize)
        .ok_or(TransportError::Truncated)?;
    if body_end > wire.len() {
        return Err(TransportError::Truncated);
    }
    if body_end < wire.len() {
        return Err(TransportError::TrailingData);
    }
    Ok(&wire[body_start..body_end])
}

fn wire_payload_len(header: &[u8], max_wire_bytes: u32) -> Result<u32, TransportError> {
    if header.len() < 9 {
        return Err(TransportError::Truncated);
    }
    if header[..4] != WIRE_MAGIC {
        return Err(TransportError::BadMagic);
    }
    if header[4] != WIRE_VERSION {
        return Err(TransportError::BadVersion);
    }
    let len = u32::from_le_bytes([header[5], header[6], header[7], header[8]]);
    if len > max_wire_bytes {
        return Err(TransportError::FrameTooLarge {
            len,
            max: max_wire_bytes,
        });
    }
    Ok(len)
}

fn read_exact(reader: &mut impl Read, buf: &mut [u8]) -> Result<(), TransportError> {
    reader.read_exact(buf)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn wire_round_trips_bfh1_payload() -> Result<(), TransportError> {
        let inner = b"BFH1-payload-bytes".to_vec();
        let mut buf = Vec::new();
        write_wire_frame(&mut buf, &inner, DEFAULT_MAX_WIRE_BYTES)?;
        let mut cursor = Cursor::new(buf);
        let back = read_wire_frame(&mut cursor, DEFAULT_MAX_WIRE_BYTES)?;
        if back != inner {
            return Err(TransportError::InvalidConfig("round trip mismatch"));
        }
        Ok(())
    }

    #[test]
    fn wire_rejects_oversized_length_prefix() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&WIRE_MAGIC);
        wire.push(WIRE_VERSION);
        wire.extend_from_slice(&(DEFAULT_MAX_WIRE_BYTES + 1).to_le_bytes());
        assert!(matches!(
            decode_wire_bytes(&wire, DEFAULT_MAX_WIRE_BYTES),
            Err(TransportError::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn wire_rejects_trailing_bytes() -> Result<(), TransportError> {
        let inner = b"hop";
        let mut wire = encode_wire_bytes(inner, DEFAULT_MAX_WIRE_BYTES)?;
        wire.push(0);
        assert!(matches!(
            decode_wire_bytes(&wire, DEFAULT_MAX_WIRE_BYTES),
            Err(TransportError::TrailingData)
        ));
        Ok(())
    }

    #[test]
    fn session_limit_rejects_when_full() {
        let active = Arc::new(AtomicUsize::new(0));
        let first = match SessionGuard::try_start(Arc::clone(&active), 1) {
            Ok(guard) => guard,
            Err(()) => panic!("first slot"),
        };
        assert!(SessionGuard::try_start(active, 1).is_err());
        drop(first);
    }
}
