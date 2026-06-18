//! Phase D egress-transport spike: prove that `matrix_sdk`'s HTTP client routes
//! through our egress sidecar via the loopback-TCP↔UDS `ProxyBridge`. Hermetic —
//! no homeserver, no real sidecar binary, no PG. A stub UDS "proxy" records the
//! CONNECT request line; the assertion is that matrix-sdk's first network call
//! reaches it as `CONNECT <host>:443`.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

use crate::bridge::ProxyBridge;

fn uds_path() -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/tmp/km-spike-{}.sock", std::process::id()))
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

#[tokio::test]
async fn matrix_sdk_routes_first_request_through_the_bridge() {
    use matrix_sdk::Client;

    let path = uds_path();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind stub uds");
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let stub = tokio::spawn(spawn_stub_proxy(listener, seen.clone()));

    let bridge = ProxyBridge::bind(path.clone()).await.expect("bind bridge");
    let proxy_url = format!("http://{}", bridge.proxy_addr());

    // A SQLite store dir for the (encrypted) state store.
    let store = std::path::PathBuf::from(format!("/tmp/km-spike-store-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&store);

    // Build a client pointed at a fake homeserver, routed through the bridge.
    let client = Client::builder()
        .homeserver_url("https://fake-homeserver.invalid")
        .sqlite_store(&store, None)
        .proxy(proxy_url)
        .build()
        .await
        .expect("client builds");

    // Trigger the first network call. It will error (no real origin), but the
    // stub records the CONNECT first. `whoami` hits the homeserver; if the
    // resolved matrix-sdk version names this differently, use any first network
    // call (e.g. `client.server_versions()` or a login attempt) — the assertion
    // is on the CONNECT, not on this call's result.
    let _ = client.whoami().await;

    // Give the stub a moment to record, then assert.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), stub).await;
    let lines = seen.lock().unwrap().clone();

    let saw_connect = lines
        .iter()
        .any(|l| l.starts_with("CONNECT") && l.contains("fake-homeserver.invalid"));
    assert!(saw_connect, "expected a CONNECT to the homeserver via the bridge; saw: {lines:?}");

    drop(bridge);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&store);
}
