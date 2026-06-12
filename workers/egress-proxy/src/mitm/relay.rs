//! Scanning bidirectional relay for the MITM path. Replaces a plain
//! `copy_bidirectional` when secret fingerprints are provisioned: each
//! direction is scanned with its own [`RollingMatcher`] *before* the bytes are
//! forwarded, so the chunk that completes a secret is never relayed. A confirmed
//! hit aborts the relay (best-effort block — earlier bytes may already have been
//! forwarded; the kill denies completion + the response round-trip).
//!
//! The two directions are independent futures driven concurrently: a `write_all`
//! flushing one half never stalls reads on the other (unlike a single
//! select-on-`read` loop, where the in-arm `write_all` would block the peer
//! direction — a head-of-line stall for full-duplex tunnels). Each pump owns its
//! own half-streams + matcher and shares nothing with the other.

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
///
/// Each direction is an independent [`pump`] future; both are driven by one
/// `select!` so neither direction's `write_all` can stall the other's reads. The
/// `if !done` guards keep each pinned future from being polled after it resolves
/// (the select! never *drops* a partially-progressed future, so `read`/`write_all`
/// cancellation-safety is moot — a stalled pump is simply re-polled next tick).
pub async fn scan_relay<C, U>(
    client: C,
    upstream: U,
    patterns: &[SecretFingerprint],
) -> Result<Option<LeakReport>, String>
where
    C: AsyncRead + AsyncWrite + Unpin,
    U: AsyncRead + AsyncWrite + Unpin,
{
    let (cr, cw) = tokio::io::split(client);
    let (ur, uw) = tokio::io::split(upstream);
    // worker → origin (the exfil vector) and origin → worker, each with its own
    // matcher (the two share no state).
    let req = pump(cr, uw, RollingMatcher::new(patterns.to_vec()), Direction::Request);
    let resp = pump(ur, cw, RollingMatcher::new(patterns.to_vec()), Direction::Response);
    tokio::pin!(req, resp);
    let mut req_done = false;
    let mut resp_done = false;

    while !(req_done && resp_done) {
        tokio::select! {
            r = &mut req, if !req_done => match r? {
                Some(report) => return Ok(Some(report)),
                None => req_done = true,
            },
            r = &mut resp, if !resp_done => match r? {
                Some(report) => return Ok(Some(report)),
                None => resp_done = true,
            },
        }
    }
    Ok(None)
}

/// Pump one direction: read → scan-before-forward → write, until EOF (`Ok(None)`,
/// after a half-close shutdown of the write side) or a confirmed leak
/// (`Ok(Some(report))`, leaving the completing chunk unforwarded). A transport
/// error on either half is `Err`.
async fn pump<R, W>(
    mut r: R,
    mut w: W,
    mut matcher: RollingMatcher,
    direction: Direction,
) -> Result<Option<LeakReport>, String>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let dir = direction.as_str();
    let mut buf = vec![0u8; RELAY_BUF];
    loop {
        let n = r
            .read(&mut buf)
            .await
            .map_err(|e| format!("relay {dir} read: {e}"))?;
        if n == 0 {
            let _ = w.shutdown().await;
            return Ok(None);
        }
        if let Some(hit) = matcher.feed(&buf[..n]) {
            return Ok(Some(LeakReport {
                sha256_hex: hit.sha256_hex,
                offset: hit.offset,
                direction,
            }));
        }
        w.write_all(&buf[..n])
            .await
            .map_err(|e| format!("relay {dir} write: {e}"))?;
    }
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

    #[tokio::test]
    async fn full_duplex_traffic_does_not_head_of_line_stall() {
        // Both directions stream a payload larger than the duplex pipe buffer at
        // the same time. The two pumps run concurrently, so neither side's
        // `write_all` blocks the other's reads — if it did, this would deadlock.
        let big = vec![b'A'; 256 * 1024];
        let (client, client_peer) = tokio::io::duplex(8 * 1024);
        let (upstream, upstream_peer) = tokio::io::duplex(8 * 1024);
        let patterns = vec![fp(b"never-present-secret-x")];
        let relay = tokio::spawn(async move { scan_relay(client, upstream, &patterns).await });

        // Peers each both send a large payload and drain the relayed one.
        let req = big.clone();
        let client_io = tokio::spawn(async move {
            let mut sink = Vec::new();
            let (mut rd, mut wr) = tokio::io::split(client_peer);
            let send = tokio::spawn(async move {
                wr.write_all(&req).await.unwrap();
                wr.shutdown().await.unwrap();
            });
            rd.read_to_end(&mut sink).await.unwrap();
            send.await.unwrap();
            sink.len()
        });
        let resp = big.clone();
        let upstream_io = tokio::spawn(async move {
            let mut sink = Vec::new();
            let (mut rd, mut wr) = tokio::io::split(upstream_peer);
            let send = tokio::spawn(async move {
                wr.write_all(&resp).await.unwrap();
                wr.shutdown().await.unwrap();
            });
            rd.read_to_end(&mut sink).await.unwrap();
            send.await.unwrap();
            sink.len()
        });

        let report = relay.await.unwrap().unwrap();
        assert!(report.is_none(), "clean full-duplex traffic must not report");
        assert_eq!(client_io.await.unwrap(), big.len(), "client got full response");
        assert_eq!(upstream_io.await.unwrap(), big.len(), "upstream got full request");
    }
}
