//! End-to-end test for the local-backend dispatch path.
//!
//! Brings up a hand-rolled tokio TcpListener that speaks just enough
//! HTTP/1.1 to canned-respond a single OpenAI-style chat-completion,
//! points a [`Router`] at it, and asserts the round-trip works:
//!
//! 1. **Happy path** — the request body decodes as the expected
//!    `ChatRequest`, the response decodes as a `ChatResponse`, and
//!    the router returns the assistant message.
//! 2. **HTTP error path** — backend returns 500; the router
//!    surfaces [`RouterError::HttpStatus`] with the captured body.
//! 3. **Decode error path** — backend returns 200 + garbage; the
//!    router surfaces [`RouterError::DecodeResponse`].
//!
//! The mock server is intentionally minimal: bind to `127.0.0.1:0`
//! to claim an ephemeral port, spawn a one-shot accept task, parse
//! the request as `<headers>\r\n\r\n<body>` with `Content-Length`,
//! and write a hand-formatted response. No `wiremock` /
//! `httpmock` / `axum` dev-dep — the dependency footprint stays
//! inspectable, matching the workspace style for `db/tests/postgres_e2e.rs`
//! and `core/tests/audit_dispatch_e2e.rs`.

use std::sync::Arc;
use std::time::Duration;

use hhagent_llm_router::{
    ChatMessage, ChatRequest, ChatRole, Router, RouterConfig, RouterError,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// What a single served request looks like once the mock has parsed
/// it. `path` is the request line target, `body` is the post-headers
/// payload. Headers are *not* surfaced to the test today — every
/// existing assertion is on the body or the path.
#[derive(Debug, Clone)]
struct ServedRequest {
    path: String,
    body: String,
}

/// Minimal canned response a test wants the mock to return.
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

/// Bind a one-shot HTTP/1.1 mock to an ephemeral port, return the
/// base URL the router should use plus a oneshot receiver that
/// fires with the parsed request once the mock has served it.
async fn spawn_one_shot_mock(
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

/// Parse the `Content-Length` header from a header block (case-
/// insensitive). Returns None if the header is missing or
/// non-numeric. Pure helper.
///
/// Lines without a `:` are skipped (the HTTP request line is the
/// canonical example). An earlier draft used `?` on the second
/// `splitn` token, which short-circuited the *whole* function on
/// the first colon-less line — silent bug that ate the Content-
/// Length header on every request.
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
        embedding_model: "embedding-default".into(),
        frontier_url: None,
        frontier_model: None,
        // Tight timeout so a hung mock fails the test fast rather than
        // waiting for the production 30 s default.
        timeout: Duration::from_secs(2),
    };
    Router::new(cfg).expect("build router")
}

#[tokio::test]
async fn happy_path_round_trips_request_and_response() {
    let canned_body = serde_json::json!({
        "id": "chatcmpl-mock-1",
        "object": "chat.completion",
        "created": 1_700_000_000_u64,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hello back"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
    })
    .to_string();
    let (base_url, served_rx) =
        spawn_one_shot_mock(CannedResponse::ok_json(canned_body)).await;

    let router = router_pointing_at(&base_url);
    let req = ChatRequest::new(
        "test-model",
        vec![
            ChatMessage::system("be terse"),
            ChatMessage::user("say hi"),
        ],
    );

    let resp = router.send(&req).await.expect("send succeeds");

    // Decode pin: the canonical envelope round-tripped end-to-end.
    assert_eq!(resp.id.as_deref(), Some("chatcmpl-mock-1"));
    assert_eq!(resp.choices.len(), 1);
    assert_eq!(resp.choices[0].message.role, ChatRole::Assistant);
    assert_eq!(resp.choices[0].message.content, "hello back");
    assert_eq!(resp.usage.unwrap().total_tokens, Some(7));

    // Path + body pin: the router POSTed to /chat/completions and
    // serialized the request the way `messages.rs` says it should.
    let served = served_rx.await.expect("mock served the request");
    assert_eq!(served.path, "/chat/completions");
    let parsed: ChatRequest =
        serde_json::from_str(&served.body).expect("body decodes as ChatRequest");
    assert_eq!(parsed.model, "test-model");
    assert_eq!(parsed.messages.len(), 2);
    assert_eq!(parsed.messages[0].role, ChatRole::System);
    assert_eq!(parsed.messages[1].role, ChatRole::User);
    assert!(parsed.max_tokens.is_none());
    // The skip_serializing_if pin from messages.rs: `max_tokens` and
    // `temperature` did *not* appear on the wire.
    assert!(!served.body.contains("max_tokens"));
    assert!(!served.body.contains("temperature"));
}

#[tokio::test]
async fn http_error_status_is_surfaced_with_truncated_body() {
    let (base_url, _served_rx) = spawn_one_shot_mock(CannedResponse::server_error_text(
        "{\"error\": {\"message\": \"the model is on fire\"}}".to_string(),
    ))
    .await;

    let router = router_pointing_at(&base_url);
    let req = ChatRequest::new(
        "test-model",
        vec![ChatMessage::user("trigger 500")],
    );
    let err = router.send(&req).await.expect_err("500 must propagate");
    match err {
        RouterError::HttpStatus { status, body } => {
            assert_eq!(status, 500);
            assert!(
                body.contains("the model is on fire"),
                "body did not preserve operator-readable text: {body:?}"
            );
        }
        other => panic!("expected HttpStatus, got {other:?}"),
    }
}

#[tokio::test]
async fn decode_error_is_surfaced_when_response_is_not_chat_response() {
    // 200 OK but body is JSON that doesn't match the ChatResponse
    // schema (no `choices` field). The router must treat this as
    // RouterError::DecodeResponse rather than silently substituting
    // an empty response.
    let (base_url, _served_rx) =
        spawn_one_shot_mock(CannedResponse::ok_json("{\"hello\": \"world\"}".to_string())).await;

    let router = router_pointing_at(&base_url);
    let req = ChatRequest::new("m", vec![ChatMessage::user("hi")]);
    let err = router
        .send(&req)
        .await
        .expect_err("decode failure must propagate");
    match err {
        RouterError::DecodeResponse { source, body } => {
            // The error message includes the bad JSON path; we only
            // assert the body was captured for triage.
            assert!(body.contains("hello"), "body missing in error: {body:?}");
            assert!(!source.to_string().is_empty());
        }
        other => panic!("expected DecodeResponse, got {other:?}"),
    }
}

#[tokio::test]
async fn router_send_routes_to_pick_backend_choice() {
    // This is a subtle pin: the policy gate runs *before* the HTTP
    // call. If a future refactor accidentally bypassed
    // `policy.pick(&request)`, the AlwaysFrontier policy here would
    // dispatch to the local URL anyway and the request would
    // succeed. We use the mock to ensure no HTTP traffic is sent
    // when the policy denies — the mock's `served_rx` should *not*
    // fire because the router refuses before reaching the wire.
    use hhagent_llm_router::{Backend, PolicyGate};

    #[derive(Debug)]
    struct AlwaysFrontier;
    impl PolicyGate for AlwaysFrontier {
        fn pick(&self, _request: &ChatRequest) -> Backend {
            Backend::Frontier
        }
    }

    let (base_url, served_rx) =
        spawn_one_shot_mock(CannedResponse::ok_json("{}".to_string())).await;
    let cfg = RouterConfig {
        local_url: base_url.clone(),
        local_model: "m".into(),
        embedding_url: base_url,
        embedding_model: "embedding-default".into(),
        frontier_url: Some("https://example.invalid/v1".into()),
        frontier_model: Some("frontier-model".into()),
        timeout: Duration::from_secs(2),
    };
    let router = Router::with_policy(cfg, Arc::new(AlwaysFrontier)).unwrap();

    let req = ChatRequest::new("m", vec![ChatMessage::user("hi")]);
    let err = router.send(&req).await.expect_err("frontier denied in Phase 0");
    assert!(matches!(err, RouterError::PolicyDeniedFrontier(_)));

    // The mock's accept loop is still pending; if the router had
    // erroneously dialed the local URL despite the frontier
    // decision, served_rx would have fired. tokio's
    // `try_recv` is the right shape: returns Empty when the channel
    // hasn't been written to.
    let mut served_rx = served_rx;
    assert!(
        matches!(served_rx.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
        "router dialed the local URL despite frontier policy"
    );
}
