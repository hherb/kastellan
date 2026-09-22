//! Unit tests for [`super::proxy_policy`], [`super::check_upstream_extra_ca`],
//! [`super::spawn_sidecar`]'s stderr-note helper, and the [`super::Mitm`]
//! posture type (#494).
//!
//! Lifted out of the parent module's inline `#[cfg(test)] mod tests` once the
//! file crossed ~600 lines (same pattern as `egress::net_worker::tests` and
//! `worker_lifecycle::force_route::tests`). Production logic lives in the
//! parent `spawn.rs`; this file is `mod tests;` from there and is only
//! compiled under `#[cfg(test)]`.

use super::*;

/// A `SidecarSpawn` with everything defaulted except the posture, so each test
/// states only what it is about.
fn spec_with(mitm: Mitm<'_>) -> SidecarSpawn<'_> {
    SidecarSpawn {
        binary: Path::new("/opt/proxy"),
        allowlist: &[],
        scratch: Path::new("/scratch"),
        worker: "email",
        cert_pins_json: None,
        mitm,
        long_lived: false,
    }
}

#[test]
fn policy_uses_proxy_egress_and_net_client() {
    let allow = vec!["example.com".to_string()];
    let mut spec = spec_with(Mitm::Intercept { upstream_extra_ca: None });
    spec.allowlist = &allow;
    spec.worker = "web-fetch";
    let p = proxy_policy(&spec);
    assert!(matches!(p.net, Net::ProxyEgress));
    assert!(matches!(p.profile, Profile::WorkerNetClient));
    assert!(p.fs_read.contains(&PathBuf::from("/etc/resolv.conf")));
    assert!(p.fs_write.contains(&PathBuf::from("/scratch")));
    // env carries the UDS path + allowlist + worker name.
    let env: std::collections::HashMap<_, _> = p.env.into_iter().collect();
    assert_eq!(env[ENV_UDS], "/scratch/egress.sock");
    assert_eq!(env[ENV_ALLOWLIST], r#"["example.com"]"#);
    assert_eq!(env[ENV_WORKER], "web-fetch");
}

/// Regression pin for the live-gate bug (5b-4a): the sidecar spawn must
/// derive the worker-prelude lockdown env from the policy. Without it the
/// proxy self-applied Landlock WITHOUT the fs_read grants, so post-lockdown
/// glibc could not read /etc/resolv.conf|hosts|nsswitch.conf and every
/// DNS-needing CONNECT failed EAI_AGAIN on Linux (hermetic literal-IP
/// suites stayed green, hiding it) — and ran with no seccomp at all.
/// `spawn_sidecar` feeds `proxy_policy` through `derive_lockdown_env`;
/// this pins what that derivation must yield for the proxy's policy.
#[test]
fn derived_proxy_policy_carries_lockdown_env_for_dns() {
    let allow = vec!["matrix.example.org:443".to_string()];
    let mut spec = spec_with(Mitm::Transparent);
    spec.allowlist = &allow;
    spec.worker = "matrix";
    spec.long_lived = true;
    let p = proxy_policy(&spec);
    let d = crate::tool_host::derive_lockdown_env(&p);
    let env: std::collections::HashMap<_, _> = d.env.into_iter().collect();
    assert_eq!(env["KASTELLAN_SECCOMP_PROFILE"], "net_client");
    let ro: Vec<String> = serde_json::from_str(&env["KASTELLAN_LANDLOCK_RO"]).unwrap();
    for path in ["/etc/resolv.conf", "/etc/hosts", "/etc/nsswitch.conf"] {
        assert!(ro.iter().any(|r| r == path), "Landlock RO must grant {path}");
    }
    let rw: Vec<String> = serde_json::from_str(&env["KASTELLAN_LANDLOCK_RW"]).unwrap();
    assert!(rw.iter().any(|r| r == "/scratch"), "Landlock RW must grant the scratch dir");
    // Long-lived sidecar: no cumulative RLIMIT_CPU (cpu_ms == 0 ⇒ env omitted).
    assert!(!env.contains_key("KASTELLAN_CPU_MS"), "no CPU rlimit for a long-lived sidecar");
}

/// Issue #395: the CPU cap is lifetime-scoped. A long-lived channel sidecar
/// (matrix, weeks) must carry NO cumulative RLIMIT_CPU — a bounded cap would
/// eventually SIGKILL it mid-flight now that the lockdown env actually
/// reaches the proxy (post `e70174b`).
#[test]
fn proxy_policy_long_lived_has_no_cpu_cap() {
    let allow = vec!["matrix.example.org:443".to_string()];
    let mut spec = spec_with(Mitm::Transparent);
    spec.allowlist = &allow;
    spec.worker = "matrix";
    spec.long_lived = true;
    let p = proxy_policy(&spec);
    assert_eq!(p.cpu_ms, 0, "long-lived sidecar must have no cumulative CPU cap");
}

/// Issue #395: a short-lived per-tool-call sidecar (web-fetch) lives 1:1 with
/// its single dispatch, so it keeps a bounded RLIMIT_CPU as defense-in-depth
/// — the only per-process CPU-governance primitive on macOS. This is the
/// path `e70174b` had regressed to `0` blanket-wide.
#[test]
fn proxy_policy_short_lived_keeps_bounded_cpu_cap() {
    let allow = vec!["example.com".to_string()];
    let mut spec = spec_with(Mitm::Intercept { upstream_extra_ca: None });
    spec.allowlist = &allow;
    spec.worker = "web-fetch";
    let p = proxy_policy(&spec);
    assert_eq!(
        p.cpu_ms, SHORT_LIVED_SIDECAR_CPU_MS,
        "short-lived sidecar must keep a bounded CPU cap",
    );
    assert!(p.cpu_ms > 0);
}

/// The short-lived cap must survive lockdown-env derivation as
/// `KASTELLAN_CPU_MS` (the wire form the worker prelude reads for
/// `setrlimit(RLIMIT_CPU)`) — the long-lived case omits it entirely (pinned
/// by `derived_proxy_policy_carries_lockdown_env_for_dns`).
#[test]
fn derived_short_lived_policy_carries_cpu_ms_env() {
    let allow = vec!["example.com".to_string()];
    let mut spec = spec_with(Mitm::Intercept { upstream_extra_ca: None });
    spec.allowlist = &allow;
    spec.worker = "web-fetch";
    let p = proxy_policy(&spec);
    let d = crate::tool_host::derive_lockdown_env(&p);
    let env: std::collections::HashMap<_, _> = d.env.into_iter().collect();
    assert_eq!(
        env["KASTELLAN_CPU_MS"],
        SHORT_LIVED_SIDECAR_CPU_MS.to_string(),
        "short-lived sidecar must derive a CPU rlimit env",
    );
}

#[test]
fn proxy_policy_omits_pins_env_when_none() {
    let allow = vec!["example.com".to_string()];
    let mut spec = spec_with(Mitm::Intercept { upstream_extra_ca: None });
    spec.allowlist = &allow;
    spec.worker = "web-fetch";
    let p = proxy_policy(&spec);
    let env: std::collections::HashMap<_, _> = p.env.into_iter().collect();
    assert!(!env.contains_key(ENV_PINS));
}

#[test]
fn proxy_policy_includes_pins_env_when_set() {
    let pins = r#"{"api.anthropic.com":["sha256/AAAA"]}"#;
    let allow = vec!["example.com".to_string()];
    let mut spec = spec_with(Mitm::Intercept { upstream_extra_ca: None });
    spec.allowlist = &allow;
    spec.worker = "web-fetch";
    spec.cert_pins_json = Some(pins);
    let p = proxy_policy(&spec);
    let env: std::collections::HashMap<_, _> = p.env.into_iter().collect();
    assert_eq!(env[ENV_PINS], pins);
}

#[test]
fn proxy_policy_sets_disable_mitm_env_when_requested() {
    let allow = vec!["example.com:443".to_string()];
    let mut spec = spec_with(Mitm::Transparent);
    spec.allowlist = &allow;
    spec.worker = "browser-driver";
    let p = proxy_policy(&spec);
    let env: std::collections::HashMap<_, _> = p.env.into_iter().collect();
    assert_eq!(env[ENV_DISABLE_MITM], "1");
}

#[test]
fn proxy_policy_omits_disable_mitm_env_when_false() {
    let allow = vec!["example.com:443".to_string()];
    let mut spec = spec_with(Mitm::Intercept { upstream_extra_ca: None });
    spec.allowlist = &allow;
    spec.worker = "web-fetch";
    let p = proxy_policy(&spec);
    let env: std::collections::HashMap<_, _> = p.env.into_iter().collect();
    assert!(!env.contains_key(ENV_DISABLE_MITM));
}

#[test]
fn proxy_policy_includes_upstream_extra_ca_env_and_fs_read_when_set() {
    let ca = PathBuf::from("/etc/localmail/ca.pem");
    let allow = vec!["127.0.0.1:8443".to_string()];
    let mut spec = spec_with(Mitm::Intercept { upstream_extra_ca: Some(&ca) });
    spec.allowlist = &allow;
    spec.worker = "mail";
    let p = proxy_policy(&spec);
    let env: std::collections::HashMap<_, _> = p.env.iter().cloned().collect();
    assert_eq!(env[ENV_UPSTREAM_EXTRA_CA], "/etc/localmail/ca.pem");
    assert!(p.fs_read.contains(&ca), "the extra CA must be bound into the proxy jail");
}

#[test]
fn proxy_policy_omits_upstream_extra_ca_when_none() {
    let allow = vec!["example.com".to_string()];
    let mut spec = spec_with(Mitm::Intercept { upstream_extra_ca: None });
    spec.allowlist = &allow;
    spec.worker = "web-fetch";
    let p = proxy_policy(&spec);
    let env: std::collections::HashMap<_, _> = p.env.iter().cloned().collect();
    assert!(!env.contains_key(ENV_UPSTREAM_EXTRA_CA));
    assert!(!p.fs_read.contains(&PathBuf::from("/etc/localmail/ca.pem")));
}

// ---- The `Mitm` posture type's own tests (#494) ----

#[test]
fn transparent_posture_sets_the_disable_mitm_env_and_no_anchor() {
    let allow = vec!["example.com:443".to_string()];
    let mut spec = spec_with(Mitm::Transparent);
    spec.allowlist = &allow;
    let p = proxy_policy(&spec);
    assert!(p.env.iter().any(|(k, v)| k == ENV_DISABLE_MITM && v == "1"));
    assert!(!p.env.iter().any(|(k, _)| k == ENV_UPSTREAM_EXTRA_CA));
}

#[test]
fn intercept_without_anchor_is_byte_identical_to_the_default_path() {
    let allow = vec!["example.com:443".to_string()];
    let mut spec = spec_with(Mitm::Intercept { upstream_extra_ca: None });
    spec.allowlist = &allow;
    let p = proxy_policy(&spec);
    assert!(!p.env.iter().any(|(k, _)| k == ENV_DISABLE_MITM));
    assert!(!p.env.iter().any(|(k, _)| k == ENV_UPSTREAM_EXTRA_CA));
    assert!(!p.fs_read.iter().any(|f| f.ends_with("ca.pem")));
}

#[test]
fn intercept_with_anchor_sets_both_the_env_and_the_fs_read_bind() {
    let allow = vec!["10.0.0.3:8443".to_string()];
    let ca = PathBuf::from("/etc/kastellan/localmail.pem");
    let mut spec = spec_with(Mitm::Intercept { upstream_extra_ca: Some(&ca) });
    spec.allowlist = &allow;
    let p = proxy_policy(&spec);
    // Both halves are load-bearing: the proxy READS the PEM before lock_down,
    // so the env key alone would name a path it cannot open.
    assert!(p.env.iter().any(|(k, v)| k == ENV_UPSTREAM_EXTRA_CA && *v == ca.to_string_lossy()));
    assert!(p.fs_read.contains(&ca));
}

#[test]
fn check_upstream_extra_ca_rejects_a_relative_anchor() {
    let ca = PathBuf::from("relative/ca.pem");
    let err = check_upstream_extra_ca(Mitm::Intercept { upstream_extra_ca: Some(&ca) })
        .expect_err("relative path must fail");
    assert!(err.to_string().contains("absolute"), "unhelpful error: {err}");
}

#[test]
fn check_upstream_extra_ca_accepts_both_postures_without_an_anchor() {
    assert!(check_upstream_extra_ca(Mitm::Transparent).is_ok());
    assert!(check_upstream_extra_ca(Mitm::Intercept { upstream_extra_ca: None }).is_ok());
}

/// With no drain thread there is nothing to report, and the note must say so
/// rather than claim a clean startup. (The populated case is covered by the
/// sandbox-gated `sidecar_with_unreadable_extra_ca_fails_fast_with_reason`
/// in `core/tests/egress_proxy_e2e.rs`, which needs a real proxy binary.)
#[test]
fn stderr_note_without_a_tail_says_nothing_was_captured() {
    assert_eq!(stderr_note(None), "no stderr captured");
}

/// A tail that already has lines is reported immediately — the settle poll
/// must not delay the common case where the drain already flushed.
#[test]
fn stderr_note_reports_captured_lines() {
    let tail = crate::worker_stderr::StderrTail::new(4);
    let end = crate::worker_stderr::drain_reader(
        0,
        std::io::Cursor::new(b"Error: build upstream TLS config: upstream extra CA: read\n"),
        Some(&tail),
    );
    tail.mark_drained(end);
    let note = stderr_note(Some(&tail));
    assert!(note.contains("upstream extra CA"), "note lost the reason: {note}");
}

/// Dropping a bare [`SidecarHandle`] must kill the child process (#502).
///
/// This is the exact shape every spawn path hits on its error branch: the
/// sidecar is up, the *worker* spawn then fails, and `?` drops a handle that
/// was never wrapped in the `Drop`-bearing `EgressSidecar`. Without
/// `SidecarHandle`'s own `Drop` that was a silent no-op — `std::process::Child`
/// does not kill on drop — and #514's bring-up retry loop would have
/// accumulated one leaked proxy per attempt for the length of an outage.
///
/// Uses a plain long-`sleep` child rather than a real proxy: what is under
/// test is the drop glue, not anything about the proxy, and this keeps the
/// test hermetic (no sandbox, no binary discovery, no UDS).
#[test]
fn dropping_a_bare_sidecar_handle_kills_the_child() {
    use std::process::{Command, Stdio};

    let child = Command::new("sleep")
        .arg("300")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn a sleep child");
    let pid = child.id();
    assert!(pid_is_alive(pid), "the child must be running before the drop");

    // The UDS path need not exist — `terminate` removes it best-effort.
    let handle = SidecarHandle {
        child,
        uds_path: std::path::PathBuf::from("/nonexistent/kastellan-test.sock"),
    };
    drop(handle);

    assert!(!pid_is_alive(pid), "dropping the handle must kill and reap the sidecar (pid {pid})");
}

/// `kill -0 <pid>` succeeds only for a live, signalable process. Shelling out
/// rather than calling `libc::kill` keeps this crate free of a `libc`
/// dependency it does not otherwise need; the flag and its meaning are
/// identical on Linux and macOS (POSIX), unlike the coreutils *output* skew
/// that has bitten shell-driven tests here before. Deliberately not
/// `#[cfg(unix)]`-gated: kastellan is Linux + macOS only, and a cfg here
/// would gate the helper without gating its only caller.
fn pid_is_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
