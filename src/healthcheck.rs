//! `healthcheck` subcommand: probe this instance's own `GET /health` and exit
//! 0 (healthy) or non-zero.
//!
//! It exists so container images can declare a Docker `HEALTHCHECK` without
//! shipping `curl`/`wget`: the probe is the binary itself. Every role that
//! serves HTTP answers `/health` (`standalone`/`orchestrator` from the API
//! router, `worker` from its minimal router), on `[web].port`.

use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

/// Connect + request + response budget. A healthy instance answers in
/// milliseconds; Docker's own `--timeout` bounds the whole command anyway.
const TIMEOUT: Duration = Duration::from_secs(3);

/// Upper bound on the response read: the status line and a few headers are
/// all that's inspected.
const MAX_RESPONSE_BYTES: u64 = 4096;

/// `GET /health` on `127.0.0.1:port`. `Ok` only for an HTTP 200.
pub fn probe(port: u16) -> Result<(), String> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, TIMEOUT)
        .map_err(|err| format!("connect {addr}: {err}"))?;
    stream
        .set_read_timeout(Some(TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(TIMEOUT)))
        .map_err(|err| format!("set timeouts: {err}"))?;

    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .map_err(|err| format!("send request: {err}"))?;

    let mut response = Vec::new();
    stream
        .take(MAX_RESPONSE_BYTES)
        .read_to_end(&mut response)
        .map_err(|err| format!("read response: {err}"))?;

    status_ok(&response)
}

/// Whether a raw HTTP response's status line is a 200.
fn status_ok(response: &[u8]) -> Result<(), String> {
    let text = String::from_utf8_lossy(response);
    let status_line = text.lines().next().unwrap_or_default();
    match status_line.split_whitespace().nth(1) {
        Some("200") => Ok(()),
        Some(code) => Err(format!("unhealthy: HTTP {code}")),
        None => Err(format!("unhealthy: unexpected response {status_line:?}")),
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::thread;

    use super::*;

    #[test]
    fn status_line_parsing() {
        assert!(status_ok(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok").is_ok());
        assert!(status_ok(b"HTTP/1.1 503 Service Unavailable\r\n\r\n").is_err());
        assert!(status_ok(b"").is_err());
        assert!(status_ok(b"garbage").is_err());
    }

    /// One-shot local server that answers the first request with `reply`.
    fn serve_once(reply: &'static [u8]) -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut buf = [0_u8; 1024];
            let _ = conn.read(&mut buf);
            conn.write_all(reply).unwrap();
        });
        port
    }

    #[test]
    fn probe_against_a_live_socket() {
        assert!(
            probe(serve_once(
                b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok"
            ))
            .is_ok()
        );
        assert!(probe(serve_once(b"HTTP/1.1 500 Internal Server Error\r\n\r\n")).is_err());
    }

    #[test]
    fn probe_fails_when_nothing_listens() {
        // Bind then drop: the port is (almost certainly) closed afterwards.
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(probe(port).is_err());
    }
}
