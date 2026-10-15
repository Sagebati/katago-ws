//! `healthcheck` subcommand: probe this instance's own `GET /healthz` and exit
//! 0 (healthy) or non-zero.
//!
//! It exists so container images can declare a Docker `HEALTHCHECK` without
//! shipping `curl`/`wget`: the probe is the binary itself. Every role that
//! serves HTTP answers `/healthz` (`standalone`/`orchestrator` from the API
//! router, `worker` from its minimal router), on `[web].port`.

use std::time::Duration;

/// Whole-request budget. A healthy instance answers in milliseconds; Docker's
/// own `--timeout` bounds the command anyway.
const TIMEOUT: Duration = Duration::from_secs(3);

/// `GET /healthz` on `127.0.0.1:port`. `Ok` only for an HTTP 200.
pub async fn probe(port: u16) -> Result<(), String> {
    let url = format!("http://127.0.0.1:{port}/healthz");
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .build()
        .map_err(|err| format!("build HTTP client: {err}"))?;
    let status = client
        .get(&url)
        .send()
        .await
        .map_err(|err| format!("GET {url}: {err}"))?
        .status();
    if status == reqwest::StatusCode::OK {
        Ok(())
    } else {
        Err(format!("unhealthy: GET {url} returned {status}"))
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::{Ipv4Addr, TcpListener};
    use std::thread;

    use super::*;

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

    #[tokio::test]
    async fn healthy_on_200() {
        let port =
            serve_once(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok");
        assert!(probe(port).await.is_ok());
    }

    #[tokio::test]
    async fn unhealthy_on_500() {
        let port = serve_once(
            b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        );
        assert!(probe(port).await.is_err());
    }

    #[tokio::test]
    async fn unhealthy_when_nothing_listens() {
        // Bind then drop: the port is (almost certainly) closed afterwards.
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(probe(port).await.is_err());
    }
}
