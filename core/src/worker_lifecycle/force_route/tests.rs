//! Unit tests for egress force-routing config + the pure decision helpers
//! (`ForceRoutingConfig`, `force_route_action`, `resolve_force_routing`,
//! `from_env`, `spawn_worker_maybe_forced`, cert-pin selection, and the
//! env-flag / scratch-root / proxy-bin-discovery plumbing).
//!
//! Lifted verbatim from the parent module's inline `#[cfg(test)] mod tests`
//! (item 9b over-cap test-lift). Production logic lives in the parent
//! `force_route.rs`; this file is `mod tests;` from there and is only compiled
//! under `#[cfg(test)]`.

use super::*;
use crate::tool_host::{ToolHostError, WorkerSpec};
use kastellan_sandbox::{SandboxBackend, SandboxError, SandboxPolicy};
use std::sync::{Arc, Mutex};

/// A no-op sink factory — proves the routing decision without a live pool.
fn noop_sink_factory() -> DecisionSinkFactory {
    Box::new(|| Box::new(|_row| {}))
}

/// A backend that records the label of each spawn attempt and always fails
/// (so no real child process is created). Two instances with distinct labels
/// let a test assert *which* backend a given spawn hit. Shared by the
/// egress-sidecar (#448 Task 1) and broker (#448 Task 2) seam tests.
struct RecordingBackend {
    label: &'static str,
    calls: Arc<Mutex<Vec<&'static str>>>,
}
impl SandboxBackend for RecordingBackend {
    fn spawn_under_policy(
        &self,
        _policy: &SandboxPolicy,
        _program: &str,
        _args: &[&str],
    ) -> Result<std::process::Child, SandboxError> {
        self.calls.lock().expect("recording mutex poisoned").push(self.label);
        Err(SandboxError::Backend(self.label.into()))
    }
}

/// Backend whose spawn always fails. The point of these tests is *which*
/// spawn path runs, told apart by the error variant: the plain
/// `spawn_worker` path surfaces `ToolHostError::Sandbox`, while the
/// force-routed path maps its sidecar-spawn failure to `ToolHostError::Io`
/// (see `egress::net_worker::spawn_net_worker`).
struct FailBackend;
impl SandboxBackend for FailBackend {
    fn spawn_under_policy(
        &self,
        _policy: &SandboxPolicy,
        _program: &str,
        _args: &[&str],
    ) -> Result<std::process::Child, SandboxError> {
        Err(SandboxError::Backend("test: spawn refused".into()))
    }
}

fn config_with(scratch_root: PathBuf) -> ForceRoutingConfig {
    ForceRoutingConfig::new(
        PathBuf::from("/nonexistent/egress-proxy"),
        scratch_root,
        noop_sink_factory(),
        None,
    )
}

fn spec_for<'a>(policy: &'a SandboxPolicy) -> WorkerSpec<'a> {
    WorkerSpec {
        policy,
        program: "/bin/worker",
        args: &[],
        wall_clock_ms: None,
    }
}

#[test]
fn none_config_uses_plain_spawn_worker_even_for_allowlist() {
    let policy = SandboxPolicy {
        net: Net::Allowlist(vec!["api.example.com:443".into()]),
        ..SandboxPolicy::default()
    };
    let res = spawn_worker_maybe_forced(None, &FailBackend, &FailBackend, &spec_for(&policy), "web-fetch");
    // Plain spawn_worker surfaces the backend error as Sandbox.
    assert!(
        matches!(res, Err(ToolHostError::Sandbox(_))),
        "None config must take the legacy spawn_worker path (Sandbox error)"
    );
}

#[test]
fn some_config_allowlist_routes_through_forced_spawn() {
    let policy = SandboxPolicy {
        net: Net::Allowlist(vec!["api.example.com:443".into()]),
        ..SandboxPolicy::default()
    };
    let scratch = tempfile::tempdir().expect("scratch root");
    let cfg = config_with(scratch.path().to_path_buf());
    let res =
        spawn_worker_maybe_forced(Some(&cfg), &FailBackend, &FailBackend, &spec_for(&policy), "web-fetch");
    // The forced path maps the sidecar-spawn failure to Io (fail-closed).
    assert!(
        matches!(res, Err(ToolHostError::Io(_))),
        "Some config + Allowlist must force-route (Io fail-closed error)"
    );
}

/// #448: on the Sidecar path the egress-proxy sidecar spawns on
/// `sidecar_backend` (the host default), NOT the worker `backend` (which may be
/// a VM). Proven with two recording backends: the sidecar spawn is attempted
/// first and fails, so only the host recorder is hit — the worker backend is
/// never reached.
#[test]
fn forced_egress_sidecar_spawns_on_sidecar_backend_not_worker_backend() {
    let policy = SandboxPolicy {
        net: Net::Allowlist(vec!["api.example.com:443".into()]),
        ..SandboxPolicy::default()
    };
    let scratch = tempfile::tempdir().expect("scratch root");
    let cfg = config_with(scratch.path().to_path_buf());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let worker_backend = RecordingBackend { label: "vm-worker", calls: Arc::clone(&calls) };
    let sidecar_backend = RecordingBackend { label: "host-sidecar", calls: Arc::clone(&calls) };

    let res = spawn_worker_maybe_forced(
        Some(&cfg),
        &worker_backend,
        &sidecar_backend,
        &spec_for(&policy),
        "web-fetch",
    );

    // Force-route path fails at the (recording) sidecar spawn → Io.
    assert!(matches!(res, Err(ToolHostError::Io(_))), "forced path maps sidecar failure to Io");
    let hit = calls.lock().unwrap().clone();
    assert_eq!(
        hit,
        vec!["host-sidecar"],
        "the egress sidecar must spawn on sidecar_backend (host); the worker backend must not be reached"
    );
}

#[test]
fn some_config_deny_net_uses_plain_spawn_worker() {
    let policy = SandboxPolicy {
        net: Net::Deny,
        ..SandboxPolicy::default()
    };
    let scratch = tempfile::tempdir().expect("scratch root");
    let cfg = config_with(scratch.path().to_path_buf());
    let res = spawn_worker_maybe_forced(Some(&cfg), &FailBackend, &FailBackend, &spec_for(&policy), "shell");
    // Net::Deny is not force-routable → legacy spawn_worker (Sandbox error).
    assert!(
        matches!(res, Err(ToolHostError::Sandbox(_))),
        "Net::Deny must take the legacy path even with config (Sandbox error)"
    );
    // And it must not have created a scratch subdir (no force-route happened).
    let entries: Vec<_> = std::fs::read_dir(scratch.path())
        .expect("read scratch root")
        .collect();
    assert!(entries.is_empty(), "Net::Deny path must not touch scratch_root");
}

// ---- force_route_action: the pure routing-decision truth table ----

#[test]
fn action_force_off_is_always_direct() {
    // worker_name no longer affects the decision (the browser exemption was
    // removed in slice #2), so this only varies over `net_force_routable`.
    for routable in [true, false] {
        assert_eq!(force_route_action(false, routable), ForceRouteAction::Direct);
    }
}

#[test]
fn action_force_on_routable_is_sidecar() {
    assert_eq!(force_route_action(true, true), ForceRouteAction::Sidecar);
}

#[test]
fn action_force_on_not_routable_is_direct() {
    assert_eq!(force_route_action(true, false), ForceRouteAction::Direct);
}

/// Slice #2: browser-driver is now a normal force-routable worker — under
/// force-routing it takes the Sidecar path (no refusal, no exemption).
#[test]
fn browser_driver_force_routed_takes_sidecar_path() {
    let policy = SandboxPolicy {
        net: Net::Allowlist(vec!["example.com:443".into()]),
        ..SandboxPolicy::default()
    };
    let scratch = tempfile::tempdir().expect("scratch root");
    let cfg = config_with(scratch.path().to_path_buf());
    let res = spawn_worker_maybe_forced(
        Some(&cfg), &FailBackend, &FailBackend, &spec_for(&policy), BROWSER_DRIVER_TOOL);
    // Sidecar path maps the (failing) sidecar spawn to Io — proving it tried
    // to force-route the browser rather than refuse or run direct.
    assert!(matches!(res, Err(ToolHostError::Io(_))),
        "browser-driver under force-routing must force-route (Io fail-closed)");
}

#[test]
fn disable_mitm_only_for_transparent_tunnel_workers() {
    // The browser + matrix do their own end-to-end TLS → transparent tunnel.
    assert!(disable_mitm_for(BROWSER_DRIVER_TOOL));
    assert!(disable_mitm_for(MATRIX_TOOL));
    // Every other worker is MITM-intercepted by its sidecar.
    assert!(!disable_mitm_for("web-fetch"));
    assert!(!disable_mitm_for("web-search"));
}

#[test]
fn allowlist_net_is_force_routable() {
    assert!(policy_net_is_force_routable(&Net::Allowlist(vec![
        "api.example.com:443".into()
    ])));
}

#[test]
fn deny_and_proxy_egress_nets_are_not_force_routable() {
    assert!(!policy_net_is_force_routable(&Net::Deny));
    assert!(!policy_net_is_force_routable(&Net::ProxyEgress));
}

#[test]
fn disabled_resolves_to_none_even_with_a_binary() {
    let out = resolve_force_routing(
        false,
        Some(PathBuf::from("/opt/egress-proxy")),
        PathBuf::from("/tmp"),
        noop_sink_factory(),
        None,
    )
    .expect("disabled never errors");
    assert!(out.is_none(), "disabled => None (legacy path)");
}

#[test]
fn enabled_with_binary_resolves_to_some() {
    let out = resolve_force_routing(
        true,
        Some(PathBuf::from("/opt/egress-proxy")),
        PathBuf::from("/tmp"),
        noop_sink_factory(),
        None,
    )
    .expect("enabled + binary => Ok(Some)");
    let cfg = out.expect("Some");
    assert_eq!(cfg.proxy_bin, PathBuf::from("/opt/egress-proxy"));
    assert_eq!(cfg.scratch_root, PathBuf::from("/tmp"));
}

#[test]
fn enabled_without_binary_fails_closed() {
    let out =
        resolve_force_routing(true, None, PathBuf::from("/tmp"), noop_sink_factory(), None);
    assert!(
        out.is_err(),
        "enabled but no proxy binary must fail closed, not fall back to unrouted egress"
    );
}

#[test]
fn env_flag_truthy_and_falsy_values() {
    // Enabled only for explicit truthy spellings; everything else (incl.
    // unset) is off so force-routing is never enabled by accident.
    for v in ["1", "true", "TRUE", "yes", "on"] {
        assert!(env_flag_enabled(Some(v.to_string())), "{v:?} should enable");
    }
    for v in ["0", "false", "no", "off", ""] {
        assert!(!env_flag_enabled(Some(v.to_string())), "{v:?} should disable");
    }
    assert!(!env_flag_enabled(None), "unset should disable");
}

/// macOS: the default scratch root must be short enough that the nested
/// `egress-<pid>-<seq>/egress.sock` still fits the 104-byte
/// `sockaddr_un.sun_path`. macOS's `$TMPDIR` is too deep, so we default to
/// `/tmp`; pin that so a regression back to `temp_dir()` (which would make
/// every force-routed spawn fail-closed on macOS) trips here.
#[cfg(target_os = "macos")]
#[test]
fn default_scratch_root_is_short_on_macos() {
    let root = default_egress_scratch_root();
    assert_eq!(root, PathBuf::from("/tmp"));
    // Worst-case projected UDS (max pid + max seq), plus the NUL
    // terminator, must fit the 104-byte `sun_path` — i.e. the path itself
    // must be < 104 bytes (`len + 1 <= 104`).
    let projected = root
        .join("egress-4294967295-18446744073709551615")
        .join("egress.sock");
    assert!(
        projected.as_os_str().len() < 104,
        "default macOS scratch root too deep for sockaddr_un.sun_path: {}",
        projected.display()
    );
}

#[test]
fn proxy_bin_override_pointing_at_a_runnable_file_wins() {
    // KASTELLAN_EGRESS_PROXY_BIN set to a runnable file is authoritative,
    // even when a sibling default also exists.
    let get_env =
        |k: &str| (k == ENV_PROXY_BIN).then(|| "/opt/custom/egress-proxy".to_string());
    let exists = |_p: &Path| true;
    let is_dir = |_p: &Path| false;
    let exe = PathBuf::from("/usr/lib/kastellan");
    let out = discover_egress_proxy_bin_with(&get_env, &exists, &is_dir, Some(exe.as_path()));
    assert_eq!(out, Some(PathBuf::from("/opt/custom/egress-proxy")));
}

#[test]
fn proxy_bin_override_set_but_invalid_fails_closed_without_sibling_fallback() {
    // The override is set but names a non-existent path. A set-but-invalid
    // override is fail-closed: we must NOT silently substitute the
    // exe-relative sibling (which would route through a *different* binary
    // than the operator named) — `from_env` then maps the `None` to
    // ProxyBinaryNotFound and the daemon refuses to start.
    let get_env =
        |k: &str| (k == ENV_PROXY_BIN).then(|| "/opt/typo/egress-proxy".to_string());
    // The override path does NOT exist; the sibling default WOULD.
    let exists = |p: &Path| p != Path::new("/opt/typo/egress-proxy");
    let is_dir = |_p: &Path| false;
    let exe = PathBuf::from("/usr/lib/kastellan");
    let out = discover_egress_proxy_bin_with(&get_env, &exists, &is_dir, Some(exe.as_path()));
    assert_eq!(
        out, None,
        "set-but-invalid override must fail closed, not fall through to the sibling"
    );
}

#[test]
fn proxy_bin_falls_back_to_exe_sibling_when_override_unset() {
    // No override → the exe-relative `kastellan-worker-egress-proxy` sibling
    // is used iff it is a runnable file.
    let get_env = |_k: &str| None;
    let exists = |_p: &Path| true;
    let is_dir = |_p: &Path| false;
    let exe = PathBuf::from("/usr/lib/kastellan");
    let out = discover_egress_proxy_bin_with(&get_env, &exists, &is_dir, Some(exe.as_path()));
    assert_eq!(
        out,
        Some(exe.join(PROXY_BIN_DEFAULT)),
        "unset override must use the exe-relative sibling default"
    );
}

#[test]
fn pins_for_selects_allowlisted_subset() {
    let pins = parse_cert_pins_env(Some(r#"{"a.com":["sha256/A"]}"#)).unwrap();
    let cfg = ForceRoutingConfig::new(
        PathBuf::from("/nonexistent/egress-proxy"),
        PathBuf::from("/tmp"),
        noop_sink_factory(),
        pins,
    );
    let json = cfg.pins_for(&["a.com:443".to_string()]).expect("pinned host in allowlist");
    assert!(json.contains("a.com"));
    assert!(json.contains("sha256/A"));
}

#[test]
fn pins_for_none_when_unconfigured() {
    let cfg = config_with(PathBuf::from("/tmp"));
    assert!(cfg.pins_for(&["a.com:443".to_string()]).is_none());
}

#[test]
fn pins_for_none_when_no_allowlist_match() {
    let pins = parse_cert_pins_env(Some(r#"{"a.com":["sha256/A"]}"#)).unwrap();
    let cfg = ForceRoutingConfig::new(
        PathBuf::from("/nonexistent/egress-proxy"),
        PathBuf::from("/tmp"),
        noop_sink_factory(),
        pins,
    );
    assert!(cfg.pins_for(&["z.com:443".to_string()]).is_none());
}

#[test]
fn parse_cert_pins_env_handles_absent_blank_and_empty() {
    assert!(parse_cert_pins_env(None).unwrap().is_none());
    assert!(parse_cert_pins_env(Some("")).unwrap().is_none());
    assert!(parse_cert_pins_env(Some("   ")).unwrap().is_none());
    // `{}` is valid but empty → normalized to None (no pins).
    assert!(parse_cert_pins_env(Some("{}")).unwrap().is_none());
}

#[test]
fn parse_cert_pins_env_parses_valid_map() {
    let got = parse_cert_pins_env(Some(r#"{"a.com":["sha256/A"]}"#))
        .unwrap()
        .expect("non-empty map => Some");
    assert!(!got.is_empty());
}

#[test]
fn parse_cert_pins_env_fails_closed_on_malformed() {
    let err = parse_cert_pins_env(Some(r#"{"a.com":[]}"#)).unwrap_err();
    assert!(matches!(err, crate::egress::cert_pins::CertPinError::EmptyPinList(_)));
}

#[test]
fn resolve_force_routing_stores_cert_pins() {
    let pins = parse_cert_pins_env(Some(r#"{"a.com":["sha256/A"]}"#)).unwrap();
    let cfg = resolve_force_routing(
        true,
        Some(PathBuf::from("/opt/egress-proxy")),
        PathBuf::from("/tmp"),
        noop_sink_factory(),
        pins.clone(),
    )
    .expect("ok")
    .expect("some");
    assert_eq!(cfg.cert_pins, pins);
}

// ----- Embed-broker (Slice B, Task 4) -----

/// `rewrite_policy_for_broker` binds the broker UDS into the jail
/// (`broker_uds`) and injects the worker-read env so `choose_embedder`
/// selects the brokered path. The jail path equals the host path (B1).
#[test]
fn rewrite_policy_for_broker_sets_uds_and_injects_env() {
    let policy = SandboxPolicy {
        net: Net::Allowlist(vec!["searx.example.org:443".into()]),
        ..SandboxPolicy::default()
    };
    let uds = PathBuf::from("/tmp/embed-1-0/embed.sock");
    let brokered = rewrite_policy_for_broker(policy, &uds, BrokerKind::Embed);
    assert_eq!(brokered.broker_uds.as_deref(), Some(uds.as_path()));
    assert!(
        brokered
            .env
            .iter()
            .any(|(k, v)| k == BrokerKind::Embed.uds_env() && v == "/tmp/embed-1-0/embed.sock"),
        "worker must get the broker UDS env: {:?}",
        brokered.env
    );
}

/// The critical composition pin: force-routing's `rewrite_worker_policy` clones
/// the (already brokered) policy and mutates only the egress fields, so the
/// broker UDS + injected env **survive** for a worker that is BOTH broker-backed
/// AND force-routed. (Slice B1 made the two orthogonal; this pins it end-to-end.)
#[test]
fn broker_uds_and_env_survive_force_route_policy_rewrite() {
    let base = SandboxPolicy {
        net: Net::Allowlist(vec!["searx.example.org:443".into()]),
        ..SandboxPolicy::default()
    };
    let broker_uds = PathBuf::from("/tmp/embed-1-0/embed.sock");
    let brokered = rewrite_policy_for_broker(base, &broker_uds, BrokerKind::Embed);
    // Now apply force-routing's policy rewrite (proxy UDS, no CA).
    let proxy_uds = PathBuf::from("/tmp/egress-1-0/egress.sock");
    let forced = crate::egress::net_worker::rewrite_worker_policy(brokered, &proxy_uds, None);
    // Broker fields preserved through the egress rewrite.
    assert_eq!(
        forced.broker_uds.as_deref(),
        Some(broker_uds.as_path()),
        "broker_uds must survive the egress policy rewrite"
    );
    assert!(
        forced
            .env
            .iter()
            .any(|(k, v)| k == BrokerKind::Embed.uds_env() && v == "/tmp/embed-1-0/embed.sock"),
        "broker env must survive the egress policy rewrite: {:?}",
        forced.env
    );
    // And the egress rewrite still did its own job (proxy UDS set).
    assert_eq!(forced.proxy_uds.as_deref(), Some(proxy_uds.as_path()));
}

/// Fail-closed: a worker that requests a broker (`broker: Some`) but has no daemon
/// `BrokerConfig` for that kind must be **refused** before any spawn — the backend
/// is never touched, and the error names the missing config (not a Sandbox spawn
/// error).
#[test]
fn broker_requested_without_config_fails_closed_before_spawn() {
    let policy = SandboxPolicy {
        net: Net::Allowlist(vec!["searx.example.org:443".into()]),
        ..SandboxPolicy::default()
    };
    let spec = spec_for(&policy);
    let broker_spec = BrokerSpec::embed("http://127.0.0.1:11434/v1/embeddings");
    let res = spawn_worker_with_optional_broker(
        None,                     // no force-routing
        &BrokerConfigs::default(), // no broker config for any kind → fail closed
        &FailBackend,
        &FailBackend,
        &spec,
        Some(&broker_spec),
        "web-research",
    );
    match res {
        Err(ToolHostError::Io(e)) => {
            assert!(
                e.to_string().contains("broker config"),
                "error should name the missing broker config, got: {e}"
            );
        }
        Err(other) => panic!("expected fail-closed Io error, got a different error: {other:?}"),
        Ok(_) => panic!("expected fail-closed Io error, but a worker was spawned"),
    }
}

/// #448: the embed broker spawns on `sidecar_backend` (the trusted host
/// sidecar), NOT the worker `backend` (which may be a VM the worker reaches the
/// broker from over vsock 1026). The broker is spawned first and fails
/// (recording backend), so only the host recorder is hit.
#[test]
fn broker_spawns_on_sidecar_backend_not_worker_backend() {
    use crate::broker::{BrokerConfig, BrokerConfigs, BrokerKind, BrokerSpec};

    let policy = SandboxPolicy {
        net: Net::Allowlist(vec!["searx.example.org:443".into()]),
        ..SandboxPolicy::default()
    };
    let scratch = tempfile::tempdir().expect("broker scratch root");
    let broker_cfg = BrokerConfig::new(
        BrokerKind::Embed,
        PathBuf::from("/nonexistent/embed-broker"),
        scratch.path().to_path_buf(),
    );
    let broker_configs = BrokerConfigs { embed: Some(Arc::new(broker_cfg)), ..Default::default() };
    let broker_spec = BrokerSpec::embed("http://127.0.0.1:11434/v1/embeddings");

    let calls = Arc::new(Mutex::new(Vec::new()));
    let worker_backend = RecordingBackend { label: "vm-worker", calls: Arc::clone(&calls) };
    let sidecar_backend = RecordingBackend { label: "host-sidecar", calls: Arc::clone(&calls) };

    let res = spawn_worker_with_optional_broker(
        None, // no force-routing — the broker spawn is what we're testing
        &broker_configs,
        &worker_backend,
        &sidecar_backend,
        &spec_for(&policy),
        Some(&broker_spec),
        "web-research",
    );

    // The broker is spawned first and fails (recording backend). Unlike the
    // egress sidecar (whose spawn error `spawn_net_worker` wraps in `Io`),
    // `spawn_broker` propagates the raw `SandboxError` via `?` → `Sandbox`.
    assert!(
        matches!(res, Err(ToolHostError::Sandbox(_))),
        "broker spawn failure must propagate the backend's SandboxError"
    );
    let hit = calls.lock().unwrap().clone();
    assert_eq!(
        hit,
        vec!["host-sidecar"],
        "the embed broker must spawn on sidecar_backend (host); the worker backend must not be reached"
    );
}

/// No broker requested → byte-identical pass-through to the legacy path
/// (Sandbox error for an allowlist worker with no force-routing).
#[test]
fn no_broker_requested_is_passthrough() {
    let policy = SandboxPolicy {
        net: Net::Allowlist(vec!["api.example.com:443".into()]),
        ..SandboxPolicy::default()
    };
    let res = spawn_worker_with_optional_broker(
        None,
        &BrokerConfigs::default(),
        &FailBackend,
        &FailBackend,
        &spec_for(&policy),
        None, // no broker
        "web-fetch",
    );
    assert!(
        matches!(res, Err(ToolHostError::Sandbox(_))),
        "no-broker path must be byte-identical to spawn_worker_maybe_forced"
    );
}
