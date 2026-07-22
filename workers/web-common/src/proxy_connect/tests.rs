use super::*;

#[test]
fn connect_line_has_host_port_and_host_header() {
    let line = build_connect_request("example.com", 443);
    assert_eq!(
        line,
        "CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n"
    );
}

#[test]
fn connect_line_brackets_ipv6_literal() {
    // `url::Url::host_str()` returns IPv6 WITH brackets, so a bracketed host
    // is what we receive and what the proxy's request-line parser (slice #1,
    // bracketed-IPv6 aware) expects. Pass it through verbatim — do NOT
    // double-bracket and do NOT strip.
    let line = build_connect_request("[2606:4700::1111]", 443);
    assert_eq!(
        line,
        "CONNECT [2606:4700::1111]:443 HTTP/1.1\r\nHost: [2606:4700::1111]:443\r\n\r\n"
    );
}

#[test]
fn parse_status_accepts_200() {
    assert_eq!(parse_status_line("HTTP/1.1 200 Connection Established\r\n").unwrap(), 200);
}

#[test]
fn parse_status_rejects_403() {
    assert_eq!(parse_status_line("HTTP/1.1 403 Forbidden\r\n").unwrap(), 403);
}

#[test]
fn parse_status_errors_on_garbage() {
    assert!(parse_status_line("garbage").is_err());
}

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::thread;
use url::Url;

/// Minimal in-test proxy: accept one conn, read the CONNECT head to the blank
/// line, reply `200`, then serve a fixed HTTP/1.1 response as the "origin".
fn spawn_stub_proxy(path: std::path::PathBuf, origin_response: &'static [u8]) {
    let listener = UnixListener::bind(&path).unwrap();
    thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        // Drain CONNECT head up to blank line.
        let mut buf = [0u8; 1024];
        let mut acc = Vec::new();
        loop {
            let n = conn.read(&mut buf).unwrap();
            acc.extend_from_slice(&buf[..n]);
            if acc.windows(4).any(|w| w == b"\r\n\r\n") || n == 0 {
                break;
            }
        }
        conn.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").unwrap();
        // Now act as the raw-HTTP origin.
        let mut req = [0u8; 1024];
        let _ = conn.read(&mut req).unwrap();
        conn.write_all(origin_response).unwrap();
    });
}

#[test]
fn new_with_unreadable_ca_fails_closed() {
    let res = ProxyConnectGet::with_trust(
        "kastellan-test/0",
        PathBuf::from("/tmp/x.sock"),
        Some(PathBuf::from("/nonexistent/ca.pem")),
    );
    assert!(res.is_err(), "set-but-unreadable CA must fail closed");
}

#[test]
fn new_without_ca_uses_webpki() {
    // No CA → infallible webpki path (back-compat with slice #1/#2).
    let g = ProxyConnectGet::with_trust("kastellan-test/0", PathBuf::from("/tmp/x.sock"), None);
    assert!(g.is_ok());
}

#[test]
fn with_extra_ca_none_is_webpki_and_ok() {
    // No extra CA → webpki roots only, infallible.
    let g = ProxyConnectGet::with_extra_ca(
        "kastellan-test/0", PathBuf::from("/tmp/x.sock"), None,
    );
    assert!(g.is_ok());
}

#[test]
fn with_extra_ca_unreadable_fails_closed() {
    // A set-but-unreadable extra CA must fail closed (never silently drop it).
    let g = ProxyConnectGet::with_extra_ca(
        "kastellan-test/0",
        PathBuf::from("/tmp/x.sock"),
        Some(PathBuf::from("/nonexistent/extra-ca.pem")),
    );
    assert!(g.is_err(), "set-but-unreadable extra CA must fail closed");
}

#[test]
fn proxy_connect_get_round_trips_loopback_http() {
    let dir = std::env::temp_dir().join(format!("kastellan-pc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let uds = dir.join("egress.sock");
    let _ = std::fs::remove_file(&uds);
    spawn_stub_proxy(
        uds.clone(),
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
    );

    let get = ProxyConnectGet::new("kastellan-test/0", uds.clone());
    let url = Url::parse("http://127.0.0.1:8888/search").unwrap();
    let resp = get.get(&url).expect("round trip");

    assert_eq!(resp.status, 200);
    assert_eq!(resp.content_type, "application/json");
    assert_eq!(resp.body, b"{}");
    let _ = std::fs::remove_file(&uds);
}

// ── I1 + M4: premature-EOF and proxy-refused tests ──────────────────────

/// Stub that sends a partial head (no blank line) then closes — client MUST
/// return Err, not Ok (I1: premature EOF is not success).
fn spawn_stub_proxy_truncated_head(path: std::path::PathBuf) {
    let listener = UnixListener::bind(&path).unwrap();
    thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        // Drain CONNECT request (we don't care about its content).
        let mut buf = [0u8; 1024];
        let _ = conn.read(&mut buf);
        // Send a partial status line — deliberately NO blank line, then close.
        conn.write_all(b"HTTP/1.1 200\r\n").unwrap();
        // Drop conn → EOF.
    });
}

#[test]
fn premature_eof_is_an_error_not_success() {
    let dir =
        std::env::temp_dir().join(format!("kastellan-pc-trunc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let uds = dir.join("trunc.sock");
    let _ = std::fs::remove_file(&uds);
    spawn_stub_proxy_truncated_head(uds.clone());

    let get = ProxyConnectGet::new("kastellan-test/0", uds.clone());
    let url = Url::parse("http://127.0.0.1:8888/search").unwrap();
    let result = get.get(&url);

    assert!(
        result.is_err(),
        "expected Err for truncated proxy head, got Ok"
    );
    let msg = result.err().unwrap();
    assert!(
        msg.contains("complete response head"),
        "expected 'complete response head' in error, got: {msg}"
    );
    let _ = std::fs::remove_file(&uds);
}

/// Stub that returns a well-formed `403 Forbidden` — client must return Err.
fn spawn_stub_proxy_403(path: std::path::PathBuf) {
    let listener = UnixListener::bind(&path).unwrap();
    thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut buf = [0u8; 1024];
        let _ = conn.read(&mut buf);
        conn.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n").unwrap();
    });
}

#[test]
fn proxy_refused_403_is_an_error() {
    let dir =
        std::env::temp_dir().join(format!("kastellan-pc-403-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let uds = dir.join("refused.sock");
    let _ = std::fs::remove_file(&uds);
    spawn_stub_proxy_403(uds.clone());

    let get = ProxyConnectGet::new("kastellan-test/0", uds.clone());
    let url = Url::parse("http://127.0.0.1:8888/search").unwrap();
    let result = get.get(&url);

    assert!(result.is_err(), "expected Err for 403 CONNECT refusal, got Ok");
    let msg = result.err().unwrap();
    assert!(
        msg.contains("403"),
        "expected '403' in error message, got: {msg}"
    );
    let _ = std::fs::remove_file(&uds);
}

/// Like `spawn_stub_proxy` but serves `n` sequential connections, each in its own
/// thread so multiple clients are handled concurrently. Every connection gets the
/// same `origin_response` after the CONNECT handshake.
fn spawn_stub_proxy_multi(
    path: std::path::PathBuf,
    origin_response: &'static [u8],
    n: usize,
) {
    let listener = UnixListener::bind(&path).unwrap();
    thread::spawn(move || {
        for _ in 0..n {
            let (mut conn, _) = listener.accept().unwrap();
            thread::spawn(move || {
                let mut buf = [0u8; 1024];
                let mut acc = Vec::new();
                loop {
                    let n = conn.read(&mut buf).unwrap();
                    acc.extend_from_slice(&buf[..n]);
                    if acc.windows(4).any(|w| w == b"\r\n\r\n") || n == 0 {
                        break;
                    }
                }
                conn.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").unwrap();
                let mut req = [0u8; 1024];
                let _ = conn.read(&mut req).unwrap();
                conn.write_all(origin_response).unwrap();
            });
        }
    });
}

/// Like `spawn_stub_proxy` but captures the forwarded origin request HEAD and
/// hands it back over a channel, so a test can assert what the client sent
/// through the tunnel (e.g. `Authorization: Bearer …`). Reads until the head's
/// blank line so a header split across TCP reads is still fully captured.
fn spawn_stub_proxy_capturing(
    path: std::path::PathBuf,
    origin_response: &'static [u8],
) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let listener = UnixListener::bind(&path).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        // Drain the CONNECT head up to its blank line.
        let mut buf = [0u8; 1024];
        let mut acc = Vec::new();
        loop {
            let n = conn.read(&mut buf).unwrap();
            acc.extend_from_slice(&buf[..n]);
            if acc.windows(4).any(|w| w == b"\r\n\r\n") || n == 0 {
                break;
            }
        }
        conn.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").unwrap();
        // Capture the raw-HTTP origin request HEAD, forward it to the test.
        let mut req = Vec::new();
        let mut b = [0u8; 512];
        loop {
            let n = conn.read(&mut b).unwrap_or(0);
            if n == 0 {
                break;
            }
            req.extend_from_slice(&b[..n]);
            if req.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let _ = tx.send(req);
        conn.write_all(origin_response).unwrap();
    });
    rx
}

#[test]
fn get_authed_forwards_bearer_over_proxy() {
    // The force-routed transport (this type) is the PRODUCTION path for the mail
    // worker under `KASTELLAN_EGRESS_FORCE_ROUTING=1`; prove the bearer header
    // actually reaches the origin through the CONNECT tunnel, not just that
    // `ReqwestGet` (the non-force-routed sibling) sends it.
    let dir = std::env::temp_dir().join(format!("kastellan-pc-auth-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let uds = dir.join("egress.sock");
    let _ = std::fs::remove_file(&uds);
    let rx = spawn_stub_proxy_capturing(
        uds.clone(),
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
    );

    let get = ProxyConnectGet::new("kastellan-test/0", uds.clone());
    let url = Url::parse("http://127.0.0.1:8888/v1/accounts").unwrap();
    let resp = get.get_authed(&url, "secret-bearer", 1024).expect("round trip");
    assert_eq!(resp.status, 200);

    let sent = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("origin request captured");
    let sent = String::from_utf8_lossy(&sent).to_lowercase();
    assert!(
        sent.contains("authorization: bearer secret-bearer"),
        "bearer header must traverse the proxy tunnel, got: {sent}"
    );
    let _ = std::fs::remove_file(&uds);
}

#[test]
fn post_authed_forwards_bearer_and_content_type_over_proxy() {
    // `mail.search` is a `post_authed` — the most-used mail path. Prove the
    // bearer + content-type + POST method all traverse the tunnel.
    let dir = std::env::temp_dir().join(format!("kastellan-pc-postauth-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let uds = dir.join("egress.sock");
    let _ = std::fs::remove_file(&uds);
    let rx = spawn_stub_proxy_capturing(
        uds.clone(),
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
    );

    let get = ProxyConnectGet::new("kastellan-test/0", uds.clone());
    let url = Url::parse("http://127.0.0.1:8888/v1/search").unwrap();
    let resp = get
        .post_authed(&url, "post-bearer", "application/json", br#"{"query":"x"}"#, 1024)
        .expect("round trip");
    assert_eq!(resp.status, 200);

    let sent = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("origin request captured");
    let head = String::from_utf8_lossy(&sent).to_lowercase();
    assert!(head.starts_with("post /v1/search"), "expected POST request line, got: {head}");
    assert!(
        head.contains("authorization: bearer post-bearer"),
        "bearer header must traverse the proxy tunnel, got: {head}"
    );
    assert!(
        head.contains("content-type: application/json"),
        "content-type must traverse the proxy tunnel, got: {head}"
    );
    let _ = std::fs::remove_file(&uds);
}

#[test]
fn concurrent_gets_share_one_transport() {
    // One ProxyConnectGet (one multi-thread runtime) driven by several threads at
    // once. Guards the runtime-flavour change: a current-thread runtime would
    // serialise/deadlock here.
    let dir = std::env::temp_dir().join(format!("kastellan-pc-conc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let uds = dir.join("egress.sock");
    let _ = std::fs::remove_file(&uds);
    let n = 4;
    spawn_stub_proxy_multi(
        uds.clone(),
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
        n,
    );

    let get = ProxyConnectGet::new("kastellan-test/0", uds.clone());
    let url = Url::parse("http://127.0.0.1:8888/search").unwrap();

    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..n)
            .map(|_| scope.spawn(|| get.get(&url).map(|r| r.status)))
            .collect();
        for h in handles {
            assert_eq!(h.join().unwrap().expect("round trip"), 200);
        }
    });
    let _ = std::fs::remove_file(&uds);
}
