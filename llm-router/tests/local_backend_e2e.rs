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

use kastellan_llm_router::logprob_score::{
    binary_token_probability, first_position_alternatives, NO_FORMS, YES_FORMS,
};
use kastellan_llm_router::{
    ChatMessage, ChatRequest, ChatRole, Router, RouterConfig, RouterError,
};
use tokio::sync::oneshot;

mod common;
use common::{spawn_one_shot_mock, CannedResponse};

fn router_pointing_at(base_url: &str) -> Router {
    router_pointing_at_with_thinking_disabled(base_url, true)
}

/// `router_pointing_at` with the thinking-suppression switch chosen
/// explicitly. The no-argument form keeps the *production* default
/// (`true`) so these tests exercise the payload real deployments send.
fn router_pointing_at_with_thinking_disabled(
    base_url: &str,
    disable_thinking: bool,
) -> Router {
    let cfg = RouterConfig {
        local_url: base_url.to_string(),
        local_model: "local-default".into(),
        embedding_url: base_url.to_string(),
        embedding_model: "embedding-default".into(),
        frontier_url: None,
        frontier_model: None,
        guard_url: None,
        guard_model: None,
        guard_tau: None,
        guard_timeout_ms: None,
        // Tight timeout so a hung mock fails the test fast rather than
        // waiting for the production 30 s default.
        timeout: Duration::from_secs(2),
        disable_thinking,
        thinking_switch: Default::default(),
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
    // ...but the thinking-suppression kwarg DID, because the router was
    // built with the production default. The caller never asked for it:
    // `dispatch_local` adds it on the local leg.
    assert_eq!(
        parsed.chat_template_kwargs,
        Some(serde_json::json!({"enable_thinking": false})),
        "expected the local leg to suppress thinking: {}",
        served.body
    );
}

/// A classifier call end-to-end: the request carries the logprobs pair
/// on the wire, and the response's distribution comes back far enough to
/// be scored.
///
/// This exists on top of the pure unit tests for the reason #566
/// established — pure-function tests alone cannot catch a *reverted
/// wiring*. `binary_token_probability` can be perfect while
/// `with_logprobs` never reaches the socket, and every test in
/// `logprob_score` would still pass.
///
/// The canned body is the shape measured against DGX Ollama 0.22.0 on
/// 2026-08-16, trimmed to two alternatives.
#[tokio::test]
async fn logprobs_round_trip_and_score() {
    let canned_body = serde_json::json!({
        "id": "chatcmpl-mock-lp",
        "model": "guard-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "yes"},
            "finish_reason": "length",
            "logprobs": {"content": [{
                "token": "yes",
                "logprob": -0.1,
                "bytes": [121, 101, 115],
                "top_logprobs": [
                    {"token": "yes", "logprob": -0.1,  "bytes": [121, 101, 115]},
                    {"token": "no",  "logprob": -2.3,  "bytes": [110, 111]}
                ]
            }]}
        }]
    })
    .to_string();
    let (base_url, served_rx) =
        spawn_one_shot_mock(CannedResponse::ok_json(canned_body)).await;

    let router = router_pointing_at(&base_url);
    let req = ChatRequest::new("guard-model", vec![ChatMessage::user("<Query>: unsafe?")])
        .with_logprobs(20);
    let resp = router.send(&req).await.expect("send succeeds");

    // Request side: both fields reached the socket, as the pair vLLM needs.
    let served = served_rx.await.expect("mock served the request");
    let parsed: ChatRequest =
        serde_json::from_str(&served.body).expect("body decodes as ChatRequest");
    assert_eq!(parsed.logprobs, Some(true), "wire body: {}", served.body);
    assert_eq!(parsed.top_logprobs, Some(20), "wire body: {}", served.body);

    // Response side: the distribution survived decoding and scores.
    let alternatives =
        first_position_alternatives(&resp).expect("alternatives decoded");
    assert_eq!(alternatives.len(), 2);
    let score = binary_token_probability(alternatives, YES_FORMS, NO_FORMS)
        .expect("both verdict spellings present");
    // sigmoid(-0.1 - -2.3) = sigmoid(2.2)
    assert!((score - 0.9002_f32).abs() < 1e-3, "unexpected score {score}");
}

/// The switch is what decides, not the call site: with it off the
/// payload must be exactly what the caller built. Without this arm a
/// hard-coded `without_thinking()` in `dispatch_local` would pass every
/// other test in this file.
#[tokio::test]
async fn thinking_kwarg_is_absent_when_the_switch_is_off() {
    let canned_body = serde_json::json!({
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop"
        }]
    })
    .to_string();
    let (base_url, served_rx) =
        spawn_one_shot_mock(CannedResponse::ok_json(canned_body)).await;

    let router = router_pointing_at_with_thinking_disabled(&base_url, false);
    let req = ChatRequest::new("test-model", vec![ChatMessage::user("say hi")]);
    router.send(&req).await.expect("send succeeds");

    let served = served_rx.await.expect("mock served the request");
    assert!(
        !served.body.contains("chat_template_kwargs"),
        "switch was off but the kwarg was still sent: {}",
        served.body
    );
}

/// An explicit caller-set value must survive: the config fills a gap,
/// it does not overwrite intent.
#[tokio::test]
async fn caller_supplied_chat_template_kwargs_are_not_overwritten() {
    let canned_body = serde_json::json!({
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop"
        }]
    })
    .to_string();
    let (base_url, served_rx) =
        spawn_one_shot_mock(CannedResponse::ok_json(canned_body)).await;

    // Switch ON, but the caller already expressed a (contrary) intent.
    let router = router_pointing_at_with_thinking_disabled(&base_url, true);
    let mut req = ChatRequest::new("test-model", vec![ChatMessage::user("say hi")]);
    req.chat_template_kwargs = Some(serde_json::json!({"enable_thinking": true}));
    router.send(&req).await.expect("send succeeds");

    let served = served_rx.await.expect("mock served the request");
    let parsed: ChatRequest =
        serde_json::from_str(&served.body).expect("body decodes as ChatRequest");
    assert_eq!(
        parsed.chat_template_kwargs,
        Some(serde_json::json!({"enable_thinking": true})),
        "config clobbered the caller's explicit kwargs: {}",
        served.body
    );
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
    use kastellan_llm_router::{Backend, PolicyGate};

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
        guard_url: None,
        guard_model: None,
        guard_tau: None,
        guard_timeout_ms: None,
        timeout: Duration::from_secs(2),
        disable_thinking: true,
        thinking_switch: Default::default(),
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
