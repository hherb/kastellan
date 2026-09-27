//! Wire-level tests for the thinking-suppression dialect (#773) and the
//! request-timeout classification the forced-synthesis retry relies on
//! (#774).
//!
//! The pure decisions are unit-tested in `src/thinking/tests.rs`; these
//! pin what actually leaves the router over HTTP, because that is the
//! layer where #773 hid: every unit of the old switch was correct, and the
//! request it produced was simply one Ollama ignores.

use std::time::Duration;

use kastellan_llm_router::{
    ChatMessage, ChatRequest, Router, RouterConfig, ThinkingPolicy, ThinkingSwitch,
};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

mod common;
use common::{spawn_one_shot_mock, CannedResponse};

fn ok_body() -> String {
    serde_json::json!({
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop"}]
    })
    .to_string()
}

fn router(base_url: &str, disable_thinking: bool, switch: ThinkingSwitch, timeout: Duration) -> Router {
    Router::new(RouterConfig {
        local_url: base_url.to_string(),
        embedding_url: base_url.to_string(),
        timeout,
        disable_thinking,
        thinking_switch: switch,
        ..Default::default()
    })
    .expect("build router")
}

/// Send `req` through a router with the given settings and return the JSON
/// body the backend received.
async fn wire_body(disable_thinking: bool, switch: ThinkingSwitch, req: ChatRequest) -> serde_json::Value {
    let (base_url, served_rx) = spawn_one_shot_mock(CannedResponse::ok_json(ok_body())).await;
    router(&base_url, disable_thinking, switch, Duration::from_secs(2))
        .send(&req)
        .await
        .expect("send succeeds");
    let served = served_rx.await.expect("mock served the request");
    serde_json::from_str(&served.body).expect("body is JSON")
}

fn plain() -> ChatRequest {
    ChatRequest::new("m", vec![ChatMessage::user("hi")])
}

#[tokio::test]
async fn the_reasoning_effort_dialect_sends_only_reasoning_effort_none() {
    let body = wire_body(true, ThinkingSwitch::ReasoningEffort, plain()).await;
    assert_eq!(body["reasoning_effort"], "none", "{body}");
    assert!(body.get("chat_template_kwargs").is_none(), "vLLM 0.15 must never see both: {body}");
    assert!(body.get("thinking").is_none(), "the policy is not a wire field: {body}");
}

#[tokio::test]
async fn the_default_dialect_still_sends_exactly_the_kwarg() {
    // Byte-level continuity for every existing vLLM / llama.cpp deployment.
    let body = wire_body(true, ThinkingSwitch::default(), plain()).await;
    assert_eq!(body["chat_template_kwargs"], serde_json::json!({"enable_thinking": false}));
    assert!(body.get("reasoning_effort").is_none(), "{body}");
}

/// The forced-synthesis retry's path: the config lets the model think, the
/// request asks for suppression anyway, and the dialect key goes out.
#[tokio::test]
async fn a_per_request_suppress_overrides_a_thinking_config() {
    let req = plain().with_thinking(ThinkingPolicy::Suppress);
    let body = wire_body(false, ThinkingSwitch::ReasoningEffort, req).await;
    assert_eq!(body["reasoning_effort"], "none", "{body}");
}

#[tokio::test]
async fn a_per_request_allow_overrides_a_suppressing_config() {
    let req = plain().with_thinking(ThinkingPolicy::Allow);
    for switch in [ThinkingSwitch::ChatTemplateKwargs, ThinkingSwitch::ReasoningEffort] {
        let body = wire_body(true, switch, req.clone()).await;
        assert!(body.get("reasoning_effort").is_none(), "{body}");
        assert!(body.get("chat_template_kwargs").is_none(), "{body}");
    }
}

/// A backend that accepts the connection and never answers is the DGX's
/// slow-synthesis shape. It must classify as a request timeout — the one
/// error a cheaper retry can fix.
#[tokio::test]
async fn a_backend_that_never_answers_is_a_request_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base_url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    // Accept, read the request, then hold the socket open without replying.
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept");
        let mut buf = [0u8; 4096];
        let _ = sock.read(&mut buf).await;
        tokio::time::sleep(Duration::from_secs(10)).await;
        drop(sock);
    });

    let err = router(&base_url, true, ThinkingSwitch::default(), Duration::from_millis(300))
        .send(&plain())
        .await
        .expect_err("no response must time out");
    server.abort();
    assert!(err.is_request_timeout(), "expected a request timeout, got {err}");
    assert!(err.to_string().contains("[request timed out]"), "{err}");
}

/// A refused connection says nothing about the model's speed: not a
/// request timeout, so no retry is spent on it.
#[tokio::test]
async fn a_refused_connection_is_not_a_request_timeout() {
    // Bind then drop to get a port that is (very likely) closed.
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        l.local_addr().unwrap().port()
    };
    let err = router(
        &format!("http://127.0.0.1:{port}"),
        true,
        ThinkingSwitch::default(),
        Duration::from_secs(2),
    )
    .send(&plain())
    .await
    .expect_err("closed port must fail");
    assert!(!err.is_request_timeout(), "a connect failure is not a timeout: {err}");
}
