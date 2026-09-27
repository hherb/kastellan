# 13 — LLM router

The `llm-router` crate is the **only** core-side egress to a model
backend. Every model call the agent core makes — scheduler reasoning,
recall embeddings, the future auto-reply drafter — goes through
`Router::send` (or `Router::embed`). (One other crate in the workspace
deliberately speaks to a model backend: the trusted `embed-broker`
sidecar, which bridges a jailed worker's Unix socket to the operator's
OpenAI-compatible embedding endpoint so in-VM workers can embed without
egress; `search-broker` is the analogous sidecar for search.) The router
remains the obvious mounting point for the future egress proxy, for the
policy gate that decides local vs. frontier, and for the audit-row
payload format.

> This chapter documents the developer-facing surface. The crate-level
> doc comment in `llm-router/src/lib.rs` is the live source of truth.

---

## Module layout

```
llm-router/src/
  lib.rs         Router type, send entry point, re-exports
  backend.rs     Backend enum (Local / Frontier) + as_tag for audit
  config.rs      Endpoint + model config, env-var schema
  messages.rs    ChatRequest / ChatResponse / ChatMessage typed wire shape
  embeddings.rs  embed() — the call path used by memory::embed_query
  policy.rs      PolicyGate — picks the backend for a request
  error.rs       RouterError surface
```

---

## Phase 0 vs. Phase 5

**Phase 0 (today):**

- A single OpenAI-compatible HTTP POST is sent to the configured **local**
  backend (vLLM/SGLang on Linux, llama.cpp/Ollama on macOS) and a
  `ChatResponse` is returned.
- The frontier backend's URL/model can be configured but `PolicyGate`
  always picks `Backend::Local`. The frontier path is wired but
  inert.

**Phase 5 (planned):**

- A real `PolicyGate` decides between local and frontier based on task
  sensitivity (the `DataClass` from CASSANDRA Stage 0), token budget,
  and per-tool capability ceilings.
- The frontier API key is read from `db::secrets` (AES-256-GCM-at-rest)
  at dispatch time and never persisted in the agent's process memory
  beyond the one call.

---

## Configuration

The router is configured from environment variables (see `config.rs`):

| Variable | Default | Purpose |
|----------|---------|---------|
| `KASTELLAN_LLM_LOCAL_URL` | `http://127.0.0.1:8000/v1` — vLLM/SGLang on Linux, oMLX on macOS | OpenAI-compatible base URL |
| `KASTELLAN_LLM_LOCAL_MODEL` | `local-default` (a placeholder every real deployment overrides) | Model name to pass in the request body |
| `KASTELLAN_LLM_TIMEOUT_MS` | `180000` | Whole-request budget; bounds **generation**, not just connect |
| `KASTELLAN_LLM_DISABLE_THINKING` | `1` (on) | Ask a reasoning model not to think (`0` = let it think) |
| `KASTELLAN_LLM_THINKING_SWITCH` | `chat_template_kwargs` | How that request is written: `chat_template_kwargs` (vLLM, SGLang, llama.cpp) or `reasoning_effort` (**Ollama**) |
| Frontier vars (Phase 5) | — | Endpoint + secret-store key; gated by `PolicyGate` |

⚠️ **On Ollama, `KASTELLAN_LLM_DISABLE_THINKING` does nothing unless
`KASTELLAN_LLM_THINKING_SWITCH=reasoning_effort`** — Ollama silently ignores
`chat_template_kwargs` (#773). The router cannot send both: vLLM 0.15 rejects
`reasoning_effort: "none"` with a 400. A backend that thinks despite being
asked not to is logged once at WARN, with the setting to change. The guard
endpoint always uses `chat_template_kwargs`, whatever this says — it names
the planner's backend.

Every `agent / plan.formulate` audit row carries `llm_usage` (prompt, cached,
completion and reasoning tokens, reasoning length, finish reason; `null` =
not reported) — the first place to look when planning gets slow (#774).

Only `rustls-tls` and `json` features of `reqwest` are enabled
(workspace-level decision, see top-level `Cargo.toml`). `openssl-sys` is
deliberately not linked.

---

## Public surface

```rust
let router = Router::new(Config::from_env()?)?;

let resp: ChatResponse = router.send(ChatRequest {
    messages: vec![
        ChatMessage::system("You are an agent."),
        ChatMessage::user("Summarise these notes: …"),
    ],
    temperature: Some(0.2),
    max_tokens: Some(512),
    ..Default::default()
}).await?;

let emb: Vec<f32> = router.embed("query text").await?;
```

The typed `ChatRequest` / `ChatResponse` / `ChatMessage` shapes mean a
future swap of the OpenAI-compatible wire format for the Anthropic-native
`/v1/messages` shape (or both) is a translation inside this crate — no
caller changes.

---

## What the chokepoint guarantees today

1. **Single core-side egress URL.** Nothing else in the agent core opens
   an outbound HTTP connection to a model backend. (The per-worker
   egress proxy that already guards *tool* egress is separate, and the
   trusted `embed-broker` sidecar is the one other deliberate
   model-backend client in the workspace — it serves jailed workers over
   a Unix socket so they can embed without egress. The router is the
   core's own single LLM-egress seam, and is where a Phase-5 LLM-egress
   policy would mount.)
2. **Stable typed surface.** Callers see `ChatRequest` / `ChatResponse`,
   not raw JSON.
3. **Audit-log friendly.** `Backend::as_tag` and the serde shapes are
   sized to fit the 4 KiB-capped `audit_log.payload` envelope
   (`db::audit::truncate_payload` will SHA-256-fingerprint the rare
   oversized payload on the dispatcher side). `actor = "llm:router"`
   audit rows are written today: Phase 1 memory recall shipped, and
   `memory::embed_query` routes every recall embedding through the
   router and audits the call — see
   [chapter 12](./12-memory-and-recall.md).

---

## What this crate does not do yet

- **Streaming.** No `stream: true` SSE handling. Phase 1+, when the
  scheduler benefits from token-level interaction.
- **Tool-call schemas.** `ChatMessage::Tool` exists but
  `function_call` / `tool_calls` argument schemas are not wired. The
  scheduler will negotiate that in Phase 1.
- **Frontier dispatch.** `PolicyGate` is the seam; the call path is
  unwired by design until Phase 5.

---

## Testing

The crate ships unit tests against a mock HTTP server (no real model
needed). End-to-end tests that hit a live local LLM are gated behind
`#[ignore]` and require the operator to start vLLM/SGLang or Ollama
themselves — see [chapter 3](./03-dev-env-macos.md#optional-local-llm-for-integration-tests)
for the macOS Ollama recipe.

```sh
cargo test -p kastellan-llm-router            # mock tests
cargo test -p kastellan-llm-router -- --ignored
                                            # live local-LLM tests (need backend running)
```

---

## Adding a new backend

1. Add a variant to `Backend` in `backend.rs` and a tag in `as_tag`.
2. Implement the dispatch path in `lib.rs` (a new arm in the match).
3. Extend `PolicyGate` in `policy.rs` to decide when to pick it. Default
   should be "never, unless explicitly opted into" — every new backend
   widens the egress surface.
4. Document the new env vars in `config.rs` and in the table above.
5. Audit-row payload: keep within the 4 KiB envelope, or the dispatcher
   will fingerprint-truncate.

Reviewers will reject a PR that adds a second egress client outside this
crate. If you find yourself wanting to bypass the router, surface the
need on an issue first — the router is the seam by which the egress
proxy and the policy gate become enforceable in Phase 5.
