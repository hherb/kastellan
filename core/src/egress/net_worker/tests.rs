use super::*;
use kastellan_sandbox::{Net, SandboxError};

#[test]
fn rewrite_worker_policy_forces_routing() {
    let base = SandboxPolicy {
        net: Net::Allowlist(vec!["api.example.com:443".into()]),
        fs_read: vec!["/etc/resolv.conf".into(), "/bin/worker".into()],
        env: vec![],
        ..SandboxPolicy::default()
    };
    let uds = std::path::PathBuf::from("/scratch/egress.sock");
    let out = rewrite_worker_policy(base, &uds, Some(std::path::Path::new("/scratch/ca.pem")));
    // proxy_uds set → bwrap/Seatbelt force-route.
    assert_eq!(out.proxy_uds.as_deref(), Some(uds.as_path()));
    // resolv.conf removed (worker no longer resolves directly).
    assert!(!out.fs_read.contains(&"/etc/resolv.conf".into()));
    // The worker binary path survives.
    assert!(out.fs_read.contains(&"/bin/worker".into()));
    // env carries the UDS path.
    assert!(out
        .env
        .iter()
        .any(|(k, v)| k == ENV_UDS && v == "/scratch/egress.sock"));
}

#[test]
fn rewrite_worker_policy_injects_ca_trust() {
    let base = SandboxPolicy {
        net: Net::Allowlist(vec!["api.example.com:443".into()]),
        fs_read: vec!["/etc/resolv.conf".into(), "/bin/worker".into()],
        env: vec![],
        ..SandboxPolicy::default()
    };
    let uds = std::path::PathBuf::from("/scratch/egress.sock");
    let ca = std::path::PathBuf::from("/scratch/ca.pem");
    let out = rewrite_worker_policy(base, &uds, Some(ca.as_path()));
    assert!(out.fs_read.contains(&ca));
    assert!(out
        .env
        .iter()
        .any(|(k, v)| k == "KASTELLAN_EGRESS_PROXY_CA" && v == "/scratch/ca.pem"));
}

#[test]
fn rewrite_worker_policy_transparent_injects_no_ca() {
    let base = SandboxPolicy {
        net: Net::Allowlist(vec!["origin.example.com:443".into()]),
        fs_read: vec!["/etc/resolv.conf".into(), "/bin/worker".into()],
        env: vec![],
        ..SandboxPolicy::default()
    };
    let uds = std::path::PathBuf::from("/scratch/egress.sock");
    let ca = std::path::PathBuf::from("/scratch/ca.pem");
    // ca = None → transparent tunnel: proxy_uds set, NO CA anywhere.
    let out = rewrite_worker_policy(base, &uds, None);
    assert_eq!(out.proxy_uds.as_deref(), Some(uds.as_path()));
    assert!(!out.fs_read.contains(&ca), "no CA in fs_read in transparent mode");
    assert!(
        !out.env.iter().any(|(k, _)| k == "KASTELLAN_EGRESS_PROXY_CA"),
        "no CA env in transparent mode"
    );
    // UDS still injected; resolv.conf still dropped; worker bin preserved.
    assert!(out.env.iter().any(|(k, v)| k == ENV_UDS && v == "/scratch/egress.sock"));
    assert!(!out.fs_read.contains(&"/etc/resolv.conf".into()));
    assert!(out.fs_read.contains(&"/bin/worker".into()));
}

#[test]
fn rewrite_overwrites_stale_uds_env() {
    let base = SandboxPolicy {
        net: Net::Allowlist(vec!["api.example.com:443".into()]),
        env: vec![(ENV_UDS.to_string(), "/old/stale.sock".to_string())],
        ..SandboxPolicy::default()
    };
    let out = rewrite_worker_policy(
        base,
        std::path::Path::new("/scratch/egress.sock"),
        Some(std::path::Path::new("/scratch/ca.pem")),
    );
    let uds_entries: Vec<&String> = out
        .env
        .iter()
        .filter(|(k, _)| k == ENV_UDS)
        .map(|(_, v)| v)
        .collect();
    assert_eq!(uds_entries, vec!["/scratch/egress.sock"], "exactly one, fresh");
}

/// A backend whose spawn always fails — stands in for "the sidecar can't
/// start" without needing a real sandbox.
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

#[test]
fn spawn_net_worker_fails_closed_when_sidecar_unavailable() {
    let backend = FailBackend;
    let policy = SandboxPolicy {
        net: Net::Allowlist(vec!["api.example.com:443".into()]),
        ..SandboxPolicy::default()
    };
    let spec = WorkerSpec {
        policy: &policy,
        program: "/bin/worker",
        args: &[],
        wall_clock_ms: None,
    };
    let allowlist = ["api.example.com:443".to_string()];
    let params = NetWorkerSpawn {
        backend: &backend,
        proxy_bin: Path::new("/nonexistent/egress-proxy"),
        spec: &spec,
        allowlist: &allowlist,
        worker_name: "web-fetch",
        secret_fingerprints: &[], // none for this fail-closed test
        cert_pins_json: None,
        disable_mitm: false,
    };
    let res = spawn_net_worker(&params, Path::new("/tmp/kastellan-net-worker-test"), |_row| {});
    assert!(res.is_err(), "no proxy => no net worker (fail-closed)");
}

fn allowlist_spec(policy: &SandboxPolicy) -> WorkerSpec<'_> {
    WorkerSpec {
        policy,
        program: "/bin/worker",
        args: &[],
        wall_clock_ms: None,
    }
}

#[test]
fn spawn_forced_net_worker_fails_closed_when_sidecar_unavailable() {
    let backend = FailBackend;
    let policy = SandboxPolicy {
        net: Net::Allowlist(vec!["api.example.com:443".into()]),
        ..SandboxPolicy::default()
    };
    let scratch_root = tempfile::tempdir().expect("scratch root");
    let spec = allowlist_spec(&policy);
    let allowlist = ["api.example.com:443".to_string()];
    let params = NetWorkerSpawn {
        backend: &backend,
        proxy_bin: Path::new("/nonexistent/egress-proxy"),
        spec: &spec,
        allowlist: &allowlist,
        worker_name: "web-fetch",
        secret_fingerprints: &[], // none for this fail-closed test
        cert_pins_json: None,
        disable_mitm: false,
    };
    let res = spawn_forced_net_worker(&params, scratch_root.path(), |_row| {});
    assert!(res.is_err(), "no proxy => no net worker (fail-closed)");
}

#[test]
fn make_worker_scratch_dir_rejects_overlong_socket_path() {
    // A deep scratch root whose projected `<dir>/egress.sock` overflows
    // sockaddr_un.sun_path must be rejected up front with a clear Io error,
    // not deferred to an opaque bind() failure inside the sidecar. The guard
    // runs before any mkdir, so the (nonexistent) root needs no setup.
    let long_root = PathBuf::from(format!("/{}", "x".repeat(2 * SUN_PATH_MAX)));
    let res = make_worker_scratch_dir(&long_root);
    assert!(
        matches!(res, Err(ToolHostError::Io(_))),
        "overlong scratch root must be rejected with an Io error, got {res:?}"
    );
}

#[test]
fn spawn_forced_net_worker_cleans_scratch_on_failure() {
    // When the sidecar can't spawn, the per-worker scratch subdir created
    // under `scratch_root` must NOT leak — there is no worker to own it, so
    // the wrapper removes it on the failure path.
    let backend = FailBackend;
    let policy = SandboxPolicy {
        net: Net::Allowlist(vec!["api.example.com:443".into()]),
        ..SandboxPolicy::default()
    };
    let scratch_root = tempfile::tempdir().expect("scratch root");
    let spec = allowlist_spec(&policy);
    let allowlist = ["api.example.com:443".to_string()];
    let params = NetWorkerSpawn {
        backend: &backend,
        proxy_bin: Path::new("/nonexistent/egress-proxy"),
        spec: &spec,
        allowlist: &allowlist,
        worker_name: "web-fetch",
        secret_fingerprints: &[],
        cert_pins_json: None,
        disable_mitm: false,
    };
    let _ = spawn_forced_net_worker(&params, scratch_root.path(), |_row| {});
    let leftovers: Vec<_> = std::fs::read_dir(scratch_root.path())
        .expect("read scratch root")
        .collect();
    assert!(
        leftovers.is_empty(),
        "failed force-route spawn left {} scratch entries behind",
        leftovers.len()
    );
}

#[test]
fn net_worker_spawn_struct_carries_pins_field() {
    let backend = FailBackend;
    let policy = SandboxPolicy {
        net: Net::Allowlist(vec!["api.example.com:443".into()]),
        ..SandboxPolicy::default()
    };
    let spec = allowlist_spec(&policy);
    let allowlist = ["example.com".to_string()];
    let params = NetWorkerSpawn {
        backend: &backend,
        proxy_bin: Path::new("/nonexistent/proxy-bin"),
        spec: &spec,
        allowlist: &allowlist,
        worker_name: "web-fetch",
        secret_fingerprints: &[],
        cert_pins_json: Some(r#"{"api.anthropic.com":["sha256/AAAA"]}"#),
        disable_mitm: false,
    };
    let scratch = tempfile::tempdir().unwrap();
    let res = spawn_net_worker(&params, scratch.path(), |_row| {});
    assert!(res.is_err(), "missing proxy binary must fail closed");
}

#[test]
fn provision_writes_secret_hashes_into_scratch() {
    use kastellan_leak_scan::{fingerprint_value, parse_hashes};
    let dir = tempfile::tempdir().expect("scratch");
    let fps = vec![fingerprint_value(b"a-spawn-time-secret").unwrap()];
    provision_secret_hashes(dir.path(), &fps).expect("write");
    let s = std::fs::read_to_string(dir.path().join("secret_hashes.json")).unwrap();
    assert_eq!(parse_hashes(&s), fps);
}

#[test]
fn provision_empty_writes_empty_list() {
    use kastellan_leak_scan::parse_hashes;
    let dir = tempfile::tempdir().expect("scratch");
    provision_secret_hashes(dir.path(), &[]).expect("write");
    let s = std::fs::read_to_string(dir.path().join("secret_hashes.json")).unwrap();
    assert!(parse_hashes(&s).is_empty());
}
