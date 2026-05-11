# Embedding Router (Option O) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire a free-text query through a new `Router::embed` method to the existing semantic-recall lane, writing the first `actor='llm:router'` audit-log row in the system.

**Architecture:** New embedding HTTP path inside `hhagent-llm-router` (in core's process, mirroring the existing `Router::send` precedent — no sandboxed worker). New caller helper `core::memory::embed_query(pool, router, text)` that calls `Router::embed`, validates dim, writes the audit row, returns `Vec<f32>`. `recall`'s signature is unchanged; callers compose `embed_query` then `recall`.

**Tech Stack:** Rust 1.75+, sqlx, reqwest (rustls), thiserror, serde, tokio. Per-test Postgres cluster for the full-flow e2e (PG 18 + pgvector, peer auth on UDS). Hand-rolled `tokio::net::TcpListener` mock for backend HTTP (no `httpmock` / `wiremock` dev-dep). Spec: [`docs/superpowers/specs/2026-05-11-embedding-router-design.md`](../specs/2026-05-11-embedding-router-design.md).

---

## File Structure

**New files:**
- `llm-router/src/embeddings.rs` — wire shapes (`EmbeddingRequest`, `EmbeddingData`, `EmbeddingResponse`)
- `llm-router/tests/embedding_backend_e2e.rs` — 4 integration tests against TCP mock
- `core/tests/embedding_recall_e2e.rs` — 4 integration tests against per-test PG + TCP mock

**Modified files:**
- `llm-router/src/lib.rs` — add `pub mod embeddings`, re-exports, `Router::embed`, constant `EMBEDDINGS_PATH`
- `llm-router/src/config.rs` — `embedding_url`, `embedding_model` fields + env-var wiring
- `llm-router/src/policy.rs` — `PolicyGate::pick_embed` default method
- `llm-router/src/error.rs` — `RouterError::EmbeddingCountMismatch`
- `core/src/memory.rs` — `MemoryError`, `build_embed_audit_payload`, `embed_query`
- `docs/devel/handovers/HANDOVER.md` — "Recently completed" entry, test-count bump
- `docs/devel/ROADMAP.md` — tick Phase 1 "Embedding worker" item

**Pattern references (read these before implementing):**
- `llm-router/src/lib.rs:158-208` — `Router::send` / `dispatch_local`: the pattern `Router::embed` mirrors
- `llm-router/tests/local_backend_e2e.rs:68-183` — `spawn_one_shot_mock`, `find_double_crlf`, `header_content_length`: copy verbatim into the new test file (issue #15 already tracks the duplication)
- `core/tests/memory_recall_e2e.rs:1-130, 292-310` — per-test PG bring-up + `text_to_embedding` helper: the e2e test extends this pattern
- `core/src/scheduler/agent.rs:51-117` — `RouterAgent::formulate_plan`: the precedent for "caller writes its own audit row, payload omits sensitive content"

---

## Task 1: Embedding wire shapes

**Files:**
- Create: `llm-router/src/embeddings.rs`
- Modify: `llm-router/src/lib.rs` (add `pub mod embeddings;`)

- [ ] **Step 1.1: Write the failing tests**

Create `llm-router/src/embeddings.rs` with this content:

```rust
//! OpenAI-compatible embedding request and response types.
//!
//! Wire shapes for `POST <base>/embeddings`. The endpoint is supported
//! identically by vLLM, SGLang, Ollama's OpenAI-compat front door, and
//! `text-embeddings-inference` / Infinity. All four backends accept
//! the array form of `input` even for a single string, so we pin
//! `Vec<String>` rather than a string-or-list enum.
//!
//! ## Why we omit `encoding_format` and `dimensions`
//! OpenAI's spec carries optional `encoding_format` (`"float"` or
//! `"base64"`) and `dimensions` (Matryoshka-truncation target).
//! Phase 1 always wants float arrays at the model's native dim
//! (bge-m3 = 1024), so we don't serialise either. Adding them later
//! is a backwards-compatible additive change (existing backends
//! already treat them as optional).

use serde::{Deserialize, Serialize};

use crate::messages::Usage;

/// Outgoing embedding request.
///
/// `input` is always a JSON array. Callers with a single text wrap it
/// in `vec![text.into()]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingRequest {
    pub model: String,
    pub input: Vec<String>,
}

impl EmbeddingRequest {
    /// Common-case constructor for a single text.
    pub fn single(model: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            input: vec![text.into()],
        }
    }
}

/// One embedding entry in the response `data` array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingData {
    /// 0-based position matching the corresponding `input[i]`.
    /// `serde(default)` because some backends omit it for single-input
    /// requests.
    #[serde(default)]
    pub index: u32,
    pub embedding: Vec<f32>,
}

/// Decoded `200 OK` response from `POST /embeddings`.
///
/// `model` and `usage` are `Option` because Ollama omits them when
/// the underlying GGUF runtime didn't surface them; vLLM always
/// emits both.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingResponse {
    pub data: Vec<EmbeddingData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn embedding_request_serializes_canonical_shape() {
        let req = EmbeddingRequest::single("bge-m3", "hello world");
        let s = serde_json::to_string(&req).unwrap();
        // Wire shape pin: exactly `model` + `input` fields. No
        // `encoding_format`, no `dimensions`, no nulls.
        assert!(s.contains("\"model\":\"bge-m3\""), "model missing: {s}");
        assert!(s.contains("\"input\":[\"hello world\"]"), "input missing: {s}");
        assert!(!s.contains("encoding_format"), "encoding_format leaked: {s}");
        assert!(!s.contains("dimensions"), "dimensions leaked: {s}");
    }

    #[test]
    fn embedding_request_input_is_array_even_for_single_string() {
        // Defensive: serde_json could theoretically be configured to
        // unwrap single-element Vec into a bare value. Pin the JSON
        // array form because not every backend handles the
        // OpenAI-spec "string-or-list" union when given a bare string.
        let req = EmbeddingRequest::single("m", "x");
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"input\":[\"x\"]"), "input not array: {s}");
    }

    #[test]
    fn embedding_response_decodes_canonical_vllm_envelope() {
        let raw = json!({
            "object": "list",
            "data": [
                {"object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3]}
            ],
            "model": "BAAI/bge-m3",
            "usage": {"prompt_tokens": 4, "total_tokens": 4}
        });
        let resp: EmbeddingResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.data.len(), 1);
        assert_eq!(resp.data[0].index, 0);
        assert_eq!(resp.data[0].embedding, vec![0.1, 0.2, 0.3]);
        assert_eq!(resp.model.as_deref(), Some("BAAI/bge-m3"));
        let u = resp.usage.unwrap();
        assert_eq!(u.prompt_tokens, Some(4));
    }

    #[test]
    fn embedding_response_decodes_minimal_envelope_without_model_or_usage() {
        // Some backends omit `model` and `usage` entirely. Decoder
        // must tolerate (serde(default) + skip_serializing_if).
        let raw = json!({
            "data": [{"embedding": [0.0, 1.0]}]
        });
        let resp: EmbeddingResponse = serde_json::from_value(raw).unwrap();
        assert!(resp.model.is_none());
        assert!(resp.usage.is_none());
        assert_eq!(resp.data[0].index, 0); // default
    }

    #[test]
    fn embedding_response_decodes_batch_envelope_preserving_order() {
        // Multi-text request: data[].index matches input position.
        let raw = json!({
            "data": [
                {"index": 0, "embedding": [0.1]},
                {"index": 1, "embedding": [0.2]},
                {"index": 2, "embedding": [0.3]}
            ]
        });
        let resp: EmbeddingResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.data.len(), 3);
        assert_eq!(resp.data[0].index, 0);
        assert_eq!(resp.data[1].index, 1);
        assert_eq!(resp.data[2].index, 2);
    }

    #[test]
    fn embedding_data_index_defaults_to_zero_when_absent() {
        let raw = json!({"embedding": [1.0, 2.0]});
        let d: EmbeddingData = serde_json::from_value(raw).unwrap();
        assert_eq!(d.index, 0);
        assert_eq!(d.embedding, vec![1.0, 2.0]);
    }
}
```

Add the module registration to `llm-router/src/lib.rs`. Modify the `pub mod` block (currently lines 52-56):

```rust
pub mod backend;
pub mod config;
pub mod embeddings;          // NEW
pub mod error;
pub mod messages;
pub mod policy;
```

- [ ] **Step 1.2: Run tests to verify they pass (no Router::embed yet, but the wire shapes are independent)**

Run:
```sh
source "$HOME/.cargo/env"
cargo test -p hhagent-llm-router --lib embeddings
```

Expected: 6 tests pass (the 6 in the `#[cfg(test)] mod tests` block above).

- [ ] **Step 1.3: Run the workspace test suite — no regressions**

Run:
```sh
cargo test --workspace
```

Expected: 305 passed (was 299 + 6 new unit tests in this task), 0 failed.

- [ ] **Step 1.4: Commit**

```sh
git add llm-router/src/embeddings.rs llm-router/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(llm-router): OpenAI-compat embedding wire shapes (Option O step 1)

New module `embeddings.rs` ships `EmbeddingRequest`, `EmbeddingData`,
`EmbeddingResponse`. Pinned by 6 serde unit tests: canonical
serialization, single-string-always-array, vLLM full envelope,
minimal envelope without model/usage, batch order, index default.

No Router method yet — wire shapes are independent and testable on
their own.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: `RouterError::EmbeddingCountMismatch`

**Files:**
- Modify: `llm-router/src/error.rs` (add one variant, one test)

- [ ] **Step 2.1: Write the failing test**

Append to the `#[cfg(test)] mod tests` block in `llm-router/src/error.rs`:

```rust
#[test]
fn embedding_count_mismatch_error_carries_expected_and_returned() {
    let err = RouterError::EmbeddingCountMismatch {
        requested: 3,
        returned: 2,
    };
    let msg = err.to_string();
    assert!(msg.contains("3"), "requested missing: {msg}");
    assert!(msg.contains("2"), "returned missing: {msg}");
    // Field-shape pin: matching by name proves the variant carries the
    // expected fields, not just positional placeholders.
    if let RouterError::EmbeddingCountMismatch { requested, returned } = err {
        assert_eq!(requested, 3);
        assert_eq!(returned, 2);
    } else {
        panic!("wrong variant");
    }
}
```

- [ ] **Step 2.2: Run test to verify it fails**

Run:
```sh
cargo test -p hhagent-llm-router --lib error::tests::embedding_count_mismatch
```

Expected: build error — `RouterError::EmbeddingCountMismatch` variant does not exist.

- [ ] **Step 2.3: Add the variant**

In `llm-router/src/error.rs`, add a new variant to the `RouterError` enum (after `PolicyDeniedFrontier`, before the closing `}`):

```rust
    #[error("embedding response carried {returned} entries, requested {requested}")]
    EmbeddingCountMismatch { requested: usize, returned: usize },
```

- [ ] **Step 2.4: Run test to verify it passes**

```sh
cargo test -p hhagent-llm-router --lib error::tests
```

Expected: 4 tests pass (3 existing + 1 new).

- [ ] **Step 2.5: Commit**

```sh
git add llm-router/src/error.rs
git commit -m "$(cat <<'EOF'
feat(llm-router): RouterError::EmbeddingCountMismatch (Option O step 2)

Typed error for "backend returned the wrong number of embedding
vectors." Fires inside Router::embed when
`response.data.len() != request.input.len()`. Pinned by 1 unit test.

EmbeddingDimMismatch is deliberately NOT in RouterError — the router
has no canonical expected dim. Dim validation lives in
core::memory::embed_query (Task 6+7).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: `RouterConfig` embedding fields + env vars

**Files:**
- Modify: `llm-router/src/config.rs` (add 2 fields, 2 env-var reads, 4 tests)

- [ ] **Step 3.1: Write the failing tests**

Append to the `#[cfg(test)] mod tests` block in `llm-router/src/config.rs`:

```rust
#[test]
fn router_config_default_embedding_model_is_embedding_default() {
    let cfg = RouterConfig::default();
    assert_eq!(cfg.embedding_model, "embedding-default");
}

#[test]
fn router_config_default_embedding_url_falls_back_to_local_url() {
    // No env vars touched here; the constructor default uses the
    // per-OS default for *both* local_url and embedding_url so a
    // Ollama-on-macOS deployment works with one URL set.
    let cfg = RouterConfig::default();
    assert_eq!(cfg.embedding_url, cfg.local_url);
}

#[test]
fn router_config_from_env_reads_embedding_url_when_set() {
    // Use the existing env-lock helper if your codebase ships one;
    // until then, a thread-local std::env::set_var/remove_var pair is
    // accepted (matches existing tests in this module).
    std::env::remove_var("HHAGENT_LLM_LOCAL_URL");
    std::env::set_var("HHAGENT_LLM_EMBEDDING_URL", "http://127.0.0.1:9999/v1");
    let cfg = RouterConfig::from_env().expect("env parse");
    assert_eq!(cfg.embedding_url, "http://127.0.0.1:9999/v1");
    std::env::remove_var("HHAGENT_LLM_EMBEDDING_URL");
}

#[test]
fn router_config_from_env_reads_embedding_model_when_set() {
    std::env::remove_var("HHAGENT_LLM_LOCAL_MODEL");
    std::env::set_var("HHAGENT_LLM_EMBEDDING_MODEL", "BAAI/bge-m3");
    let cfg = RouterConfig::from_env().expect("env parse");
    assert_eq!(cfg.embedding_model, "BAAI/bge-m3");
    std::env::remove_var("HHAGENT_LLM_EMBEDDING_MODEL");
}
```

**Note on env-test isolation:** existing tests in `config.rs` modify process env directly. If your branch has multiple env-touching tests, they may race when run in parallel. If you see flakes, mark these `#[ignore]` and run them serially with `cargo test -- --test-threads=1`, or use the existing `db::env_lock` mutex pattern (cf. `db/src/env_lock.rs` if present in this branch).

- [ ] **Step 3.2: Run tests to verify they fail**

```sh
cargo test -p hhagent-llm-router --lib config::tests::router_config_default_embedding
```

Expected: build error — `embedding_url` / `embedding_model` fields do not exist on `RouterConfig`.

- [ ] **Step 3.3: Add the fields and env wiring**

In `llm-router/src/config.rs`, modify the `RouterConfig` struct (lines 65-75) to add two fields:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterConfig {
    pub local_url: String,
    pub local_model: String,
    /// Base URL for the embedding backend. Defaults to `local_url`
    /// so a single OpenAI-compat server (Ollama, vLLM with both chat
    /// and embed loaded) works without setting two env vars.
    pub embedding_url: String,
    /// Default model name passed in the `model` field of
    /// `POST /embeddings`. Defaults to `"embedding-default"` — a
    /// placeholder that vLLM will reject with 4xx in production,
    /// forcing the operator to set `HHAGENT_LLM_EMBEDDING_MODEL`
    /// explicitly (loud failure preferred to silent fallback).
    pub embedding_model: String,
    pub frontier_url: Option<String>,
    pub frontier_model: Option<String>,
    pub timeout: Duration,
}
```

Add a constant near `DEFAULT_LOCAL_MODEL` (line 43):

```rust
pub const DEFAULT_EMBEDDING_MODEL: &str = "embedding-default";
```

Modify the `Default` impl (line 77-87) to populate the new fields:

```rust
impl Default for RouterConfig {
    fn default() -> Self {
        let default_url = default_local_url_for_os().to_string();
        Self {
            local_url: default_url.clone(),
            local_model: DEFAULT_LOCAL_MODEL.to_string(),
            embedding_url: default_url,
            embedding_model: DEFAULT_EMBEDDING_MODEL.to_string(),
            frontier_url: None,
            frontier_model: None,
            timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
        }
    }
}
```

Modify `from_env` (line 96-116) to read the two new env vars. Place these reads **after** the `local_url` and `local_model` reads so the fallback semantics are right:

```rust
    pub fn from_env() -> Result<Self, RouterError> {
        let mut cfg = Self::default();

        if let Some(v) = read_env("HHAGENT_LLM_LOCAL_URL")? {
            cfg.local_url = v.clone();
            // local_url change also drives the embedding fallback —
            // re-sync embedding_url unless the operator has already
            // overridden it explicitly below.
            cfg.embedding_url = v;
        }
        if let Some(v) = read_env("HHAGENT_LLM_LOCAL_MODEL")? {
            cfg.local_model = v;
        }
        if let Some(v) = read_env("HHAGENT_LLM_EMBEDDING_URL")? {
            cfg.embedding_url = v;
        }
        if let Some(v) = read_env("HHAGENT_LLM_EMBEDDING_MODEL")? {
            cfg.embedding_model = v;
        }
        cfg.frontier_url = read_env("HHAGENT_LLM_FRONTIER_URL")?;
        cfg.frontier_model = read_env("HHAGENT_LLM_FRONTIER_MODEL")?;
        if let Some(v) = read_env("HHAGENT_LLM_TIMEOUT_MS")? {
            let ms: u64 = v.parse().map_err(|_| {
                RouterError::Config(format!(
                    "HHAGENT_LLM_TIMEOUT_MS must be a non-negative integer, got {v:?}"
                ))
            })?;
            cfg.timeout = Duration::from_millis(ms);
        }
        Ok(cfg)
    }
```

Also update the module-level env-vars doc table (top of file, around lines 19-25) to list the two new variables.

- [ ] **Step 3.4: Run tests to verify they pass**

```sh
cargo test -p hhagent-llm-router --lib config::tests
```

Expected: 4 new tests pass (plus existing tests still pass).

- [ ] **Step 3.5: Run the workspace test suite — verify config_e2e routers still build**

```sh
cargo test --workspace
```

Expected: 0 regressions. Test count up by 4 from Task 2.

- [ ] **Step 3.6: Commit**

```sh
git add llm-router/src/config.rs
git commit -m "$(cat <<'EOF'
feat(llm-router): embedding_url/embedding_model fields + env (Option O step 3)

Two new fields on RouterConfig with two new env vars:
- HHAGENT_LLM_EMBEDDING_URL — falls back to HHAGENT_LLM_LOCAL_URL,
  then per-OS default. Lets Ollama-on-macOS work with one URL set.
- HHAGENT_LLM_EMBEDDING_MODEL — defaults to "embedding-default"
  placeholder that vLLM rejects with 4xx, forcing operators to set
  the production model explicitly.

Pinned by 4 unit tests: default-model-pin, default-url-fallback,
env-reads-url, env-reads-model.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: `PolicyGate::pick_embed` default method

**Files:**
- Modify: `llm-router/src/policy.rs` (add default trait method, 2 tests)

- [ ] **Step 4.1: Write the failing tests**

Append to the `#[cfg(test)] mod tests` block in `llm-router/src/policy.rs`:

```rust
#[test]
fn default_local_policy_pick_embed_returns_local() {
    use crate::embeddings::EmbeddingRequest;
    let p = DefaultLocalPolicy;
    let req = EmbeddingRequest::single("m", "hi");
    assert_eq!(p.pick_embed(&req), Backend::Local);
}

#[test]
fn custom_policy_inherits_pick_embed_default_when_only_pick_is_overridden() {
    use crate::embeddings::EmbeddingRequest;
    // A test-only impl that only defines `pick`; `pick_embed` must
    // come from the trait default and return Local.
    #[derive(Debug)]
    struct AlwaysFrontierChat;
    impl PolicyGate for AlwaysFrontierChat {
        fn pick(&self, _request: &ChatRequest) -> Backend {
            Backend::Frontier
        }
    }
    let p = AlwaysFrontierChat;
    let req = EmbeddingRequest::single("m", "hi");
    assert_eq!(
        p.pick_embed(&req),
        Backend::Local,
        "default impl on the trait must return Local for embed"
    );
}
```

- [ ] **Step 4.2: Run tests to verify they fail**

```sh
cargo test -p hhagent-llm-router --lib policy::tests::default_local_policy_pick_embed
```

Expected: build error — `pick_embed` method does not exist on `PolicyGate`.

- [ ] **Step 4.3: Add the trait method**

In `llm-router/src/policy.rs`, modify the `PolicyGate` trait (lines 36-38):

```rust
pub trait PolicyGate: Send + Sync + std::fmt::Debug {
    fn pick(&self, request: &ChatRequest) -> Backend;

    /// Decide which backend serves an embedding request.
    ///
    /// Default: always [`Backend::Local`]. Phase 5's gate may
    /// override this independently of [`pick`] so chat-policy and
    /// embed-policy can diverge (e.g. "chat sometimes goes frontier,
    /// embed always stays local"). Phase 0/1 inherit the default.
    fn pick_embed(&self, _request: &crate::embeddings::EmbeddingRequest) -> Backend {
        Backend::Local
    }
}
```

Update the module-level docstring to mention the new method exists alongside `pick`.

- [ ] **Step 4.4: Run tests to verify they pass**

```sh
cargo test -p hhagent-llm-router --lib policy::tests
```

Expected: 4 tests pass (2 existing + 2 new).

- [ ] **Step 4.5: Commit**

```sh
git add llm-router/src/policy.rs
git commit -m "$(cat <<'EOF'
feat(llm-router): PolicyGate::pick_embed default method (Option O step 4)

Trait method with default body returning Backend::Local. Symmetric
with the chat `pick` method; lets Phase 5's gate override the two
independently. DefaultLocalPolicy inherits the default — no change
to existing impl.

Pinned by 2 unit tests: default-returns-local, custom-impl-inherits.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: `Router::embed` + integration tests

**Files:**
- Modify: `llm-router/src/lib.rs` (re-exports + `Router::embed` + `dispatch_embed_local` + `EMBEDDINGS_PATH` constant)
- Create: `llm-router/tests/embedding_backend_e2e.rs` (4 integration tests + mock helpers)

- [ ] **Step 5.1: Write the failing integration tests**

Create `llm-router/tests/embedding_backend_e2e.rs` with this content (mock helpers copied from `local_backend_e2e.rs` — issue #15 tracks the hoist):

```rust
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
    // Request body carries the model + input.
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
    // 200 OK with body that isn't an EmbeddingResponse shape.
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
```

- [ ] **Step 5.2: Run tests to verify they fail**

```sh
cargo test -p hhagent-llm-router --test embedding_backend_e2e
```

Expected: build error — `Router::embed` method does not exist.

- [ ] **Step 5.3: Implement `Router::embed` and `dispatch_embed_local`**

In `llm-router/src/lib.rs`:

Add a `pub use` for the embedding types (after the existing re-exports at lines 60-64):

```rust
pub use embeddings::{EmbeddingData, EmbeddingRequest, EmbeddingResponse};
```

Add a constant near `CHAT_COMPLETIONS_PATH` (line 72):

```rust
/// The OpenAI-compatible embeddings sub-path appended to every
/// backend's base URL. Same pinning rationale as
/// [`CHAT_COMPLETIONS_PATH`].
const EMBEDDINGS_PATH: &str = "/embeddings";
```

Add the `Router::embed` method to the `impl Router` block (after `send` at line 167):

```rust
    /// Send an embedding request and return the decoded response.
    ///
    /// The policy gate picks the backend via `pick_embed`; for Phase
    /// 0/1 that is always [`Backend::Local`] under the default impl
    /// of `PolicyGate::pick_embed`. A Phase-5 policy that selects
    /// `Backend::Frontier` for embed will fall through to the
    /// `PolicyDeniedFrontier` arm (frontier dispatch unwired).
    ///
    /// Validates `response.data.len() == request.input.len()` and
    /// surfaces a mismatch as
    /// [`RouterError::EmbeddingCountMismatch`]. Does NOT validate the
    /// per-vector dimension — that is the caller's concern (e.g.
    /// `core::memory::embed_query` checks against `EMBEDDING_DIM`).
    pub async fn embed(
        &self,
        request: &EmbeddingRequest,
    ) -> Result<EmbeddingResponse, RouterError> {
        let backend = self.policy.pick_embed(request);
        match backend {
            Backend::Local => self.dispatch_embed_local(request).await,
            Backend::Frontier => Err(RouterError::PolicyDeniedFrontier(
                "frontier embed dispatch is unwired; only DefaultLocalPolicy is supported"
                    .to_string(),
            )),
        }
    }

    /// Dispatch an embedding request to the local backend.
    ///
    /// Pure HTTP: POST to `<embedding_url>/embeddings` with the
    /// JSON-encoded [`EmbeddingRequest`]. Same status / decode error
    /// handling as `dispatch_local`; additional invariant check on
    /// `data.len() == input.len()` after decode.
    async fn dispatch_embed_local(
        &self,
        request: &EmbeddingRequest,
    ) -> Result<EmbeddingResponse, RouterError> {
        let url = compose_url(&self.config.embedding_url, EMBEDDINGS_PATH);
        tracing::debug!(
            target: "hhagent::llm_router",
            backend = "local",
            url = %url,
            model = %request.model,
            n_inputs = request.input.len(),
            "dispatching embedding"
        );

        let resp = self.http.post(&url).json(request).send().await?;
        let status = resp.status();

        if !status.is_success() {
            let body = resp.text().await.unwrap_or_else(|_| {
                "<error body could not be read as UTF-8 text>".to_string()
            });
            return Err(RouterError::HttpStatus {
                status: status.as_u16(),
                body: truncate_for_error(&body, ERROR_BODY_CAP),
            });
        }

        let body = resp.text().await?;
        let decoded: EmbeddingResponse = serde_json::from_str(&body).map_err(|source| {
            RouterError::DecodeResponse {
                source,
                body: truncate_for_error(&body, ERROR_BODY_CAP),
            }
        })?;

        if decoded.data.len() != request.input.len() {
            return Err(RouterError::EmbeddingCountMismatch {
                requested: request.input.len(),
                returned: decoded.data.len(),
            });
        }

        Ok(decoded)
    }
```

Add a `pick_embed_backend` helper on the public surface (after `pick_backend` at line 145), for symmetry with chat:

```rust
    /// Which backend would the router pick for an embedding request?
    /// Pure delegation to the configured [`PolicyGate::pick_embed`].
    pub fn pick_embed_backend(&self, request: &EmbeddingRequest) -> Backend {
        self.policy.pick_embed(request)
    }
```

The `EMBEDDINGS_PATH` constant is pinned end-to-end by the
happy-path integration test below (`served.path == "/embeddings"`),
matching how the spec accounted for the test surface. No separate
unit pin is added — that would be defence-in-depth at this layer,
not load-bearing.

- [ ] **Step 5.4: Run tests to verify they pass**

```sh
cargo test -p hhagent-llm-router --test embedding_backend_e2e
cargo test -p hhagent-llm-router --lib
```

Expected: 4 integration tests + all existing unit tests pass. (Integration count: 4 new. Unit count: unchanged from end of Task 4.)

- [ ] **Step 5.5: Run the workspace test suite — full no-regression check**

```sh
cargo test --workspace
```

Expected: 0 failures. Running total: 299 + 13 (Tasks 1–4) + 4 (Task 5) = **316 tests passing**.

- [ ] **Step 5.6: Commit**

```sh
git add llm-router/src/lib.rs llm-router/tests/embedding_backend_e2e.rs
git commit -m "$(cat <<'EOF'
feat(llm-router): Router::embed + integration tests (Option O step 5)

`Router::embed(&EmbeddingRequest) -> Result<EmbeddingResponse, _>`
mirrors `Router::send`: routes through `policy.pick_embed`, dispatches
to `<embedding_url>/embeddings`, decodes, validates count. Frontier
arm returns PolicyDeniedFrontier (unwired in Phase 0/1).

Count mismatch (`response.data.len() != request.input.len()`)
surfaces as `RouterError::EmbeddingCountMismatch`. Dim validation is
NOT done here — that's the caller's concern.

Pinned by 4 hand-rolled-mock integration tests in
`llm-router/tests/embedding_backend_e2e.rs`: happy path round-trip,
count mismatch, HTTP error with truncated body, decode error.

Mock helpers (spawn_one_shot_mock, find_double_crlf,
header_content_length) duplicated from local_backend_e2e.rs — issue
#15 already tracks the workspace-level hoist.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: `build_embed_audit_payload` pure helper

**Files:**
- Modify: `core/src/memory.rs` (add pure helper + 3 unit tests)

- [ ] **Step 6.1: Write the failing tests**

Append to the `#[cfg(test)] mod tests` block at the bottom of `core/src/memory.rs`:

```rust
    /// The audit payload must NOT carry user text or embeddings —
    /// privacy + size. Pinned so a future refactor that "adds context"
    /// to the row gets caught at the right moment.
    #[test]
    fn embed_audit_payload_excludes_input_text_and_embeddings() {
        let v = build_embed_audit_payload("bge-m3", 1, 1024, "local", 42);
        let s = serde_json::to_string(&v).unwrap();
        assert!(!s.contains("input"), "input leaked: {s}");
        assert!(!s.contains("text"), "text leaked: {s}");
        assert!(!s.contains("query"), "query leaked: {s}");
        assert!(!s.contains("embedding"), "embedding leaked: {s}");
        assert!(!s.contains("\"data\""), "data leaked: {s}");
    }

    /// The audit payload must carry the operator-facing summary fields.
    #[test]
    fn embed_audit_payload_includes_load_bearing_fields() {
        let v = build_embed_audit_payload("bge-m3", 1, 1024, "local", 87);
        assert_eq!(v["model"], "bge-m3");
        assert_eq!(v["n_texts"], 1);
        assert_eq!(v["dim"], 1024);
        assert_eq!(v["backend"], "local");
        assert_eq!(v["latency_ms"], 87);
    }

    /// `latency_ms` is `u64` upstream; pin that it serialises as a
    /// JSON number (not stringly).
    #[test]
    fn embed_audit_payload_latency_is_numeric() {
        let v = build_embed_audit_payload("m", 1, 4, "local", 12345);
        assert!(v["latency_ms"].is_number(), "latency must be a JSON number");
        assert_eq!(v["latency_ms"].as_u64(), Some(12345));
    }
```

- [ ] **Step 6.2: Run tests to verify they fail**

```sh
cargo test -p hhagent-core --lib memory::tests::embed_audit_payload
```

Expected: build error — `build_embed_audit_payload` does not exist.

- [ ] **Step 6.3: Implement the helper**

In `core/src/memory.rs`, add this pure helper near the bottom of the file but before the `#[cfg(test)] mod tests` line. Add `pub(crate)` visibility — internal use only, but unit-testable from the module's test submodule:

```rust
/// Build the audit-log payload for an `actor='llm:router' action='embed'`
/// row.
///
/// Pure function — no I/O, no clock reads, no global state. The
/// caller (`embed_query`) measures latency, picks the backend
/// string, knows the request's model and the agreed dim, then calls
/// this helper to compose the JSON object that the row's `payload`
/// column carries.
///
/// **What the payload deliberately omits:**
/// * The input texts (privacy — query may carry user PII).
/// * The output embeddings (size + uselessness as audit signal).
/// * HTTP status / body (failures don't write an audit row at all;
///   matches `Router::send` and `tool_host::dispatch` precedent).
///
/// **What it includes** is the minimal operator-facing summary: which
/// model, how many texts, what dimension, which backend, how long.
pub(crate) fn build_embed_audit_payload(
    model: &str,
    n_texts: usize,
    dim: usize,
    backend: &str,
    latency_ms: u64,
) -> serde_json::Value {
    serde_json::json!({
        "model":      model,
        "n_texts":    n_texts,
        "dim":        dim,
        "backend":    backend,
        "latency_ms": latency_ms,
    })
}
```

- [ ] **Step 6.4: Run tests to verify they pass**

```sh
cargo test -p hhagent-core --lib memory::tests::embed_audit_payload
```

Expected: 3 new tests pass.

- [ ] **Step 6.5: Commit**

```sh
git add core/src/memory.rs
git commit -m "$(cat <<'EOF'
feat(core/memory): build_embed_audit_payload pure helper (Option O step 6)

Pure JSON-building helper for the `actor='llm:router' action='embed'`
audit row payload. Pinned by 3 unit tests:
- excludes input text and embeddings (privacy + size)
- includes load-bearing fields (model, n_texts, dim, backend, latency_ms)
- latency_ms serialises as a JSON number

`pub(crate)` visibility — internal use only, but unit-testable from
the module's test submodule. Same pattern as
`db::audit::truncate_payload`.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: `embed_query` async helper + full e2e integration

**Files:**
- Modify: `core/src/memory.rs` (add `MemoryError`, `embed_query`)
- Create: `core/tests/embedding_recall_e2e.rs` (4 integration tests)

- [ ] **Step 7.1: Write the failing integration tests**

**READ FIRST (if reading this task out of context):**
- `core/tests/memory_recall_e2e.rs` lines 1–290 — the canonical per-test PG cluster bring-up pattern used by the 7 other PG-touching integration tests in this workspace. The 8th duplication site is precisely the issue #15 hoist will close.
- `llm-router/tests/embedding_backend_e2e.rs` (created in Task 5) — the mock helpers.
- `core/tests/memory_recall_e2e.rs` line 292+ — the `text_to_embedding(text: &str) -> Vec<f32>` SHA-256-seeded deterministic embedding helper. Copy it verbatim.

Create `core/tests/embedding_recall_e2e.rs`. Copy these helpers verbatim from the references above:
- From `memory_recall_e2e.rs`: `skip_if_no_supervisor`, `pg_bin_dir_or_skip`, `unique_suffix`, `unique_temp_root`, `current_username`, `ServiceGuard`, `PathGuard`, the PG bring-up function (whichever name it uses in your branch — `bring_up_per_test_pg` or similar), and `text_to_embedding`.
- From `embedding_backend_e2e.rs`: `ServedRequest`, `CannedResponse`, `spawn_one_shot_mock`, `find_double_crlf`, `header_content_length`.

Then add a small Router-builder helper specific to this test:

```rust
fn build_router_pointing_at(base_url: &str) -> hhagent_llm_router::Router {
    use hhagent_llm_router::{Router, RouterConfig};
    use std::time::Duration;
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
```

The 4 tests in this file:

```rust
//! End-to-end test for `core::memory::embed_query` and the full
//! free-text-to-recall flow.
//!
//! Per-test Postgres cluster (8th duplication site; issue #15 tracks
//! the hoist). Per-test hand-rolled TCP mock for `/embeddings`.
//!
//! Four cases:
//!
//!   1. Happy path — mock returns 1024-float vector; `embed_query`
//!      returns `Ok(Vec<f32>)` of length 1024.
//!   2. Audit row written — after `embed_query` returns Ok, the
//!      `audit_log` table has exactly one row with
//!      `actor='llm:router' action='embed'`, payload shape matching
//!      `build_embed_audit_payload` invariants.
//!   3. Dim mismatch — mock returns 512-float vector; `embed_query`
//!      returns `Err(MemoryError::EmbeddingDimMismatch)`; `audit_log`
//!      has only the probe bring-up row (no llm:router row).
//!   4. Full text-to-recall flow — seed 3 memories with deterministic
//!      embeddings; mock returns the embedding for memory A;
//!      `embed_query("alpha bravo charlie")` → recall(SEMANTIC_ONLY)
//!      → top-1 is memory A; one `actor='llm:router'` row in audit
//!      log.
//!
//! Skips silently with `[SKIP]` lines on hosts without Postgres or a
//! reachable supervisor; `cargo test -- --nocapture` to see them.
```

For each test:
- Bring up per-test PG cluster (same pattern as `memory_recall_e2e.rs`).
- Bring up `spawn_one_shot_mock` for `/embeddings`.
- Build a `Router` pointing at the mock URL.
- Run the assertion.

**Headline assertion (test 2):**

```rust
// After embed_query returns Ok...
let rows: Vec<(String, String, serde_json::Value)> = sqlx::query_as(
    "SELECT actor, action, payload FROM audit_log WHERE actor = 'llm:router' ORDER BY id"
).fetch_all(&pool).await.expect("query audit_log");

assert_eq!(rows.len(), 1, "exactly one llm:router row");
let (actor, action, payload) = &rows[0];
assert_eq!(actor, "llm:router");
assert_eq!(action, "embed");
assert_eq!(payload["model"], "embedding-test");
assert_eq!(payload["n_texts"], 1);
assert_eq!(payload["dim"], 1024);
assert_eq!(payload["backend"], "local");
assert!(payload["latency_ms"].as_u64().unwrap() > 0,
    "latency_ms must be > 0: {payload:?}");
// Privacy invariants — the text and embedding must not be in the row.
let payload_str = serde_json::to_string(payload).unwrap();
assert!(!payload_str.contains("input"), "input leaked: {payload_str}");
assert!(!payload_str.contains("alpha"), "user text leaked: {payload_str}");
assert!(!payload_str.contains("embedding"), "embedding leaked: {payload_str}");
```

**Test 4 (full text-to-recall) skeleton:**

```rust
// Seed 3 memories with deterministic embeddings (same helper as
// memory_recall_e2e.rs).
let emb_a = text_to_embedding(BODY_A);
let _id_a = insert_memory(&pool, BODY_A, &emb_a, None, None).await.unwrap();
let emb_b = text_to_embedding(BODY_B);
let _id_b = insert_memory(&pool, BODY_B, &emb_b, None, None).await.unwrap();
let emb_c = text_to_embedding(BODY_C);
let _id_c = insert_memory(&pool, BODY_C, &emb_c, None, None).await.unwrap();

// Mock returns the embedding for BODY_A — same SHA-256-seeded vector
// the seed used. So embed_query("alpha bravo charlie") yields exactly
// emb_a, which has cosine distance 0 to row A and ~1 to rows B and C.
let canned = serde_json::json!({
    "data": [{"index": 0, "embedding": emb_a.clone()}],
    "model": "embedding-test"
});
let (base_url, _served) = spawn_one_shot_mock(
    CannedResponse::ok_json(canned.to_string())
).await;
let router = build_router_pointing_at(&base_url);

// embed_query the matching text.
let emb = embed_query(&pool, &router, BODY_A).await.expect("embed");
assert_eq!(emb.len(), 1024);

// Plug into recall — semantic-only lane.
let mems = recall(&pool, &RecallParams {
    query_text: None,
    query_embedding: Some(&emb),
    k: 3,
    modes: RecallModes::SEMANTIC_ONLY,
}).await.expect("recall");
assert!(!mems.is_empty(), "recall returned nothing");
assert_eq!(mems[0].body, BODY_A, "top-1 must be A: {mems:?}");

// Audit log has the llm:router row.
let n: i64 = sqlx::query_scalar(
    "SELECT COUNT(*) FROM audit_log WHERE actor = 'llm:router' AND action = 'embed'"
).fetch_one(&pool).await.unwrap();
assert_eq!(n, 1);
```

**Test 3 (dim mismatch) assertion:**

```rust
// Mock returns 512-dim vector for a 1024-dim expectation.
let bad_emb: Vec<f32> = (0..512).map(|i| (i as f32) * 0.01).collect();
let canned = serde_json::json!({
    "data": [{"index": 0, "embedding": bad_emb}],
    "model": "embedding-test"
});
let (base_url, _served) = spawn_one_shot_mock(
    CannedResponse::ok_json(canned.to_string())
).await;
let router = build_router_pointing_at(&base_url);

let err = embed_query(&pool, &router, "hello").await
    .expect_err("dim must mismatch");
match err {
    MemoryError::EmbeddingDimMismatch { expected, actual, model } => {
        assert_eq!(expected, 1024);
        assert_eq!(actual, 512);
        assert_eq!(model, "embedding-test");
    }
    other => panic!("expected EmbeddingDimMismatch, got {other:?}"),
}

// No audit row for the failure (chokepoint precedent).
let n: i64 = sqlx::query_scalar(
    "SELECT COUNT(*) FROM audit_log WHERE actor = 'llm:router'"
).fetch_one(&pool).await.unwrap();
assert_eq!(n, 0, "failure must not write audit row");
```

Use this top-of-file content for imports:

```rust
#![cfg(any(target_os = "linux", target_os = "macos"))]

use hhagent_core::memory::{embed_query, recall, MemoryError, RecallModes, RecallParams};
use hhagent_db::memories::{insert_memory, EMBEDDING_DIM};
use hhagent_llm_router::embeddings::EmbeddingRequest;
use hhagent_llm_router::{Router, RouterConfig};
// ... plus everything memory_recall_e2e.rs imports for PG bring-up ...
```

**Don't forget:** the full file will be ~450 LOC. Copy the PG-bring-up helpers and `text_to_embedding` from `memory_recall_e2e.rs` verbatim; copy the mock helpers from `llm-router/tests/embedding_backend_e2e.rs` (or accept the duplication once more — issue #15).

- [ ] **Step 7.2: Run tests to verify they fail**

```sh
cargo test -p hhagent-core --test embedding_recall_e2e
```

Expected: build error — `embed_query`, `MemoryError`, and `MemoryError::EmbeddingDimMismatch` do not exist in `core::memory`.

- [ ] **Step 7.3: Implement `MemoryError` and `embed_query`**

In `core/src/memory.rs`, near the top after the existing `use` block, add:

```rust
use hhagent_db::audit;
use hhagent_llm_router::embeddings::EmbeddingRequest;
use hhagent_llm_router::{Router, RouterError};
use std::time::Instant;

/// Errors returned by `core::memory` helpers that touch the LLM
/// router and/or write audit rows.
///
/// `recall` itself is `Result<_, DbError>`-typed and is unchanged by
/// this slice; `MemoryError` is the wider surface used by
/// [`embed_query`].
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("router: {0}")]
    Router(#[from] RouterError),
    #[error("db: {0}")]
    Db(#[from] hhagent_db::DbError),
    #[error("audit insert: {0}")]
    AuditSqlx(#[from] sqlx::Error),
    #[error("embedding dim mismatch: expected {expected}, got {actual} from model {model}")]
    EmbeddingDimMismatch {
        expected: usize,
        actual: usize,
        model: String,
    },
}
```

Add the `embed_query` function (near the bottom of the file, before the `#[cfg(test)]` block):

```rust
/// Turn a free-text query into a [`EMBEDDING_DIM`]-length embedding
/// vector via the LLM router's embedding backend, writing the first
/// `actor='llm:router' action='embed'` audit row in the process.
///
/// ## Flow
/// 1. Build `EmbeddingRequest::single(router.config().embedding_model, text)`.
/// 2. Time the call to `router.embed(&req).await`.
/// 3. Validate `data.len() == 1` (router already validated against
///    request input length; this is a defensive check for the
///    single-text shape).
/// 4. Validate the returned embedding's length equals
///    [`EMBEDDING_DIM`]; otherwise [`MemoryError::EmbeddingDimMismatch`].
/// 5. Insert one row into `audit_log` with
///    `actor='llm:router' action='embed'` and the payload shape
///    pinned by [`build_embed_audit_payload`].
///    **Best-effort:** an audit-insert failure is logged at
///    `tracing::error!` but does **not** mask the embed `Ok(emb)` —
///    matches `tool_host::dispatch` precedent.
/// 6. Return the embedding vector.
///
/// ## What this does NOT do
/// - Does not call `recall`. Caller composes `embed_query` →
///   `RecallParams { query_embedding: Some(&emb), ... }` → `recall`.
/// - Does not retry. The router's reqwest client carries the configured
///   timeout; transport-level retries are a Phase-1-cont. optimisation.
/// - Does not cache. Stateless function.
pub async fn embed_query(
    pool: &sqlx::PgPool,
    router: &Router,
    text: &str,
) -> Result<Vec<f32>, MemoryError> {
    let model = router.config().embedding_model.clone();
    let req = EmbeddingRequest::single(model.clone(), text);

    let start = Instant::now();
    let resp = router.embed(&req).await?;
    let latency_ms = start.elapsed().as_millis() as u64;

    if resp.data.len() != 1 {
        // Router's own count check should fire first; this is
        // belt-and-braces.
        return Err(MemoryError::Router(RouterError::EmbeddingCountMismatch {
            requested: 1,
            returned: resp.data.len(),
        }));
    }
    let emb = resp.data.into_iter().next().unwrap().embedding;

    if emb.len() != EMBEDDING_DIM {
        return Err(MemoryError::EmbeddingDimMismatch {
            expected: EMBEDDING_DIM,
            actual: emb.len(),
            model,
        });
    }

    // Best-effort audit. We hardcode "local" here matching
    // RouterAgent::formulate_plan (core/src/scheduler/agent.rs:111).
    // When Phase 5's PolicyGate may select Frontier for embed, swap
    // this for `router.pick_embed_backend(&req).as_tag()`.
    let payload = build_embed_audit_payload(&req.model, 1, EMBEDDING_DIM, "local", latency_ms);
    if let Err(e) = audit::insert(pool, "llm:router", "embed", payload).await {
        tracing::error!(
            target: "hhagent::memory",
            error = %e,
            "embed_query audit insert failed; embedding result preserved"
        );
    }

    Ok(emb)
}
```

If `core/src/memory.rs` imports list does not already include the right items, the additional `use` lines above should be sufficient.

- [ ] **Step 7.4: Run tests to verify they pass**

```sh
cargo test -p hhagent-core --test embedding_recall_e2e
```

Expected: 4 integration tests pass (or `[SKIP]` cleanly on macOS without PG).

Run 5× determinism check (the cli_ask_e2e precedent):

```sh
for i in 1 2 3 4 5; do cargo test -p hhagent-core --test embedding_recall_e2e || break; done
```

Expected: 5/5 green runs.

- [ ] **Step 7.5: Run the workspace test suite**

```sh
cargo test --workspace
```

Expected: 323 passed total, 0 failed, 0 warnings.

- [ ] **Step 7.6: Commit**

```sh
git add core/src/memory.rs core/tests/embedding_recall_e2e.rs
git commit -m "$(cat <<'EOF'
feat(core/memory): embed_query + actor='llm:router' audit row (Option O step 7)

`embed_query(pool, router, text) -> Result<Vec<f32>, MemoryError>` is
the production path that turns a free-text query into the embedding
that `recall(SEMANTIC_ONLY)` consumes. Writes the first
actor='llm:router' action='embed' audit row in the system.

Flow: build EmbeddingRequest::single → router.embed → validate
count == 1 → validate dim == EMBEDDING_DIM (1024) → best-effort
audit insert → return Vec<f32>. Audit failure does NOT mask the
Ok embed (matches tool_host::dispatch precedent).

New `MemoryError` enum: Router (#[from] RouterError), Db (#[from]
DbError), AuditSqlx (#[from] sqlx::Error), EmbeddingDimMismatch
(typed dim+actual+model).

Pinned by 4 integration tests in core/tests/embedding_recall_e2e.rs:
- happy path returns Vec<f32> of EMBEDDING_DIM
- audit row written with privacy-safe payload (excludes input text
  and embeddings)
- dim mismatch surfaces typed error and writes NO audit row
- full text-to-recall flow: embed_query → recall → top-1 is the
  matching memory; audit log has the llm:router row

Per-test PG cluster (8th duplication site; issue #15). Hand-rolled
TCP mock for /embeddings. 5/5 deterministic local runs.

recall's signature is unchanged. Callers compose embed_query then
recall — pure-function principle (CLAUDE.md rule #1).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: HANDOVER + ROADMAP update

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

- [ ] **Step 8.1: Confirm the workspace is fully green**

```sh
cargo test --workspace
```

Expected: **323 passed, 0 failed, 0 warnings**.

- [ ] **Step 8.2: Update HANDOVER.md**

Bump the header `Last updated` / `Last commit` lines (replace the existing 5-line header block) to reflect the latest commit hash from Task 7 and that Option O shipped this session.

In the "Recently completed (this session, ...)" section, **prepend a new entry** (above the existing Task 4.4 entry) titled "Option O — embedding router + first actor='llm:router' audit row". The entry should include:

1. Why this slice now (single-paragraph: free-text queries had no production path to a query_embedding; this slice closes that gap with the first actor='llm:router' audit row).
2. Shape (5 modules touched, 2 new test files, 4 + 13 + 3 = +20 unit + +8 integration = +24 tests).
3. Design decision recap (HTTP call in Router::embed in core, not in a worker — see spec).
4. What this slice deliberately does NOT do (no batch helper, no recall signature change, no worker, no frontier embed support).
5. Test-count delta: 299 → 323 (+24).
6. Files added/modified: bullet list mirroring the Task table at the top of this plan.

In the "Working state (what's green right now)" section, update the `llm-router` crate description to mention `Router::embed` and the new fields/methods. Also update the `core` description to mention `embed_query` and the `MemoryError` enum. Update the test-count line and the per-suite test count table.

In the "Next TODO (pick one)" section, mark **Option O as done** (move it to "Recently completed" effectively — it's already in the prepended section), and re-rank the remaining options. The new top priority is **Option P (entity↔memory linkage + graph lane)** since Option N+O are now both complete; **Issue #15 (tests-common hoist)** is increasingly cheap with 8 duplication sites and should probably leapfrog onto the list.

- [ ] **Step 8.3: Update ROADMAP.md**

In `docs/devel/ROADMAP.md`, find the Phase 1 line item:

```
- [ ] Embedding worker (small local embedding model behind OpenAI HTTP) — first concrete consumer of `Router::send`, first `actor='llm:router'` audit-log row; lets `recall` produce its own embeddings from `query_text` instead of requiring a pre-computed `query_embedding` (Phase 1 cont. — Option O)
```

Change `[ ]` to `[x]` and append the shipped detail + commit hash placeholder (fill in the actual hash from Task 7's commit before pushing):

```
- [x] Embedding router method (Phase 1 cont. — Option O) — landed 2026-05-11. `Router::embed(&EmbeddingRequest)` mirrors `Router::send`: HTTP POST to `<embedding_url>/embeddings`, OpenAI-compat wire shapes, count-mismatch validation. `core::memory::embed_query` is the caller helper that validates dim against `EMBEDDING_DIM` and writes the first `actor='llm:router' action='embed'` audit row. Worker-process design rejected during brainstorming (see spec) because no existing chat call goes through a worker either; symmetry preserved. Pinned by 13 unit tests + 4 router integration tests + 3 memory unit tests + 4 memory integration tests = +24 tests. Workspace 299 → 323. Commits: <fill-in hash range>.
```

- [ ] **Step 8.4: Final workspace test run**

```sh
cargo test --workspace
```

Expected: still 323 passed, 0 failed.

- [ ] **Step 8.5: Commit HANDOVER + ROADMAP**

```sh
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "$(cat <<'EOF'
docs(handover,roadmap): Option O shipped — embedding router (+24 tests)

Records the embedding-router slice:
- Router::embed in llm-router (HTTP path, mirrors Router::send)
- embed_query in core::memory (validates dim, writes the first
  actor='llm:router' action='embed' audit row)
- 4 new test files / module extensions

Workspace test count 299 → 323. Phase 1 ROADMAP item "Embedding
worker" ticked complete.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 8.6: Push (optional — confirm with the operator first)**

```sh
git log --oneline -10
git push origin main   # only if main is the working branch and the operator confirms
```

If the operator wants a PR instead, push to a feature branch and `gh pr create`:

```sh
git checkout -b feat/embedding-router
git push -u origin feat/embedding-router
gh pr create --title "feat: embedding router + first actor='llm:router' audit row (Option O)" \
             --body "$(cat <<'EOF'
## Summary
- `Router::embed` mirrors `Router::send`: HTTP path to `/embeddings`, OpenAI-compat wire shapes, count-mismatch validation
- `core::memory::embed_query` validates dim against `EMBEDDING_DIM` and writes the first `actor='llm:router' action='embed'` audit row
- No new worker process — design decision recorded in [the spec](docs/superpowers/specs/2026-05-11-embedding-router-design.md)

## Test plan
- [x] `cargo test --workspace` green (299 → 323 tests)
- [x] 5/5 deterministic runs of `embedding_recall_e2e`
- [x] HANDOVER + ROADMAP updated

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

---

## Done criteria

After Task 8 lands, all of the following must be true:

1. `cargo test --workspace` reports 323 passed, 0 failed, 0 warnings on Linux (and 323 passed with PG-dependent tests skipping cleanly on macOS).
2. The audit_log row shape pinned by `build_embed_audit_payload` is verified end-to-end in `embedding_recall_e2e.rs::audit_row_written_with_privacy_safe_payload`.
3. HANDOVER.md's "Working state" section mentions `Router::embed` and `embed_query`; the test-count line reads `**cargo test --workspace on Linux: 323 tests passed...**`.
4. ROADMAP.md's Phase 1 "Embedding worker" item is checked.
5. No new dev-dep added (no `httpmock`, no `wiremock`).
6. No file in the workspace exceeds 500 LOC after this slice. (`core/src/memory.rs` is ~485 LOC after the additions; verify with `wc -l core/src/memory.rs`.)

---

## Self-review notes (already applied in this plan)

- **Spec coverage:** every section of the spec maps to a task. Wire shapes → Task 1. Error variant → Task 2. Config → Task 3. Policy → Task 4. Router::embed → Task 5. Audit payload helper → Task 6. embed_query + e2e → Task 7. Docs → Task 8.
- **Type consistency:** `EmbeddingRequest::single(model, text)` → `Router::embed(&req)` → `EmbeddingResponse { data: Vec<EmbeddingData> }` → `data[0].embedding` is `Vec<f32>` of length 1024. All sites consistent.
- **TDD discipline:** every task has Write failing test → Run (red) → Implement → Run (green) → Commit. No "implement then test later."
- **No placeholders:** every code block contains the exact code the engineer needs. The only `<fill-in>` is the commit-hash range in ROADMAP.md, which is intrinsically deferred until after the commits land.
