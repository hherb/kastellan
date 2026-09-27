//! Shared test fixture: a one-shot, hand-rolled HTTP/1.1 mock of an
//! OpenAI-compatible backend.
//!
//! Moved verbatim out of `local_backend_e2e.rs` so more than one test
//! binary can use it (`thinking_switch_e2e.rs` is the second); only the
//! visibility changed (`pub` on the items a test calls). Each test binary
//! compiles this module separately and uses a different subset of it,
//! hence the `dead_code` allowance.
#![allow(dead_code)]

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// What a single served request looks like once the mock has parsed
/// it. `path` is the request line target, `body` is the post-headers
/// payload. Headers are *not* surfaced to the test today — every
/// existing assertion is on the body or the path.
#[derive(Debug, Clone)]
pub struct ServedRequest {
    pub path: String,
    pub body: String,
}

/// Minimal canned response a test wants the mock to return.
#[derive(Debug, Clone)]
pub struct CannedResponse {
    pub status_line: &'static str,
    pub body: String,
}

impl CannedResponse {
    pub fn ok_json(body: impl Into<String>) -> Self {
        Self {
            status_line: "HTTP/1.1 200 OK",
            body: body.into(),
        }
    }
    pub fn server_error_text(body: impl Into<String>) -> Self {
        Self {
            status_line: "HTTP/1.1 500 Internal Server Error",
            body: body.into(),
        }
    }
}

/// Bind a one-shot HTTP/1.1 mock to an ephemeral port, return the
/// base URL the router should use plus a oneshot receiver that
/// fires with the parsed request once the mock has served it.
pub async fn spawn_one_shot_mock(
    canned: CannedResponse,
) -> (String, oneshot::Receiver<ServedRequest>) {
    // 127.0.0.1:0 → kernel assigns a free port. We read it back via
    // local_addr() so the test can compose the full URL the router
    // will dial. No race with other tests because the kernel only
    // hands out ports that aren't in use.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{port}");

    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let (mut sock, _peer) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("mock accept failed: {e}");
                return;
            }
        };

        // Read until we have headers + Content-Length bytes of body.
        // This is the bare minimum HTTP/1.1 to support the canonical
        // `POST /chat/completions HTTP/1.1\r\nContent-Type:
        // application/json\r\nContent-Length: N\r\n\r\n{...}` shape
        // reqwest produces.
        let mut buf = Vec::with_capacity(4096);
        let mut tmp = [0u8; 1024];
        loop {
            let n = sock.read(&mut tmp).await.expect("read socket");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(headers_end) = find_double_crlf(&buf) {
                let header_str = std::str::from_utf8(&buf[..headers_end])
                    .expect("headers are utf-8");
                let content_length = header_content_length(header_str).unwrap_or(0);
                let body_start = headers_end + 4; // past the CRLFCRLF
                let total_needed = body_start + content_length;
                if buf.len() >= total_needed {
                    // Parse the request line: "POST /chat/completions HTTP/1.1"
                    let request_line =
                        header_str.lines().next().unwrap_or("").to_string();
                    let path = request_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("")
                        .to_string();
                    let body = String::from_utf8(buf[body_start..total_needed].to_vec())
                        .expect("body is utf-8");
                    let _ = tx.send(ServedRequest { path, body });

                    // Write canned response back. We hand-format the
                    // headers because the body length varies per test.
                    let resp = format!(
                        "{status}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
                        status = canned.status_line,
                        len = canned.body.len(),
                        body = canned.body,
                    );
                    sock.write_all(resp.as_bytes())
                        .await
                        .expect("write response");
                    sock.flush().await.expect("flush");
                    let _ = sock.shutdown().await;
                    break;
                }
            }
            if buf.len() > 1 << 20 {
                // 1 MiB safety cap; our tests send tiny payloads.
                break;
            }
        }
    });

    (base_url, rx)
}

/// Find the byte index of `\r\n\r\n` if present (returns the index
/// of the first `\r`). Pure helper, kept inline to avoid a test-only
/// crate dep.
pub fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    if buf.len() < 4 {
        return None;
    }
    for i in 0..(buf.len() - 3) {
        if &buf[i..i + 4] == b"\r\n\r\n" {
            return Some(i);
        }
    }
    None
}

/// Parse the `Content-Length` header from a header block (case-
/// insensitive). Returns None if the header is missing or
/// non-numeric. Pure helper.
///
/// Lines without a `:` are skipped (the HTTP request line is the
/// canonical example). An earlier draft used `?` on the second
/// `splitn` token, which short-circuited the *whole* function on
/// the first colon-less line — silent bug that ate the Content-
/// Length header on every request.
pub fn header_content_length(headers: &str) -> Option<usize> {
    for line in headers.lines() {
        let mut parts = line.splitn(2, ':');
        let Some(name) = parts.next() else { continue };
        let Some(value) = parts.next() else { continue };
        if name.trim().eq_ignore_ascii_case("content-length") {
            return value.trim().parse().ok();
        }
    }
    None
}
