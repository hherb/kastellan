use super::*;
use crate::channel::ConversationId;

#[test]
fn parse_matrix_poll_decodes_wire_events() {
    let v = serde_json::json!({"events": [
        {"peer": "@me:srv", "conversation": "!room:srv", "body": "hi"}
    ]});
    let evs = parse_matrix_poll(v).unwrap();
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0].peer, "@me:srv");
    assert_eq!(evs[0].conversation, "!room:srv");
    assert_eq!(evs[0].body, "hi");
    assert!(parse_matrix_poll(serde_json::json!("garbage")).is_err());
}

#[test]
fn encode_matrix_send_matches_the_wire_shape() {
    let msg = OutgoingMessage {
        channel: ChannelId("matrix".into()),
        peer: PeerId(String::new()),
        conversation: ConversationId("!room:srv".into()),
        body: "pong".into(),
    };
    assert_eq!(
        encode_matrix_send(&msg),
        serde_json::json!({"conversation": "!room:srv", "body": "pong"})
    );
}

#[test]
fn host_from_url_strips_scheme_path_and_port() {
    assert_eq!(host_from_url("https://matrix.kastellan.dev").unwrap(), "matrix.kastellan.dev");
    assert_eq!(host_from_url("https://matrix.example.org:8448/").unwrap(), "matrix.example.org");
    assert_eq!(host_from_url("http://127.0.0.1:6167").unwrap(), "127.0.0.1");
    assert_eq!(host_from_url("matrix.bare.host").unwrap(), "matrix.bare.host");
    // IPv6 literals: strip brackets + port.
    assert_eq!(host_from_url("https://[::1]:8448").unwrap(), "::1");
    assert_eq!(host_from_url("http://[2001:db8::1]/_matrix").unwrap(), "2001:db8::1");
    assert!(host_from_url("https://").is_err());
}

#[test]
fn host_port_from_url_extracts_port_and_scheme_defaults() {
    // Scheme defaults when no explicit port.
    assert_eq!(host_port_from_url("https://matrix.kastellan.dev").unwrap(), ("matrix.kastellan.dev".into(), 443));
    assert_eq!(host_port_from_url("http://127.0.0.1").unwrap(), ("127.0.0.1".into(), 80));
    assert_eq!(host_port_from_url("matrix.bare.host").unwrap(), ("matrix.bare.host".into(), 443));
    // Explicit port wins over the scheme default — the self-hosted-on-8448 case.
    assert_eq!(host_port_from_url("https://matrix.example.org:8448/").unwrap(), ("matrix.example.org".into(), 8448));
    assert_eq!(host_port_from_url("http://127.0.0.1:6167").unwrap(), ("127.0.0.1".into(), 6167));
    // IPv6 literals, with and without an explicit port.
    assert_eq!(host_port_from_url("https://[::1]:8448").unwrap(), ("::1".into(), 8448));
    assert_eq!(host_port_from_url("http://[2001:db8::1]/_matrix").unwrap(), ("2001:db8::1".into(), 80));
    // Malformed: empty host, non-numeric port.
    assert!(host_port_from_url("https://").is_err());
    assert!(host_port_from_url("https://h:notaport").is_err());
}

fn daemon_get(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: std::collections::HashMap<String, String> =
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    move |k: &str| map.get(k).cloned()
}

#[test]
fn daemon_cfg_none_when_required_unset() {
    let exe = std::path::Path::new("/exe");
    let st = std::path::Path::new("/st");
    // Nothing set.
    assert!(parse_daemon_spawn_config(daemon_get(&[]), Some(exe), Some(st)).is_none());
    // Homeserver set but user missing.
    let g = daemon_get(&[("KASTELLAN_MATRIX_HOMESERVER_URL", "https://m")]);
    assert!(parse_daemon_spawn_config(g, Some(exe), Some(st)).is_none());
}

#[test]
fn daemon_cfg_defaults_worker_bin_and_store_and_sandbox_on() {
    let g = daemon_get(&[
        ("KASTELLAN_MATRIX_HOMESERVER_URL", "https://m"),
        ("KASTELLAN_MATRIX_USER", "@b:m"),
    ]);
    let c = parse_daemon_spawn_config(
        g,
        Some(std::path::Path::new("/exe")),
        Some(std::path::Path::new("/st/matrix/store")),
    )
    .expect("config");
    assert_eq!(c.worker_bin, PathBuf::from("/exe/kastellan-worker-matrix"));
    assert_eq!(c.store_dir, PathBuf::from("/st/matrix/store"));
    assert!(c.enforce_sandbox, "sandbox must default ON");
    assert!(c.password.is_none(), "daemon relies on persisted session");
}

#[test]
fn daemon_cfg_enforce_sandbox_off_only_for_explicit_falsey() {
    let mk = |val: &str| {
        daemon_get(&[
            ("KASTELLAN_MATRIX_HOMESERVER_URL", "https://m"),
            ("KASTELLAN_MATRIX_USER", "@b:m"),
            ("KASTELLAN_MATRIX_ENFORCE_SANDBOX", val),
        ])
    };
    let exe = std::path::Path::new("/e");
    let st = std::path::Path::new("/s");
    assert!(!parse_daemon_spawn_config(mk("0"), Some(exe), Some(st)).unwrap().enforce_sandbox);
    assert!(!parse_daemon_spawn_config(mk("false"), Some(exe), Some(st)).unwrap().enforce_sandbox);
    assert!(!parse_daemon_spawn_config(mk("FALSE"), Some(exe), Some(st)).unwrap().enforce_sandbox);
    assert!(parse_daemon_spawn_config(mk("1"), Some(exe), Some(st)).unwrap().enforce_sandbox);
}

#[test]
fn daemon_cfg_env_overrides_worker_bin_and_store() {
    let g = daemon_get(&[
        ("KASTELLAN_MATRIX_HOMESERVER_URL", "https://m"),
        ("KASTELLAN_MATRIX_USER", "@b:m"),
        ("KASTELLAN_MATRIX_WORKER_BIN", "/opt/w"),
        ("KASTELLAN_MATRIX_STORE", "/data/store"),
    ]);
    // No exe_dir / default_store needed when both are overridden.
    let c = parse_daemon_spawn_config(g, None, None).expect("config");
    assert_eq!(c.worker_bin, PathBuf::from("/opt/w"));
    assert_eq!(c.store_dir, PathBuf::from("/data/store"));
}

#[test]
fn parse_daemon_config_defaults_use_microvm_false_and_password_none() {
    let g = daemon_get(&[
        ("KASTELLAN_MATRIX_HOMESERVER_URL", "https://matrix.kastellan.dev"),
        ("KASTELLAN_MATRIX_USER", "@kastellan:kastellan.dev"),
        ("KASTELLAN_MATRIX_STORE", "/state/matrix/store"),
        ("KASTELLAN_MATRIX_WORKER_BIN", "/bin/kastellan-worker-matrix"),
    ]);
    let cfg = parse_daemon_spawn_config(g, None, None).unwrap();
    assert!(!cfg.use_microvm);
    assert_eq!(cfg.password, None);
}

#[test]
fn parse_daemon_config_reads_use_microvm_and_password() {
    let g = daemon_get(&[
        ("KASTELLAN_MATRIX_HOMESERVER_URL", "https://matrix.kastellan.dev"),
        ("KASTELLAN_MATRIX_USER", "@kastellan:kastellan.dev"),
        ("KASTELLAN_MATRIX_STORE", "/state/matrix/store"),
        ("KASTELLAN_MATRIX_WORKER_BIN", "/bin/kastellan-worker-matrix"),
        ("KASTELLAN_MATRIX_USE_MICROVM", "1"),
        ("KASTELLAN_MATRIX_PASSWORD", "s3cret"),
    ]);
    let cfg = parse_daemon_spawn_config(g, None, None).unwrap();
    assert!(cfg.use_microvm);
    assert_eq!(cfg.password.as_deref(), Some("s3cret"));
}

#[test]
fn policy_builder_shape() {
    let p = build_matrix_policy(
        PathBuf::from("/opt/kastellan/kastellan-worker-matrix"),
        "matrix.example.org",
        443,
        PathBuf::from("/var/lib/kastellan/matrix/store"),
        Some(PathBuf::from("/run/egress.sock")),
        Some(PathBuf::from("/run/ca.pem")),
    );
    assert!(matches!(p.net, Net::Allowlist(ref v) if v == &["matrix.example.org:443"]));
    assert!(matches!(p.profile, Profile::WorkerMatrixClient));
    assert_eq!(p.fs_write, vec![PathBuf::from("/var/lib/kastellan/matrix/store")]);
    assert!(p.fs_read.contains(&PathBuf::from("/run/ca.pem")));
    assert!(p.fs_read.contains(&PathBuf::from("/etc/resolv.conf")));
    // System CA trust store must be bound regardless of force-routing —
    // matrix-sdk 0.18 validates homeserver TLS against it (transparent tunnel,
    // not MITM), so its absence fails the client build at startup.
    assert!(p.fs_read.contains(&PathBuf::from("/etc/ssl/certs")));
    assert_eq!(p.proxy_uds, Some(PathBuf::from("/run/egress.sock")));
}

#[test]
fn parse_peers_csv_trims_and_drops_empties() {
    assert!(parse_peers_csv("").is_empty());
    assert!(parse_peers_csv("  , ,, ").is_empty());
    assert_eq!(
        parse_peers_csv(" @a:s , @b:s ,, @c:s "),
        vec![PeerId("@a:s".into()), PeerId("@b:s".into()), PeerId("@c:s".into())]
    );
}

#[test]
fn policy_builder_omits_ca_when_not_force_routed() {
    let p = build_matrix_policy(
        PathBuf::from("/opt/k/kastellan-worker-matrix"),
        "m.example.org",
        443,
        PathBuf::from("/store"),
        None,
        None,
    );
    assert!(p.proxy_uds.is_none());
    assert!(!p.fs_read.iter().any(|x| x.to_string_lossy().contains("ca.pem")));
}
