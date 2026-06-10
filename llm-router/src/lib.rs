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
pub mod config;
pub mod embeddings;
pub mod error;
pub mod messages;
pub mod policy;

use std::sync::Arc;

pub use backend::Backend;
pub use config::RouterConfig;
pub use embeddings::{EmbeddingData, EmbeddingRequest, EmbeddingResponse};
pub use error::RouterError;
pub use messages::{ChatChoice, ChatMessage, ChatRequest, ChatResponse, ChatRole, Usage};
pub use policy::{DefaultLocalPolicy, PolicyGate};

use error::{truncate_for_error, ERROR_BODY_CAP};

/// The OpenAI-compatible chat-completion sub-path appended to every
/// backend's base URL. Pinned as a constant so a refactor that
/// changes it does so deliberately at one site — every conforming
/// backend uses this exact path.
const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";

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
            // Best effort: read the body for operator triage. If the
            // body itself fails to read we still want a useful error.
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

        let resp = self.http.post(&url).json(request).send().await?;
        let status = resp.status();

        if !status.is_success() {
            // Best effort: read the body for operator triage. If the
            // body itself fails to read we still want a useful error.
            let body = resp.text().await.unwrap_or_else(|_| {
                "<error body could not be read as UTF-8 text>".to_string()
            });
            return Err(RouterError::HttpStatus {
                status: status.as_u16(),
                body: truncate_for_error(&body, ERROR_BODY_CAP),
            });
        }

        let body = resp.text().await?;
        let decoded: ChatResponse = serde_json::from_str(&body).map_err(|source| {
            RouterError::DecodeResponse {
                source,
                body: truncate_for_error(&body, ERROR_BODY_CAP),
            }
        })?;
        Ok(decoded)
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
