//! `kastellan-llm-router` — sole egress for LLM calls.
//!
//! ## Role in the system
//!
//! Every model call the agent core makes — for memory recall ranking,
//! the scheduler's reasoning step, the (future) channel auto-reply
//! drafting, anything — funnels through one [`Router::send`]. That is
//! the chokepoint at which:
//!
//! * **Phase 0 today**: a single OpenAI-compatible HTTP POST is sent
//!   to the configured local backend (vLLM/SGLang on Linux,
//!   llama.cpp/Ollama on macOS) and a [`ChatResponse`] is returned.
//!   The frontier backend's URL/model can be configured but the
//!   router refuses to dispatch to it — the [`PolicyGate`] always
//!   picks [`Backend::Local`].
//! * **Phase 5 (planned)**: a real policy gate decides between local
//!   and frontier based on task sensitivity, token budget, and
//!   per-tool capability ceilings. The frontier API key is read from
//!   `db::secrets` (the AES-256-GCM-at-rest store shipped 2026-05-10)
//!   at dispatch time, never persisted in the agent's process memory
//!   beyond the one call.
//!
//! ## What the chokepoint guarantees Phase 0
//!
//! 1. **Single egress URL.** No worker, tool, or library elsewhere in
//!    the workspace opens an outbound HTTP connection to a model
//!    backend. The (future) egress proxy will see exactly one client.
//! 2. **Stable typed surface.** Callers see [`ChatRequest`] /
//!    [`ChatResponse`], not raw JSON. A future swap of the
//!    OpenAI-compat shape for the Anthropic-native `/v1/messages`
//!    shape (or both) is a translation inside this crate.
//! 3. **Audit-log friendly.** [`Backend::as_tag`] and the
//!    [`ChatRequest`] / [`ChatResponse`] serde shapes are designed to
//!    fit inside the existing 4 KiB-capped `audit_log.payload`
//!    envelope (`db::audit::truncate_payload` will fingerprint
//!    oversized payloads on the dispatcher side).
//!
//! ## What this crate does **not** do (yet)
//!
//! * **Streaming.** The OpenAI spec carries `stream: true` for SSE.
//!   Phase 1+ when the scheduler benefits from token-level interaction.
//! * **Tool-call schemas.** The `ChatMessage::Tool` role is wired,
//!   but `function_call`/`tool_calls` argument schemas are not. The
//!   scheduler will negotiate that contract in Phase 1.
//! * **Frontier dispatch.** [`PolicyGate`] is the seam; the call path
//!   is unwired by design.
//! * **Direct integration with `core::tool_host::dispatch`.** Phase 0
//!   ships the typed surface; the dispatcher chokepoint will route
//!   `actor = "llm:router"` audit rows when the first concrete
//!   consumer (Phase 1 memory recall) lands.

pub mod backend;
pub mod completion_stats;
pub mod config;
pub mod embeddings;
pub mod error;
pub mod logprob_score;
pub mod messages;
pub mod policy;
pub mod thinking;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub use backend::Backend;
pub use completion_stats::CompletionStats;
pub use config::RouterConfig;
pub use embeddings::{EmbeddingData, EmbeddingRequest, EmbeddingResponse};
pub use error::RouterError;
pub use messages::{
    ChatChoice, ChatMessage, ChatRequest, ChatResponse, ChatRole, CompletionTokensDetails,
    PromptTokensDetails, Usage,
};
pub use policy::{DefaultLocalPolicy, PolicyGate};
pub use thinking::{ThinkingPolicy, ThinkingSwitch};

use error::{truncate_for_error, ERROR_BODY_CAP};

/// Read a non-2xx response body for operator triage, **preserving a
/// timeout instead of flattening it into [`RouterError::HttpStatus`]**.
///
/// The best-effort read is right for its stated purpose: the status code
/// is the actionable part and a body that will not decode should not mask
/// it. But one read failure is not cosmetic. When the failure is the
/// request BUDGET EXPIRING, `HttpStatus` erases that fact — and the guard
/// tier's boot probe classifies a timeout as a *measurement* (it derives
/// the CEILING, because an overrun budget is an upper bound on
/// throughput) and every other failure as the FLOOR. Swallowing the
/// timeout here therefore handed the slowest hosts the SHORTEST guard
/// timeout, which is a fail-open, through the one door
/// `kastellan_core::cassandra::guard_model::tier::probe::is_timeout`
/// structurally cannot watch: it matches on `Transport`, and this path
/// had already thrown the `Transport` away.
///
/// Every other read failure keeps a placeholder. The old wording blamed
/// UTF-8, which is essentially never the cause — `Response::text`
/// decodes lossily and fails on transport, not on encoding.
/// Hard cap on a model/embedding/`/props` response body. A backend (or an
/// on-path party, when the base URL is plain HTTP) answering `200` with an
/// unbounded body would otherwise be buffered whole by `Response::text` inside
/// the one process that holds the vault and the DB role (security audit
/// 2026-09-02, router F1). 64 MiB is far above any legitimate completion or
/// embedding batch and matches the protocol crate's record cap.
pub const MAX_RESPONSE_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Read the body as (lossy) UTF-8 text, refusing to buffer past `cap` bytes.
async fn read_body_capped(mut resp: reqwest::Response, cap: usize) -> Result<String, RouterError> {
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if buf.len().saturating_add(chunk.len()) > cap {
            return Err(RouterError::BodyTooLarge { cap });
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

async fn error_body(resp: reqwest::Response) -> Result<String, RouterError> {
    match resp.text().await {
        Ok(body) => Ok(body),
        Err(e) if e.is_timeout() => Err(RouterError::Transport(e)),
        Err(_) => Ok("<error body could not be read>".to_string()),
    }
}

/// The OpenAI-compatible chat-completion sub-path appended to every
/// backend's base URL. Pinned as a constant so a refactor that
/// changes it does so deliberately at one site — every conforming
/// backend uses this exact path.
const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";

/// llama.cpp's server-properties endpoint, at the server **root** rather
/// than under the OpenAI-compat prefix. Used by the guard-weights pin to
/// learn which file the server actually loaded — see
/// [`Router::props`].
const PROPS_PATH: &str = "/props";

/// The OpenAI-compatible embeddings sub-path appended to every
/// backend's base URL. Same pinning rationale as
/// [`CHAT_COMPLETIONS_PATH`].
const EMBEDDINGS_PATH: &str = "/embeddings";

/// Sole-egress LLM client.
///
/// Construct with [`Router::new`] (uses [`DefaultLocalPolicy`]) or
/// [`Router::with_policy`] (for tests / Phase-5 wiring). The struct
/// is `Clone`-friendly via the `Arc`-wrapped policy so a single
/// router can be shared across tokio tasks.
#[derive(Debug, Clone)]
pub struct Router {
    config: RouterConfig,
    http: reqwest::Client,
    policy: Arc<dyn PolicyGate>,
    /// Set once the thinking-leak warning has been logged (#773). Shared
    /// by clones: a leak is a fact about the configuration, so one line
    /// per process says it; one per planning call would bury everything.
    thinking_leak_reported: Arc<AtomicBool>,
}

impl Router {
    /// Build a new router with the default policy gate
    /// ([`DefaultLocalPolicy`]).
    ///
    /// Returns [`RouterError::Config`] only on `reqwest::Client`
    /// construction failures (e.g. an invalid TLS root store). The
    /// timeout is sourced from `config.timeout`.
    pub fn new(config: RouterConfig) -> Result<Self, RouterError> {
        Self::with_policy(config, Arc::new(DefaultLocalPolicy))
    }

    /// Build a new router with a caller-supplied [`PolicyGate`].
    ///
    /// The policy is `Arc`-wrapped so the same gate instance can be
    /// reused across cloned routers without forcing it `Clone`.
    pub fn with_policy(
        config: RouterConfig,
        policy: Arc<dyn PolicyGate>,
    ) -> Result<Self, RouterError> {
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            // The router is the core's sole model egress and speaks to the
            // operator-configured URL directly. Never let an ambient
            // `HTTP(S)_PROXY`/`ALL_PROXY` in the daemon environment silently
            // re-route every prompt (the whole system prompt — recalled
            // memories included) through an unlisted host (security audit
            // 2026-09-02, router F2). A proxy, if ever wanted, is a config
            // decision, not an environment accident.
            .no_proxy()
            // Connect timeout shorter than the overall timeout so a
            // dead local-backend port surfaces fast — connection
            // refused on the OpenAI-compat URL is the most common
            // operator-error signal here.
            .connect_timeout(std::cmp::min(
                config.timeout,
                std::time::Duration::from_secs(5),
            ))
            .build()
            .map_err(|e| RouterError::Config(format!("failed to build reqwest client: {e}")))?;

        Ok(Self {
            config,
            http,
            policy,
            thinking_leak_reported: Arc::new(AtomicBool::new(false)),
        })
    }

    /// The configured timeout. Exposed so tests and integration
    /// harnesses can confirm the construction-time wire-through.
    pub fn timeout(&self) -> std::time::Duration {
        self.config.timeout
    }

    /// Borrow the router's configuration. Used by the agent adapter to
    /// learn the active local model name without holding a separate copy.
    pub fn config(&self) -> &RouterConfig {
        &self.config
    }

    /// Which backend would the router pick for `request`?
    ///
    /// Pure delegation to the configured [`PolicyGate`]; exposed for
    /// the audit-log payload writer that wants to record the
    /// decision alongside the request, *not* a substitute for
    /// actually calling [`Router::send`]. (The router calls `pick`
    /// itself on the dispatch path.)
    pub fn pick_backend(&self, request: &ChatRequest) -> Backend {
        self.policy.pick(request)
    }

    /// Send a chat-completion request and return the decoded response.
    ///
    /// The policy gate picks the backend; for Phase 0 that is always
    /// [`Backend::Local`] under [`DefaultLocalPolicy`]. A future
    /// frontier-picking policy will trigger
    /// [`RouterError::PolicyDeniedFrontier`] **today** because this
    /// slice does not implement the frontier dispatch path — that
    /// lands with the Phase-5 policy gate that introduces a real
    /// frontier policy.
    pub async fn send(&self, request: &ChatRequest) -> Result<ChatResponse, RouterError> {
        let backend = self.policy.pick(request);
        match backend {
            Backend::Local => self.dispatch_local(request).await,
            Backend::Frontier => Err(RouterError::PolicyDeniedFrontier(
                "frontier dispatch is unwired in Phase 0; only DefaultLocalPolicy is supported"
                    .to_string(),
            )),
        }
    }

    /// Which backend would the router pick for an embedding request?
    /// Pure delegation to the configured [`PolicyGate::pick_embed`].
    pub fn pick_embed_backend(&self, request: &EmbeddingRequest) -> Backend {
        self.policy.pick_embed(request)
    }

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
                "frontier embed dispatch is unwired in Phase 0; only DefaultLocalPolicy is supported"
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
            target: "kastellan::llm_router",
            backend = "local",
            url = %url,
            model = %request.model,
            n_inputs = request.input.len(),
            "dispatching embedding"
        );

        let resp = self.http.post(&url).json(request).send().await?;
        let status = resp.status();

        if !status.is_success() {
            let body = error_body(resp).await?;
            return Err(RouterError::HttpStatus {
                status: status.as_u16(),
                body: truncate_for_error(&body, ERROR_BODY_CAP),
            });
        }

        let body = read_body_capped(resp, MAX_RESPONSE_BODY_BYTES).await?;
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

    /// Fetch llama.cpp's `/props` from the configured local backend.
    ///
    /// Returned as a raw [`serde_json::Value`] rather than a typed
    /// struct on purpose: `/props` is a large, version-dependent grab
    /// bag (sampler defaults, the whole chat template, modality flags)
    /// and we want exactly one field from it. A typed mirror would be a
    /// standing maintenance cost that breaks on llama.cpp upgrades
    /// which do not affect us.
    ///
    /// Its one consumer today is the guard-weights pin (issue #592),
    /// which reads `model_path` to learn which file the server opened
    /// — see `kastellan_core::cassandra::guard_model::weights_pin`. It
    /// lives here rather than in `core` because `llm-router` is the sole
    /// core-side model egress, and a GET to the model server is model
    /// egress; putting a `reqwest` call in `core` to do it would breach
    /// that invariant.
    ///
    /// Note this addresses the server **root** (see the private
    /// `props_url` helper), not the OpenAI-compat prefix that
    /// [`Router::send`] uses.
    pub async fn props(&self) -> Result<serde_json::Value, RouterError> {
        let url = props_url(&self.config.local_url);
        tracing::debug!(
            target: "kastellan::llm_router",
            backend = "local",
            url = %url,
            "fetching /props"
        );

        let resp = self.http.get(&url).send().await?;
        let status = resp.status();

        if !status.is_success() {
            let body = error_body(resp).await?;
            return Err(RouterError::HttpStatus {
                status: status.as_u16(),
                body: truncate_for_error(&body, ERROR_BODY_CAP),
            });
        }

        let body = read_body_capped(resp, MAX_RESPONSE_BODY_BYTES).await?;
        serde_json::from_str(&body).map_err(|source| RouterError::DecodeProps {
            source,
            body: truncate_for_error(&body, ERROR_BODY_CAP),
        })
    }

    /// Dispatch a request to the local backend.
    ///
    /// Pure HTTP: POST to `<local_url>/chat/completions` with the
    /// JSON-encoded [`ChatRequest`]. On 2xx, decode as
    /// [`ChatResponse`]; on non-2xx, capture a truncated body and
    /// return [`RouterError::HttpStatus`].
    async fn dispatch_local(&self, request: &ChatRequest) -> Result<ChatResponse, RouterError> {
        let url = compose_url(&self.config.local_url, CHAT_COMPLETIONS_PATH);
        tracing::debug!(
            target: "kastellan::llm_router",
            backend = "local",
            url = %url,
            model = %request.model,
            "dispatching chat-completion"
        );

        // Thinking suppression is a property of the *local leg*, not of
        // any one caller, so it is applied here rather than at each call
        // site — every local completion gets it or none does, unless the
        // caller's `ThinkingPolicy` says otherwise. It is written in the
        // configured dialect (#773); an explicit value for that dialect's
        // key from the caller always wins.
        let suppress = request.thinking.suppresses(self.config.disable_thinking);
        let request =
            thinking::apply_thinking_switch(request, suppress, self.config.thinking_switch);

        let resp = self.http.post(&url).json(request.as_ref()).send().await?;
        let status = resp.status();

        if !status.is_success() {
            let body = error_body(resp).await?;
            return Err(RouterError::HttpStatus {
                status: status.as_u16(),
                body: truncate_for_error(&body, ERROR_BODY_CAP),
            });
        }

        let body = read_body_capped(resp, MAX_RESPONSE_BODY_BYTES).await?;
        let decoded: ChatResponse = serde_json::from_str(&body).map_err(|source| {
            RouterError::DecodeResponse {
                source,
                body: truncate_for_error(&body, ERROR_BODY_CAP),
            }
        })?;
        self.report_thinking_leak_once(suppress, &decoded);
        Ok(decoded)
    }

    /// Log, once per process, that the backend thought although asked not
    /// to — the "config switch that silently does nothing" of #773. The
    /// decision and the wording are pure ([`thinking::detect_thinking_leak`],
    /// [`thinking::leak_warning`]); this only owns the once-latch.
    fn report_thinking_leak_once(&self, suppressed: bool, response: &ChatResponse) {
        let Some(leak) = thinking::detect_thinking_leak(suppressed, response) else {
            return;
        };
        if !self.thinking_leak_reported.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                target: "kastellan::llm_router",
                reasoning_chars = leak.reasoning_chars,
                reasoning_tokens = ?leak.reasoning_tokens,
                thinking_switch = self.config.thinking_switch.as_str(),
                "{}",
                thinking::leak_warning(&leak, self.config.thinking_switch, self.config.disable_thinking)
            );
        }
    }
}

/// Join a base URL and a sub-path, collapsing exactly one `/` between
/// them. Pure helper kept in the crate root because both backends
/// share it; deliberately not a public re-export.
///
/// This intentionally does *not* use `url::Url::join` (would add a
/// dep) — the OpenAI-compat path is always a literal `/foo`-style
/// constant on our side, so trimming a single trailing `/` from the
/// base is sufficient and pinned by unit tests.
fn compose_url(base: &str, path: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    if path.starts_with('/') {
        format!("{trimmed}{path}")
    } else {
        format!("{trimmed}/{path}")
    }
}

/// The llama.cpp `/props` URL for a backend configured at `base`.
///
/// Pure. Kept beside [`compose_url`] because it is the same class of
/// helper, but it cannot *be* `compose_url`: `/chat/completions` lives
/// **under** the OpenAI-compat prefix and `/props` lives at the server
/// **root**, so this has to remove a segment where `compose_url` only
/// ever appends one.
///
/// Two ways to get this wrong, each pinned by its own unit test:
///
/// * stripping a bare `v1` rather than the `/v1` segment would rewrite
///   a base that merely *ends* in those two characters
///   (`…/apiv1` → `…/api`), addressing a different server entirely;
/// * `trim_end_matches` **repeats**, so a base ending `/v1/v1` would
///   lose both. `strip_suffix` removes at most one, which is what
///   "strip the compat segment" means.
fn props_url(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    let root = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    compose_url(root, PROPS_PATH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_url_trims_trailing_slash_on_base() {
        assert_eq!(
            compose_url("http://127.0.0.1:8000/v1", "/chat/completions"),
            "http://127.0.0.1:8000/v1/chat/completions"
        );
        assert_eq!(
            compose_url("http://127.0.0.1:8000/v1/", "/chat/completions"),
            "http://127.0.0.1:8000/v1/chat/completions"
        );
        assert_eq!(
            compose_url("http://127.0.0.1:8000/v1//", "/chat/completions"),
            "http://127.0.0.1:8000/v1/chat/completions"
        );
    }

    #[test]
    fn compose_url_inserts_slash_when_path_lacks_leading() {
        assert_eq!(
            compose_url("http://127.0.0.1:8000/v1", "chat/completions"),
            "http://127.0.0.1:8000/v1/chat/completions"
        );
    }

    #[test]
    fn chat_completions_path_is_pinned() {
        // Wire-shape pin shared with every OpenAI-compatible backend.
        assert_eq!(CHAT_COMPLETIONS_PATH, "/chat/completions");
    }

    #[test]
    fn props_url_strips_the_openai_compat_segment() {
        // llama.cpp serves /props at the SERVER ROOT, not under /v1 —
        // so this cannot go through `compose_url` on the configured
        // base the way `/chat/completions` does.
        assert_eq!(props_url("http://127.0.0.1:8081/v1"), "http://127.0.0.1:8081/props");
        assert_eq!(props_url("http://127.0.0.1:8081/v1/"), "http://127.0.0.1:8081/props");
        assert_eq!(props_url("http://127.0.0.1:8081/v1//"), "http://127.0.0.1:8081/props");
    }

    #[test]
    fn props_url_leaves_a_base_that_has_no_compat_segment() {
        assert_eq!(props_url("http://127.0.0.1:8081"), "http://127.0.0.1:8081/props");
        assert_eq!(props_url("http://127.0.0.1:8081/"), "http://127.0.0.1:8081/props");
    }

    #[test]
    fn props_url_strips_only_a_whole_path_segment() {
        // The failure this pins: stripping a bare `v1` (no slash)
        // would eat the tail of a base that merely *ends* in those
        // two characters, silently addressing a different server.
        // Only a complete `/v1` path segment may be removed.
        assert_eq!(props_url("http://127.0.0.1:8081/apiv1"), "http://127.0.0.1:8081/apiv1/props");
        assert_eq!(props_url("http://127.0.0.1:8081/openai/v1"), "http://127.0.0.1:8081/openai/props");
    }

    #[test]
    fn props_url_strips_at_most_one_compat_segment() {
        // `trim_end_matches` repeats; `strip_suffix` does not. A base
        // ending `/v1/v1` is a misconfiguration, but eating both
        // segments would silently address a different server root.
        assert_eq!(props_url("http://127.0.0.1:8081/v1/v1"), "http://127.0.0.1:8081/v1/props");
    }

    #[test]
    fn router_new_succeeds_with_default_config() {
        let cfg = RouterConfig::default();
        let r = Router::new(cfg.clone()).expect("default config should build");
        assert_eq!(r.timeout(), cfg.timeout);
    }

    #[test]
    fn router_pick_backend_delegates_to_policy() {
        // Confirms the public `pick_backend` proxy and pins
        // `DefaultLocalPolicy`'s Phase-0 behaviour at the router
        // level (in addition to the policy module's own test).
        let r = Router::new(RouterConfig::default()).unwrap();
        let req = ChatRequest::new("m", vec![ChatMessage::user("hi")]);
        assert_eq!(r.pick_backend(&req), Backend::Local);
    }

    /// A test-only [`PolicyGate`] that always picks [`Backend::Frontier`].
    /// Used to prove that Phase 0's `dispatch_frontier` rejection path
    /// fires without needing a real frontier URL or to disturb the
    /// default policy's pin elsewhere.
    #[derive(Debug)]
    struct AlwaysFrontier;
    impl PolicyGate for AlwaysFrontier {
        fn pick(&self, _request: &ChatRequest) -> Backend {
            Backend::Frontier
        }
    }

    #[tokio::test]
    async fn router_send_rejects_frontier_choice_in_phase_0() {
        let r = Router::with_policy(RouterConfig::default(), Arc::new(AlwaysFrontier)).unwrap();
        let req = ChatRequest::new("m", vec![ChatMessage::user("hi")]);
        let err = r.send(&req).await.expect_err("frontier dispatch must be refused");
        match err {
            RouterError::PolicyDeniedFrontier(msg) => {
                assert!(msg.contains("frontier"), "msg={msg}");
            }
            other => panic!("expected PolicyDeniedFrontier, got {other:?}"),
        }
    }

    /// A test-only [`PolicyGate`] that overrides only `pick_embed` to
    /// return [`Backend::Frontier`], leaving `pick` at the chat default
    /// (`Backend::Local`). Used to prove `Router::embed`'s frontier
    /// rejection path fires independently of the chat path.
    #[derive(Debug)]
    struct AlwaysFrontierEmbed;
    impl PolicyGate for AlwaysFrontierEmbed {
        fn pick(&self, _request: &ChatRequest) -> Backend {
            Backend::Local
        }
        fn pick_embed(&self, _request: &EmbeddingRequest) -> Backend {
            Backend::Frontier
        }
    }

    #[tokio::test]
    async fn router_embed_rejects_frontier_choice_in_phase_0() {
        let r = Router::with_policy(RouterConfig::default(), Arc::new(AlwaysFrontierEmbed)).unwrap();
        let req = EmbeddingRequest::single("m", "hi");
        let err = r.embed(&req).await.expect_err("frontier embed dispatch must be refused");
        match err {
            RouterError::PolicyDeniedFrontier(msg) => {
                assert!(msg.contains("frontier"), "msg={msg}");
                assert!(msg.contains("Phase 0"), "msg must mention Phase 0: {msg}");
            }
            other => panic!("expected PolicyDeniedFrontier, got {other:?}"),
        }
    }

    #[test]
    fn router_pick_embed_backend_delegates_to_policy() {
        // Confirms the public `pick_embed_backend` proxy and pins
        // `DefaultLocalPolicy`'s Phase-0/1 behaviour for embed at the
        // router level (in addition to the policy module's own test).
        let r = Router::new(RouterConfig::default()).unwrap();
        let req = EmbeddingRequest::single("m", "hi");
        assert_eq!(r.pick_embed_backend(&req), Backend::Local);
    }
}
