//! `ProxyConnectGet`: an `HttpGet` that reaches origins **only** through the
//! per-worker egress proxy's UDS via HTTP CONNECT. Used when force-routing is
//! active (`KASTELLAN_EGRESS_PROXY_UDS` set) — the worker has no other route
//! out. TLS stays end-to-end worker↔origin (the proxy tunnels ciphertext).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use http_body_util::BodyExt;
use hyper::Request;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::{Host, Url};

use crate::http::{HttpGet, RawResponse, MAX_BODY_BYTES, TIMEOUT_SECS};

/// Read cap for the proxy's CONNECT response head (mirrors the proxy's 8 KiB).
const MAX_PROXY_HEAD_BYTES: usize = 8 * 1024;

/// `HttpGet` that reaches origins only via the egress-proxy UDS (HTTP CONNECT).
pub struct ProxyConnectGet {
    user_agent: String,
    uds: PathBuf,
    /// Shared TLS config built once at construction; cheap to clone per-connection.
    tls: Arc<rustls::ClientConfig>,
    rt: tokio::runtime::Runtime,
}

impl ProxyConnectGet {
    /// Back-compat constructor: webpki public roots, infallible. Used where no
    /// MITM CA is configured (slice #1/#2 posture, dev/no-proxy). `uds` is the
    /// proxy socket path (`KASTELLAN_EGRESS_PROXY_UDS`).
    ///
    /// `allow(dead_code)`: this is a published back-compat shim for external
    /// callers (and the round-trip/EOF/403 unit tests below). The crate's own
    /// `make_get_inner` now routes through `with_trust`, so the lib target has
    /// no non-test caller — that's the intended posture, not dead code.
    #[allow(dead_code)]
    pub fn new(user_agent: &str, uds: PathBuf) -> Self {
        // Delegating to `with_trust(.., None)` keeps a single TLS-build path.
        // The `None` branch is infallible (it can only `extend` webpki roots),
        // so the `expect` here can never fire — proven by `new_without_ca_uses_webpki`.
        Self::with_trust(user_agent, uds, None).expect("webpki-only config is infallible")
    }

    /// Build the transport with an explicit trust posture. When `ca_path` is
    /// `Some`, the worker trusts ONLY that CA (the per-instance MITM CA) and
    /// public roots are dropped — egress fails closed unless the proxy
    /// terminates the TLS. A set-but-unreadable/invalid CA is an error (fail
    /// closed; never silently fall back to webpki). When `None`, webpki roots
    /// (slice #1/#2 back-compat, dev/no-proxy).
    pub fn with_trust(
        user_agent: &str,
        uds: PathBuf,
        ca_path: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");

        // Build the trust anchors once — cloning the root set on every HTTPS
        // call is measurably expensive; the resulting config lives behind an Arc.
        let mut root_store = rustls::RootCertStore::empty();
        match ca_path {
            Some(path) => {
                // MITM posture: trust ONLY this per-instance CA. Any failure to
                // read/parse/add it is fatal — we must NOT fall back to webpki,
                // or the fail-closed guarantee (egress only via the proxy that
                // terminates TLS) would silently degrade to "trust the world".
                let pem = std::fs::read(&path)
                    .map_err(|e| anyhow::anyhow!("read MITM CA {path:?}: {e}"))?;
                let mut added = 0usize;
                for der in CertificateDer::pem_slice_iter(&pem) {
                    let der = der.map_err(|e| anyhow::anyhow!("parse MITM CA {path:?}: {e}"))?;
                    root_store
                        .add(der)
                        .map_err(|e| anyhow::anyhow!("add MITM CA {path:?}: {e}"))?;
                    added += 1;
                }
                if added == 0 {
                    anyhow::bail!("MITM CA {path:?} contained no certificates");
                }
            }
            None => {
                root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            }
        }
        let tls = Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth(),
        );

        Ok(Self { user_agent: user_agent.to_string(), uds, tls, rt })
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
                let tls = tls_connect(stream, url, Arc::clone(&self.tls)).await?;
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

    fn transport_kind(&self) -> &'static str {
        "proxy-connect"
    }
}

/// Read from `stream` until `\r\n\r\n` or EOF, bounded by `MAX_PROXY_HEAD_BYTES`.
/// Returns the first line of the response (the status line).
///
/// # Errors
/// Returns `Err` if the proxy closes the connection before the blank-line
/// terminator (`\r\n\r\n`) arrives — a truncated head must not be parsed as
/// success.
async fn read_proxy_head(stream: &mut tokio::net::UnixStream) -> Result<String, String> {
    // Read ONE byte at a time on purpose: the CONNECT response head is
    // immediately followed by the tunnelled stream (the origin's TLS
    // ClientHello/records). A buffered/chunked read would over-consume bytes
    // belonging to that tunnel and corrupt the handshake. Stopping exactly at
    // the `\r\n\r\n` terminator leaves the tunnel byte-aligned for `tls_connect`.
    // Heads are ~40 bytes, so the syscall count is trivial — do NOT "optimise"
    // this into a chunked read.
    let mut buf = [0u8; 1];
    let mut acc: Vec<u8> = Vec::new();
    let mut terminated = false;
    loop {
        let n = stream
            .read(&mut buf)
            .await
            .map_err(|e| format!("read proxy head: {e}"))?;
        if n == 0 {
            // EOF before the blank-line terminator — truncated head.
            break;
        }
        acc.push(buf[0]);
        if acc.len() > MAX_PROXY_HEAD_BYTES {
            return Err(format!("proxy head exceeds {MAX_PROXY_HEAD_BYTES} bytes"));
        }
        if acc.ends_with(b"\r\n\r\n") {
            terminated = true;
            break;
        }
    }

    if !terminated {
        return Err(
            "proxy closed connection before sending a complete response head (no \\r\\n\\r\\n)"
                .to_string(),
        );
    }

    // Extract the first line (status line).
    let head_str = String::from_utf8_lossy(&acc);
    let first_line = head_str
        .lines()
        .next()
        .ok_or_else(|| "empty proxy response".to_string())?;
    Ok(first_line.to_string())
}

/// Wrap the raw `UnixStream` in a TLS layer using the pre-built `ClientConfig`.
/// Builds the `ServerName` from `url.host()` (NOT the raw `host_str()`) so
/// that IPv6 literals are not passed with their URL-authority brackets.
async fn tls_connect(
    stream: tokio::net::UnixStream,
    url: &Url,
    tls_config: Arc<rustls::ClientConfig>,
) -> Result<tokio_rustls::client::TlsStream<tokio::net::UnixStream>, String> {
    let server_name: ServerName<'static> = match url.host() {
        Some(Host::Domain(d)) => ServerName::try_from(d.to_owned())
            .map_err(|e| format!("invalid dns name: {e}"))?,
        Some(Host::Ipv4(ip)) => ServerName::IpAddress(ip.into()),
        Some(Host::Ipv6(ip)) => ServerName::IpAddress(ip.into()),
        None => return Err("url has no host for TLS".to_string()),
    };

    let connector = tokio_rustls::TlsConnector::from(tls_config);
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

    // Collect body with a running cap. Check BEFORE extending so the oversized
    // chunk is never copied in — mirrors ReqwestGet's hard-limit posture.
    let mut body_bytes = Vec::new();
    let mut frames = resp.into_body();
    while let Some(frame) = frames.frame().await {
        let frame = frame.map_err(|e| format!("body read: {e}"))?;
        if let Some(data) = frame.data_ref() {
            if body_bytes.len() + data.len() > MAX_BODY_BYTES {
                return Err(format!("response body exceeds {MAX_BODY_BYTES} bytes"));
            }
            body_bytes.extend_from_slice(data);
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
}
