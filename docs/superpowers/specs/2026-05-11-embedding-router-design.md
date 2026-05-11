# Embedding Router — Free-text → Embedding → Recall (Option O)

**Date:** 2026-05-11
**Author:** Claude (hherb)
**Status:** Approved 2026-05-11

## Why

`core::memory::recall` (shipped 2026-05-10 as Option N) is the agent's
hybrid-search entry point. It runs three lanes — semantic (pgvector
cosine), lexical (`tsvector` + `ts_rank`), graph (deferred) — and
fuses them via Reciprocal Rank Fusion. The semantic lane requires a
**pre-computed query embedding**: `RecallParams.query_embedding:
Option<&[f32]>`. If the caller passes `None`, the lane is skipped with
a warn.

Today there is **no production path that turns a free-text query into
that embedding**. Every test seeds embeddings with a deterministic
SHA-256-seeded helper, and the only existing `Router` consumer is
`RouterAgent::formulate_plan` (chat completions, no embedding).

This slice ships the embedding HTTP path through the existing
`hhagent-llm-router` crate, plus a thin caller helper
`core::memory::embed_query` that writes the **first
`actor='llm:router'` audit-log row** in the system. After this slice,
a caller with `query_text` only can call `embed_query` to materialize
the embedding, then pass it to `recall`.

This is **Option O from HANDOVER's "Next TODO" list**, sized for one
session per the brainstorming pass on 2026-05-11.

## Design decision: HTTP call lives in `Router::embed`, not in a new worker

HANDOVER's brief mixed two designs (a new `workers/embedding-worker/`
crate AND a `Router::embed` method). The 2026-05-11 brainstorming pass
resolved this in favour of the simpler shape:

- **The existing `Router::send` is called directly from core** (see
  `core/src/scheduler/agent.rs:51` — `RouterAgent::formulate_plan`
  invokes `self.router.send(&req).await`). There is no worker in
  front of the chat HTTP call today.
- Adding a worker for embeddings alone would be asymmetric vs the
  shipping chat path and would add ~30 ms of bwrap spawn latency per
  call.
- The threat-model constraint "the agent core never speaks to LLM
  directly from a worker" is about *workers*, not about *core*; core
  is allowed to make LLM egress.

A future slice may migrate **both** `Router::send` and
`Router::embed` into a sandboxed worker (consistent with Phase 3's
"all net egress in workers" goal). That decision is out of scope here.

## What is in scope

Six modules touched (additions only — no behavioural removals); three
new test files.

### New module: `llm-router/src/embeddings.rs` (~200 LOC)

The OpenAI-compat wire shapes for `POST <base>/embeddings`:

```rust
pub struct EmbeddingRequest {
    pub model: String,
    pub input: Vec<String>,        // always a JSON array, even when single
}

pub struct EmbeddingData {
    #[serde(default)]
    pub index: u32,
    pub embedding: Vec<f32>,
}

pub struct EmbeddingResponse {
    pub data: Vec<EmbeddingData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,        // re-uses `messages::Usage`
}
```

**Wire compatibility:** vLLM's `/v1/embeddings` (since vLLM 0.5+),
SGLang's `/v1/embeddings`, Ollama's `/v1/embeddings` (OpenAI-compat
front door since 0.1.32), and `text-embeddings-inference` /
Infinity all speak this shape. Bge-m3 specifically: served by any of
those backends, returns 1024-float vectors as expected.

**Why `Vec<String>` instead of a `string-or-list` enum:** keeps the
type simple; serialises as a single-element array when the caller has
one string. All four reference backends accept the array form.

### `llm-router/src/lib.rs` (modified, ~50 LOC added)

```rust
impl Router {
    pub async fn embed(
        &self,
        request: &EmbeddingRequest,
    ) -> Result<EmbeddingResponse, RouterError> {
        // 1. policy.pick_embed(request) — Phase 5 seam
        // 2. compose URL: <embedding_url>/embeddings
        // 3. reqwest POST, decode 200 -> EmbeddingResponse
        // 4. if data.len() != input.len() -> EmbeddingCountMismatch
        // 5. return Ok(response)
    }
}
```

Mirrors `Router::send` for HTTP plumbing, including the 1 KiB error
body cap via `error::truncate_for_error`.

**Dim validation NOT done in router** — the router has no canonical
expected dim. The caller (`core::memory::embed_query`) passes
`EMBEDDING_DIM` from `db::memories` and does the check itself.

### `llm-router/src/config.rs` (modified, ~30 LOC added)

```rust
pub struct RouterConfig {
    pub local_url: String,
    pub local_model: String,
    pub embedding_url: String,       // NEW
    pub embedding_model: String,     // NEW
    pub frontier_url: Option<String>,
    pub frontier_model: Option<String>,
    pub timeout: Duration,
}
```

Defaults:
- `embedding_url` defaults to `local_url` (so Ollama-on-macOS works
  with one URL set).
- `embedding_model` defaults to `"embedding-default"` (placeholder
  that vLLM will reject with 4xx for production — forces operator to
  set it explicitly).

New env vars:
- `HHAGENT_LLM_EMBEDDING_URL` (falls back to `HHAGENT_LLM_LOCAL_URL`,
  then per-OS default)
- `HHAGENT_LLM_EMBEDDING_MODEL` (`"embedding-default"`)

### `llm-router/src/policy.rs` (modified, ~10 LOC added)

```rust
pub trait PolicyGate: Send + Sync + std::fmt::Debug {
    fn pick(&self, request: &ChatRequest) -> Backend;
    fn pick_embed(&self, _request: &EmbeddingRequest) -> Backend {
        Backend::Local
    }
}
```

`pick_embed` is a separate method with a default body returning
`Backend::Local`. `DefaultLocalPolicy` inherits the default and
requires no change. Phase 5's gate may override chat-policy and
embed-policy independently.

### `llm-router/src/error.rs` (modified, ~10 LOC added)

```rust
pub enum RouterError {
    // ... existing variants ...
    EmbeddingCountMismatch { requested: usize, returned: usize },
}
```

`EmbeddingCountMismatch` fires inside `Router::embed` when
`response.data.len() != request.input.len()`. Dim validation is
**not** a `RouterError` — the router has no canonical expected dim;
that's the caller's concern. See `MemoryError::EmbeddingDimMismatch`
below.

### `core/src/memory.rs` (modified, ~70 LOC added)

```rust
pub async fn embed_query(
    pool: &PgPool,
    router: &Router,
    text: &str,
) -> Result<Vec<f32>, MemoryError> {
    // 1. Build EmbeddingRequest { model: router.config().embedding_model,
    //                             input: vec![text.into()] }
    // 2. let start = Instant::now();
    // 3. let resp = router.embed(&req).await?;  // RouterError surface
    // 4. Validate resp.data.len() == 1
    // 5. let emb = resp.data.into_iter().next().unwrap().embedding;
    // 6. if emb.len() != EMBEDDING_DIM -> EmbeddingDimMismatch
    // 7. let latency_ms = start.elapsed().as_millis() as u64;
    // 8. audit::insert(pool, "llm:router", "embed",
    //                  build_embed_audit_payload(
    //                    &req.model, 1, EMBEDDING_DIM, "local", latency_ms))
    //    — best-effort: warn-on-error, do not mask Ok(emb)
    //    The literal "local" mirrors RouterAgent's hardcoding
    //    (agent.rs:111) — Phase 0/1's PolicyGate always returns Local
    //    for embed; when Phase 5's gate may select Frontier, this slice
    //    will swap the literal for `pick_embed(req).as_tag()`.
    // 9. Ok(emb)
}

pub(crate) fn build_embed_audit_payload(
    model: &str,
    n_texts: usize,
    dim: usize,
    backend: &str,
    latency_ms: u64,
) -> serde_json::Value {
    serde_json::json!({
        "model":     model,
        "n_texts":   n_texts,
        "dim":       dim,
        "backend":   backend,
        "latency_ms": latency_ms,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("router: {0}")]      Router(#[from] RouterError),
    #[error("db: {0}")]           Db(#[from] DbError),
    #[error("dim mismatch: expected {expected}, got {actual} from model {model}")]
    EmbeddingDimMismatch { expected: usize, actual: usize, model: String },
}
```

**`recall`'s signature stays unchanged.** Callers compose:

```rust
let emb = embed_query(&pool, &router, "the meeting last Tuesday").await?;
let mems = recall(&pool, &RecallParams {
    query_text:      Some("the meeting last Tuesday"),
    query_embedding: Some(&emb),
    k: 10,
    modes: RecallModes::ALL,
}).await?;
```

### Audit row exact shape

```json
{
  "id":      42,
  "ts":      "2026-05-11T14:30:00.123456Z",
  "actor":   "llm:router",
  "action":  "embed",
  "payload": {
    "model":      "embedding-default",
    "n_texts":    1,
    "dim":        1024,
    "backend":    "local",
    "latency_ms": 87
  }
}
```

**Deliberately absent:** the input texts (privacy — query may contain
PII), the output embeddings (size and uselessness), HTTP-error
context (failures don't write the row — symmetric with `Router::send`
and `tool_host::dispatch` precedent).

### Error policy

| Failure | Surfaces as | Audit row? |
| --- | --- | --- |
| Backend unreachable | `RouterError::Transport(...)` | No |
| 4xx/5xx from backend | `RouterError::HttpStatus { status, body }` (1 KiB body cap) | No |
| Bad JSON in 200 body | `RouterError::DecodeResponse { detail, raw }` | No |
| `data.len()` mismatch | `RouterError::EmbeddingCountMismatch { requested, returned }` | No |
| Embedding dim mismatch | `MemoryError::EmbeddingDimMismatch { expected, actual, model }` | No |
| Policy denied | `RouterError::PolicyDeniedFrontier` (Phase 5+ only) | No |
| Audit insert fails | `tracing::error!` only — does NOT mask the embed `Ok(...)` | (silent) |

Audit rows are written only on a *complete* call. The daemon log +
the failed downstream operation are the operator's signal for
failures; the audit log is a *positive* trail of completed actions.
This matches `tool_host::dispatch`'s best-effort audit precedent.

## What is deliberately NOT in scope

- **No new worker process.** See the "Design decision" section above.
- **No change to `recall`'s signature.** Callers compose
  `embed_query` then `recall`. Keeps `recall` pure-data, `embed_query`
  network+audit.
- **No batch helper.** `Router::embed` accepts `Vec<String>` (so the
  wire shape supports batch), but the only caller helper exposed is
  single-text (`embed_query(text: &str)`). When the first batch
  consumer materialises (likely a memory indexer for Phase 1 cont.),
  add `embed_queries(texts: &[&str]) -> Vec<Vec<f32>>` then.
- **No `actor='llm:router'` rows for failures.** See "Error policy"
  above.
- **No `recall_with_router` convenience wrapper.** Three-line caller
  composition (`embed_query` → build `RecallParams` → `recall`) is
  cheap and explicit. If repetition surfaces, hoist later.
- **No frontier embedding support.** `pick_embed` is the seam; Phase 5
  fills it.

## Implementation plan (commit-by-commit, TDD)

Each commit: red test → implementation → green test → `cargo test --workspace`.

1. **Wire shapes.** `llm-router/src/embeddings.rs` types + 6 serde
   unit tests (canonical, minimal, batch, default-index, single-input
   array, skip-none). RED → GREEN.
2. **Error variant.** Add `EmbeddingCountMismatch` to `RouterError` + 1
   unit test pinning field shape. (Dim mismatch lives in
   `MemoryError` — see step 7.) RED → GREEN.
3. **Config.** Add `embedding_url`/`embedding_model` fields + 4
   env-var tests (read-each + fallback + default-model-pin). RED →
   GREEN.
4. **Policy.** `PolicyGate::pick_embed` default method + 2 unit tests
   (default returns Local; custom impl inherits default). RED →
   GREEN.
5. **`Router::embed`.** Async method + 4 integration tests in
   `llm-router/tests/embedding_backend_e2e.rs` (happy, count
   mismatch, HTTP error, decode error). Hand-rolled TCP mock — no
   `httpmock` dep. RED → GREEN.
6. **`build_embed_audit_payload`.** Pure helper in `core/src/memory.rs`
   + 3 unit tests pinning the no-input-text / has-load-bearing /
   no-embeddings invariants. RED → GREEN.
7. **`embed_query`.** Full helper + 4 integration tests in
   `core/tests/embedding_recall_e2e.rs` (returns expected dim, writes
   audit row, dim mismatch surfaces typed error, full text-to-recall
   flow). Per-test PG cluster (issue #15 — 8th duplication site).
   RED → GREEN.
8. **Documentation + commit.** Final `cargo test --workspace` clean
   run; update HANDOVER + ROADMAP; commit. Optional `gh pr create`
   depending on session-end status.

Each commit lands as its own message of the form
`feat(llm-router|memory): <chunk> (Option O)` or `test(...): ...`.

## Test count delta

| Layer | Tests | Where |
| --- | --- | --- |
| `llm-router` unit | +13 | `embeddings.rs::tests` (6), `config.rs::tests` (4), `policy.rs::tests` (2), `error.rs::tests` (1) |
| `llm-router` integration | +4 | `embedding_backend_e2e.rs` (NEW) |
| `core` unit | +3 | `memory.rs::tests` (extends existing module) |
| `core` integration | +4 | `embedding_recall_e2e.rs` (NEW) |
| **Total** | **+24** | Workspace 299 → 323 |

## Determinism + flake avoidance

- Hand-rolled TCP mock (matches `local_backend_e2e.rs`,
  `router_agent_mock_e2e.rs`, `cli_ask_e2e.rs`) → no `httpmock` /
  `wiremock` dev-dep, no flake from network mock internals.
- Per-test PG cluster → no test cross-talk; same pattern as 7 other
  e2e tests.
- Deterministic SHA-256-seeded embeddings in the full-flow test → no
  reliance on a real embedding model.
- 5-runs determinism check before merging (`for i in 1..=5; do cargo
  test -p hhagent-core --test embedding_recall_e2e; done`), matching
  the cli_ask_e2e precedent.

## Files added / modified

**New:**
- `llm-router/src/embeddings.rs` (~200 LOC)
- `llm-router/tests/embedding_backend_e2e.rs` (~250 LOC)
- `core/tests/embedding_recall_e2e.rs` (~450 LOC)
- `docs/superpowers/specs/2026-05-11-embedding-router-design.md`
  (this file)

**Modified:**
- `llm-router/src/lib.rs` — `Router::embed` method + module
  registration + 1–2 unit tests
- `llm-router/src/config.rs` — 2 new fields + env-var wiring + 4 unit
  tests
- `llm-router/src/policy.rs` — `pick_embed` default method + 2 unit
  tests
- `llm-router/src/error.rs` — 2 new variants + 2 unit tests
- `core/src/memory.rs` — `embed_query` + `build_embed_audit_payload`
  + `MemoryError` + 3 unit tests

**Total LOC delta (estimate):** +1300 lines source / test, no removals.

## Open questions parked for this slice

- **Should `embed_query` cache repeated queries within a single
  `tasks` lifetime?** Not in scope. The scheduler currently calls
  `recall` once per plan formulation; per-task caching is a Phase-1
  follow-up optimization. Caching at the Router layer would require
  the Router to grow state — keep it stateless.
- **Should the embedding model dim be config-driven instead of
  hardcoded to 1024?** Not today. `EMBEDDING_DIM = 1024` is set by
  the `memories.embedding vector(1024)` column type; changing the dim
  requires a migration. Keep them coupled.
- **Should `pick_embed` see the request body for content-based
  routing?** Yes, signature is `pick_embed(&self, &EmbeddingRequest)`
  — symmetric with `pick`. Default impl ignores the arg.
