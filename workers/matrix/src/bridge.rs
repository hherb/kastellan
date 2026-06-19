//! `ProxyBridge`: matrix-sdk's reqwest client speaks HTTP-proxy CONNECT over
//! TCP, but our egress sidecar listens on a Unix-domain socket. This bridge
//! binds a loopback TCP port, and for each accepted connection opens the sidecar
//! UDS and copies bytes both ways — the Rust analogue of browser-driver's
//! `shim.py ProxyShim`. The SDK is pointed at `proxy_addr()` via `.proxy()`.

use std::net::SocketAddr;
use std::path::PathBuf;

use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::task::JoinHandle;

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
                    Err(_) => break,
                };
                let path = uds_path.clone();
                tokio::spawn(async move { relay(tcp, path).await });
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
async fn relay(mut tcp: TcpStream, uds_path: PathBuf) {
    let Ok(mut uds) = UnixStream::connect(&uds_path).await else {
        return; // sidecar gone / not listening: drop this connection
    };
    let _ = copy_bidirectional(&mut tcp, &mut uds).await;
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
}
