use super::looks_like_tls;

#[test]
fn tls_handshake_record_byte_is_recognised() {
    // 0x16 == TLS ContentType::Handshake — the first byte of a ClientHello.
    assert!(looks_like_tls(0x16));
}

#[test]
fn plaintext_http_first_bytes_are_not_tls() {
    // 'G', 'C', etc. — none are 0x16.
    assert!(!looks_like_tls(b'G'));
    assert!(!looks_like_tls(b'C'));
    assert!(!looks_like_tls(0x00));
    assert!(!looks_like_tls(0x17)); // application-data, not handshake
}

use std::sync::Arc;

use crate::ca::generate_ca;
use rustls::pki_types::ServerName;
// `pem_slice_iter` (and `from_pem`) are provided by the `PemObject` trait, which
// must be in scope to call them on `CertificateDer`.
use rustls::pki_types::pem::PemObject;

/// rustls needs a process-default CryptoProvider. Idempotent across the test
/// binary (first install wins; later calls error and are ignored).
fn install_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

// A self-contained loopback HTTPS origin built with rcgen, returning its own CA
// (as a rustls RootCertStore) so `intercept`'s upstream leg can validate it.
async fn spawn_tls_origin() -> (std::net::SocketAddr, Arc<rustls::RootCertStore>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut params = rcgen::CertificateParams::new(vec!["origin.test".to_string()]).unwrap();
    params
        .extended_key_usages
        .push(rcgen::ExtendedKeyUsagePurpose::ServerAuth);
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = params.self_signed(&key).unwrap();
    let cert_der = cert.der().clone();
    let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(
        rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()),
    );
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert_der.clone()).unwrap();

    let server_cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_cfg));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((tcp, _)) = listener.accept().await {
            if let Ok(mut tls) = acceptor.accept(tcp).await {
                let mut buf = [0u8; 1024];
                let _ = tls.read(&mut buf).await; // read the request line
                let _ = tls
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nPONG")
                    .await;
                let _ = tls.shutdown().await;
            }
        }
    });
    (addr, Arc::new(roots))
}

#[tokio::test]
async fn mitm_terminates_and_reoriginates_a_real_tls_session() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    install_provider();
    let (origin_addr, upstream_roots) = spawn_tls_origin().await;
    let ca = Arc::new(generate_ca().unwrap());

    let (worker_end, proxy_end) = tokio::net::UnixStream::pair().unwrap();

    let upstream_tls = Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates((*upstream_roots).clone())
            .with_no_client_auth(),
    );

    // Run intercept on the proxy end (server side). It dials the origin itself.
    let ca_for_proxy = Arc::clone(&ca);
    let proxy = tokio::spawn(async move {
        let mut cache = crate::leaf_cache::LeafCache::new();
        super::intercept(
            proxy_end,
            origin_addr,
            "origin.test",
            &ca_for_proxy,
            &mut cache,
            upstream_tls,
            &[],
        )
        .await
    });

    // Worker: TLS-connect through the UDS trusting only the per-instance CA.
    let mut worker_roots = rustls::RootCertStore::empty();
    for der in rustls::pki_types::CertificateDer::pem_slice_iter(ca.cert_pem().as_bytes()) {
        worker_roots.add(der.unwrap()).unwrap();
    }
    let worker_tls = Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(worker_roots)
            .with_no_client_auth(),
    );
    let connector = tokio_rustls::TlsConnector::from(worker_tls);
    let sni = ServerName::try_from("origin.test").unwrap();
    let mut tls = connector.connect(sni, worker_end).await.expect("worker TLS handshake");
    tls.write_all(b"GET / HTTP/1.1\r\nHost: origin.test\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut resp = Vec::new();
    tls.read_to_end(&mut resp).await.unwrap();
    assert!(
        resp.windows(4).any(|w| w == b"PONG"),
        "expected the origin body through the MITM, got {:?}",
        String::from_utf8_lossy(&resp)
    );
    // Cleanly close the worker's TLS write half (sends a TLS close_notify).
    // `copy_bidirectional` inside `intercept` only completes once BOTH directions
    // hit EOF: the origin already half-closed (giving us PONG), but the
    // worker→origin direction stays open until the worker closes. A real
    // `Connection: close` client closes after the exchange; `shutdown()` is the
    // faithful equivalent. A bare `drop` would close the socket WITHOUT a
    // close_notify, which rustls reports as an unexpected-EOF error — so we
    // shut down gracefully and let `intercept` return cleanly.
    tls.shutdown().await.unwrap();
    proxy.await.unwrap().expect("intercept ok");
}
