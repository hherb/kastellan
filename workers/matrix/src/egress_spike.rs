//! Phase D egress-transport spike: prove that `matrix_sdk`'s HTTP client routes
//! through our egress sidecar via the loopback-TCP↔UDS `ProxyBridge`. Hermetic —
//! no homeserver, no real sidecar binary, no PG. A stub UDS "proxy" records the
//! CONNECT request line; the assertion is that matrix-sdk's first network call
//! reaches it as `CONNECT <host>:443`.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

use crate::bridge::ProxyBridge;

fn uds_path(tag: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/tmp/km-spike-{}-{}.sock", tag, std::process::id()))
}

fn store_path(tag: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/tmp/km-spike-store-{}-{}", tag, std::process::id()))
}

/// Stub UDS proxy: accept one connection, read the first request line, record
/// it, reply `200 Connection established`, then drop (the SDK's TLS handshake to
/// the non-existent origin then fails — irrelevant; we only assert the CONNECT).
async fn spawn_stub_proxy(listener: UnixListener, seen: Arc<Mutex<Vec<String>>>) {
    if let Ok((mut s, _)) = listener.accept().await {
        let mut buf = [0u8; 256];
        if let Ok(n) = s.read(&mut buf).await {
            let line = String::from_utf8_lossy(&buf[..n])
                .lines()
                .next()
                .unwrap_or("")
                .to_string();
            seen.lock().unwrap().push(line);
        }
        let _ = s.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").await;
    }
}

/// Drive matrix-sdk's first network call against the stub UDS proxy and return
/// the request lines the stub saw. All filesystem setup/teardown happens here so
/// the caller asserts on the returned `Vec` *after* cleanup — a failing
/// assertion never leaks `/tmp` artifacts. `use_proxy` toggles the `.proxy()`
/// wiring so the same harness drives both the positive and the negative control.
async fn drive_first_request(tag: &str, use_proxy: bool) -> Vec<String> {
    use matrix_sdk::Client;

    let path = uds_path(tag);
    let store = store_path(tag);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&store);

    let listener = UnixListener::bind(&path).expect("bind stub uds");
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let stub = tokio::spawn(spawn_stub_proxy(listener, seen.clone()));

    let bridge = ProxyBridge::bind(path.clone()).await.expect("bind bridge");

    // A SQLite store dir for the (encrypted) state store.
    let _ = std::fs::create_dir_all(&store);

    // Build a client pointed at a fake homeserver, optionally routed through the
    // bridge.
    let mut builder = Client::builder()
        .homeserver_url("https://fake-homeserver.invalid")
        .sqlite_store(&store, None);
    if use_proxy {
        builder = builder.proxy(format!("http://{}", bridge.proxy_addr()));
    }
    let client = builder.build().await.expect("client builds");

    // Trigger the first network call. It will error (no real origin), but with
    // the proxy wired the stub records the CONNECT first. `whoami` hits the
    // homeserver; if the resolved matrix-sdk version names this differently, use
    // any first network call (e.g. `client.server_versions()` or a login
    // attempt) — the assertion is on the CONNECT, not on this call's result.
    let _ = client.whoami().await;

    // Give the stub a moment to record (it blocks until its accept() returns or
    // the timeout fires — in the negative case nothing connects, so it times
    // out), then snapshot.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), stub).await;
    let lines = seen.lock().unwrap().clone();

    drop(bridge);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&store);
    lines
}

#[tokio::test]
async fn matrix_sdk_routes_first_request_through_the_bridge() {
    let lines = drive_first_request("proxy", true).await;
    let saw_connect = lines
        .iter()
        .any(|l| l.starts_with("CONNECT") && l.contains("fake-homeserver.invalid"));
    assert!(saw_connect, "expected a CONNECT to the homeserver via the bridge; saw: {lines:?}");
}

/// Negative control: strip `.proxy()` and the stub must see *nothing*. This
/// proves the positive assertion is non-spurious — it's the bridge routing the
/// SDK's traffic, not some ambient connection to the stub UDS. Without the proxy
/// the SDK dials `fake-homeserver.invalid` directly (a reserved RFC 6761 name
/// that never resolves), so the stub stays silent.
#[tokio::test]
async fn without_proxy_nothing_reaches_the_bridge() {
    let lines = drive_first_request("noproxy", false).await;
    assert!(lines.is_empty(), "expected no traffic to the stub without .proxy(); saw: {lines:?}");
}
