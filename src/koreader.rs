use log::{debug, warn};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::time::Duration;

const PORT: u16 = 8080;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
const WRITE_TIMEOUT: Duration = Duration::from_millis(300);
const READ_TIMEOUT: Duration = Duration::from_millis(300);

/// Send an event to KOReader's HTTP Inspector, e.g. `GotoViewRel/1`. False
/// means KOReader did not take it, so the caller can try the native reader.
/// The inspector closes the connection after every response, so there is
/// nothing to keep alive between calls.
pub fn send_event(event: &str) -> bool {
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, PORT));
    let mut stream = match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
        Ok(s) => s,
        Err(e) => {
            debug!("KOReader not reachable on port {}: {}", PORT, e);
            return false;
        }
    };
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));

    let req = format!(
        "GET /koreader/event/{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        event, PORT
    );
    if let Err(e) = stream.write_all(req.as_bytes()) {
        debug!("KOReader event '{}' not sent: {}", event, e);
        return false;
    }

    // KOReader only polls the socket on its 50ms UI tick, so the reply lags.
    // The request is delivered either way, so a timeout still counts as taken.
    let mut buf = [0u8; 64];
    match stream.read(&mut buf) {
        Ok(0) => debug!("KOReader closed without a reply to '{}'", event),
        Ok(n) => match status_code(&buf[..n]) {
            Some(code) if (200..300).contains(&code) => debug!("KOReader {} -> {}", event, code),
            Some(code) => warn!("KOReader rejected event '{}' with HTTP {}", event, code),
            None => warn!("KOReader sent no HTTP status for event '{}'", event),
        },
        Err(e) => debug!("No reply from KOReader for '{}': {}", event, e),
    }
    true
}

fn status_code(response: &[u8]) -> Option<u16> {
    let head = std::str::from_utf8(response).ok()?;
    head.split_whitespace().nth(1)?.parse().ok()
}
