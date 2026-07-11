//! Trusted broker sidecar — core-side wiring (kind-parameterized).
//!
//! A jailed / force-routed / micro-VM worker cannot reach an operator backend
//! (embedding backend, search backend) directly: loopback services are
//! SSRF-blocked by the egress proxy, and a MITM re-origination is webpki-only.
//! Instead of routing those requests through the general egress proxy or baking a
//! backend into every worker, core spawns a single-purpose **broker sidecar** per
//! consuming worker (1:1, mirroring the egress force-routing sidecar). The broker
//! binds a UDS, which core binds into the worker's jail via
//! [`kastellan_sandbox::SandboxPolicy::broker_uds`]; the worker reaches the
//! backend *only* through that socket, so the backend host leaves the worker's
//! `Net::Allowlist` entirely.
//!
//! Every per-kind string (binary name, the socket / env / scratch naming
//! contracts) is owned by [`BrokerKind`], so a second broker kind reuses all of
//! this plumbing. `BrokerKind::Embed` reproduces every string the merged
//! embed-broker used, so web-research is byte-for-byte unaffected.
//!
//! Layout:
//! * [`kind`] — [`BrokerKind`], the single source of truth for every per-kind
//!   string.
//! * [`BrokerSpec`] (this file) — the per-worker declaration a manifest emits in
//!   broker mode: which kind + which backend the broker forwards to.
//! * [`config`] — [`config::BrokerConfig`] (the daemon-level discovered-binary +
//!   scratch-root, analogous to `ForceRoutingConfig`) and [`config::BrokerConfigs`]
//!   (the per-kind registry).
//! * [`spawn`] — [`spawn::spawn_broker`], mirroring
//!   `egress::net_worker::spawn_forced_net_worker`.

pub mod config;
pub mod kind;
pub mod spawn;

pub use config::{from_env, BrokerConfig, BrokerConfigs};
pub use kind::BrokerKind;
pub use spawn::{spawn_broker, BrokerSidecar};

/// Per-worker declaration that a worker wants a trusted broker sidecar of a
/// given kind, carrying the backend the broker forwards to. Set by a manifest;
/// core's chokepoint spawns the broker, binds its UDS into the jail
/// (`SandboxPolicy::broker_uds`), and injects `kind.uds_env()`.
///
/// The manifest also drops the backend host from the worker's `Net::Allowlist`
/// and omits the worker's direct-endpoint env, so the worker reaches the backend
/// only through the broker. Any model/param the *worker* needs (e.g. an embed
/// model) is set by the manifest in the worker's own env — it is not carried
/// here (the spawn path needs only kind + endpoint).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrokerSpec {
    pub kind: BrokerKind,
    pub endpoint: String,
}

impl BrokerSpec {
    pub fn embed(endpoint: impl Into<String>) -> Self {
        Self { kind: BrokerKind::Embed, endpoint: endpoint.into() }
    }
    pub fn search(endpoint: impl Into<String>) -> Self {
        Self { kind: BrokerKind::Search, endpoint: endpoint.into() }
    }
}
