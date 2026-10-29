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
