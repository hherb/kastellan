//! End-to-end test for the local-backend embedding dispatch path.
//!
//! Same hand-rolled `tokio::net::TcpListener` mock as
//! `local_backend_e2e.rs`. Four cases:
//!
//!   1. Happy path — request body decodes as the expected
//!      `EmbeddingRequest`, response decodes as an
//!      `EmbeddingResponse`, router returns the single embedding.
//!   2. Count mismatch — backend returns `data: []`; router surfaces
//!      `RouterError::EmbeddingCountMismatch { requested: 1, returned: 0 }`.
//!   3. HTTP error — backend returns 500; router surfaces
//!      `RouterError::HttpStatus { status: 500, body }` (truncated).
//!   4. Decode error — backend returns 200 + bad JSON; router
//!      surfaces `RouterError::DecodeResponse`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::time::Duration;

use hhagent_llm_router::embeddings::{EmbeddingRequest, EmbeddingResponse};
use hhagent_llm_router::{Router, RouterConfig, RouterError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

// ---- Mock helpers (copied verbatim from local_backend_e2e.rs;
//      hoist tracked in issue #15) ------------------------------------

#[derive(Debug, Clone)]
struct ServedRequest {
    path: String,
    body: String,
}

#[derive(Debug, Clone)]
struct CannedResponse {
    status_line: &'static str,
    body: String,
}

impl CannedResponse {
    fn ok_json(body: impl Into<String>) -> Self {
        Self {
            status_line: "HTTP/1.1 200 OK",
            body: body.into(),
        }
    }
    fn server_error_text(body: impl Into<String>) -> Self {
        Self {
            status_line: "HTTP/1.1 500 Internal Server Error",
            body: body.into(),
        }
    }
}

async fn spawn_one_shot_mock(
    canned: CannedResponse,
) -> (String, oneshot::Receiver<ServedRequest>) {
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
                let body_start = headers_end + 4;
                let total_needed = body_start + content_length;
                if buf.len() >= total_needed {
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
                break;
            }
        }
    });
    (base_url, rx)
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
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

fn header_content_length(headers: &str) -> Option<usize> {
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

fn router_pointing_at(base_url: &str) -> Router {
    let cfg = RouterConfig {
        local_url: base_url.to_string(),
        local_model: "local-default".into(),
        embedding_url: base_url.to_string(),
        embedding_model: "embedding-test".into(),
        frontier_url: None,
        frontier_model: None,
        timeout: Duration::from_secs(2),
    };
    Router::new(cfg).expect("build router")
}

// ---- The four tests ---------------------------------------------------

#[tokio::test]
async fn embed_happy_path_round_trips_request_and_response() {
    let canned = serde_json::json!({
        "object": "list",
        "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3]}],
        "model": "embedding-test",
        "usage": {"prompt_tokens": 4, "total_tokens": 4}
    });
    let (base, served) = spawn_one_shot_mock(CannedResponse::ok_json(canned.to_string())).await;
    let r = router_pointing_at(&base);

    let req = EmbeddingRequest::single("embedding-test", "hello");
    let resp: EmbeddingResponse = r.embed(&req).await.expect("happy path");
    assert_eq!(resp.data.len(), 1);
    assert_eq!(resp.data[0].embedding, vec![0.1_f32, 0.2, 0.3]);
    assert_eq!(resp.model.as_deref(), Some("embedding-test"));

    let served = served.await.expect("mock served");
    assert_eq!(served.path, "/embeddings");
    assert!(served.body.contains("\"model\":\"embedding-test\""), "body: {}", served.body);
    assert!(served.body.contains("\"input\":[\"hello\"]"), "body: {}", served.body);
}

#[tokio::test]
async fn embed_count_mismatch_when_backend_returns_zero_entries() {
    let canned = serde_json::json!({"data": []});
    let (base, _served) = spawn_one_shot_mock(CannedResponse::ok_json(canned.to_string())).await;
    let r = router_pointing_at(&base);

    let req = EmbeddingRequest::single("embedding-test", "hello");
    let err = r.embed(&req).await.expect_err("must mismatch");
    match err {
        RouterError::EmbeddingCountMismatch { requested, returned } => {
            assert_eq!(requested, 1);
            assert_eq!(returned, 0);
        }
        other => panic!("expected EmbeddingCountMismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn embed_http_error_status_is_surfaced_with_truncated_body() {
    let big = "x".repeat(2048); // > ERROR_BODY_CAP (1 KiB)
    let (base, _served) = spawn_one_shot_mock(CannedResponse::server_error_text(big)).await;
    let r = router_pointing_at(&base);

    let req = EmbeddingRequest::single("embedding-test", "hello");
    let err = r.embed(&req).await.expect_err("must error");
    match err {
        RouterError::HttpStatus { status, body } => {
            assert_eq!(status, 500);
            assert!(body.ends_with("…[truncated]"), "body: {body}");
            assert!(body.len() <= 1024 + 14, "len={} body={}", body.len(), body);
        }
        other => panic!("expected HttpStatus, got {other:?}"),
    }
}

#[tokio::test]
async fn embed_decode_error_when_body_is_not_embedding_response() {
    let (base, _served) = spawn_one_shot_mock(CannedResponse::ok_json(
        "{\"unexpected\": \"shape\"}".to_string(),
    ))
    .await;
    let r = router_pointing_at(&base);

    let req = EmbeddingRequest::single("embedding-test", "hello");
    let err = r.embed(&req).await.expect_err("must error");
    match err {
        RouterError::DecodeResponse { body, .. } => {
            assert!(body.contains("unexpected"), "body: {body}");
        }
        other => panic!("expected DecodeResponse, got {other:?}"),
    }
}
