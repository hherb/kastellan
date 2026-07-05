//! Shared loopback TLS-origin harness for the net-demo egress e2e tests (#390).
//!
//! The net-demo egress tests each stand up a tiny self-signed rustls origin on
//! `127.0.0.1:0` that answers every request with `204 No Content`; the real
//! egress-proxy sidecar then dials `127.0.0.1:<port>` and the worker validates
//! the TLS chain against the origin's cert (delivered as its `extra_ca`). The
//! server-spawn was byte-copied across
//! [`net_demo_egress_e2e`](../../../core/tests/net_demo_egress_e2e.rs) and
//! [`net_demo_firecracker_egress_e2e`](../../../core/tests/net_demo_firecracker_egress_e2e.rs),
//! differing **only** in where each writes the cert PEM. This centralizes the
//! server-spawn and hands back the cert PEM so each test keeps its own distinct
//! CA-path tail:
//!
//! - hermetic e2e → `std::env::temp_dir()/kastellan-netdemo-5c-ca-<pid>/origin-ca.pem`
//! - firecracker e2e → `/tmp/netdemo-<pid>-ca.pem` (a slice-3 `SHARE_ANCHOR`,
//!   delivered in-guest at the identical path by the VM RO-share).
//!
//! The worker's own in-process dev harness
//! (`workers/net-demo/src/main.rs` `mod probe_harness`) serves a different
//! purpose and is intentionally left as-is.

use std::sync::Arc;
use std::thread;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// Spawn a multi-connection loopback rustls origin on `127.0.0.1:0` that answers
/// any request with `HTTP/1.1 204 No Content`. Returns `(port, cert_pem)` — the
/// caller writes `cert_pem` wherever its `extra_ca` needs to live (each net-demo
/// e2e keeps its own distinct CA-path tail; see the module doc / #390).
///
/// The origin is served on a detached current-thread Tokio runtime and lives for
/// the remainder of the test process. It handles many connections (the initial
/// `net.tls_probe`, then a fresh one after each respawn), each on its own task so
/// a slow/aborted probe never blocks the next. The self-signed cert carries a
/// `127.0.0.1` IP SAN so rustls' server-name (IP) verification succeeds.
pub fn spawn_loopback_tls_origin() -> (u16, String) {
    // Self-signed cert with a 127.0.0.1 IP SAN so rustls' server-name (IP)
    // verification against the origin succeeds.
    let ck = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()])
        .expect("generate self-signed cert");
    let cert_pem = ck.cert.pem();
    let cert_der = ck.cert.der().clone();
    let key_der = rustls_pki_types::PrivateKeyDer::Pkcs8(
        rustls_pki_types::PrivatePkcs8KeyDer::from(ck.key_pair.serialize_der()),
    );

    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .expect("build server config");
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    // A current-thread runtime dedicated to the origin. It binds the port
    // synchronously so the caller can read it, then serves connections in a
    // loop (one per probe). Detached — lives for the test's duration.
    let (tx, rx) = std::sync::mpsc::channel::<u16>();
    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("origin runtime");
        rt.block_on(async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind origin");
            let port = listener.local_addr().unwrap().port();
            tx.send(port).unwrap();
            loop {
                let (tcp, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                let acceptor = acceptor.clone();
                // Each connection gets its own task so a slow/aborted probe
                // never blocks the next one.
                tokio::spawn(async move {
                    let mut tls = match acceptor.accept(tcp).await {
                        Ok(t) => t,
                        Err(_) => return,
                    };
                    let mut buf = [0u8; 1024];
                    let _ = tls.read(&mut buf).await;
                    let _ = tls.write_all(b"HTTP/1.1 204 No Content\r\n\r\n").await;
                    let _ = tls.shutdown().await;
                });
            }
        });
    });

    let port = rx.recv().expect("origin port");
    (port, cert_pem)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, ServerName};
    use tokio::net::TcpStream;
    use tokio_rustls::TlsConnector;

    /// The harness is the single source of truth for two e2e gates, so pin its
    /// contract directly: a client that trusts *only* the returned `cert_pem`
    /// completes a TLS handshake against `127.0.0.1:<port>` and reads back the
    /// `204 No Content` line. This exercises the same trust path the e2e tests
    /// rely on (the returned PEM used as the worker's `extra_ca`) without any
    /// sandbox.
    #[tokio::test]
    async fn origin_serves_204_over_tls_trusting_returned_cert() {
        let (port, cert_pem) = spawn_loopback_tls_origin();

        let mut roots = rustls::RootCertStore::empty();
        let cert = CertificateDer::from_pem_slice(cert_pem.as_bytes())
            .expect("parse origin cert PEM");
        roots.add(cert).expect("add origin cert to root store");

        let client_config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(client_config));

        let tcp = TcpStream::connect(("127.0.0.1", port)).await.expect("connect origin");
        let server_name = ServerName::IpAddress(std::net::Ipv4Addr::LOCALHOST.into());
        let mut tls = connector.connect(server_name, tcp).await.expect("tls handshake");

        tls.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await.expect("write");
        let mut buf = [0u8; 64];
        let n = tls.read(&mut buf).await.expect("read");
        let resp = String::from_utf8_lossy(&buf[..n]);
        assert!(resp.starts_with("HTTP/1.1 204 No Content"), "unexpected origin response: {resp:?}");
    }
}
