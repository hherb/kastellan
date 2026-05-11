//! [`RouterConfig`] — the static description of which backends the
//! router can reach and what to call by default.
//!
//! The config is populated from environment variables (test-friendly
//! seam, same shape as `HHAGENT_DATA_DIR` / `HHAGENT_STATE_DIR` in
//! `core`) with a per-OS default for the local backend so a fresh
//! checkout works without any setup on a machine that has the
//! expected runtime installed:
//!
//! * **Linux:** `http://127.0.0.1:8000/v1` — the default vLLM /
//!   SGLang OpenAI-compat port.
//! * **macOS:** `http://127.0.0.1:11434/v1` — the default Ollama
//!   port. Ollama is the most common local-LLM runtime on macOS;
//!   llama.cpp's `--api` server lives on a user-chosen port and
//!   so doesn't have a sane default.
//!
//! ## Environment variables
//!
//! | Var | Purpose | Default |
//! | --- | --- | --- |
//! | `HHAGENT_LLM_LOCAL_URL` | Base URL of the local backend (no trailing `/`) | per-OS, see above |
//! | `HHAGENT_LLM_LOCAL_MODEL` | Default model name passed to the local backend | `local-default` |
//! | `HHAGENT_LLM_EMBEDDING_URL` | Base URL of the embedding backend | falls back to local URL |
//! | `HHAGENT_LLM_EMBEDDING_MODEL` | Default model name passed to the embedding backend | `embedding-default` |
//! | `HHAGENT_LLM_FRONTIER_URL` | Base URL of the frontier backend | unset (frontier disabled) |
//! | `HHAGENT_LLM_FRONTIER_MODEL` | Default model on the frontier backend | unset |
//! | `HHAGENT_LLM_TIMEOUT_MS` | Request timeout, milliseconds | 30_000 |
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

pub const DEFAULT_LOCAL_MODEL: &str = "local-default";
pub const DEFAULT_EMBEDDING_MODEL: &str = "embedding-default";
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Per-OS default base URL for the local backend.
///
/// Pure function (no env reads, no I/O). Returned as `&'static str`
/// so it composes into [`RouterConfig::default`] without an
/// allocation. Linux gets the vLLM/SGLang port; macOS gets Ollama.
pub fn default_local_url_for_os() -> &'static str {
    if cfg!(target_os = "linux") {
        "http://127.0.0.1:8000/v1"
    } else if cfg!(target_os = "macos") {
        "http://127.0.0.1:11434/v1"
    } else {
        // Other Unixes: pick the vLLM/SGLang port. Better to point at
        // *something* than to require an env var; the smoke test on
        // an unsupported host will fail fast with a connection-refused
        // error, which is the right signal.
        "http://127.0.0.1:8000/v1"
    }
}

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
    /// Set if and only if the operator has expressed intent to use a
    /// frontier backend. Phase 0 still refuses to dispatch even when
    /// set — the policy gate lands in Phase 5.
    pub frontier_url: Option<String>,
    pub frontier_model: Option<String>,
    pub timeout: Duration,
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
            timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
        }
    }
}

impl RouterConfig {
    /// Read the config from environment variables, falling back to
    /// [`RouterConfig::default`] on any unset key.
    ///
    /// Returns [`RouterError::Config`] only for **invalid** values
    /// (e.g. a non-numeric `HHAGENT_LLM_TIMEOUT_MS`); an *unset* var
    /// is always fine and just means "use the default".
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
}

/// Read an env var, treating *unset* and *empty* both as "absent".
///
/// Empty-string is treated as absent so a stray `export
/// HHAGENT_LLM_FRONTIER_URL=` (common when an operator clears a value
/// without unsetting it) does not poison the config with an unusable
/// empty URL. The fail-loudly path is the typed parse in
/// [`RouterConfig::from_env`] for `HHAGENT_LLM_TIMEOUT_MS`.
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
mod tests {
    use super::*;

    /// Mutex serialising every env-touching test in this module.
    /// `cargo test` runs unit tests on multiple threads inside the
    /// same process, and `std::env::set_var` is process-global. The
    /// secret-rest tests in `db/src/secrets.rs` and the audit-tail
    /// tests in `core/src/audit_tail.rs` do not touch env so this is
    /// a llm-router-local concern; the secrets module solved the
    /// same problem the same way.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard that sets a list of env vars on construction and
    /// restores their prior values on Drop, even if the test panics.
    /// Mutating process-global state mid-test is unavoidable here
    /// because [`RouterConfig::from_env`] reads `std::env`; using
    /// `temp-env` would add a dev-dep for a five-line helper.
    struct EnvScope {
        prior: Vec<(String, Option<String>)>,
    }

    impl EnvScope {
        fn new(pairs: &[(&str, Option<&str>)]) -> Self {
            let mut prior = Vec::with_capacity(pairs.len());
            for (k, v) in pairs {
                prior.push((k.to_string(), std::env::var(k).ok()));
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
            Self { prior }
        }
    }

    impl Drop for EnvScope {
        fn drop(&mut self) {
            for (k, v) in &self.prior {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    fn clear_all() -> EnvScope {
        EnvScope::new(&[
            ("HHAGENT_LLM_LOCAL_URL", None),
            ("HHAGENT_LLM_LOCAL_MODEL", None),
            ("HHAGENT_LLM_EMBEDDING_URL", None),
            ("HHAGENT_LLM_EMBEDDING_MODEL", None),
            ("HHAGENT_LLM_FRONTIER_URL", None),
            ("HHAGENT_LLM_FRONTIER_MODEL", None),
            ("HHAGENT_LLM_TIMEOUT_MS", None),
        ])
    }

    #[test]
    fn default_local_url_resolves_per_os() {
        // Pure function — no env. Pin the per-OS strings so a
        // refactor that swaps ports does so deliberately.
        let url = default_local_url_for_os();
        if cfg!(target_os = "linux") {
            assert_eq!(url, "http://127.0.0.1:8000/v1");
        } else if cfg!(target_os = "macos") {
            assert_eq!(url, "http://127.0.0.1:11434/v1");
        } else {
            assert_eq!(url, "http://127.0.0.1:8000/v1");
        }
    }

    #[test]
    fn default_constants_are_pinned() {
        // Operators read these via the public re-exports; rotating
        // them silently would surprise a config audit.
        assert_eq!(DEFAULT_LOCAL_MODEL, "local-default");
        assert_eq!(DEFAULT_TIMEOUT_MS, 30_000);
    }

    #[test]
    fn default_config_uses_per_os_url_no_frontier_30s_timeout() {
        let cfg = RouterConfig::default();
        assert_eq!(cfg.local_url, default_local_url_for_os());
        assert_eq!(cfg.local_model, DEFAULT_LOCAL_MODEL);
        assert!(cfg.frontier_url.is_none());
        assert!(cfg.frontier_model.is_none());
        assert_eq!(cfg.timeout, Duration::from_millis(DEFAULT_TIMEOUT_MS));
    }

    #[test]
    fn from_env_with_no_vars_set_equals_default() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _scope = clear_all();
        let cfg = RouterConfig::from_env().unwrap();
        assert_eq!(cfg, RouterConfig::default());
    }

    #[test]
    fn from_env_overrides_each_field() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _scope = EnvScope::new(&[
            ("HHAGENT_LLM_LOCAL_URL", Some("http://10.0.0.1:9000/v1")),
            ("HHAGENT_LLM_LOCAL_MODEL", Some("Qwen/Qwen2.5-7B-Instruct")),
            ("HHAGENT_LLM_FRONTIER_URL", Some("https://api.anthropic.com/v1")),
            ("HHAGENT_LLM_FRONTIER_MODEL", Some("claude-opus-4-7")),
            ("HHAGENT_LLM_TIMEOUT_MS", Some("5000")),
        ]);
        let cfg = RouterConfig::from_env().unwrap();
        assert_eq!(cfg.local_url, "http://10.0.0.1:9000/v1");
        assert_eq!(cfg.local_model, "Qwen/Qwen2.5-7B-Instruct");
        assert_eq!(cfg.frontier_url.as_deref(), Some("https://api.anthropic.com/v1"));
        assert_eq!(cfg.frontier_model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(cfg.timeout, Duration::from_millis(5_000));
    }

    #[test]
    fn from_env_treats_empty_string_as_absent() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _scope = EnvScope::new(&[
            ("HHAGENT_LLM_LOCAL_URL", Some("")),
            ("HHAGENT_LLM_LOCAL_MODEL", None),
            ("HHAGENT_LLM_FRONTIER_URL", Some("")),
            ("HHAGENT_LLM_FRONTIER_MODEL", None),
            ("HHAGENT_LLM_TIMEOUT_MS", Some("")),
        ]);
        let cfg = RouterConfig::from_env().unwrap();
        // Empty fell back to the per-OS default rather than producing an
        // unusable empty URL.
        assert_eq!(cfg.local_url, default_local_url_for_os());
        assert!(cfg.frontier_url.is_none());
        assert_eq!(cfg.timeout, Duration::from_millis(DEFAULT_TIMEOUT_MS));
    }

    #[test]
    fn from_env_rejects_non_numeric_timeout() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _scope = EnvScope::new(&[
            ("HHAGENT_LLM_LOCAL_URL", None),
            ("HHAGENT_LLM_LOCAL_MODEL", None),
            ("HHAGENT_LLM_FRONTIER_URL", None),
            ("HHAGENT_LLM_FRONTIER_MODEL", None),
            ("HHAGENT_LLM_TIMEOUT_MS", Some("not-a-number")),
        ]);
        let err = RouterConfig::from_env().unwrap_err();
        match err {
            RouterError::Config(msg) => {
                assert!(msg.contains("HHAGENT_LLM_TIMEOUT_MS"), "msg={msg}");
                assert!(msg.contains("not-a-number"), "msg={msg}");
            }
            other => panic!("expected RouterError::Config, got {other:?}"),
        }
    }

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
        let _lock = ENV_LOCK.lock().unwrap();
        let _scope = EnvScope::new(&[
            ("HHAGENT_LLM_LOCAL_URL", None),
            ("HHAGENT_LLM_EMBEDDING_URL", Some("http://127.0.0.1:9999/v1")),
            ("HHAGENT_LLM_LOCAL_MODEL", None),
            ("HHAGENT_LLM_EMBEDDING_MODEL", None),
        ]);
        let cfg = RouterConfig::from_env().expect("env parse");
        assert_eq!(cfg.embedding_url, "http://127.0.0.1:9999/v1");
    }

    #[test]
    fn router_config_from_env_reads_embedding_model_when_set() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _scope = EnvScope::new(&[
            ("HHAGENT_LLM_LOCAL_URL", None),
            ("HHAGENT_LLM_EMBEDDING_URL", None),
            ("HHAGENT_LLM_LOCAL_MODEL", None),
            ("HHAGENT_LLM_EMBEDDING_MODEL", Some("BAAI/bge-m3")),
        ]);
        let cfg = RouterConfig::from_env().expect("env parse");
        assert_eq!(cfg.embedding_model, "BAAI/bge-m3");
    }

    #[test]
    fn router_config_from_env_embedding_url_overrides_local_url() {
        // Pin the load-bearing override contract: when both env vars are
        // set, EMBEDDING_URL wins for embedding_url; local_url is
        // unaffected. A refactor that swaps the two from_env blocks
        // would break this contract silently otherwise.
        let _lock = ENV_LOCK.lock().unwrap();
        let _scope = EnvScope::new(&[
            ("HHAGENT_LLM_LOCAL_URL", Some("http://local:8080/v1")),
            ("HHAGENT_LLM_EMBEDDING_URL", Some("http://embed:9999/v1")),
            ("HHAGENT_LLM_LOCAL_MODEL", None),
            ("HHAGENT_LLM_EMBEDDING_MODEL", None),
        ]);
        let cfg = RouterConfig::from_env().expect("env parse");
        assert_eq!(cfg.local_url, "http://local:8080/v1");
        assert_eq!(cfg.embedding_url, "http://embed:9999/v1");
    }

    #[test]
    fn router_config_from_env_local_url_drives_embedding_url_when_embedding_unset() {
        // The fallback path: with only LOCAL_URL set, embedding_url
        // resolves to the same value (the load-bearing semantic that
        // makes Ollama-on-macOS work with one env var set).
        let _lock = ENV_LOCK.lock().unwrap();
        let _scope = EnvScope::new(&[
            ("HHAGENT_LLM_LOCAL_URL", Some("http://local:8080/v1")),
            ("HHAGENT_LLM_EMBEDDING_URL", None),
            ("HHAGENT_LLM_LOCAL_MODEL", None),
            ("HHAGENT_LLM_EMBEDDING_MODEL", None),
        ]);
        let cfg = RouterConfig::from_env().expect("env parse");
        assert_eq!(cfg.local_url, "http://local:8080/v1");
        assert_eq!(cfg.embedding_url, "http://local:8080/v1");
    }
}
