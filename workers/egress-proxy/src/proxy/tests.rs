//! Hermetic drive-loop tests. `decide` is tested directly (pure); `handle_conn`
//! is driven over a real UDS with a test CONNECT client against a localhost
//! origin, with a stubbed resolver for the SSRF path.

use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener};
use std::os::unix::net::{UnixListener, UnixStream};

use kastellan_worker_web_common::allowlist::HostAllowlist;

use super::*;
use crate::report::{Decision, Reporter, Verdict};

/// rustls needs a process-default CryptoProvider before any ClientConfig/ServerConfig
/// builder runs. Idempotent across the test binary.
fn install_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
fn test_ca() -> crate::ca::CaMaterial {
    crate::ca::generate_ca().unwrap()
}
fn webpki_upstream() -> std::sync::Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    std::sync::Arc::new(
        rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth(),
    )
}

fn al(entries: &[&str]) -> HostAllowlist {
    HostAllowlist::from_env_json(&serde_json::to_string(entries).unwrap()).unwrap()
}

/// Port-scoped allowlist (the live proxy path — `host:port` entries).
fn eps(entries: &[&str]) -> HostAllowlist {
    let owned: Vec<String> = entries.iter().map(|s| s.to_string()).collect();
    HostAllowlist::from_endpoints(&owned)
}

struct StubResolve(Vec<IpAddr>);
impl Resolve for StubResolve {
    fn resolve(&self, _host: &str, _port: u16) -> std::io::Result<Vec<IpAddr>> {
        Ok(self.0.clone())
    }
}

#[derive(Default)]
struct VecReporter(Vec<Decision>);
impl Reporter for VecReporter {
    fn report(&mut self, d: Decision) { self.0.push(d); }
}

#[test]
fn decide_blocks_off_allowlist() {
    let r = StubResolve(vec!["203.0.113.5".parse().unwrap()]);
    match decide("evil.test", 443, &al(&["good.test"]), &r) {
        Target::Block(Verdict::BlockedAllowlist, _) => {}
        _ => panic!("expected allowlist block"),
    }
}

#[test]
fn decide_blocks_rebinding_to_private() {
    // public-looking name on the allowlist resolving to a private IP → SSRF block.
    let r = StubResolve(vec!["10.0.0.1".parse().unwrap()]);
    match decide("blocked.test", 443, &al(&["blocked.test"]), &r) {
        Target::Block(Verdict::BlockedSsrf, _) => {}
        _ => panic!("expected SSRF block"),
    }
}

#[test]
fn decide_allows_literal_loopback_when_allowlisted() {
    // The local-SearxNG carve-out: literal 127.0.0.1 explicitly allowlisted.
    let r = StubResolve(vec![]); // resolver must NOT be consulted for a literal.
    match decide("127.0.0.1", 8888, &al(&["127.0.0.1"]), &r) {
        Target::Dial(ip) => assert_eq!(ip, "127.0.0.1".parse::<IpAddr>().unwrap()),
        _ => panic!("expected dial to literal loopback"),
    }
}

#[test]
fn decide_pins_first_public_ip() {
    let r = StubResolve(vec!["10.0.0.1".parse().unwrap(), "203.0.113.9".parse().unwrap()]);
    match decide("ok.test", 443, &al(&["ok.test"]), &r) {
        Target::Dial(ip) => assert_eq!(ip, "203.0.113.9".parse::<IpAddr>().unwrap()),
        _ => panic!("expected dial to first non-denied IP"),
    }
}

#[test]
fn handle_conn_tunnels_allowed_literal_origin() {
    // Origin: a localhost TCP server that echoes a fixed response after reading.
    let origin = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin_port = origin.local_addr().unwrap().port();
    let origin_thread = std::thread::spawn(move || {
        let (mut s, _) = origin.accept().unwrap();
        let mut buf = [0u8; 16];
        let _ = s.read(&mut buf);
        s.write_all(b"HELLO").unwrap();
    });

    // Proxy: bind a UDS, accept one connection, handle it.
    let dir = std::env::temp_dir().join(format!("egress-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("egress.sock");
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).unwrap();
    let allow = al(&["127.0.0.1"]);
    let proxy_thread = std::thread::spawn(move || {
        let (conn, _) = listener.accept().unwrap();
        let mut reporter = VecReporter::default();
        // Construct CA/cache/ctx inside the thread so they own their lifetime
        // across the spawned proxy thread (no cross-thread borrow).
        install_provider();
        let ca = test_ca();
        let mut cache = crate::leaf_cache::LeafCache::new();
        let mut mitm = MitmCtx { ca: &ca, leaf_cache: &mut cache, upstream_tls: webpki_upstream(), secret_hashes_path: None };
        handle_conn(conn, "web-fetch", &allow, &StdResolve, &mut reporter, &mut mitm);
        reporter.0
    });

    // Client: CONNECT to the literal-allowlisted origin, then read the echo.
    let mut client = UnixStream::connect(&sock).unwrap();
    write!(client, "CONNECT 127.0.0.1:{origin_port} HTTP/1.1\r\n\r\n").unwrap();
    let mut head = [0u8; 39]; // "HTTP/1.1 200 Connection Established\r\n\r\n"
    client.read_exact(&mut head).unwrap();
    assert!(std::str::from_utf8(&head).unwrap().starts_with("HTTP/1.1 200"));
    client.write_all(b"ping").unwrap();
    let mut echo = [0u8; 5];
    client.read_exact(&mut echo).unwrap();
    assert_eq!(&echo, b"HELLO");

    origin_thread.join().unwrap();
    let decisions = proxy_thread.join().unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].verdict, Verdict::Allowed);
    assert!(!decisions[0].tls_intercepted, "plaintext tunnel is pass-through, not MITM");
    let _ = std::fs::remove_file(&sock);
}

#[test]
fn handle_conn_reports_block_for_off_allowlist() {
    let dir = std::env::temp_dir().join(format!("egress-test-block-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("egress.sock");
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).unwrap();
    let allow = al(&["good.test"]);
    let proxy_thread = std::thread::spawn(move || {
        let (conn, _) = listener.accept().unwrap();
        let mut reporter = VecReporter::default();
        install_provider();
        let ca = test_ca();
        let mut cache = crate::leaf_cache::LeafCache::new();
        let mut mitm = MitmCtx { ca: &ca, leaf_cache: &mut cache, upstream_tls: webpki_upstream(), secret_hashes_path: None };
        handle_conn(conn, "web-fetch", &allow, &StdResolve, &mut reporter, &mut mitm);
        reporter.0
    });

    let mut client = UnixStream::connect(&sock).unwrap();
    write!(client, "CONNECT evil.test:443 HTTP/1.1\r\n\r\n").unwrap();
    let mut resp = String::new();
    let _ = client.read_to_string(&mut resp);
    assert!(resp.starts_with("HTTP/1.1 403"), "got: {resp:?}");

    let decisions = proxy_thread.join().unwrap();
    assert_eq!(decisions[0].verdict, Verdict::BlockedAllowlist);
    let _ = std::fs::remove_file(&sock);
}

// ---- port-scoping (#241) ------------------------------------------------

#[test]
fn decide_blocks_allowed_host_on_wrong_port() {
    let r = StubResolve(vec!["93.184.216.34".parse().unwrap()]);
    match decide("example.com", 22, &eps(&["example.com:443"]), &r) {
        Target::Block(Verdict::BlockedAllowlist, _) => {}
        _ => panic!("allowed host on undeclared port must be blocked"),
    }
}

#[test]
fn decide_allows_host_on_declared_port() {
    let r = StubResolve(vec!["93.184.216.34".parse().unwrap()]);
    match decide("example.com", 443, &eps(&["example.com:443"]), &r) {
        Target::Dial(_) => {}
        _ => panic!("allowed host on its declared port must dial"),
    }
}

#[test]
fn decide_blocks_literal_ip_on_wrong_port() {
    // The literal carve-out is now port-scoped too: 127.0.0.1:8888 pins both.
    let r = StubResolve(vec![]); // resolver must not be consulted for a literal.
    match decide("127.0.0.1", 443, &eps(&["127.0.0.1:8888"]), &r) {
        Target::Block(Verdict::BlockedAllowlist, _) => {}
        _ => panic!("literal IP on undeclared port must be blocked"),
    }
}

#[test]
fn decide_allowed_via_bare_host_entry_is_flagged() {
    // A bare host:port-scoped entry yields the plain "ok" reason; a bare
    // host-only entry yields the distinct port-unconstrained marker so the
    // weaker grant is visible in the audit trail.
    assert_eq!(allowed_reason(&eps(&["a.com:443"]), "a.com"), "ok");
    assert_eq!(
        allowed_reason(&eps(&["a.com"]), "a.com"),
        "allowed:host-only-entry"
    );
}

#[test]
fn classify_mitm_error_detects_pin_mismatch() {
    let (verdict, reason) =
        super::classify_mitm_error("origin TLS handshake: certificate pin mismatch");
    assert_eq!(verdict, crate::report::Verdict::BlockedTlsPin);
    assert_eq!(reason, "pin_mismatch");
}

#[test]
fn classify_mitm_error_generic_failure_is_allowed_mitm_failed() {
    let (verdict, reason) = super::classify_mitm_error("origin TLS handshake: connection reset");
    assert_eq!(verdict, crate::report::Verdict::Allowed);
    assert!(reason.starts_with("mitm_failed:"));
}
