//! `ProxyConnectGet`: an `HttpGet` that reaches origins **only** through the
//! per-worker egress proxy's UDS via HTTP CONNECT. Used when force-routing is
//! active (`KASTELLAN_EGRESS_PROXY_UDS` set) — the worker has no other route
//! out. TLS stays end-to-end worker↔origin (the proxy tunnels ciphertext).

use std::path::PathBuf;
use std::time::Duration;

use http_body_util::BodyExt;
use hyper::Request;
use rustls_pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::{Host, Url};

use crate::http::{HttpGet, RawResponse, MAX_BODY_BYTES, TIMEOUT_SECS};

/// Read cap for the proxy's CONNECT response head (mirrors the proxy's 8 KiB).
const MAX_PROXY_HEAD_BYTES: usize = 8 * 1024;

/// `HttpGet` that reaches origins only via the egress-proxy UDS (HTTP CONNECT).
pub struct ProxyConnectGet {
    user_agent: String,
    uds: PathBuf,
    rt: tokio::runtime::Runtime,
}

impl ProxyConnectGet {
    /// Build the transport. `uds` is the proxy socket path
    /// (`KASTELLAN_EGRESS_PROXY_UDS`).
    pub fn new(user_agent: &str, uds: PathBuf) -> Self {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        Self { user_agent: user_agent.to_string(), uds, rt }
    }

    async fn get_async(&self, url: &Url) -> Result<RawResponse, String> {
        let host = url.host_str().ok_or("url has no host")?;
        let port = url
            .port_or_known_default()
            .ok_or("url has no port and no known default")?;

        // 1. Dial the proxy UDS and issue CONNECT.
        let mut stream = tokio::net::UnixStream::connect(&self.uds)
            .await
            .map_err(|e| format!("connect proxy uds: {e}"))?;
        stream
            .write_all(build_connect_request(host, port).as_bytes())
            .await
            .map_err(|e| format!("write CONNECT: {e}"))?;

        // 2. Read the proxy status head (bounded), require 200.
        let head = read_proxy_head(&mut stream).await?;
        let status = parse_status_line(&head)?;
        if status != 200 {
            return Err(format!("proxy refused CONNECT: {status}"));
        }

        // 3. Layer transport and run one GET.
        match url.scheme() {
            "https" => {
                let tls = tls_connect(stream, url).await?;
                run_get(tls, url, host, &self.user_agent).await
            }
            "http" => run_get(stream, url, host, &self.user_agent).await,
            other => Err(format!("unsupported scheme: {other}")),
        }
    }
}

impl HttpGet for ProxyConnectGet {
    fn get(&self, url: &Url) -> Result<RawResponse, String> {
        self.rt.block_on(async {
            match tokio::time::timeout(Duration::from_secs(TIMEOUT_SECS), self.get_async(url)).await
            {
                Ok(r) => r,
                Err(_) => Err(format!("request exceeded {TIMEOUT_SECS}s")),
            }
        })
    }
}

/// Read from `stream` until `\r\n\r\n` or EOF, bounded by `MAX_PROXY_HEAD_BYTES`.
/// Returns the first line of the response (the status line).
async fn read_proxy_head(stream: &mut tokio::net::UnixStream) -> Result<String, String> {
    let mut buf = [0u8; 1];
    let mut acc: Vec<u8> = Vec::new();
    loop {
        let n = stream
            .read(&mut buf)
            .await
            .map_err(|e| format!("read proxy head: {e}"))?;
        if n == 0 {
            break;
        }
        acc.push(buf[0]);
        if acc.len() > MAX_PROXY_HEAD_BYTES {
            return Err(format!("proxy head exceeds {MAX_PROXY_HEAD_BYTES} bytes"));
        }
        if acc.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    // Extract the first line (status line).
    let head_str = String::from_utf8_lossy(&acc);
    let first_line = head_str
        .lines()
        .next()
        .ok_or_else(|| "empty proxy response".to_string())?;
    Ok(first_line.to_string())
}

/// Wrap the raw `UnixStream` in a TLS layer using the system roots.
/// Builds the `ServerName` from `url.host()` (NOT the raw `host_str()`) so
/// that IPv6 literals are not passed with their URL-authority brackets.
async fn tls_connect(
    stream: tokio::net::UnixStream,
    url: &Url,
) -> Result<tokio_rustls::client::TlsStream<tokio::net::UnixStream>, String> {
    let server_name: ServerName<'static> = match url.host() {
        Some(Host::Domain(d)) => ServerName::try_from(d.to_owned())
            .map_err(|e| format!("invalid dns name: {e}"))?,
        Some(Host::Ipv4(ip)) => ServerName::IpAddress(ip.into()),
        Some(Host::Ipv6(ip)) => ServerName::IpAddress(ip.into()),
        None => return Err("url has no host for TLS".to_string()),
    };

    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
    connector
        .connect(server_name, stream)
        .await
        .map_err(|e| format!("TLS handshake failed: {e}"))
}

/// Drive a single HTTP/1.1 GET over `io` (raw or TLS stream), return `RawResponse`.
/// The body is capped at `MAX_BODY_BYTES`; exceeding that returns `Err`.
async fn run_get<IO>(io: IO, url: &Url, host: &str, user_agent: &str) -> Result<RawResponse, String>
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http1::handshake(
        hyper_util::rt::TokioIo::new(io),
    )
    .await
    .map_err(|e| format!("http1 handshake: {e}"))?;

    // Drive the connection in the background.
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let path_and_query = match url.query() {
        Some(q) => format!("{}?{}", url.path(), q),
        None => url.path().to_string(),
    };

    let req = Request::builder()
        .method("GET")
        .uri(&path_and_query)
        .header(hyper::header::HOST, host)
        .header(hyper::header::USER_AGENT, user_agent)
        .header(hyper::header::ACCEPT_ENCODING, "identity")
        .header(hyper::header::CONNECTION, "close")
        .body(http_body_util::Empty::<bytes::Bytes>::new())
        .map_err(|e| format!("build request: {e}"))?;

    let resp = sender
        .send_request(req)
        .await
        .map_err(|e| format!("send request: {e}"))?;

    let status = resp.status().as_u16();
    let location = resp
        .headers()
        .get(hyper::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let content_type = resp
        .headers()
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // Collect body with a running cap.
    let mut body_bytes = Vec::new();
    let mut frames = resp.into_body();
    while let Some(frame) = frames.frame().await {
        let frame = frame.map_err(|e| format!("body read: {e}"))?;
        if let Some(data) = frame.data_ref() {
            body_bytes.extend_from_slice(data);
            if body_bytes.len() > MAX_BODY_BYTES {
                return Err(format!("response body exceeds {MAX_BODY_BYTES} bytes"));
            }
        }
    }

    Ok(RawResponse { status, location, content_type, body: body_bytes })
}

/// Build the CONNECT request head for `host:port`. Host is passed verbatim
/// (a name, never a resolved IP — the proxy resolves + range-checks). Pass the
/// host exactly as `url::Url::host_str()` yields it: IPv6 literals arrive
/// already bracketed (`[2606:4700::1111]`), which is the form both this request
/// line and the proxy's bracketed-IPv6 parser require — do not re-bracket.
fn build_connect_request(host: &str, port: u16) -> String {
    format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n")
}

/// Parse the proxy's status line, returning the numeric status code.
fn parse_status_line(line: &str) -> Result<u16, String> {
    let code = line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| format!("malformed status line: {line:?}"))?;
    code.parse::<u16>().map_err(|e| format!("bad status code: {e}"))
}

#[cfg(test)]
mod tests {
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
                if acc.windows(4).any(|w| w == b"\r\n\r\n") || n == 0 { break; }
            }
            conn.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").unwrap();
            // Now act as the raw-HTTP origin.
            let mut req = [0u8; 1024];
            let _ = conn.read(&mut req).unwrap();
            conn.write_all(origin_response).unwrap();
        });
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
        // Give the listener a moment to bind.
        std::thread::sleep(std::time::Duration::from_millis(50));

        let get = ProxyConnectGet::new("kastellan-test/0", uds.clone());
        let url = Url::parse("http://127.0.0.1:8888/search").unwrap();
        let resp = get.get(&url).expect("round trip");

        assert_eq!(resp.status, 200);
        assert_eq!(resp.content_type, "application/json");
        assert_eq!(resp.body, b"{}");
        let _ = std::fs::remove_file(&uds);
    }
}
