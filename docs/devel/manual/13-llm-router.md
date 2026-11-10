# 13 — LLM router

The `llm-router` crate (`kastellan-llm-router`) is the **only** core-side
egress to a model backend. Every model call the agent core makes — the
planner, recall and memory embeddings, the notebook's sorter, the
CASSANDRA guard tier — goes through `Router::send` or `Router::embed`.

Two other crates in the workspace deliberately speak to a model or search
backend, and both are trusted sidecars rather than core: `embed-broker`
bridges a jailed worker's Unix socket to the operator's
OpenAI-compatible embedding endpoint (so in-VM workers can embed without
egress), and `search-broker` does the same for search. Neither is a way
for the core to reach a model.

> The crate-level doc comments in `llm-router/src/lib.rs` and
> `llm-router/src/config.rs` are the live source of truth; this chapter is
> the developer tour, as of early November 2026.

---

## Module layout

```
llm-router/src/
  lib.rs              Router: send, embed, props; response-size cap
  backend.rs          Backend enum (Local / Frontier) + as_tag for audit
  config.rs           RouterConfig, from_env, for_guard; env-var table
  messages.rs         ChatRequest / ChatResponse / ChatMessage / Usage
  embeddings.rs       EmbeddingRequest / EmbeddingResponse
  policy.rs           PolicyGate trait + DefaultLocalPolicy
  thinking.rs         ThinkingSwitch (wire dialect), ThinkingPolicy (per
                      request), thinking-leak detector
  completion_stats.rs CompletionStats — token/reasoning counts for audit rows
  logprob_score.rs    yes/no logprob renormalisation (the guard tier's score)
  error.rs            RouterError
```

---

## What it talks to

Every backend speaks the OpenAI-compatible HTTP API; there is no
authentication header (local backends on loopback). The router appends
`/chat/completions`, `/embeddings`, or — for llama.cpp — `/props` to a
configured base URL.

| Role | Config | Used by |
|---|---|---|
| **Local chat** | `KASTELLAN_LLM_LOCAL_URL` / `_LOCAL_MODEL` | Planner, notebook, sorter |
| **Embeddings** | `KASTELLAN_LLM_EMBEDDING_URL` / `_EMBEDDING_MODEL` (URL falls back to the local URL) | `memory::embed_query`, L1 and entity embed-on-insert |
| **Guard** | `KASTELLAN_LLM_GUARD_URL` / `_GUARD_MODEL` / `_GUARD_TAU` / `_GUARD_TIMEOUT_MS` | The injection guard's model tier ([chapter 11](./11-cassandra-pipeline.md)) |
| **Frontier** | `KASTELLAN_LLM_FRONTIER_URL` / `_FRONTIER_MODEL` | Nothing — refused (below) |

The guard is a separate `Router` built from `RouterConfig::for_guard`, with
its own timeout and its thinking dialect pinned to `chat_template_kwargs`
whatever the planner's setting says. Setting only one of its URL/model
pair is a config error, so a broken guard cannot look like an absent one.

The router's built-in default local URL is `http://127.0.0.1:8000/v1` on
every OS (vLLM or SGLang on Linux, oMLX on macOS). Ollama (`:11434`) and
llama.cpp's server work by setting `KASTELLAN_LLM_LOCAL_URL` — and
`kastellan-cli install` does exactly that on Linux, where it writes the
Ollama endpoint `http://127.0.0.1:11434` by default (macOS gets oMLX on
`:8000`; see `core/src/install/plan.rs`).

---

## Configuration

`RouterConfig::from_env()` reads (an empty value counts as unset; an
invalid value is a config error, never a silent default):

| Variable | Default | Purpose |
|----------|---------|---------|
| `KASTELLAN_LLM_LOCAL_URL` | `http://127.0.0.1:8000/v1` | OpenAI-compatible base URL |
| `KASTELLAN_LLM_LOCAL_MODEL` | `local-default` (placeholder; every deployment overrides) | Model name in the request body |
| `KASTELLAN_LLM_EMBEDDING_URL` | the local URL | Embedding base URL |
| `KASTELLAN_LLM_EMBEDDING_MODEL` | `embedding-default` | Embedding model name |
| `KASTELLAN_LLM_TIMEOUT_MS` | `180000` | Whole-request budget — bounds **generation**, not just connect (connect is separately capped at 5 s) |
| `KASTELLAN_LLM_DISABLE_THINKING` | `1` (on) | Ask a reasoning model not to think; `0` lets it think |
| `KASTELLAN_LLM_THINKING_SWITCH` | `chat_template_kwargs` | How that request is written: `chat_template_kwargs` (vLLM, SGLang, llama.cpp) or `reasoning_effort` (**Ollama**) |
| `KASTELLAN_LLM_GUARD_*` | unset (tier off) | See [chapter 11](./11-cassandra-pipeline.md) |
| `KASTELLAN_LLM_FRONTIER_URL` / `_MODEL` | unset | Forward-compatible seam only |

### Thinking

There are two incompatible wire conventions for "don't think":
`chat_template_kwargs: {"enable_thinking": false}` and
`reasoning_effort: "none"`. ⚠️ **Ollama silently ignores the first**, so on
Ollama `KASTELLAN_LLM_DISABLE_THINKING` does nothing unless
`KASTELLAN_LLM_THINKING_SWITCH=reasoning_effort`. The router cannot send
both: vLLM 0.15 rejects `reasoning_effort: "none"` with a 400 on every
call. After a call that asked for suppression, the router checks the
response for reasoning text or reasoning-token counts and, if the backend
thought anyway, logs **one** WARN per process naming the setting to change.

A caller can override the config per request with
`ChatRequest::with_thinking(ThinkingPolicy::{RouterDefault, Suppress,
Allow})`. The notebook picks it per turn (`scheduler::notebook::thinking`):
`Suppress` for the forced-answer turn and for every re-ask (the #945 re-ask
of an unusable reply, the table show-back); `Allow` for plan, replan and
answer-rewrite turns; routine turns keep the router default. Thinking-on vs.
off is a measured choice per deployment: on the reference hosts planning completed
more reliably with thinking on, so they run `KASTELLAN_LLM_DISABLE_THINKING=0`
with a much longer `KASTELLAN_LLM_TIMEOUT_MS` than the default.

### Timeouts and output caps

- **Client-wide**: `KASTELLAN_LLM_TIMEOUT_MS`, set on the HTTP client.
- **Per request**: `ChatRequest.timeout: Option<Duration>` (never sent on
  the wire) bounds one call. The planner derives it from the fast lane's
  wall-clock ceiling — the time left, never below a 120 s floor, never
  above the configured timeout — so a slow call cannot outlive its task.
- **Per reply**: `ChatRequest.max_tokens`. The planner caps a reply at
  `PLANNER_MAX_TOKENS` (16 384); a reply cut off there is recovered the
  same way as a timeout. How the planner spends these budgets is in
  [chapter 15](./15-planner.md).
- **Response body**: any model, embedding or `/props` response larger than
  `MAX_RESPONSE_BODY_BYTES` (64 MiB) is refused rather than buffered.
- A timeout stays a timeout: error-body reads preserve it as a transport
  error rather than flattening it into an HTTP-status error (the guard's
  boot probe depends on telling them apart).

### Usage accounting

Every `agent / plan.formulate` audit row carries `llm_usage` from
`CompletionStats`: prompt, cached, completion and reasoning tokens,
reasoning length, finish reason. `null` means "the backend did not
report", never zero. Only counts, never text — the reasoning restates the
user's data. This is the first place to look when planning gets slow.

---

## Prompt-cache-friendly ordering

Local backends (Ollama and llama.cpp's in-memory prefix cache, oMLX's SSD
cache) skip prefill for the longest byte prefix a prompt shares with the
previous one. Kastellan's prompts are ordered to exploit that:

- The system message is assembled **most stable first** —
  `<base>`, `<handoff>`, `<tools>`, `<l0_meta_rules>`, `<l1_insights>`,
  `<skills>`, `<recalled>` (`core/src/prompt_assembly/assemble.rs`).
- The current time changes every minute, so it is **not** in the system
  message: it is the last key of the user message, behind the run
  history.

Measured on a ~39k-token prompt: 69 s to answer with its first bytes
changed, 4.2 s with only its tail changed. When you add anything to a
prompt, put it after everything that changes less often than it does.
`llm_usage.cached_tokens` tells you whether the cache is being hit.

---

## Public surface

```rust
use kastellan_llm_router::{ChatMessage, ChatRequest, EmbeddingRequest, Router, RouterConfig};

let cfg = RouterConfig::from_env()?;
let router = Router::new(cfg)?;

let req = ChatRequest {
    max_tokens: Some(512),
    temperature: Some(0.2),
    ..ChatRequest::new(
        router.config().local_model.clone(),
        vec![ChatMessage::system("You are an agent."),
             ChatMessage::user("Summarise these notes: …")],
    )
};
let resp = router.send(&req).await?;

let emb = router
    .embed(&EmbeddingRequest::single(router.config().embedding_model.clone(), "query text"))
    .await?;
```

Most core code does not call `embed` directly but goes through
`memory::embed_query`, which Matryoshka-truncates to the stored dimension
and writes the `actor='llm:router' action='embed'` audit row (see
[chapter 12](./12-memory-and-recall.md)).

The typed request/response shapes mean a future switch of wire format (an
Anthropic-native `/v1/messages`, say) is a translation inside this crate,
not a change at call sites.

---

## What the chokepoint guarantees today

1. **One core-side model-egress seam.** Nothing else in the core opens an
   HTTP connection to a model backend. The router's HTTP client ignores
   proxy environment variables (`no_proxy()`), so model traffic cannot be
   silently redirected through one. (Tool egress is a separate system: the
   per-worker egress proxy.)
2. **Stable typed surface.** Callers see `ChatRequest` / `ChatResponse`,
   not raw JSON; optional fields are omitted from the wire when unset, so a
   backend that never heard of a key sees a byte-identical payload.
3. **Audit-friendly.** `Backend::as_tag`, `CompletionStats` and the serde
   shapes fit the 4 KiB-capped `audit_log.payload` envelope
   (`db::audit::truncate_payload` fingerprints the rare oversized one).

---

## What is not built

- **Frontier dispatch (Phase 5).** The frontier URL/model can be set, but
  `DefaultLocalPolicy` always picks `Backend::Local`, and a policy that
  picked `Frontier` would get `RouterError::PolicyDeniedFrontier`. The
  planned gate decides local vs. frontier from task sensitivity (the
  `DataClass` CASSANDRA already enforces), token budget and per-tool
  ceilings, with the API key read from the secrets vault at dispatch time
  and never from env. `PolicyGate` (with separate `pick` / `pick_embed`)
  is the seam it plugs into.
- **Streaming.** No `stream: true` / SSE handling.
- **Native tool-call schemas.** The planner emits its plan as JSON in the
  reply text; `tool_calls` argument schemas are not used.

---

## Testing

The crate's integration tests (`llm-router/tests/*_e2e.rs`) bring up a
hand-rolled loopback HTTP listener that canned-responds — no mock-server
dependency and no real model needed. Core's daemon e2es use the shared
`tests_common::scripted_llm` (a queued, per-endpoint fake). Note its
default answers `/props` with a 503, which a configured guard reads as
"still loading" and waits up to 60 s for.

```sh
cargo test -p kastellan-llm-router
```

Live-model instruments live in `core/tests/` behind `#[ignore]` (e.g. the
Shieldstral checks in `guard_model_e2e.rs`, and `observation_capture.rs`)
and need a backend you started yourself — see
[chapter 3](./03-dev-env-macos.md#optional-local-llm-for-integration-tests)
for a local recipe.

---

## Adding a new backend

1. Add a variant to `Backend` in `backend.rs` and a tag in `as_tag`.
2. Add its config to `RouterConfig` and `from_env` (and the table in
   `config.rs`'s module doc and above). Invalid values must be errors.
3. Implement the dispatch arm in `Router::send` / `embed`.
4. Teach a `PolicyGate` when to pick it. The default must stay "never,
   unless explicitly opted into" — every backend widens the egress
   surface.
5. Keep any audit payload within the 4 KiB envelope.

Reviewers will reject a PR that adds a second model client outside this
crate. If you need to bypass the router, raise it on an issue first — the
router is where the Phase-5 policy gate becomes enforceable.
