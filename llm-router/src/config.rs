//! [`RouterConfig`] — the static description of which backends the
//! router can reach and what to call by default.
//!
//! The config is populated from environment variables (test-friendly
//! seam, same shape as `KASTELLAN_DATA_DIR` / `KASTELLAN_STATE_DIR` in
//! `core`) with a default local-backend URL — per *host runtime* by
//! contract, one value on every OS today — so a fresh checkout works
//! without any setup on a machine that has the expected runtime
//! installed:
//!
//! * **Linux:** `http://127.0.0.1:8000/v1` — the default vLLM /
//!   SGLang OpenAI-compat port.
//! * **macOS:** `http://127.0.0.1:8000/v1` — the default oMLX port.
//!   oMLX serves MLX-quantised models through the same
//!   OpenAI-compatible surface and is materially faster than Ollama
//!   on Apple silicon, the gap widening with model size. Ollama
//!   (`:11434`) and llama.cpp's `--api` server remain supported —
//!   set `KASTELLAN_LLM_LOCAL_URL`.
//!
//! ## Environment variables
//!
//! | Var | Purpose | Default |
//! | --- | --- | --- |
//! | `KASTELLAN_LLM_LOCAL_URL` | Base URL of the local backend (no trailing `/`) | `http://127.0.0.1:8000/v1`, see above |
//! | `KASTELLAN_LLM_LOCAL_MODEL` | Default model name passed to the local backend | `local-default` |
//! | `KASTELLAN_LLM_EMBEDDING_URL` | Base URL of the embedding backend | falls back to local URL |
//! | `KASTELLAN_LLM_EMBEDDING_MODEL` | Default model name passed to the embedding backend | `embedding-default` |
//! | `KASTELLAN_LLM_FRONTIER_URL` | Base URL of the frontier backend | unset (frontier disabled) |
//! | `KASTELLAN_LLM_FRONTIER_MODEL` | Default model on the frontier backend | unset |
//! | `KASTELLAN_LLM_GUARD_URL` | Base URL of the model-based guard backend (Shieldstral) | unset (guard tier disabled) |
//! | `KASTELLAN_LLM_GUARD_MODEL` | Default model on the guard backend | unset |
//! | `KASTELLAN_LLM_TIMEOUT_MS` | Request timeout, milliseconds | 180_000 |
//! | `KASTELLAN_LLM_DISABLE_THINKING` | Suppress the local model's thinking block | `1` (on) |
//! | `KASTELLAN_LLM_THINKING_SWITCH` | How to say "don't think": `chat_template_kwargs` (vLLM, SGLang, llama.cpp) or `reasoning_effort` (**Ollama**) | `chat_template_kwargs` |
//!
//! ⚠️ **On Ollama, `KASTELLAN_LLM_DISABLE_THINKING` does nothing unless
//! `KASTELLAN_LLM_THINKING_SWITCH=reasoning_effort`** — Ollama silently
//! ignores the default dialect (#773). Why the router cannot just send both
//! is in [`crate::thinking`]; a backend that thinks anyway is reported once
//! at WARN with the fix.
//!
//! `KASTELLAN_LLM_DISABLE_THINKING` accepts `1`/`true`/`yes`/`on` and
//! `0`/`false`/`no`/`off` (trimmed, case-insensitive) and **rejects any
//! other non-empty value with a config error**. Empty is the one
//! exception, and it is not special-cased here: `read_env` treats an
//! empty value as *absent* for every var in this table, so a stray
//! `export KASTELLAN_LLM_DISABLE_THINKING=` falls through to the
//! default — which for this var is `on`, i.e. the safe direction.
//! Rejecting the rest is a deliberate
//! divergence from `kastellan-core`'s canonical `env_flag_enabled`
//! dialect, which reads an unrecognised value as *off*: those flags all
//! default off, so a typo there fails safe, whereas this one defaults
//! **on** — a typo read as "off" would silently restore the runaway
//! thinking this switch exists to prevent, and nothing downstream names
//! thinking when it does. (`llm-router` cannot call `env_flag_enabled`
//! regardless: `core` depends on this crate, not the reverse.)
//!
//! The frontier URL/model are deliberately *not* defaulted. Phase 0
//! refuses to dispatch to the frontier even when set; setting the
//! env vars is purely a forward-compatible seam so Phase 5 can wire
//! the policy gate without re-plumbing.
//!
//! Authentication keys for the frontier backend are *not* read from
//! env. They live in `db::secrets` (cf. the secrets-at-rest slice
//! shipped 2026-05-10) and will be fetched at dispatch time when
//! Phase 5's policy gate lands. Reading them from env at config-load
//! time would defeat the purpose of the keyring-wrapped at-rest
//! encryption.

use std::time::Duration;

use crate::error::RouterError;
use crate::thinking::{ThinkingSwitch, THINKING_SWITCH_ENV};

pub const DEFAULT_LOCAL_MODEL: &str = "local-default";
pub const DEFAULT_EMBEDDING_MODEL: &str = "embedding-default";
/// Overall per-request timeout (the `reqwest` total `.timeout()`), in
/// milliseconds. This bounds **generation**, not just connect — a local
/// agentic plan over a 26B-class model with a multi-KB system prompt was
/// measured at ~86 s on the DGX, and the previous 30 s value cut those
/// calls off mid-generation, surfacing as a misleading
/// `RouterError::Transport("error sending request …")` (a `reqwest`
/// timeout displays exactly like a send failure). A dead backend still
/// fails fast via the separate 5 s `connect_timeout`, so this generous
/// value only ever bites a genuinely slow/hung generation. Operators
/// override with `KASTELLAN_LLM_TIMEOUT_MS`.
pub const DEFAULT_TIMEOUT_MS: u64 = 180_000;

/// Default base URL for the local backend — per host runtime by contract,
/// one value on every OS today.
///
/// Pure function (no env reads, no I/O). Returned as `&'static str`
/// so it composes into [`RouterConfig::default`] without an
/// allocation.
///
/// **Every OS currently resolves to the same value**, because the
/// default runtimes happen to agree on the port: vLLM/SGLang on Linux,
/// oMLX on macOS, and `:8000` as the least-bad guess elsewhere (better
/// to point at *something* than to require an env var — an unsupported
/// host then fails fast with connection-refused, which is the right
/// signal). It was per-OS while macOS defaulted to Ollama's `:11434`;
/// git history has the migration, this comment does not need to.
///
/// The function is kept — rather than inlined to a constant — because
/// the *contract* is "whatever this host's default local runtime
/// listens on", and that is what callers depend on. It is the seam
/// where a future divergence goes; a `cfg!` chain whose arms all agree
/// is not (clippy's `if_same_then_else` rejects it, correctly).
pub fn default_local_url_for_os() -> &'static str {
    "http://127.0.0.1:8000/v1"
}

// `Eq` is deliberately NOT derived: `guard_tau` is an `Option<f32>`,
// and floats are not `Eq` because `NaN != NaN`. That is not a nuisance
// to work around here — a config holding a NaN threshold is exactly the
// case `validate_tau` refuses, and claiming total equality over a type
// that can hold one would be a lie the compiler is right to reject.
// Nothing keys a map on a `RouterConfig`; `PartialEq` is all the tests
// need.
#[derive(Debug, Clone, PartialEq)]
pub struct RouterConfig {
    pub local_url: String,
    pub local_model: String,
    /// Base URL for the embedding backend. Defaults to `local_url`
    /// so a single OpenAI-compat server (oMLX, Ollama, or vLLM with
    /// both chat and embed loaded) works without setting two env vars.
    pub embedding_url: String,
    /// Default model name passed in the `model` field of
    /// `POST /embeddings`. Defaults to `"embedding-default"` — a
    /// placeholder that vLLM will reject with 4xx in production,
    /// forcing the operator to set `KASTELLAN_LLM_EMBEDDING_MODEL`
    /// explicitly (loud failure preferred to silent fallback).
    pub embedding_model: String,
    /// Set if and only if the operator has expressed intent to use a
    /// frontier backend. Phase 0 still refuses to dispatch even when
    /// set — the policy gate lands in Phase 5.
    pub frontier_url: Option<String>,
    pub frontier_model: Option<String>,
    /// Base URL for the model-based guard tier (Shieldstral on
    /// llama.cpp). `None` means the tier is unconfigured.
    ///
    /// **Never falls back to `local_url`.** That endpoint serves the
    /// planner model, which would answer the guard's
    /// `<Instruct>`/`<Query>` prompt with fluent prose rather than a
    /// calibrated yes/no logit pair — producing a number that looks
    /// exactly like a score and means nothing. Unconfigured yields an
    /// explicit "unmeasured", never a probability.
    pub guard_url: Option<String>,
    /// Model name sent to [`RouterConfig::guard_url`]. `None` means
    /// unconfigured; both must be set for the tier to be usable.
    pub guard_model: Option<String>,
    /// The guard tier's decision threshold, from
    /// `KASTELLAN_LLM_GUARD_TAU`. `None` means the operator supplied
    /// none.
    ///
    /// **There is deliberately no default** (wiring-spec D1): a
    /// provisional threshold that becomes a default is an unfitted
    /// number wearing the appearance of a sanctioned one. The fitted
    /// value from measurement 3 is 0.79552656 and it is the operator's
    /// to supply.
    ///
    /// **Parsed here, validated elsewhere.** This field holds whatever
    /// float the operator wrote; whether it is a *usable* threshold is
    /// a security decision and lives beside the comparison it feeds, in
    /// `kastellan_core::cassandra::guard_model::tier::validate_tau`.
    /// Splitting it that way keeps this module free of policy and keeps
    /// the range check next to the `p >= tau` it protects.
    pub guard_tau: Option<f32>,
    /// Operator override for the guard tier's request timeout, in
    /// milliseconds, from `KASTELLAN_LLM_GUARD_TIMEOUT_MS`.
    ///
    /// `None` does **not** mean "use `timeout`" — it means *derive one
    /// at boot* by probing the guard backend's prompt-processing
    /// throughput (wiring-spec D9). A constant cannot be right for
    /// hosts that differ by more than an order of magnitude on the same
    /// document, and too short a guard timeout does not error: it fails
    /// open.
    pub guard_timeout_ms: Option<u64>,
    pub timeout: Duration,
    /// Ask the local backend's chat template to suppress the model's
    /// thinking block on every chat completion (see
    /// [`crate::messages::ChatRequest::without_thinking`]). Defaults to
    /// `true`: a reasoning model left to think freely on a large prompt
    /// overruns any sane request timeout, and the failure surfaces as a
    /// transport error or an empty completion rather than as anything
    /// that names thinking. Backends without the switch ignore the key,
    /// so the default is inert for them.
    ///
    /// Set `KASTELLAN_LLM_DISABLE_THINKING=0` to let the model think —
    /// appropriate when the local model is not a reasoning model, or
    /// when reasoning quality matters more than latency.
    ///
    /// ⚠️ **Only as good as [`RouterConfig::thinking_switch`]**: a backend
    /// that does not speak the configured dialect thinks anyway (#773).
    pub disable_thinking: bool,
    /// The wire dialect used to suppress thinking, from
    /// `KASTELLAN_LLM_THINKING_SWITCH` — see [`crate::thinking`]. Used both
    /// for `disable_thinking` and for a caller's per-request
    /// [`crate::thinking::ThinkingPolicy::Suppress`].
    pub thinking_switch: ThinkingSwitch,
}

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
            guard_url: None,
            guard_model: None,
            guard_tau: None,
            guard_timeout_ms: None,
            timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
            disable_thinking: true,
            thinking_switch: ThinkingSwitch::ChatTemplateKwargs,
        }
    }
}

impl RouterConfig {
    /// Read the config from environment variables, falling back to
    /// [`RouterConfig::default`] on any unset key.
    ///
    /// Returns [`RouterError::Config`] only for **invalid** values
    /// (e.g. a non-numeric `KASTELLAN_LLM_TIMEOUT_MS`); an *unset* var
    /// is always fine and just means "use the default".
    pub fn from_env() -> Result<Self, RouterError> {
        let mut cfg = Self::default();

        if let Some(v) = read_env("KASTELLAN_LLM_LOCAL_URL")? {
            cfg.local_url = v.clone();
            // local_url change also drives the embedding fallback —
            // re-sync embedding_url unless the operator has already
            // overridden it explicitly below.
            cfg.embedding_url = v;
        }
        if let Some(v) = read_env("KASTELLAN_LLM_LOCAL_MODEL")? {
            cfg.local_model = v;
        }
        if let Some(v) = read_env("KASTELLAN_LLM_EMBEDDING_URL")? {
            cfg.embedding_url = v;
        }
        if let Some(v) = read_env("KASTELLAN_LLM_EMBEDDING_MODEL")? {
            cfg.embedding_model = v;
        }
        cfg.frontier_url = read_env("KASTELLAN_LLM_FRONTIER_URL")?;
        cfg.frontier_model = read_env("KASTELLAN_LLM_FRONTIER_MODEL")?;
        cfg.guard_url = read_env("KASTELLAN_LLM_GUARD_URL")?;
        cfg.guard_model = read_env("KASTELLAN_LLM_GUARD_MODEL")?;
        if let Some(v) = read_env("KASTELLAN_LLM_GUARD_TAU")? {
            // Parsed, not range-checked: `validate_tau` in `core` owns
            // the range because it owns the comparison. What is refused
            // here is text that is not a float at all — the same
            // fail-loudly posture KASTELLAN_LLM_TIMEOUT_MS takes.
            let tau: f32 = v.parse().map_err(|_| {
                RouterError::Config(format!(
                    "KASTELLAN_LLM_GUARD_TAU must be a decimal number, got {v:?}"
                ))
            })?;
            cfg.guard_tau = Some(tau);
        }
        if let Some(v) = read_env("KASTELLAN_LLM_GUARD_TIMEOUT_MS")? {
            let ms: u64 = v.parse().map_err(|_| {
                RouterError::Config(format!(
                    "KASTELLAN_LLM_GUARD_TIMEOUT_MS must be a non-negative integer, got {v:?}"
                ))
            })?;
            cfg.guard_timeout_ms = Some(ms);
        }
        if let Some(v) = read_env("KASTELLAN_LLM_TIMEOUT_MS")? {
            let ms: u64 = v.parse().map_err(|_| {
                RouterError::Config(format!(
                    "KASTELLAN_LLM_TIMEOUT_MS must be a non-negative integer, got {v:?}"
                ))
            })?;
            cfg.timeout = Duration::from_millis(ms);
        }
        if let Some(v) = read_env("KASTELLAN_LLM_DISABLE_THINKING")? {
            cfg.disable_thinking = match v.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => true,
                "0" | "false" | "no" | "off" => false,
                _ => {
                    return Err(RouterError::Config(format!(
                        "KASTELLAN_LLM_DISABLE_THINKING must be one of \
                         1/0/true/false/yes/no/on/off, got {v:?}"
                    )))
                }
            };
        }
        if let Some(v) = read_env(THINKING_SWITCH_ENV)? {
            cfg.thinking_switch = ThinkingSwitch::parse(&v)?;
        }
        Ok(cfg)
    }

    /// Derive a config that talks to the **guard** endpoint.
    ///
    /// `Router::dispatch_local` reads `local_url`, so "reach the guard"
    /// is expressed as a config whose `local_url` *is* the guard's.
    /// That keeps the dispatch path and the backend enum untouched.
    ///
    /// Three outcomes, and the middle one is the point:
    ///
    /// - `Ok(None)` — **neither** key set. The operator did not ask for a
    ///   guard. Expected, not an error.
    /// - `Err(..)` — **exactly one** key set. That is a misconfiguration,
    ///   not an opt-out, and it must not be reported as one. A URL
    ///   without a model would otherwise send `local_model` to a server
    ///   that does not serve it, and the resulting 4xx reads as an
    ///   outage; a model without a URL would silently do nothing at all.
    /// - `Ok(Some(cfg))` — both set, guard reachable.
    ///
    /// **Why this is not `Option<RouterConfig>`.** It was, and the
    /// half-configured case collapsed into `None` — indistinguishable
    /// from a deliberate opt-out. On a host where `kastellan-cli install`
    /// regenerates `kastellan.env` and has been observed dropping
    /// hand-added keys, losing `KASTELLAN_LLM_GUARD_MODEL` while keeping
    /// the URL would turn the security tier off behind a
    /// correct-looking "guard unconfigured" log line.
    ///
    /// **`timeout` is a parameter, not an inheritance.** It used to be
    /// inherited through the struct update below, and that inheritance
    /// was documented as "observed, not endorsed" — the planner's
    /// generous 180 s is not a guard budget, and wiring-spec D9 derives
    /// the guard's own bound from a boot-time throughput probe. Taking
    /// it here means every caller has to state which budget it is
    /// spending, and there is exactly one method rather than an
    /// inheriting and a non-inheriting one that could drift.
    ///
    /// Pure — no I/O, no env read.
    pub fn for_guard(&self, timeout: Duration) -> Result<Option<RouterConfig>, RouterError> {
        match (self.guard_url.as_ref(), self.guard_model.as_ref()) {
            (None, None) => Ok(None),
            (Some(_), None) => Err(RouterError::Config(
                "KASTELLAN_LLM_GUARD_URL is set but KASTELLAN_LLM_GUARD_MODEL is not; \
                 the guard tier needs both. This is a misconfiguration, not an opt-out."
                    .to_string(),
            )),
            (None, Some(_)) => Err(RouterError::Config(
                "KASTELLAN_LLM_GUARD_MODEL is set but KASTELLAN_LLM_GUARD_URL is not; \
                 the guard tier needs both. This is a misconfiguration, not an opt-out."
                    .to_string(),
            )),
            (Some(url), Some(model)) => Ok(Some(RouterConfig {
                local_url: url.clone(),
                local_model: model.clone(),
                timeout,
                // Pinned, NOT inherited. `KASTELLAN_LLM_THINKING_SWITCH`
                // describes the PLANNER's backend (Ollama on the DGX); the
                // guard is a llama.cpp server whose classifier prompt was
                // measured byte-identical under this dialect (see
                // `ChatRequest::with_logprobs`). Inheriting `reasoning_effort`
                // would change what the calibrated tau was fitted against,
                // on the strength of a setting about a different server.
                thinking_switch: ThinkingSwitch::ChatTemplateKwargs,
                ..self.clone()
            })),
        }
    }
}

/// Read an env var, treating *unset* and *empty* both as "absent".
///
/// Empty-string is treated as absent so a stray `export
/// KASTELLAN_LLM_FRONTIER_URL=` (common when an operator clears a value
/// without unsetting it) does not poison the config with an unusable
/// empty URL. The fail-loudly path is the typed parse in
/// [`RouterConfig::from_env`] for `KASTELLAN_LLM_TIMEOUT_MS`.
fn read_env(key: &str) -> Result<Option<String>, RouterError> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Ok(Some(v)),
        Ok(_) => Ok(None),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(RouterError::Config(format!("env var {key} is not valid Unicode")))
        }
    }
}

#[cfg(test)]
mod tests;
