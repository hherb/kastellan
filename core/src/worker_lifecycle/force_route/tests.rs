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

/// A no-op sink factory — proves the routing decision without a live pool.
fn noop_sink_factory() -> DecisionSinkFactory {
    Box::new(|| Box::new(|_row| {}))
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
    let res = spawn_worker_maybe_forced(None, &FailBackend, &spec_for(&policy), "web-fetch");
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
        spawn_worker_maybe_forced(Some(&cfg), &FailBackend, &spec_for(&policy), "web-fetch");
    // The forced path maps the sidecar-spawn failure to Io (fail-closed).
    assert!(
        matches!(res, Err(ToolHostError::Io(_))),
        "Some config + Allowlist must force-route (Io fail-closed error)"
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
    let res = spawn_worker_maybe_forced(Some(&cfg), &FailBackend, &spec_for(&policy), "shell");
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
        Some(&cfg), &FailBackend, &spec_for(&policy), BROWSER_DRIVER_TOOL);
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
