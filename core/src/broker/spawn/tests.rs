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
