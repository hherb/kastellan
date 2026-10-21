use super::*;

#[test]
fn allowlist_uses_explicit_port() {
    assert_eq!(
        broker_allowlist_from_endpoint("http://127.0.0.1:11434/v1/embeddings"),
        vec!["127.0.0.1:11434".to_string()]
    );
}

#[test]
fn allowlist_defaults_https_port() {
    assert_eq!(
        broker_allowlist_from_endpoint("https://embed.example.org/embed"),
        vec!["embed.example.org:443".to_string()]
    );
}

#[test]
fn allowlist_empty_for_unparseable_endpoint() {
    assert!(broker_allowlist_from_endpoint("not a url").is_empty());
}

#[test]
fn policy_shape_is_net_client_allowlist_with_env() {
    let p = broker_policy(
        Path::new("/opt/embed-broker"),
        "http://127.0.0.1:11434/v1/embeddings",
        Path::new("/scratch"),
        BrokerKind::Embed,
    );
    assert!(matches!(p.profile, Profile::WorkerNetClient));
    match &p.net {
        Net::Allowlist(hosts) => assert_eq!(hosts, &vec!["127.0.0.1:11434".to_string()]),
        other => panic!("expected Net::Allowlist, got {other:?}"),
    }
    assert!(p.fs_read.contains(&PathBuf::from("/etc/resolv.conf")));
    assert!(p.fs_write.contains(&PathBuf::from("/scratch")));
    assert!(p.broker_uds.is_none(), "the broker itself has no upstream broker");
    assert!(p.proxy_uds.is_none());
    let env: std::collections::HashMap<_, _> = p.env.into_iter().collect();
    assert_eq!(env[BrokerKind::Embed.uds_env()], "/scratch/embed.sock");
    assert_eq!(
        env[BrokerKind::Embed.endpoint_env()],
        "http://127.0.0.1:11434/v1/embeddings"
    );
}

#[test]
fn derived_broker_policy_carries_lockdown_env_for_dns() {
    // Regression pin (e70174b lesson): the broker spawn derives the
    // worker-prelude lockdown env from its policy, so seccomp + Landlock RO
    // grants for the DNS resolver files are present. Without them the broker's
    // own lock_down would block its backend DNS post-lockdown.
    let p = broker_policy(
        Path::new("/opt/embed-broker"),
        "http://127.0.0.1:11434/v1/embeddings",
        Path::new("/scratch"),
        BrokerKind::Embed,
    );
    let d = derive_lockdown_env(&p);
    let env: std::collections::HashMap<_, _> = d.env.into_iter().collect();
    assert_eq!(env["KASTELLAN_SECCOMP_PROFILE"], "net_client");
    let ro: Vec<String> = serde_json::from_str(&env["KASTELLAN_LANDLOCK_RO"]).unwrap();
    for path in ["/etc/resolv.conf", "/etc/hosts", "/etc/nsswitch.conf"] {
        assert!(ro.iter().any(|r| r == path), "Landlock RO must grant {path}");
    }
}

#[test]
fn scratch_dir_name_uses_kind_prefix() {
    let dir = make_broker_scratch_dir(Path::new("/tmp"), BrokerKind::Embed).expect("mint under /tmp");
    let name = dir.file_name().unwrap().to_string_lossy();
    assert!(
        name.starts_with(BrokerKind::Embed.scratch_prefix()),
        "unexpected name: {name}"
    );
    // Clean up the real dir this created.
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scratch_dir_rejects_overlong_sun_path() {
    // A pathological scratch root whose projected socket overflows sun_path
    // must fail-closed BEFORE creating the dir.
    let long_root = PathBuf::from(format!("/tmp/{}", "x".repeat(SUN_PATH_MAX)));
    let err = make_broker_scratch_dir(&long_root, BrokerKind::Embed)
        .expect_err("must reject overlong path");
    match err {
        ToolHostError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput),
        other => panic!("expected Io(InvalidInput), got {other:?}"),
    }
}

#[test]
fn wait_for_broker_ready_times_out_when_absent_and_alive() {
    // Hermetic readiness pin: a socket that never appears AND a broker that
    // never exits → TimedOut quickly (the live bind path is DGX-gated).
    let missing = PathBuf::from("/tmp/kastellan-broker-nonexistent-xyz.sock");
    let _ = std::fs::remove_file(&missing);
    let ready = wait_for_broker_ready(&missing, Duration::from_millis(60), || Ok(None));
    assert!(matches!(ready, BrokerReady::TimedOut));
}

#[test]
fn wait_for_broker_ready_reports_early_exit_without_waiting_full_timeout() {
    // A broker that exits before binding is reported as `Exited` immediately,
    // NOT after the (here 10s) timeout — the whole point of watching liveness.
    let missing = PathBuf::from("/tmp/kastellan-broker-early-exit-xyz.sock");
    let _ = std::fs::remove_file(&missing);
    // Spawn a real short-lived process just to obtain a genuine ExitStatus.
    let status = std::process::Command::new("true")
        .status()
        .expect("run `true`");
    let start = Instant::now();
    let ready = wait_for_broker_ready(&missing, Duration::from_secs(10), || Ok(Some(status)));
    assert!(matches!(ready, BrokerReady::Exited(_)));
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "must return on early exit, not block the full timeout"
    );
}

#[test]
fn wait_for_broker_ready_reports_a_failed_exit_poll_instead_of_timing_out() {
    // #754's shape: a `try_wait` that ERRORS (e.g. `ECHILD`) was read as
    // "still running", so the loop spun to the timeout and blamed the UDS
    // bind — a fact about our own `waitpid` reported as one about the broker.
    let missing = PathBuf::from("/tmp/kastellan-broker-unpollable-xyz.sock");
    let _ = std::fs::remove_file(&missing);
    let start = Instant::now();
    let ready = wait_for_broker_ready(&missing, Duration::from_secs(10), || {
        Err(std::io::Error::from_raw_os_error(libc::ECHILD))
    });
    match ready {
        BrokerReady::Unpollable(e) => assert_eq!(e.raw_os_error(), Some(libc::ECHILD)),
        _ => panic!("a failed exit poll must be reported as such, not as a bind outcome"),
    }
    assert!(start.elapsed() < Duration::from_secs(1), "must not wait out the timeout");
}

/// Backend whose spawn always fails — proves the endpoint check runs *before*
/// any spawn (a passing test means the backend was never reached).
struct FailBackend;
impl SandboxBackend for FailBackend {
    fn spawn_under_policy(
        &self,
        _policy: &SandboxPolicy,
        _program: &str,
        _args: &[&str],
    ) -> Result<Child, kastellan_sandbox::SandboxError> {
        Err(kastellan_sandbox::SandboxError::Backend("test: spawn refused".into()))
    }
}

#[test]
fn spawn_rejects_malformed_endpoint_before_touching_the_backend() {
    // A hostless/unparseable endpoint is refused up front (fail-fast, clear
    // error) rather than surfacing as a 5s bind-timeout. The check runs before
    // scratch is minted or the backend is spawned — `FailBackend` (which would
    // error if reached) is never invoked, so the `InvalidInput` error proves
    // the early rejection.
    let cfg = BrokerConfig::new(
        BrokerKind::Embed,
        PathBuf::from("/nonexistent/kastellan-worker-embed-broker"),
        PathBuf::from("/tmp"),
    );
    let spec = BrokerSpec::embed("not a url");
    // `BrokerSidecar` isn't `Debug`, so match rather than `expect_err`.
    match spawn_broker(&cfg, &spec, &FailBackend) {
        Err(ToolHostError::Io(e)) => {
            assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
            assert!(
                e.to_string().contains("no parseable host:port"),
                "error should name the malformed endpoint, got: {e}"
            );
        }
        Err(other) => panic!("expected Io(InvalidInput), got {other:?}"),
        Ok(_) => panic!("expected malformed endpoint to be rejected, but a broker spawned"),
    }
}
