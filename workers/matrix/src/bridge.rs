//! `ProxyBridge`: matrix-sdk's reqwest client speaks HTTP-proxy CONNECT over
//! TCP, but our egress sidecar listens on a Unix-domain socket. This bridge
//! binds a loopback TCP port, and for each accepted connection opens the sidecar
//! UDS and copies bytes both ways — the Rust analogue of browser-driver's
//! `shim.py ProxyShim`. The SDK is pointed at `proxy_addr()` via `.proxy()`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::task::JoinHandle;

/// How long to pause after a non-trivial `accept()` error before retrying. Long
/// enough that a *persistent* failure (e.g. file-descriptor exhaustion) logs at
/// a readable cadence instead of spinning a hot loop, short enough that a
/// genuinely transient blip barely delays the next connection.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// What the accept loop should do after a failed [`TcpListener::accept`].
///
/// The bridge never *breaks* its accept loop: tearing it down would leave the
/// worker alive but the bridge silently dead (the SDK would then see only
/// opaque connection failures — exactly the regression #312 closes). Instead
/// every error is logged and retried; this enum only decides whether to retry
/// at once or after [`ACCEPT_BACKOFF`].
#[derive(Debug, PartialEq, Eq)]
enum AcceptRetry {
    /// Retry immediately — nothing is wrong with the listener itself.
    Immediate,
    /// Pause [`ACCEPT_BACKOFF`] before retrying — the error may persist (e.g.
    /// resource exhaustion), so we avoid a hot loop.
    Backoff,
}

/// Classify an `accept()` error into a retry strategy.
///
/// Pure (no I/O): the loop in [`ProxyBridge::bind`] calls this and acts on the
/// result, which keeps the policy unit-testable in isolation.
fn classify_accept_error(err: &std::io::Error) -> AcceptRetry {
    use std::io::ErrorKind;
    match err.kind() {
        // A peer aborted between connect and accept (`ECONNABORTED`), or a
        // signal interrupted the syscall (`EINTR`): the listener is healthy, so
        // retry without delay.
        ErrorKind::ConnectionAborted | ErrorKind::Interrupted => AcceptRetry::Immediate,
        // Resource exhaustion (`EMFILE`/`ENFILE`/`ENOBUFS`/`ENOMEM` surface as
        // `OutOfMemory` or an uncategorized kind) or anything else unexpected:
        // back off so a persistent condition is logged steadily, not hot-looped.
        _ => AcceptRetry::Backoff,
    }
}

/// A loopback-TCP↔UDS relay. Constructed by [`LiveSdk`](crate::sdk_live::LiveSdk)
/// (under `live-matrix`) and exercised by the `egress_spike` test.
pub struct ProxyBridge {
    addr: SocketAddr,
    accept_task: JoinHandle<()>,
}

impl ProxyBridge {
    /// Bind `127.0.0.1:0`, spawn the accept loop relaying to `uds_path`, and
    /// return immediately. The accept loop runs until the `ProxyBridge` is
    /// dropped.
    pub async fn bind(uds_path: PathBuf) -> std::io::Result<ProxyBridge> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;
        let accept_task = tokio::spawn(async move {
            loop {
                let (tcp, _peer) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(e) => {
                        // Never break: a single (possibly transient) accept
                        // error must not silently kill the bridge for the
                        // worker's lifetime (#312). Log, then retry — backing
                        // off on non-trivial errors to avoid a hot loop.
                        match classify_accept_error(&e) {
                            AcceptRetry::Immediate => {
                                eprintln!(
                                    "kastellan-worker-matrix: proxy bridge accept error (retrying): {e}"
                                );
                            }
                            AcceptRetry::Backoff => {
                                eprintln!(
                                    "kastellan-worker-matrix: proxy bridge accept error (backing off {}ms): {e}",
                                    ACCEPT_BACKOFF.as_millis()
                                );
                                tokio::time::sleep(ACCEPT_BACKOFF).await;
                            }
                        }
                        continue;
                    }
                };
                let path = uds_path.clone();
                tokio::spawn(async move {
                    if let Err(e) = relay(tcp, path.clone()).await {
                        // A dead/misconfigured sidecar UDS, or a relay I/O
                        // error, is now diagnosable instead of presenting as an
                        // unexplained SDK timeout (#312, silent path #2).
                        eprintln!(
                            "kastellan-worker-matrix: proxy bridge relay to {} failed: {e}",
                            path.display()
                        );
                    }
                });
            }
        });
        Ok(ProxyBridge { addr, accept_task })
    }

    /// The bound loopback address to hand to matrix-sdk's `.proxy()`.
    pub fn proxy_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for ProxyBridge {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

/// Relay one accepted TCP connection to the sidecar UDS, both directions.
///
/// Returns the first I/O error so the caller can log it: `Err` means either the
/// sidecar UDS could not be reached (gone / not listening) or the byte-copy
/// itself failed. A normal connection close is `Ok` — `copy_bidirectional`
/// reports a clean EOF as success, so this never logs spuriously on shutdown.
async fn relay(mut tcp: TcpStream, uds_path: PathBuf) -> std::io::Result<()> {
    let mut uds = UnixStream::connect(&uds_path).await?;
    copy_bidirectional(&mut tcp, &mut uds).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpStream, UnixListener};

    // A short, unique UDS path under /tmp (stays well under the 108-byte
    // sun_path limit; /tmp is the macOS egress scratch root).
    fn uds_path(tag: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(format!("/tmp/km-bridge-{}-{}.sock", tag, std::process::id()))
    }

    #[tokio::test]
    async fn relays_tcp_bytes_to_uds_and_back() {
        let path = uds_path("relay");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind uds");

        // Echo server on the UDS side: read one chunk, write it back uppercased.
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.expect("accept uds");
            let mut buf = [0u8; 64];
            let n = s.read(&mut buf).await.expect("read");
            let upper: Vec<u8> = buf[..n].iter().map(|b| b.to_ascii_uppercase()).collect();
            s.write_all(&upper).await.expect("write back");
        });

        let bridge = ProxyBridge::bind(path.clone()).await.expect("bind bridge");
        let mut client = TcpStream::connect(bridge.proxy_addr()).await.expect("connect tcp");
        client.write_all(b"hello").await.expect("write");
        let mut resp = [0u8; 5];
        client.read_exact(&mut resp).await.expect("read");
        assert_eq!(&resp, b"HELLO");

        server.await.expect("server task");
        drop(bridge);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn proxy_addr_is_loopback() {
        let path = uds_path("addr");
        let _ = std::fs::remove_file(&path);
        let _listener = UnixListener::bind(&path).expect("bind uds");
        let bridge = ProxyBridge::bind(path.clone()).await.expect("bind bridge");
        assert!(bridge.proxy_addr().ip().is_loopback());
        drop(bridge);
        let _ = std::fs::remove_file(&path);
    }

    // A relay to a non-existent UDS must surface the connect failure (so the
    // caller can log it) rather than swallowing it — issue #312, silent path #2.
    #[tokio::test]
    async fn relay_surfaces_uds_connect_failure() {
        let path = uds_path("missing");
        let _ = std::fs::remove_file(&path); // ensure the UDS is absent

        // A throwaway loopback TCP peer so `relay` has something to accept.
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind tcp");
        let addr = listener.local_addr().expect("addr");
        let accept = tokio::spawn(async move { listener.accept().await });
        let _client = TcpStream::connect(addr).await.expect("connect tcp");
        let (server_side, _) = accept.await.expect("join").expect("accept");

        let err = relay(server_side, path).await.expect_err("must surface UDS connect failure");
        assert!(
            matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ),
            "unexpected error kind: {err:?}"
        );
    }

    // Transient accept errors (peer aborted, signal-interrupted syscall) must be
    // retried immediately — nothing is wrong with the listener.
    #[test]
    fn transient_accept_errors_retry_immediately() {
        use std::io::{Error, ErrorKind};
        assert_eq!(
            classify_accept_error(&Error::from(ErrorKind::ConnectionAborted)),
            AcceptRetry::Immediate,
        );
        assert_eq!(
            classify_accept_error(&Error::from(ErrorKind::Interrupted)),
            AcceptRetry::Immediate,
        );
    }

    // Resource exhaustion / unexpected accept errors must back off before
    // retrying so a persistent condition logs steadily instead of hot-looping.
    #[test]
    fn resource_and_unknown_accept_errors_back_off() {
        use std::io::{Error, ErrorKind};
        assert_eq!(
            classify_accept_error(&Error::from(ErrorKind::OutOfMemory)),
            AcceptRetry::Backoff,
        );
        assert_eq!(
            classify_accept_error(&Error::from(ErrorKind::PermissionDenied)),
            AcceptRetry::Backoff,
        );
    }
}
