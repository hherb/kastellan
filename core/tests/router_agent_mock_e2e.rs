//! End-to-end test for the scheduler's `RouterAgent` against a
//! hand-rolled mock HTTP backend.
//!
//! `RouterAgent::formulate_plan` is the only production path that
//! turns a `TaskContext` into a `Plan`. Every other test in the
//! scheduler suite swaps it out for a `ScriptedFormulator`, so this
//! file is what pins:
//!
//! 1. **Happy path** — backend returns a `choices[0].message.content`
//!    that is a valid `Plan` JSON; `formulate_plan` returns
//!    `Ok((plan, meta))` and `FormulationMeta` carries the prompt
//!    name + sha256 the inner-loop audit row needs.
//! 2. **Decode error** — backend returns a `choices[0].message.content`
//!    that is *not* valid JSON; `formulate_plan` returns
//!    `Err(AgentError::Decode { detail, raw })`. A silent default or
//!    panic here would corrupt the audit trail and let bad plans
//!    propagate.
//! 3. **Missing prompt** — the in-memory `PromptCache` does not have
//!    `agent_planner` loaded; `formulate_plan` returns
//!    `Err(AgentError::PromptMissing)` without dialing the backend.
//!
//! Mock-server style matches `llm-router/tests/local_backend_e2e.rs`:
//! a one-shot tokio TcpListener that speaks just enough HTTP/1.1 to
//! canned-respond a single chat-completion. No `wiremock` /
//! `httpmock` / `axum` dev-dep — the dependency footprint stays
//! inspectable and the test is self-contained.
//!
//! Runtime: default `#[tokio::test]` (current-thread). This is fine
//! today because `Router::send` is pure async — it does not call
//! `tokio::task::block_in_place`. If a future refactor wraps a sync
//! call here (e.g. fetching a frontier API key from `db::secrets`
//! via `block_in_place`), these tests will need
//! `#[tokio::test(flavor = "multi_thread")]` or they will panic at
//! runtime ("can call blocking only when running on the multi-threaded runtime").

use std::sync::Arc;
use std::time::Duration;

use hhagent_core::cassandra::types::DataClass;
use hhagent_core::prompt_assembly::StaticSystemPromptBuilder;
use hhagent_core::recall_assembly::StaticRecallBuilder;
use hhagent_core::scheduler::agent::{AgentError, PlanFormulator, RouterAgent};
use hhagent_core::scheduler::inner_loop::TaskContext;
use hhagent_core::scheduler::prompts::{PromptCache, PromptEntry};
use hhagent_db::tasks::Lane;
use hhagent_llm_router::{Router, RouterConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

// ---------------------------------------------------------------------------
// Mock server boilerplate — mirrors llm-router/tests/local_backend_e2e.rs.
// Deliberately duplicated rather than hoisted into a shared dev-dep until
// the broader fixture refactor lands (Issue #15).
// ---------------------------------------------------------------------------

// Intentionally slimmer than `local_backend_e2e.rs`'s `ServedRequest`
// (no `path` field) — this test only needs to introspect the body to
// pin the cached prompt was sent. If #15 hoists the helpers into a
// shared fixture, this struct should adopt the richer shape.
#[derive(Debug, Clone)]
struct ServedRequest {
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
}

/// Hard cap on inbound request bytes the mock will buffer before
/// giving up. Real chat-completion requests are a few KiB; 1 MiB is
/// generous headroom and a defensive guard so a buggy client cannot
/// pin the mock task in unbounded reads.
const MOCK_MAX_REQUEST_BYTES: usize = 1 << 20;

/// Returns the base URL, a oneshot that fires when the request body
/// has been served, and the spawned task's `JoinHandle`. The handle
/// matters for the "never-dialed" assertion in
/// `prompt_missing_short_circuits_before_dialing_backend`: that test
/// must `abort()` the handle so the still-pending `accept().await`
/// does not leak past the test boundary. Tests that *expect* the mock
/// to serve a request can let the handle drop — the task exits
/// naturally once it has served + responded + shut down the socket.
async fn spawn_one_shot_mock(
    canned: CannedResponse,
) -> (String, oneshot::Receiver<ServedRequest>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{port}");

    let (tx, rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
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
                    let body = String::from_utf8(buf[body_start..total_needed].to_vec())
                        .expect("body is utf-8");
                    let _ = tx.send(ServedRequest { body });

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
            if buf.len() > MOCK_MAX_REQUEST_BYTES {
                break;
            }
        }
    });

    (base_url, rx, handle)
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

// ---------------------------------------------------------------------------
// Construction helpers
// ---------------------------------------------------------------------------

fn router_pointing_at(base_url: &str) -> Arc<Router> {
    let cfg = RouterConfig {
        local_url: base_url.to_string(),
        local_model: "test-local-model".into(),
        embedding_url: base_url.to_string(),
        embedding_model: "embedding-default".into(),
        frontier_url: None,
        frontier_model: None,
        timeout: Duration::from_secs(2),
    };
    Arc::new(Router::new(cfg).expect("build router"))
}

const PLANNER_PROMPT_CONTENT: &str = "you are a planner; emit a Plan JSON";
// 64 hex chars — shape-realistic so a future audit-writer that validates
// the sha256 column format (length / hex-only) doesn't reject this stub.
const PLANNER_PROMPT_SHA: &str =
    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn prompts_with_agent_planner() -> Arc<PromptCache> {
    Arc::new(PromptCache::new_for_test(vec![(
        "agent_planner".into(),
        PromptEntry {
            sha256: PLANNER_PROMPT_SHA.into(),
            content: PLANNER_PROMPT_CONTENT.into(),
        },
    )]))
}

fn empty_prompts() -> Arc<PromptCache> {
    Arc::new(PromptCache::new_for_test(vec![]))
}

fn ctx() -> TaskContext {
    TaskContext {
        task_id: 7,
        lane: Lane::Fast,
        instruction: "ping".into(),
        classification_floor: DataClass::Public,
        classification_floor_source: hhagent_core::scheduler::inner_loop::ClassificationFloorSource::Default,
        classification_floor_signals: vec![],
        plans: vec![],
        advisories: vec![],
        blocks: vec![],
        plan_count: 0,
        max_plans: 3,
    }
}

/// Wrap `plan_json` in an OpenAI-compatible ChatResponse envelope so
/// the mock can return it as the backend's response body.
fn envelope_for(plan_json_string: &str) -> String {
    serde_json::json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "created": 1_700_000_000_u64,
        "model": "test-local-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": plan_json_string},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn happy_path_decodes_plan_and_populates_meta() {
    // The agent emits a valid Plan as its assistant-message content.
    let plan_json = serde_json::json!({
        "context":    "ping handler",
        "decision":   "task_complete",
        "rationale":  "trivial",
        "steps":      [],
        "result":     {"kind": "text", "body": "pong"},
        "data_ceiling": "Public",
    })
    .to_string();

    let (base_url, served_rx, _mock_join) =
        spawn_one_shot_mock(CannedResponse::ok_json(envelope_for(&plan_json))).await;

    let router = router_pointing_at(&base_url);
    let prompts = prompts_with_agent_planner();
    // StaticSystemPromptBuilder::new(PLANNER_PROMPT_CONTENT): no L0/L1 added,
    // but the cached base prompt flows to the wire verbatim.
    let agent = RouterAgent::new(
        router,
        prompts,
        Arc::new(StaticSystemPromptBuilder::new(PLANNER_PROMPT_CONTENT)),
        Arc::new(StaticRecallBuilder::empty()),
        Arc::new(hhagent_core::entity_extraction::NoOpEntityExtractor::new()),
    );

    let (plan, meta) = agent.formulate_plan(&ctx()).await.expect("happy path");

    // Plan decode round-trip — the terminal flag is checked structurally,
    // so all three conditions must survive.
    assert!(plan.is_terminal(), "plan should be terminal: {plan:?}");
    assert_eq!(plan.decision, "task_complete");
    assert_eq!(
        plan.result.as_ref().and_then(|v| v.get("body")).and_then(|v| v.as_str()),
        Some("pong"),
    );

    // FormulationMeta carries the prompt-traceability fields the inner
    // loop writes into the `plan.formulate` audit-log payload. A
    // regression on any of these breaks CASSANDRA correlation.
    assert_eq!(meta.prompt_name, "agent_planner");
    assert_eq!(meta.prompt_sha256, PLANNER_PROMPT_SHA);
    assert_eq!(meta.llm_model, "test-local-model");
    assert_eq!(meta.llm_backend, "local");
    assert_eq!(meta.retry_count, 0);
    // latency is non-deterministic; allow zero (sub-millisecond on fast
    // hosts), but a value above the router timeout would mean the agent
    // measured something unrelated to this call.
    assert!(
        meta.latency_ms < 60_000,
        "latency_ms wildly out of range: {}",
        meta.latency_ms
    );

    // Recall fields (slice D 2026-05-17): StaticRecallBuilder::empty()
    // returns no rows + the canonical SHA-256 of the empty string.
    assert!(meta.recalled_memory_ids.is_empty(),
        "StaticRecallBuilder::empty() → no recalled ids; got {:?}", meta.recalled_memory_ids);
    assert_eq!(meta.recall_count, 0,
        "StaticRecallBuilder::empty() → recall_count 0; got {}", meta.recall_count);
    assert_eq!(meta.recall_query_sha256.len(), 64,
        "recall_query_sha256 must always be 64 hex chars; got len {}", meta.recall_query_sha256.len());

    // The system prompt sent on the wire was the cached prompt content
    // verbatim — pin via the served body. (We don't pin the full
    // request shape because that's `local_backend_e2e.rs`'s job.)
    let served = served_rx.await.expect("mock served the request");
    assert!(
        served.body.contains(PLANNER_PROMPT_CONTENT),
        "system prompt missing from wire: {}",
        served.body
    );
}

#[tokio::test]
async fn decode_error_when_assistant_content_is_not_a_plan() {
    // Backend returns an envelope whose assistant message is plain
    // text (not a JSON plan). `formulate_plan` must surface this as
    // `AgentError::Decode`, NOT panic and NOT substitute a default
    // plan — the inner loop's failure-handling depends on the typed
    // error.
    let envelope = envelope_for("not a plan, just chatter");
    let (base_url, served_rx, _mock_join) =
        spawn_one_shot_mock(CannedResponse::ok_json(envelope)).await;

    let router = router_pointing_at(&base_url);
    let prompts = prompts_with_agent_planner();
    // StaticSystemPromptBuilder::new(PLANNER_PROMPT_CONTENT): no L0/L1 added,
    // but the cached base prompt flows to the wire verbatim.
    let agent = RouterAgent::new(
        router,
        prompts,
        Arc::new(StaticSystemPromptBuilder::new(PLANNER_PROMPT_CONTENT)),
        Arc::new(StaticRecallBuilder::empty()),
        Arc::new(hhagent_core::entity_extraction::NoOpEntityExtractor::new()),
    );

    let err = agent
        .formulate_plan(&ctx())
        .await
        .expect_err("malformed plan must surface as Decode");
    match err {
        AgentError::Decode { detail, raw } => {
            assert!(
                raw.contains("not a plan"),
                "raw body must be preserved for triage: {raw:?}"
            );
            assert!(
                !detail.is_empty(),
                "detail must carry the underlying serde_json error: {detail:?}"
            );
        }
        other => panic!("expected AgentError::Decode, got {other:?}"),
    }

    // Sanity: the agent reached the network before decoding failed —
    // otherwise the Decode error would be masking a different bug
    // (e.g. an early short-circuit that swallows the typed error).
    let served = served_rx.await.expect("mock served the request");
    assert!(
        served.body.contains(PLANNER_PROMPT_CONTENT),
        "system prompt missing from wire: {}",
        served.body
    );
}

#[tokio::test]
async fn prompt_missing_short_circuits_before_dialing_backend() {
    // No `agent_planner` in the cache → `formulate_plan` must return
    // PromptMissing without touching the network. The mock's
    // served_rx is the witness: if the agent dialed the backend
    // anyway, the oneshot would fire.
    let (base_url, served_rx, mock_join) = spawn_one_shot_mock(CannedResponse::ok_json(
        envelope_for("never sent"),
    ))
    .await;

    let router = router_pointing_at(&base_url);
    let prompts = empty_prompts();
    let agent = RouterAgent::new(
        router,
        prompts,
        Arc::new(StaticSystemPromptBuilder::empty()),
        Arc::new(StaticRecallBuilder::empty()),
        Arc::new(hhagent_core::entity_extraction::NoOpEntityExtractor::new()),
    );

    let err = agent.formulate_plan(&ctx()).await.expect_err("must fail");
    assert!(
        matches!(err, AgentError::PromptMissing),
        "expected PromptMissing, got {err:?}"
    );

    // `try_recv` returns Empty iff the mock's accept loop never
    // received a connection — i.e. the agent never dialed.
    //
    // Invariant this relies on: `formulate_plan` is synchronous-to-
    // completion w.r.t. the network — `router.send().await` is awaited
    // inline, not spawned. If a future refactor moves dialing into a
    // `tokio::spawn`, this assertion will silently false-pass; switch
    // to a short timeout-bounded `recv` then.
    let mut served_rx = served_rx;
    assert!(
        matches!(
            served_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty),
        ),
        "RouterAgent dialed the backend despite missing prompt"
    );

    // The mock task is still parked in `accept().await`. Abort it so
    // the still-pending listener does not leak past the test
    // boundary. (For the other two tests the task exits naturally
    // after serving + responding + shutting down the socket; no
    // explicit abort is needed there.)
    mock_join.abort();
}
