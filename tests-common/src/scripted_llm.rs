//! Queued multi-shot mock LLM for daemon e2e tests: an OpenAI-compatible HTTP
//! listener that dispatches canned responses from a per-endpoint FIFO (embed vs.
//! chat), chosen by the request path. Lifted from `core/tests/cli_ask_e2e.rs`
//! (mail-worker live-test coverage) so more than one daemon e2e can drive a
//! scripted planner. The `ScriptedLlm` name distinguishes it from the inert
//! `daemon::MockLlm` (which 503s every request).

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Hard cap on inbound request bytes the mock will buffer before
/// giving up. Real chat-completion requests are a few KiB; 1 MiB is
/// generous headroom that defends against a buggy client pinning the
/// mock task in an unbounded read.
const MOCK_MAX_REQUEST_BYTES: usize = 1 << 20;

/// The kind of endpoint a captured request targets.
///
/// Used by the URL-routing mock to dispatch responses from the right
/// per-endpoint queue and to keep capture lists separate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndpointKind {
    Embedding,
    Chat,
    /// llama.cpp's `GET /props`, which is **not** OpenAI-compatible and
    /// is not a scripted turn.
    ///
    /// It reports a property of the backend (its per-request context
    /// window), so the mock answers every `/props` with the *same* body
    /// rather than popping one from a FIFO. A queue would be wrong in
    /// both directions: a second call would 503 where a real server
    /// answers, and a caller that never dials `/props` would leave an
    /// undrained queue that no assertion notices.
    Props,
}

/// Classify a request path into one of the three endpoint kinds the
/// daemon actually exercises.
///
/// Production paths look like `/v1/embeddings` and `/v1/chat/completions`,
/// but we deliberately match by substring rather than exact equality
/// — that way a future router refactor that changes the URL prefix
/// (or adds a trailing `?stream=false`) does not silently break this
/// classifier. Anything that contains `embeddings` is an embed request;
/// every other path is treated as a chat-completion. Pure: `&str → Kind`.
///
/// `/props` is matched **ahead of the `Chat` fall-through and with its
/// leading slash**, and both halves of that are deliberate:
///
/// * *Ahead of the fall-through*, because the `else` arm is `Chat` — a
///   `/props` GET that reached it would pop a chat envelope the caller
///   had counted for a plan iteration, desyncing the queue rather than
///   erroring. (Being *first* of the three is not itself load-bearing:
///   `/props` contains no `embeddings`, so the embed arm would decline
///   it either way. What matters is that it is matched at all.)
/// * *With the slash*, because the bare substring `props` also appears
///   in paths that are not this endpoint — `/v1/chat/completions?props=1`
///   is the realistic one — and misrouting such a path to the stored
///   props body starves the chat queue, the same silent desync in
///   reverse. `Router::props` composes the URL as
///   `<base minus /v1>/props`, so the leading slash is always there.
fn classify_endpoint(path: &str) -> EndpointKind {
    if path.contains("/props") {
        EndpointKind::Props
    } else if path.contains("embeddings") {
        EndpointKind::Embedding
    } else {
        EndpointKind::Chat
    }
}

/// Extract the request-target (path) from an HTTP request-line string,
/// e.g. `"POST /v1/embeddings HTTP/1.1"` → `"/v1/embeddings"`.
///
/// Returns `None` if the line doesn't split into at least three
/// whitespace-separated tokens. Pure: `&str → Option<&str>`.
fn parse_request_path(headers: &str) -> Option<&str> {
    let first_line = headers.lines().next()?;
    let mut parts = first_line.split_whitespace();
    let _method = parts.next()?;
    parts.next()
}

/// Multi-shot HTTP mock for the LLM router, dispatching by URL path.
///
/// Serves canned 200-OK JSON bodies from one of two queues — embedding
/// or chat-completion — chosen by the request's URL path. Each queue
/// is FIFO; once a queue is exhausted, every subsequent request to that
/// endpoint gets a `503 Service Unavailable` so an unexpected extra
/// LLM call surfaces as `RouterError::HttpStatus` in the daemon log
/// AND as a `tasks.state = "failed"` row in the test's final assertion
/// — i.e. loud, not silent.
///
/// **Why per-endpoint queues, not a single FIFO** — the daemon's
/// `PgRecallBuilder::build` issues an embed before the chat-completion
/// today, but that ordering is not load-bearing on production behaviour.
/// A single shared FIFO would desync silently if a future refactor
/// parallelises embed+chat (the chat handler pops an embedding body or
/// vice-versa) or if any new caller adds an extra embed somewhere
/// upstream. Two queues fail loudly: an unexpected dial-count mismatch
/// surfaces as a 503 on the correct endpoint, not a misleading body-
/// shape error in the consumer.
///
/// The accept loop runs forever (one connection at a time) until the
/// `JoinHandle` is aborted. `Drop` aborts it for us so the mock cannot
/// leak past the test boundary.
pub struct ScriptedLlm {
    pub base_url: String,
    /// Captured embedding-request bodies in arrival order. Useful for
    /// asserting the daemon dialed the embed endpoint N times.
    pub embed_requests: Arc<Mutex<Vec<String>>>,
    /// Captured chat-completion request bodies in arrival order.
    pub chat_requests: Arc<Mutex<Vec<String>>>,
    /// How many `GET /props` requests this listener **received**.
    ///
    /// Received, not answered, and the distinction is worth spelling
    /// out because it is invisible at the call site: the counter is
    /// incremented before the stored-body-or-503 decision, so a mock
    /// spawned by [`spawn_scripted_llm`] (which 503s every `/props`)
    /// reports the same 1 as one that served a context window. **A
    /// `props_requests` assertion is therefore evidence that the dial
    /// happened, never that the guard tier booted** — pair it with an
    /// assertion on the resulting row, as `guard_boot_row_e2e` does.
    ///
    /// A **count**, not a body list like its two neighbours, because a
    /// `GET` carries no body — there is nothing to capture but the fact
    /// that it happened. That fact is worth an assertion: one daemon
    /// boot verifies the guard context exactly once, so a value other
    /// than 1 means a retry loop or a second tier construction, neither
    /// of which any other signal in a test would show.
    pub props_requests: Arc<Mutex<usize>>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for ScriptedLlm {
    fn drop(&mut self) {
        if let Some(h) = self.join.take() {
            h.abort();
        }
    }
}

/// Spawn a mock that serves the two OpenAI-compatible endpoints and
/// **503s every `/props`**.
///
/// The 503 is the right default rather than an oversight: a caller that
/// has not thought about `/props` is not configuring a guard tier, and
/// a daemon that dials it anyway should fail loudly at boot rather than
/// receive a fabricated context window.
///
/// ⚠️ **Loudly, but not fast** (#609): a configured guard tier treats a
/// 503 on `/props` as a backend still loading its model and re-asks for
/// up to `PROPS_WAIT_BUDGET_MS` (60 s) before refusing to boot. A test
/// that wants a prompt `PropsUnavailable` needs a backend that answers
/// something else (a 404, or a body that is not JSON).
pub async fn spawn_scripted_llm(
    embed_responses: Vec<String>,
    chat_responses: Vec<String>,
) -> ScriptedLlm {
    spawn_scripted_llm_with_props(embed_responses, chat_responses, None).await
}

/// Spawn a mock that additionally answers `GET /props` with `props_body`.
///
/// Needed by any test that boots a **configured guard tier**:
/// `GuardTier::from_router_config` treats `/props` as its one fatal
/// network call, so without an answer here the daemon exits before it
/// writes a `guard_tier.boot` row at all. Two distinct errors stop it,
/// and a reader debugging a dead daemon wants to know which:
/// an unreachable or unparseable `/props` is
/// `GuardTierError::PropsUnavailable` — the 503 this function's `None`
/// default produces — while a context below `REQUIRED_GUARD_N_CTX` is
/// D8's `GuardTierError::Context`.
///
/// `None` for `props_body` is exactly [`spawn_scripted_llm`].
///
/// **Point the guard at its own listener, not the planner's.** A
/// configured tier adjudicates tool output on every dispatch, and those
/// are chat-completions like any other — sharing one mock would have
/// the guard silently eating envelopes a test counted out for plan
/// iterations.
pub async fn spawn_scripted_llm_with_props(
    embed_responses: Vec<String>,
    chat_responses: Vec<String>,
    props_body: Option<String>,
) -> ScriptedLlm {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{port}");

    let embed_queue = Arc::new(Mutex::new(embed_responses));
    let embed_queue_for_task = embed_queue.clone();
    let chat_queue = Arc::new(Mutex::new(chat_responses));
    let chat_queue_for_task = chat_queue.clone();
    let embed_requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let embed_requests_for_task = embed_requests.clone();
    let chat_requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let chat_requests_for_task = chat_requests.clone();
    let props_requests = Arc::new(Mutex::new(0_usize));
    let props_requests_for_task = props_requests.clone();

    let join = tokio::spawn(async move {
        loop {
            let (mut sock, _peer) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => return,
            };

            let mut buf = Vec::with_capacity(4096);
            let mut tmp = [0u8; 1024];
            // Two outputs from the read loop: the request body string
            // (for capture) and the URL kind (for dispatch). `None` on
            // either means "malformed / truncated — serve 503 and move
            // on" rather than panicking.
            let parsed: Option<(EndpointKind, String)> = loop {
                let n = match sock.read(&mut tmp).await {
                    Ok(n) => n,
                    Err(_) => break None,
                };
                if n == 0 {
                    break None;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(headers_end) = find_double_crlf(&buf) {
                    let header_str = match std::str::from_utf8(&buf[..headers_end]) {
                        Ok(s) => s,
                        Err(_) => break None,
                    };
                    let kind = parse_request_path(header_str)
                        .map(classify_endpoint)
                        .unwrap_or(EndpointKind::Chat);
                    let content_length = header_content_length(header_str).unwrap_or(0);
                    let body_start = headers_end + 4;
                    let total_needed = body_start + content_length;
                    if buf.len() >= total_needed {
                        match String::from_utf8(buf[body_start..total_needed].to_vec()) {
                            Ok(b) => break Some((kind, b)),
                            Err(_) => break None,
                        }
                    }
                }
                if buf.len() > MOCK_MAX_REQUEST_BYTES {
                    break None;
                }
            };

            // Capture into the per-endpoint list and dequeue the next
            // canned response from the matching queue. Each endpoint
            // has its own FIFO so an unexpected extra dial to one side
            // surfaces as a 503 on that side, not a body-shape error
            // on the other.
            let next: Option<String> = if let Some((kind, body)) = parsed {
                match kind {
                    EndpointKind::Embedding => {
                        embed_requests_for_task.lock().unwrap().push(body);
                        let mut q = embed_queue_for_task.lock().unwrap();
                        if q.is_empty() { None } else { Some(q.remove(0)) }
                    }
                    EndpointKind::Chat => {
                        chat_requests_for_task.lock().unwrap().push(body);
                        let mut q = chat_queue_for_task.lock().unwrap();
                        if q.is_empty() { None } else { Some(q.remove(0)) }
                    }
                    // Counted, then answered from the same stored body
                    // every time — see `EndpointKind::Props`. `body` is
                    // the empty string for a `GET` and is deliberately
                    // not captured.
                    EndpointKind::Props => {
                        *props_requests_for_task.lock().unwrap() += 1;
                        props_body.clone()
                    }
                }
            } else {
                None
            };

            let resp = match next {
                Some(body) => format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                ),
                None => {
                    let empty = "{}";
                    format!(
                        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        empty.len(),
                        empty,
                    )
                }
            };

            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
            let _ = sock.shutdown().await;
        }
    });

    ScriptedLlm {
        base_url,
        embed_requests,
        chat_requests,
        props_requests,
        join: Some(join),
    }
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

/// Wrap `plan_json` in an OpenAI-compatible chat-completion envelope.
pub fn envelope_for(plan_json_string: &str) -> String {
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

/// Build a llama.cpp `/props` body reporting `n_ctx` as the
/// **per-request** context window.
///
/// The nesting under `default_generation_settings` is not decoration:
/// the live DGX guard server reports the per-request context *only*
/// there and carries no top-level `n_ctx` at all, so a mock that put it
/// at the root would pass a test the real backend fails.
/// `context_pin::n_ctx_from_props` reads the nested key first and falls
/// back to the root, which is why this envelope pins the harder of the
/// two shapes.
///
/// `model_path` and `total_slots` ride along because a real `/props`
/// carries them; nothing on the tier's boot path reads either (the
/// weights pin lives in `guard calibrate`), and they are here so the
/// fixture looks like the thing it stands in for.
pub fn props_envelope(n_ctx: u64) -> String {
    serde_json::json!({
        "default_generation_settings": {"n_ctx": n_ctx},
        "total_slots": 1,
        "model_path": "/models/shieldstral-test.gguf"
    })
    .to_string()
}

/// Build an OpenAI-compatible embedding response envelope.
///
/// `PgRecallBuilder::build` calls `embed_query` (→ `router.embed`) once
/// per plan iteration, BEFORE the chat-completion call. The mock now
/// dispatches by URL path, so the embed queue holds one envelope per
/// expected embed dial and the chat queue holds one envelope per
/// expected plan-iteration — independent of call ordering.
///
/// `embed_query` Matryoshka-truncates the returned embedding to
/// `EMBEDDING_DIM` (256) elements; a vector at least that long succeeds
/// (this 768-long filler mirrors embeddinggemma's native width), while
/// a shorter one causes a `MemoryError::EmbeddingDimMismatch` that
/// triggers the degrade-and-warn path in `formulate_plan`. The byte
/// values don't matter for these tests: the `memories` table is never
/// seeded, so both recall lanes return 0 rows regardless of the query
/// vector. Using `0.001` (a small non-zero value) keeps the embedding
/// numerically well-defined for pgvector's cosine operator without
/// relying on any implementation-defined behaviour for the all-zeros
/// edge case.
pub fn embedding_envelope() -> String {
    let filler: Vec<f32> = vec![0.001f32; 768];
    serde_json::json!({
        "object": "list",
        "data": [{"object": "embedding", "index": 0, "embedding": filler}],
        "model": "test-local-model"
    })
    .to_string()
}

/// Convenience JSON builder for the plan body the planner emits as the
/// assistant message content.
pub fn plan_json(
    decision: &str,
    steps: serde_json::Value,
    result: Option<serde_json::Value>,
) -> String {
    let mut obj = serde_json::json!({
        "context":      "test context",
        "decision":     decision,
        "rationale":    "test rationale",
        "steps":        steps,
        "data_ceiling": "Public",
    });
    if let Some(r) = result {
        obj.as_object_mut().unwrap().insert("result".into(), r);
    } else {
        obj.as_object_mut()
            .unwrap()
            .insert("result".into(), serde_json::Value::Null);
    }
    obj.to_string()
}

// ---------------------------------------------------------------------------
// Unit tests for the URL-routing dispatcher helpers.
//
// The daemon e2e tests that consume this module skip on hosts without a
// supervisor / sandbox / Postgres toolchain, so the load-bearing classifier +
// path parser get their coverage from these in-module unit tests. Keep them
// here so the helpers and their pins live in one file.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod mock_router_unit_tests {
    use super::*;

    #[test]
    fn classify_endpoint_routes_embeddings_paths_to_embedding() {
        assert_eq!(classify_endpoint("/v1/embeddings"), EndpointKind::Embedding);
        assert_eq!(classify_endpoint("/embeddings"), EndpointKind::Embedding);
        // Query string / version drift defends against a future router
        // refactor that adds extra suffix bytes.
        assert_eq!(
            classify_endpoint("/v2/embeddings?stream=false"),
            EndpointKind::Embedding,
        );
    }

    #[test]
    fn classify_endpoint_routes_props_paths_to_props() {
        // The shape `Router::props` actually composes: the `/v1` compat
        // segment stripped, `/props` at the server root.
        //
        // This is also the whole of the fall-through hazard. Without a
        // `Props` arm the same path lands on `Chat` and pops an envelope
        // the caller had counted for a plan iteration — a desynced queue
        // rather than an error, which is the failure mode the
        // per-endpoint queues exist to avoid. An `assert_ne!` against
        // `Chat` would restate this one rather than add to it.
        assert_eq!(classify_endpoint("/props"), EndpointKind::Props);
    }

    #[test]
    fn classify_endpoint_does_not_treat_props_lookalikes_as_props() {
        // The mirror hazard, and the reason the match carries its
        // leading slash. Both of these contain the bare substring
        // `props`, so a `contains("props")` classifier answers them from
        // the stored props body and starves the chat queue instead —
        // each therefore fails under that mutant, which is what makes
        // them worth asserting.
        //
        // (`/v1/properties` is NOT such a case and is deliberately not
        // here: `properties` is `p-r-o-p-e-…`, so it contains no `props`
        // at all and would pass under either spelling.)
        assert_eq!(
            classify_endpoint("/v1/chat/completions?props=1"),
            EndpointKind::Chat,
        );
        assert_eq!(classify_endpoint("/v1/models?props=true"), EndpointKind::Chat);
    }

    #[test]
    fn classify_endpoint_defaults_unknown_paths_to_chat() {
        assert_eq!(
            classify_endpoint("/v1/chat/completions"),
            EndpointKind::Chat,
        );
        // No "embeddings" substring → falls through to Chat.
        assert_eq!(classify_endpoint("/v1/anything-else"), EndpointKind::Chat);
        assert_eq!(classify_endpoint("/"), EndpointKind::Chat);
    }

    #[test]
    fn parse_request_path_extracts_the_target_from_a_request_line() {
        let headers = "POST /v1/embeddings HTTP/1.1\r\nHost: localhost\r\n";
        assert_eq!(parse_request_path(headers), Some("/v1/embeddings"));
    }

    #[test]
    fn parse_request_path_handles_chat_completions_target() {
        let headers = "POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\n";
        assert_eq!(parse_request_path(headers), Some("/v1/chat/completions"));
    }

    #[test]
    fn parse_request_path_returns_none_for_malformed_input() {
        // Single-token request line — no path field.
        assert_eq!(parse_request_path("GET"), None);
        // Empty input.
        assert_eq!(parse_request_path(""), None);
    }
}
