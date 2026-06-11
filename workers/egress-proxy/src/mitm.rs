//! TLS interception: decide whether a tunnel is TLS, and if so terminate the
//! worker's TLS with a per-instance-CA leaf and re-originate a validated TLS
//! session to the pinned origin. The pure peek predicate is split from the
//! async I/O so the branch logic is unit-testable without sockets.

/// True iff `first_byte` is the TLS record ContentType for `handshake` (0x16),
/// i.e. the first byte of a ClientHello. Anything else is treated as an
/// already-plaintext tunnel (plain-HTTP-over-CONNECT) and passed through.
pub fn looks_like_tls(first_byte: u8) -> bool {
    first_byte == 0x16
}

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::ServerName;
use tokio::net::{TcpStream, UnixStream};

use crate::ca::CaMaterial;
use crate::leaf_cache::LeafCache;

/// Bound on the re-origination TCP connect so an origin that becomes unreachable
/// between the sync reachability check and this async re-dial cannot pin the MITM
/// thread open indefinitely. Mirrors `proxy::CONNECT_TIMEOUT`. (The bidirectional
/// copy itself is still not idle-capped — that is workload-dependent and tracked
/// in #242.)
const ORIGIN_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Build the SNI `ServerName` for the upstream leg from the CONNECT authority
/// host. Domains go through `try_from`; IP literals (incl. bracketed IPv6) are
/// parsed as `IpAddress` so rustls validates them correctly.
fn upstream_server_name(host: &str) -> Result<ServerName<'static>, String> {
    let unbracketed = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = unbracketed.parse::<std::net::IpAddr>() {
        return Ok(ServerName::IpAddress(ip.into()));
    }
    ServerName::try_from(host.to_string()).map_err(|e| format!("invalid SNI {host:?}: {e}"))
}

/// Terminate the worker's TLS (presenting a CA-signed leaf for `host`) and
/// re-originate a validated TLS session to the already-resolved `upstream_addr`,
/// then copy plaintext both ways until either side closes.
///
/// `upstream_tls` is the trust config for the **real origin** — production wires
/// `webpki-roots`; tests wire a test-origin CA. Taking it as a parameter keeps
/// the round-trip test hermetic without a test-only env var.
pub async fn intercept(
    worker_side: UnixStream,
    upstream_addr: SocketAddr,
    host: &str,
    ca: &CaMaterial,
    leaf_cache: &mut LeafCache,
    upstream_tls: Arc<rustls::ClientConfig>,
) -> Result<(), String> {
    use tokio::io::copy_bidirectional;

    // 1. Server-side: present a leaf for `host`, handshake with the worker.
    let server_cfg = leaf_cache.get_or_issue(ca, host)?;
    let acceptor = tokio_rustls::TlsAcceptor::from(server_cfg);
    let mut client_tls = acceptor
        .accept(worker_side)
        .await
        .map_err(|e| format!("worker TLS handshake: {e}"))?;

    // 2. Client-side: re-originate to the pinned origin, validating its real cert.
    let upstream_tcp = tokio::time::timeout(ORIGIN_CONNECT_TIMEOUT, TcpStream::connect(upstream_addr))
        .await
        .map_err(|_| format!("dial origin {upstream_addr}: timed out after {ORIGIN_CONNECT_TIMEOUT:?}"))?
        .map_err(|e| format!("dial origin {upstream_addr}: {e}"))?;
    let connector = tokio_rustls::TlsConnector::from(upstream_tls);
    let sni = upstream_server_name(host)?;
    let mut upstream_tls_stream = connector
        .connect(sni, upstream_tcp)
        .await
        .map_err(|e| format!("origin TLS handshake: {e}"))?;

    // 3. Plaintext flows through here. (Slice #3b scans it; 3a only relays.)
    copy_bidirectional(&mut client_tls, &mut upstream_tls_stream)
        .await
        .map_err(|e| format!("tunnel copy: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests;
