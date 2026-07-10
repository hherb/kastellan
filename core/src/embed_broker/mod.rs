//! Trusted embedding-broker sidecar — core-side wiring (Slice B).
//!
//! A jailed / force-routed / micro-VM worker cannot reach the operator's
//! embedding backend directly (loopback Ollama is SSRF-blocked by the egress
//! proxy; a MITM re-origination is webpki-only). Instead of routing embeddings
//! through the general egress proxy or baking an embedding model into every
//! worker, core spawns a single-purpose **embed-broker sidecar** per consuming
//! worker (1:1, mirroring the egress force-routing sidecar). The broker binds a
//! UDS, which core binds into the worker's jail via
//! [`kastellan_sandbox::SandboxPolicy::embed_broker_uds`] (Slice B1); the worker
//! reaches the backend *only* through that socket, so the embed host leaves the
//! worker's `Net::Allowlist` entirely.
//!
//! Slice A (merged `b077629`) built the two ends of the pipe — the
//! `kastellan-worker-embed-broker` crate and the worker-side `BrokeredEmbedder`
//! / `choose_embedder`. This module is the middle: core actually *spawns* the
//! broker and binds it in.
//!
//! Layout:
//! * [`EmbedBrokerSpec`] (this file) — the per-worker declaration a manifest
//!   emits in broker mode: which backend the broker forwards to.
//! * `config` (Slice B, Task 3) — [`config::EmbedBrokerConfig`], the daemon-level
//!   discovered-binary + scratch-root, analogous to `ForceRoutingConfig`.
//! * `spawn` (Slice B, Task 3) — `spawn_embed_broker`, mirroring
//!   `egress::net_worker::spawn_forced_net_worker`.

pub mod config;
pub mod spawn;

pub use config::EmbedBrokerConfig;
pub use spawn::{spawn_embed_broker, EmbedBrokerSidecar};

/// Per-worker declaration that a worker wants a trusted embed-broker sidecar,
/// carrying the backend the broker forwards to.
///
/// Set by a worker manifest in broker mode (e.g. web-research under
/// `KASTELLAN_WEB_RESEARCH_USE_EMBED_BROKER=1`). When a [`crate::scheduler::ToolEntry`]
/// carries `embed_broker: Some(spec)`, the manifest also **omits** the embed
/// backend host from the worker's `Net::Allowlist` and does **not** inject the
/// worker's direct embed-endpoint env — the worker never reaches the backend
/// directly. Core's spawn chokepoint (Task 4) reads this field, spawns the
/// broker with `Net::Allowlist([host_of(endpoint)])`, and injects
/// `KASTELLAN_EMBED_BROKER_UDS` (the jail path of the bound socket) so the
/// worker's `choose_embedder` selects `BrokeredEmbedder`.
///
/// * `endpoint` — the backend's OpenAI-compatible embeddings URL. Goes to the
///   *broker's* `KASTELLAN_EMBED_BROKER_ENDPOINT` env (not the worker's), and
///   its host:port becomes the broker's own `Net::Allowlist` entry.
/// * `model` — the embedding model name. Goes to the *worker's* embed-model env
///   (`KASTELLAN_WEB_RESEARCH_EMBED_MODEL`), since the worker's `BrokeredEmbedder`
///   sends the model per request; the broker forwards it verbatim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmbedBrokerSpec {
    /// Backend embeddings URL the broker forwards to (e.g.
    /// `http://127.0.0.1:11434/v1/embeddings`).
    pub endpoint: String,
    /// Embedding model name the worker requests through the broker.
    pub model: String,
}

impl EmbedBrokerSpec {
    /// Convenience constructor.
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        Self { endpoint: endpoint.into(), model: model.into() }
    }
}
