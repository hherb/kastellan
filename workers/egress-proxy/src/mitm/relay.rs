//! Scanning bidirectional relay for the MITM path. Replaces a plain
//! `copy_bidirectional` when secret fingerprints are provisioned: each
//! direction is scanned with its own [`RollingMatcher`] *before* the bytes are
//! forwarded, so the chunk that completes a secret is never relayed. A confirmed
//! hit aborts the relay (best-effort block — earlier bytes may already have been
//! forwarded; the kill denies completion + the response round-trip).

use kastellan_leak_scan::{RollingMatcher, SecretFingerprint};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Which half of the tunnel a leak was found on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// worker → origin (the exfil vector).
    Request,
    /// origin → worker.
    Response,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Request => "request",
            Direction::Response => "response",
        }
    }
}

/// A confirmed leak surfaced by [`scan_relay`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeakReport {
    pub sha256_hex: String,
    pub offset: u64,
    pub direction: Direction,
}

const RELAY_BUF: usize = 16 * 1024;

/// Relay `client` ↔ `upstream`, scanning both directions for `patterns`.
/// Returns `Ok(Some(report))` on a confirmed leak (caller kills the connection),
/// `Ok(None)` on clean EOF of both directions, `Err` on a transport error.
pub async fn scan_relay<C, U>(
    client: C,
    upstream: U,
    patterns: &[SecretFingerprint],
) -> Result<Option<LeakReport>, String>
where
    C: AsyncRead + AsyncWrite + Unpin,
    U: AsyncRead + AsyncWrite + Unpin,
{
    let (mut cr, mut cw) = tokio::io::split(client);
    let (mut ur, mut uw) = tokio::io::split(upstream);
    let mut req = RollingMatcher::new(patterns.to_vec());
    let mut resp = RollingMatcher::new(patterns.to_vec());
    let mut req_buf = vec![0u8; RELAY_BUF];
    let mut resp_buf = vec![0u8; RELAY_BUF];
    let mut req_done = false;
    let mut resp_done = false;

    while !(req_done && resp_done) {
        tokio::select! {
            r = cr.read(&mut req_buf), if !req_done => match r {
                Ok(0) => { let _ = uw.shutdown().await; req_done = true; }
                Ok(n) => {
                    if let Some(hit) = req.feed(&req_buf[..n]) {
                        return Ok(Some(LeakReport {
                            sha256_hex: hit.sha256_hex, offset: hit.offset,
                            direction: Direction::Request,
                        }));
                    }
                    uw.write_all(&req_buf[..n]).await.map_err(|e| format!("relay req write: {e}"))?;
                }
                Err(e) => return Err(format!("relay req read: {e}")),
            },
            r = ur.read(&mut resp_buf), if !resp_done => match r {
                Ok(0) => { let _ = cw.shutdown().await; resp_done = true; }
                Ok(n) => {
                    if let Some(hit) = resp.feed(&resp_buf[..n]) {
                        return Ok(Some(LeakReport {
                            sha256_hex: hit.sha256_hex, offset: hit.offset,
                            direction: Direction::Response,
                        }));
                    }
                    cw.write_all(&resp_buf[..n]).await.map_err(|e| format!("relay resp write: {e}"))?;
                }
                Err(e) => return Err(format!("relay resp read: {e}")),
            },
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_leak_scan::fingerprint_value;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn fp(v: &[u8]) -> SecretFingerprint {
        fingerprint_value(v).unwrap()
    }

    #[tokio::test]
    async fn detects_secret_in_request_direction() {
        let secret = b"exfiltrated-secret-1";
        // client<->upstream wired with in-memory duplex pipes.
        let (client, mut client_peer) = tokio::io::duplex(4096);
        let (upstream, upstream_peer) = tokio::io::duplex(4096);
        let patterns = vec![fp(secret)];

        let relay = tokio::spawn(async move { scan_relay(client, upstream, &patterns).await });

        // Worker side writes a request body carrying the secret.
        client_peer
            .write_all(b"POST / HTTP/1.1\r\n\r\nleak=exfiltrated-secret-1")
            .await
            .unwrap();
        // Drop the peers so the relay's reads eventually unblock.
        drop(client_peer);
        drop(upstream_peer);

        let report = relay.await.unwrap().unwrap();
        assert!(report.is_some(), "expected a leak report");
        let report = report.unwrap();
        assert_eq!(report.direction, Direction::Request);
    }

    #[tokio::test]
    async fn clean_traffic_relays_without_report() {
        let (client, mut client_peer) = tokio::io::duplex(4096);
        let (upstream, mut upstream_peer) = tokio::io::duplex(4096);
        let patterns = vec![fp(b"never-sent-secret-9")];
        let relay = tokio::spawn(async move { scan_relay(client, upstream, &patterns).await });

        client_peer.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
        // Upstream side: drain the relayed request then answer.
        let mut req_buf = [0u8; 256];
        let _ = upstream_peer.read(&mut req_buf).await;
        upstream_peer.write_all(b"HTTP/1.1 200 OK\r\n\r\nbody").await.unwrap();
        // Client side: drain the relayed response.
        let mut resp_buf = [0u8; 256];
        let _ = client_peer.read(&mut resp_buf).await;
        // Both sides close; relay reaches dual-EOF → Ok(None).
        drop(client_peer);
        drop(upstream_peer);

        let report = relay.await.unwrap().unwrap();
        assert!(report.is_none(), "clean traffic must produce no report");
    }
}
