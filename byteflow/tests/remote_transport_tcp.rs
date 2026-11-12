//! TCP wire transport helpers (integration with the library).

use std::time::Duration;

#[test]
fn wire_envelope_round_trip() -> Result<(), byteflow::TransportError> {
    use std::io::Cursor;

    let inner = b"BFH1-bytes".as_slice();
    let mut buf = Vec::new();
    byteflow::write_wire_frame(&mut buf, inner, byteflow::DEFAULT_MAX_WIRE_BYTES)?;
    let mut cursor = Cursor::new(buf);
    let back = byteflow::read_wire_frame(&mut cursor, byteflow::DEFAULT_MAX_WIRE_BYTES)?;
    if back.as_slice() != inner {
        return Err(byteflow::TransportError::InvalidConfig("wire mismatch"));
    }
    Ok(())
}

#[test]
fn inbound_stops_without_blocking_on_accept() -> Result<(), String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let deliver =
        std::sync::Arc::new(|_bytes: &[u8]| -> Result<(), byteflow::RemoteFrameError> { Ok(()) });
    let inbound =
        byteflow::spawn_tcp_inbound(listener, deliver, byteflow::TcpInboundConfig::default())
            .map_err(|e| e.to_string())?;
    std::thread::sleep(Duration::from_millis(50));
    inbound.stop().map_err(|e| e.to_string())?;
    Ok(())
}

#[test]
fn inbound_stop_cancels_idle_session() -> Result<(), String> {
    use std::io::Write;
    use std::net::TcpStream;
    use std::time::Instant;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let deliver =
        std::sync::Arc::new(|_bytes: &[u8]| -> Result<(), byteflow::RemoteFrameError> { Ok(()) });
    let inbound = byteflow::spawn_tcp_inbound(
        listener,
        deliver,
        byteflow::TcpInboundConfig {
            // Keep a finite timeout, but stop must not wait the full window.
            read_timeout: Some(Duration::from_secs(60)),
            write_timeout: Some(Duration::from_secs(60)),
            ..byteflow::TcpInboundConfig::default()
        },
    )
    .map_err(|e| e.to_string())?;

    let mut peer = TcpStream::connect(addr).map_err(|e| e.to_string())?;
    // Half-open: leave the session blocked in read_exact awaiting a frame.
    peer.write_all(b"BF").map_err(|e| e.to_string())?;
    std::thread::sleep(Duration::from_millis(50));

    let started = Instant::now();
    inbound.stop().map_err(|e| e.to_string())?;
    if started.elapsed() > Duration::from_secs(2) {
        return Err(format!(
            "stop took {:?}; expected socket cancel to unblock quickly",
            started.elapsed()
        ));
    }
    Ok(())
}
