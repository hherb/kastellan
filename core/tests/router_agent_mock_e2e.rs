//! End-to-end test for the scheduler's `RouterAgent` against a
//! hand-rolled mock HTTP backend.
//!
//! `RouterAgent::notebook_turn` is the only production path that turns a
//! `TaskContext` into a planner request. Every other scheduler test swaps it
//! out for a scripted double, so this file is what pins:
//!
//! 1. **Happy path** — the reply text comes back verbatim, and
//!    `FormulationMeta` carries the prompt name + sha256, the model and the
//!    seeds the `plan.formulate` audit row needs.
//! 2. **The re-ask** — the refused reply and the reason go on the wire after
//!    the turn, with thinking suppressed.
//! 3. **Missing prompt** — the in-memory `PromptCache` does not have
//!    `agent_notebook` loaded; the turn returns
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

use kastellan_core::cassandra::types::DataClass;
use kastellan_core::entity_extraction::{SeedSource, StaticEntityExtractor};
use kastellan_core::prompt_assembly::StaticSystemPromptBuilder;
use kastellan_core::recall_assembly::{
    RecallBuilder, RecallError, RecalledContext, StaticRecallBuilder,
};
use kastellan_core::scheduler::agent::{AgentError, RouterAgent};
use kastellan_core::scheduler::inner_loop::TaskContext;
use kastellan_core::scheduler::notebook::formulator::{NotebookFormulator, NOTEBOOK_PROMPT};
use kastellan_core::scheduler::prompts::{PromptCache, PromptEntry};
use kastellan_db::tasks::Lane;
use kastellan_llm_router::{Router, RouterConfig, ThinkingPolicy};
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
/// `a_notebook_turn_with_no_prompt_never_dials_the_backend`: that test
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
        guard_url: None,
        guard_model: None,
        guard_tau: None,
        guard_timeout_ms: None,
        timeout: Duration::from_secs(2),
        disable_thinking: true,
        thinking_switch: Default::default(),
    };
    Arc::new(Router::new(cfg).expect("build router"))
}

const PLANNER_PROMPT_CONTENT: &str = "you are a planner; emit a Plan JSON";
// 64 hex chars — shape-realistic so a future audit-writer that validates
// the sha256 column format (length / hex-only) doesn't reject this stub.
const PLANNER_PROMPT_SHA: &str =
    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn empty_prompts() -> Arc<PromptCache> {
    Arc::new(PromptCache::new_for_test(vec![]))
}

fn ctx() -> TaskContext {
    TaskContext {
        task_id: 7,
        lane: Lane::Fast,
        instruction: "ping".into(),
        classification_floor: DataClass::Public,
        classification_floor_source: kastellan_core::scheduler::inner_loop::ClassificationFloorSource::Default,
        classification_floor_signals: vec![],
        plans: vec![],
        advisories: vec![],
        blocks: vec![],
        plan_count: 0,
        max_plans: 3,
        deadline: None,
        resolved_asks: Vec::new(),
        origin: None,
        conversation: Vec::new(),
        conversation_task_ids: Some(Vec::new()),
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

/// Shared slot holding the `(query, seeds)` a [`CapturingRecallBuilder`]
/// observed. Named alias rather than the inline `Arc<Mutex<Option<…>>>`
/// to stay under `clippy::type_complexity` (the `-D warnings` gate).
type CapturedCall = Arc<std::sync::Mutex<Option<(String, Vec<i64>)>>>;

/// A [`RecallBuilder`] that records the `(query, seeds)` it was called
/// with so a test can assert what `RouterAgent::notebook_turn` passed
/// into [`RecallBuilder::build_with_seeds`].
///
/// The production [`StaticRecallBuilder`] deliberately *discards* its
/// `seeds` argument (it returns a fixed context regardless of input), so
/// it cannot pin the seed-threading. This double exists purely to
/// capture that argument; the returned context is always empty because
/// the recall *content* is irrelevant to the seed-threading pin.
#[derive(Clone, Default)]
struct CapturingRecallBuilder {
    /// `Some((query, seeds))` after the first `build_with_seeds` call.
    /// `Arc<Mutex<_>>` so the test holds a clone and can read what the
    /// agent passed once `notebook_turn` returns.
    captured: CapturedCall,
}

#[async_trait::async_trait]
impl RecallBuilder for CapturingRecallBuilder {
    async fn build_with_seeds(
        &self,
        query: &str,
        seeds: &[i64],
    ) -> Result<RecalledContext, RecallError> {
        *self.captured.lock().expect("captured mutex poisoned") =
            Some((query.to_string(), seeds.to_vec()));
        Ok(RecalledContext::empty())
    }
}

// ---------------------------------------------------------------------------
// Tests — the notebook's door, `RouterAgent::notebook_turn` (#922, #993).
// ---------------------------------------------------------------------------

/// A prompt cache holding only the notebook's prompt.
fn prompts_with_agent_notebook() -> Arc<PromptCache> {
    Arc::new(PromptCache::new_for_test(vec![(
        NOTEBOOK_PROMPT.into(),
        PromptEntry { sha256: PLANNER_PROMPT_SHA.into(), content: PLANNER_PROMPT_CONTENT.into() },
    )]))
}

#[tokio::test]
async fn a_notebook_turn_threads_extracted_seeds_into_recall_and_meta() {
    let reply = r#"{"steps": [{"id": 1, "purpose": "p", "tool": "none", "done_when": "d"}], "answer": {"body": "pong"}}"#;
    let (base_url, served_rx, _mock_join) = spawn_one_shot_mock(CannedResponse::ok_json(envelope_for(reply))).await;
    let recall = CapturingRecallBuilder::default();
    let captured = recall.captured.clone();
    let seed_ids = vec![101_i64, 202, 303];
    let agent = RouterAgent::new(
        router_pointing_at(&base_url),
        prompts_with_agent_notebook(),
        Arc::new(StaticSystemPromptBuilder::new(PLANNER_PROMPT_CONTENT)),
        Arc::new(recall),
        Arc::new(StaticEntityExtractor::with_ids(seed_ids.clone())),
    );

    let task = ctx();
    let (raw, meta) = agent
        .notebook_turn(&task, &|_| "the turn".to_string(), None, ThinkingPolicy::RouterDefault)
        .await
        .expect("happy path");
    assert_eq!(raw, reply, "the model's text comes back as it was sent");

    // The extracted seeds reach recall verbatim, with the instruction as the
    // query — drop the thread and the graph lane is a no-op — and the meta the
    // `plan.formulate` row is written from.
    let (seen_query, seen_seeds) = captured.lock().unwrap().clone().expect("recall builder must have been called");
    assert_eq!((seen_query.as_str(), seen_seeds.as_slice()), (task.instruction.as_str(), seed_ids.as_slice()));
    assert_eq!((meta.graph_seed_entity_ids, meta.graph_seed_count), (seed_ids.clone(), seed_ids.len() as u32));
    assert!(matches!(meta.graph_seed_source, SeedSource::GlinerRelex), "{:?}", meta.graph_seed_source);
    // The prompt-traceability fields the `plan.formulate` row carries; a
    // regression on any of these breaks CASSANDRA correlation.
    assert_eq!(meta.prompt_name, NOTEBOOK_PROMPT);
    assert_eq!(meta.prompt_sha256, PLANNER_PROMPT_SHA);
    assert_eq!((meta.llm_model.as_str(), meta.llm_backend.as_str()), ("test-local-model", "local"));
    assert_eq!(meta.retry_count, 0);
    assert!(meta.latency_ms < 60_000, "latency_ms wildly out of range: {}", meta.latency_ms);
    assert_eq!(meta.recall_query_sha256.len(), 64, "always 64 hex chars");
    let served = served_rx.await.expect("mock served the request");
    assert!(served.body.contains(PLANNER_PROMPT_CONTENT), "system prompt missing from wire: {}", served.body);
    // #941: every planner call caps its reply.
    assert!(served.body.contains("\"max_tokens\":16384"), "max_tokens missing from wire: {}", served.body);
}

/// The #945 re-ask on the wire: after the turn, the model's refused reply as
/// the assistant's, then the reason and the request for the reply again; with
/// `Suppress`, thinking is off even though this router's config lets the
/// model think.
#[tokio::test]
async fn a_notebook_re_ask_shows_the_reply_and_the_reason_with_thinking_off() {
    let (base_url, served_rx, _mock_join) = spawn_one_shot_mock(CannedResponse::ok_json(envelope_for("{}"))).await;
    let router = Arc::new(
        Router::new(RouterConfig {
            local_url: base_url,
            local_model: "test-local-model".into(),
            timeout: Duration::from_secs(2),
            disable_thinking: false,
            ..Default::default()
        })
        .expect("build router"),
    );
    let agent = RouterAgent::new(
        router,
        prompts_with_agent_notebook(),
        Arc::new(StaticSystemPromptBuilder::new(PLANNER_PROMPT_CONTENT)),
        Arc::new(StaticRecallBuilder::empty()),
        Arc::new(kastellan_core::entity_extraction::NoOpEntityExtractor::new()),
    );
    let reply = "Here are the payments due: …";
    agent
        .notebook_turn(&ctx(), &|_| "the turn".to_string(), Some((reply, "no `tick`")), ThinkingPolicy::Suppress)
        .await
        .expect("the re-ask is sent");

    let served = served_rx.await.expect("mock served the request");
    let body: serde_json::Value = serde_json::from_str(&served.body).expect("request is JSON");
    let messages = body["messages"].as_array().expect("messages array");
    let roles: Vec<&str> = messages.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["system", "user", "assistant", "user"], "{}", served.body);
    assert_eq!(messages[1]["content"], "the turn");
    assert_eq!(messages[2]["content"], reply, "shown its own reply verbatim");
    let ask = messages[3]["content"].as_str().unwrap();
    assert!(ask.starts_with("Your previous reply could not be used: no `tick`"), "{ask}");
    assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false, "thinking suppressed: {}", served.body);
}

#[tokio::test]
async fn a_notebook_turn_with_no_prompt_never_dials_the_backend() {
    let (base_url, served_rx, mock_join) = spawn_one_shot_mock(CannedResponse::ok_json(envelope_for("never sent"))).await;
    let agent = RouterAgent::new(
        router_pointing_at(&base_url),
        empty_prompts(),
        Arc::new(StaticSystemPromptBuilder::empty()),
        Arc::new(StaticRecallBuilder::empty()),
        Arc::new(kastellan_core::entity_extraction::NoOpEntityExtractor::new()),
    );
    let err = agent
        .notebook_turn(&ctx(), &|_| "the turn".to_string(), None, ThinkingPolicy::RouterDefault)
        .await
        .expect_err("must fail");
    assert!(
        matches!(&err, AgentError::PromptMissing(name) if name == "agent_notebook"),
        "expected PromptMissing naming the notebook prompt, got {err:?}"
    );
    // Empty iff the mock never accepted a connection (the send is awaited
    // inline).
    let mut served_rx = served_rx;
    assert!(matches!(served_rx.try_recv(), Err(oneshot::error::TryRecvError::Empty)), "dialed despite no prompt");
    mock_join.abort();
}
