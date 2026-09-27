//! Unit tests for [`super`] — `RouterConfig` defaults, env parsing and
//! `for_guard`.
//!
//! Moved verbatim (de-indented one level) from the inline `mod tests`
//! block at the tail of `config.rs`.

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
        ("KASTELLAN_LLM_LOCAL_URL", None),
        ("KASTELLAN_LLM_LOCAL_MODEL", None),
        ("KASTELLAN_LLM_EMBEDDING_URL", None),
        ("KASTELLAN_LLM_EMBEDDING_MODEL", None),
        ("KASTELLAN_LLM_FRONTIER_URL", None),
        ("KASTELLAN_LLM_FRONTIER_MODEL", None),
        ("KASTELLAN_LLM_GUARD_URL", None),
        ("KASTELLAN_LLM_GUARD_MODEL", None),
        ("KASTELLAN_LLM_TIMEOUT_MS", None),
        ("KASTELLAN_LLM_DISABLE_THINKING", None),
        (THINKING_SWITCH_ENV, None),
    ])
}

/// Pin the default URL so a port change is deliberate.
///
/// Deliberately **not** a `cfg!` chain, and the reason is stronger
/// than "the arms would be duplicates": `default_local_url_for_os`
/// now contains no conditional compilation at all, so a run on any
/// single host proves the value for *every* target. A `cfg!` chain
/// here would assert less, not more — on a given host two of its
/// three arms are dead code.
///
/// Should a host's runtime diverge again, the `cfg!` split belongs
/// here *and* in the function together. Note that nothing mechanical
/// enforces that pairing: this crate's tests do not run in CI (see
/// `.github/workflows/linux-check.yml`), so a macOS-only arm added to
/// one side would pass the Linux gate untouched. Pin both platforms'
/// values through `const`s if that day comes — the pattern is
/// `install::plan::both_platform_default_sets_are_pinned_and_paired`.
#[test]
fn default_local_url_is_port_8000_on_every_os() {
    assert_eq!(default_local_url_for_os(), "http://127.0.0.1:8000/v1");
}

#[test]
fn default_constants_are_pinned() {
    // Operators read these via the public re-exports; rotating
    // them silently would surprise a config audit.
    assert_eq!(DEFAULT_LOCAL_MODEL, "local-default");
    assert_eq!(DEFAULT_TIMEOUT_MS, 180_000);
}

#[test]
fn default_config_uses_per_os_url_no_frontier_180s_timeout() {
    let cfg = RouterConfig::default();
    assert_eq!(cfg.local_url, default_local_url_for_os());
    assert_eq!(cfg.local_model, DEFAULT_LOCAL_MODEL);
    assert!(cfg.frontier_url.is_none());
    assert!(cfg.frontier_model.is_none());
    assert_eq!(cfg.timeout, Duration::from_millis(DEFAULT_TIMEOUT_MS));
    // Default-ON: a reasoning model left to think freely overruns the
    // request timeout on a large prompt, and the resulting failure
    // names neither thinking nor the model.
    assert!(cfg.disable_thinking);
}

#[test]
fn disable_thinking_accepts_both_boolean_spellings() {
    let _lock = ENV_LOCK.lock().unwrap();
    // `from_env` reads every router var, so an ambient
    // `KASTELLAN_LLM_TIMEOUT_MS=abc` would fail this test for an
    // unrelated reason. Clear the lot first.
    let _all = clear_all();
    for (raw, want) in [
        ("0", false),
        ("false", false),
        ("FALSE", false),
        ("no", false),
        ("off", false),
        ("1", true),
        ("true", true),
        ("  True  ", true),
        ("yes", true),
        ("on", true),
    ] {
        let _scope = EnvScope::new(&[("KASTELLAN_LLM_DISABLE_THINKING", Some(raw))]);
        let cfg = RouterConfig::from_env().unwrap();
        assert_eq!(cfg.disable_thinking, want, "for input {raw:?}");
    }
}

#[test]
fn from_env_rejects_non_boolean_disable_thinking() {
    let _lock = ENV_LOCK.lock().unwrap();
    // Clear first, so the error we assert on can only have come from
    // this var (see `disable_thinking_accepts_both_boolean_spellings`).
    let _all = clear_all();
    let _scope =
        EnvScope::new(&[("KASTELLAN_LLM_DISABLE_THINKING", Some("maybe"))]);
    let err = RouterConfig::from_env()
        .expect_err("a non-boolean must fail loudly, not silently default");
    assert!(
        err.to_string().contains("KASTELLAN_LLM_DISABLE_THINKING"),
        "error should name the offending var: {err}"
    );
}

/// The one value that is neither accepted nor rejected: empty.
///
/// `read_env` treats an empty var as absent for every router var, so
/// a stray `export KASTELLAN_LLM_DISABLE_THINKING=` does NOT error —
/// it falls through to the default. Pinned because the module docs
/// otherwise read as "rejects anything that is not one of the eight
/// spellings", and because the fall-through direction is the
/// load-bearing part: this var defaults **on**, so an emptied value
/// keeps thinking suppressed rather than silently re-enabling it.
#[test]
fn empty_disable_thinking_falls_through_to_the_default() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _all = clear_all();
    let _scope = EnvScope::new(&[("KASTELLAN_LLM_DISABLE_THINKING", Some(""))]);
    let cfg = RouterConfig::from_env()
        .expect("an empty value is absent, not invalid");
    assert!(
        cfg.disable_thinking,
        "empty must fall through to the default (on), not to off"
    );
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
        ("KASTELLAN_LLM_LOCAL_URL", Some("http://10.0.0.1:9000/v1")),
        ("KASTELLAN_LLM_LOCAL_MODEL", Some("Qwen/Qwen2.5-7B-Instruct")),
        ("KASTELLAN_LLM_FRONTIER_URL", Some("https://api.anthropic.com/v1")),
        ("KASTELLAN_LLM_FRONTIER_MODEL", Some("claude-opus-4-7")),
        ("KASTELLAN_LLM_TIMEOUT_MS", Some("5000")),
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
        ("KASTELLAN_LLM_LOCAL_URL", Some("")),
        ("KASTELLAN_LLM_LOCAL_MODEL", None),
        ("KASTELLAN_LLM_FRONTIER_URL", Some("")),
        ("KASTELLAN_LLM_FRONTIER_MODEL", None),
        ("KASTELLAN_LLM_TIMEOUT_MS", Some("")),
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
        ("KASTELLAN_LLM_LOCAL_URL", None),
        ("KASTELLAN_LLM_LOCAL_MODEL", None),
        ("KASTELLAN_LLM_FRONTIER_URL", None),
        ("KASTELLAN_LLM_FRONTIER_MODEL", None),
        ("KASTELLAN_LLM_TIMEOUT_MS", Some("not-a-number")),
    ]);
    let err = RouterConfig::from_env().unwrap_err();
    match err {
        RouterError::Config(msg) => {
            assert!(msg.contains("KASTELLAN_LLM_TIMEOUT_MS"), "msg={msg}");
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
    // per-OS default for *both* local_url and embedding_url so an
    // oMLX-on-macOS deployment works with one URL set.
    let cfg = RouterConfig::default();
    assert_eq!(cfg.embedding_url, cfg.local_url);
}

#[test]
fn router_config_from_env_reads_embedding_url_when_set() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _scope = EnvScope::new(&[
        ("KASTELLAN_LLM_LOCAL_URL", None),
        ("KASTELLAN_LLM_EMBEDDING_URL", Some("http://127.0.0.1:9999/v1")),
        ("KASTELLAN_LLM_LOCAL_MODEL", None),
        ("KASTELLAN_LLM_EMBEDDING_MODEL", None),
    ]);
    let cfg = RouterConfig::from_env().expect("env parse");
    assert_eq!(cfg.embedding_url, "http://127.0.0.1:9999/v1");
}

#[test]
fn router_config_from_env_reads_embedding_model_when_set() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _scope = EnvScope::new(&[
        ("KASTELLAN_LLM_LOCAL_URL", None),
        ("KASTELLAN_LLM_EMBEDDING_URL", None),
        ("KASTELLAN_LLM_LOCAL_MODEL", None),
        ("KASTELLAN_LLM_EMBEDDING_MODEL", Some("BAAI/bge-m3")),
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
        ("KASTELLAN_LLM_LOCAL_URL", Some("http://local:8080/v1")),
        ("KASTELLAN_LLM_EMBEDDING_URL", Some("http://embed:9999/v1")),
        ("KASTELLAN_LLM_LOCAL_MODEL", None),
        ("KASTELLAN_LLM_EMBEDDING_MODEL", None),
    ]);
    let cfg = RouterConfig::from_env().expect("env parse");
    assert_eq!(cfg.local_url, "http://local:8080/v1");
    assert_eq!(cfg.embedding_url, "http://embed:9999/v1");
}

#[test]
fn router_config_from_env_local_url_drives_embedding_url_when_embedding_unset() {
    // The fallback path: with only LOCAL_URL set, embedding_url
    // resolves to the same value (the load-bearing semantic that
    // makes oMLX-on-macOS work with one env var set).
    let _lock = ENV_LOCK.lock().unwrap();
    let _scope = EnvScope::new(&[
        ("KASTELLAN_LLM_LOCAL_URL", Some("http://local:8080/v1")),
        ("KASTELLAN_LLM_EMBEDDING_URL", None),
        ("KASTELLAN_LLM_LOCAL_MODEL", None),
        ("KASTELLAN_LLM_EMBEDDING_MODEL", None),
    ]);
    let cfg = RouterConfig::from_env().expect("env parse");
    assert_eq!(cfg.local_url, "http://local:8080/v1");
    assert_eq!(cfg.embedding_url, "http://local:8080/v1");
}

/// Both guard tuning keys parse, and **absence is not zero**.
///
/// `guard_tau: None` means the operator supplied no threshold —
/// which wiring-spec D1 makes a misconfiguration when a guard URL
/// *is* set, not a silent 0.0. A `Some(0.0)` would flag every
/// document ever fetched; a defaulted `None` would look configured
/// and be off. Neither may arise from a missing key.
#[test]
fn from_env_parses_guard_tau_and_guard_timeout() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _scope = EnvScope::new(&[
        ("KASTELLAN_LLM_GUARD_TAU", Some("0.79552656")),
        ("KASTELLAN_LLM_GUARD_TIMEOUT_MS", Some("26419")),
    ]);
    let cfg = RouterConfig::from_env().unwrap();
    // The fitted value from measurement 3, round-tripped through
    // f32 — the number an operator actually pastes.
    assert_eq!(cfg.guard_tau, Some(0.795_526_56_f32));
    assert_eq!(cfg.guard_timeout_ms, Some(26_419));
}

#[test]
fn from_env_leaves_guard_tuning_absent_when_unset() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _scope = EnvScope::new(&[
        ("KASTELLAN_LLM_GUARD_TAU", None),
        ("KASTELLAN_LLM_GUARD_TIMEOUT_MS", None),
    ]);
    let cfg = RouterConfig::from_env().unwrap();
    assert_eq!(cfg.guard_tau, None, "no tau key must NOT become a default threshold");
    assert_eq!(cfg.guard_timeout_ms, None, "no timeout key means DERIVE, not 0");
}

/// Text that is not a number is refused loudly rather than
/// silently ignored. Same posture as `KASTELLAN_LLM_TIMEOUT_MS`:
/// a typo'd threshold that fell back to "unconfigured" would turn
/// the security tier off behind a correct-looking log line.
#[test]
fn from_env_rejects_non_numeric_guard_tuning() {
    let _lock = ENV_LOCK.lock().unwrap();
    {
        let _scope = EnvScope::new(&[("KASTELLAN_LLM_GUARD_TAU", Some("nought-point-eight"))]);
        let err = RouterConfig::from_env().expect_err("a non-numeric tau must be refused");
        assert!(
            err.to_string().contains("KASTELLAN_LLM_GUARD_TAU"),
            "the error must name the key: {err}"
        );
    }
    {
        let _scope = EnvScope::new(&[("KASTELLAN_LLM_GUARD_TIMEOUT_MS", Some("15s"))]);
        let err = RouterConfig::from_env().expect_err("a non-numeric timeout must be refused");
        assert!(
            err.to_string().contains("KASTELLAN_LLM_GUARD_TIMEOUT_MS"),
            "the error must name the key: {err}"
        );
    }
}

#[test]
fn for_guard_is_ok_none_when_neither_key_is_set() {
    let cfg = RouterConfig::default();
    assert!(
        matches!(cfg.for_guard(Duration::from_secs(15)), Ok(None)),
        "no guard keys at all is a deliberate opt-out, not an error"
    );
}

/// A half-configured guard is a MISCONFIGURATION and must not
/// collapse into the same answer as "no guard wanted". An installer
/// that drops one of the two keys would otherwise turn the security
/// tier off behind a correct-looking "guard unconfigured" line.
#[test]
fn for_guard_errors_when_exactly_one_key_is_set() {
    // Struct-update form rather than field reassignment, per
    // clippy::field_reassign_with_default.
    let url_only = RouterConfig {
        guard_url: Some("http://127.0.0.1:8080/v1".to_string()),
        ..Default::default()
    };
    let err = url_only
        .for_guard(Duration::from_secs(15))
        .expect_err("url without model must be an error");
    assert!(
        err.to_string().contains("KASTELLAN_LLM_GUARD_MODEL"),
        "the error must name the MISSING key: {err}"
    );

    let model_only = RouterConfig {
        guard_model: Some("shieldstral".to_string()),
        ..Default::default()
    };
    let err = model_only
        .for_guard(Duration::from_secs(15))
        .expect_err("model without url must be an error");
    assert!(
        err.to_string().contains("KASTELLAN_LLM_GUARD_URL"),
        "the error must name the MISSING key: {err}"
    );
}

/// The whole point of the seam: a configured guard must NOT inherit
/// the planner's endpoint. That endpoint serves a different model,
/// which would answer the guard prompt with prose and yield a number
/// that looks exactly like a score and means nothing.
#[test]
fn for_guard_overrides_local_url_and_model_and_never_falls_back() {
    // Deliberately unequal to the default 180 s so the two
    // assertions below can tell "took the parameter" from "kept the
    // parent's value".
    const GUARD_BUDGET: Duration = Duration::from_millis(26_419);
    let planner_url = RouterConfig::default().local_url;
    let cfg = RouterConfig {
        guard_url: Some("http://127.0.0.1:8080/v1".to_string()),
        guard_model: Some("shieldstral-1.0-3b-q8".to_string()),
        ..Default::default()
    };

    let guard = cfg
        .for_guard(GUARD_BUDGET)
        .expect("no misconfiguration")
        .expect("configured");
    assert_eq!(guard.local_url, "http://127.0.0.1:8080/v1");
    assert_eq!(guard.local_model, "shieldstral-1.0-3b-q8");
    assert_ne!(guard.local_url, planner_url, "must not be the planner endpoint");
    // The inheritance is GONE, and this assertion is what closes
    // slice-1's open risk 6 / issue #586. It used to read
    // `guard.timeout == cfg.timeout` and was documented as
    // "observed, not endorsed": 180 s is the planner's budget, and
    // on the dispatcher hot path an endpoint that is up-but-hung
    // would stall three minutes per document. The guard now spends
    // a budget its caller states — derived at boot from a
    // throughput probe (wiring-spec D9), or set by the operator.
    //
    // `assert_ne!` against the parent is the half that matters: an
    // implementation that took the parameter and then let the
    // struct update overwrite it would satisfy the `assert_eq!`
    // alone only by coincidence, so both are asserted.
    assert_eq!(guard.timeout, GUARD_BUDGET, "the passed budget must win");
    assert_ne!(
        guard.timeout, cfg.timeout,
        "the planner's timeout must NOT be inherited (issue #586)"
    );
    assert_eq!(
        guard.disable_thinking, cfg.disable_thinking,
        "thinking suppression is inherited: measured byte-identical on Shieldstral"
    );
}

#[test]
fn from_env_reads_guard_url_and_model() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _scope = EnvScope::new(&[
        ("KASTELLAN_LLM_GUARD_URL", Some("http://127.0.0.1:8080/v1")),
        ("KASTELLAN_LLM_GUARD_MODEL", Some("shieldstral-1.0-3b-q8")),
    ]);
    let cfg = RouterConfig::from_env().expect("valid");
    assert_eq!(cfg.guard_url.as_deref(), Some("http://127.0.0.1:8080/v1"));
    assert_eq!(cfg.guard_model.as_deref(), Some("shieldstral-1.0-3b-q8"));
}

/// Pins that an absent guard stays absent — i.e. nothing in
/// `from_env` invents a default guard endpoint out of `local_url`.
///
/// **Deliberately narrow, and named for what it pins.** It cannot
/// fail on the two `read_env` lines themselves: `RouterConfig::default()`
/// already sets both to `None`, so deleting those lines leaves this
/// green. `from_env_reads_guard_url_and_model` is what covers the
/// reads.
#[test]
fn from_env_does_not_invent_a_guard_when_the_vars_are_absent() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _scope = EnvScope::new(&[
        ("KASTELLAN_LLM_GUARD_URL", None),
        ("KASTELLAN_LLM_GUARD_MODEL", None),
    ]);
    let cfg = RouterConfig::from_env().expect("valid");
    assert!(cfg.guard_url.is_none());
    assert!(cfg.guard_model.is_none());
}

// ── KASTELLAN_LLM_THINKING_SWITCH (#773) ─────────────────────────────

#[test]
fn thinking_switch_defaults_to_the_kwargs_dialect() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _scope = clear_all();
    let cfg = RouterConfig::from_env().expect("valid");
    assert_eq!(cfg.thinking_switch, ThinkingSwitch::ChatTemplateKwargs);
    assert_eq!(RouterConfig::default().thinking_switch, ThinkingSwitch::ChatTemplateKwargs);
}

#[test]
fn thinking_switch_reads_reasoning_effort_and_rejects_a_typo() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _clear = clear_all();
    {
        let _scope = EnvScope::new(&[(THINKING_SWITCH_ENV, Some("reasoning_effort"))]);
        let cfg = RouterConfig::from_env().expect("valid");
        assert_eq!(cfg.thinking_switch, ThinkingSwitch::ReasoningEffort);
    }
    {
        let _scope = EnvScope::new(&[(THINKING_SWITCH_ENV, Some("ollama"))]);
        let err = RouterConfig::from_env().expect_err("a typo must not pick a dialect");
        assert!(err.to_string().contains(THINKING_SWITCH_ENV), "{err}");
    }
    {
        // Empty = absent, like every var in the table.
        let _scope = EnvScope::new(&[(THINKING_SWITCH_ENV, Some(""))]);
        let cfg = RouterConfig::from_env().expect("valid");
        assert_eq!(cfg.thinking_switch, ThinkingSwitch::ChatTemplateKwargs);
    }
}

/// The planner's dialect setting must not reach the guard: it names the
/// planner's backend, and the guard's classifier prompt was measured
/// under the kwargs dialect only.
#[test]
fn for_guard_pins_the_kwargs_dialect_instead_of_inheriting_it() {
    let cfg = RouterConfig {
        guard_url: Some("http://127.0.0.1:8081/v1".to_string()),
        guard_model: Some("shieldstral".to_string()),
        thinking_switch: ThinkingSwitch::ReasoningEffort,
        ..Default::default()
    };
    let guard = cfg
        .for_guard(Duration::from_secs(15))
        .expect("no misconfiguration")
        .expect("configured");
    assert_eq!(guard.thinking_switch, ThinkingSwitch::ChatTemplateKwargs);
    assert_eq!(cfg.thinking_switch, ThinkingSwitch::ReasoningEffort, "parent untouched");
}
