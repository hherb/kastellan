use super::*;
use std::path::Path;

fn ctx<'a>(
    get_env: &'a dyn Fn(&str) -> Option<String>,
    exists: &'a dyn Fn(&Path) -> bool,
    allowlist: &'a dyn Fn(&str) -> Vec<String>,
) -> ResolveCtx<'a> {
    ResolveCtx {
        get_env,
        exists,
        is_dir: &|_p| false,
        exe_dir: None,
        canonicalize: &|_p| None,
        allowlist,
    }
}

#[test]
fn resolve_registers_with_net_client_policy_and_endpoint_net() {
    let get_env = |k: &str| match k {
        BIN_ENV => Some("/opt/web-search".to_string()),
        ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
        _ => None,
    };
    let exists = |_p: &Path| true;
    let allowlist = |_t: &str| vec!["127.0.0.1".to_string()];
    let c = ctx(&get_env, &exists, &allowlist);

    match WebSearchManifest.resolve(&c) {
        Resolution::Register(entry) => {
            assert_eq!(entry.binary, PathBuf::from("/opt/web-search"));
            assert!(matches!(entry.policy.profile, Profile::WorkerNetClient));
            assert_eq!(entry.policy.cpu_ms, 5_000);
            assert_eq!(entry.policy.mem_mb, 256);
            assert_eq!(entry.wall_clock_ms, Some(60_000));
            assert!(entry.policy.fs_read.contains(&PathBuf::from("/etc/resolv.conf")));
            // Net::Allowlist carries the endpoint host:port (loopback :8888).
            match &entry.policy.net {
                Net::Allowlist(hosts) => {
                    assert_eq!(hosts, &vec!["127.0.0.1:8888".to_string()]);
                }
                other => panic!("expected Net::Allowlist, got {other:?}"),
            }
            // Env carries the endpoint + the verbatim allowlist JSON.
            assert_eq!(entry.policy.env[0].0, ENDPOINT_ENV);
            assert_eq!(entry.policy.env[0].1, "http://127.0.0.1:8888/search");
            assert_eq!(entry.policy.env[1].0, "KASTELLAN_WEB_SEARCH_ALLOWLIST");
            assert_eq!(entry.policy.env[1].1, r#"["127.0.0.1"]"#);
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}

#[test]
fn resolve_https_endpoint_maps_to_port_443() {
    let get_env = |k: &str| match k {
        BIN_ENV => Some("/opt/web-search".to_string()),
        ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
        _ => None,
    };
    let exists = |_p: &Path| true;
    let allowlist = |_t: &str| vec!["searx.example.org".to_string()];
    let c = ctx(&get_env, &exists, &allowlist);

    match WebSearchManifest.resolve(&c) {
        Resolution::Register(entry) => match &entry.policy.net {
            Net::Allowlist(hosts) => {
                assert_eq!(hosts, &vec!["searx.example.org:443".to_string()]);
            }
            other => panic!("expected Net::Allowlist, got {other:?}"),
        },
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}

#[test]
fn web_search_has_no_db_argv0_allowlist() {
    // web-search's host allowlist derives from its single endpoint, not the
    // argv0-path `tool_allowlists` DB table (which structurally cannot hold a
    // hostname — CLI + DB CHECK both require a leading '/').
    assert!(
        WebSearchManifest.allowlist_tool().is_none(),
        "web-search must not claim an argv0 DB allowlist"
    );
}

#[test]
fn resolve_derives_worker_allowlist_from_endpoint_not_db() {
    // In the daemon the prefetched DB allowlist is ALWAYS empty for web-search
    // (the table can't hold a host), so the worker allowlist must come from the
    // endpoint host — otherwise `from_env` fails closed on an empty allowlist and
    // web-search never registers. Simulate the daemon: empty ctx.allowlist.
    let get_env = |k: &str| match k {
        BIN_ENV => Some("/opt/web-search".to_string()),
        ENDPOINT_ENV => Some("https://searx.kastellan.dev/search".to_string()),
        _ => None,
    };
    let exists = |_p: &Path| true;
    let allowlist = |_t: &str| Vec::<String>::new();
    let c = ctx(&get_env, &exists, &allowlist);

    match WebSearchManifest.resolve(&c) {
        Resolution::Register(entry) => {
            let al = entry
                .policy
                .env
                .iter()
                .find(|(k, _)| k == "KASTELLAN_WEB_SEARCH_ALLOWLIST")
                .map(|(_, v)| v.clone())
                .expect("allowlist env present");
            assert_eq!(
                al, r#"["searx.kastellan.dev"]"#,
                "worker allowlist must derive from the endpoint host, not the empty DB allowlist"
            );
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}

#[test]
fn resolve_misconfigured_when_no_binary_found() {
    let get_env = |_k: &str| None;
    let exists = |_p: &Path| false;
    let allowlist = |_t: &str| Vec::new();
    let c = ctx(&get_env, &exists, &allowlist);

    match WebSearchManifest.resolve(&c) {
        Resolution::Misconfigured { detail } => {
            assert!(detail.contains("kastellan-worker-web-search"), "detail: {detail}");
        }
        other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
    }
}

#[test]
fn resolve_broker_mode_drops_egress_and_declares_search_broker() {
    let get_env = |k: &str| match k {
        BIN_ENV => Some("/opt/web-search".to_string()),
        ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
        "KASTELLAN_WEB_SEARCH_USE_BROKER" => Some("1".to_string()),
        _ => None,
    };
    let exists = |_p: &Path| true;
    let allowlist = |_t: &str| Vec::<String>::new();
    let c = ctx(&get_env, &exists, &allowlist);

    match WebSearchManifest.resolve(&c) {
        Resolution::Register(entry) => {
            // Broker declared, carrying the SearxNG endpoint it forwards to.
            let spec = entry.broker.as_ref().expect("broker set in broker mode");
            assert_eq!(spec.kind, crate::broker::BrokerKind::Search);
            assert_eq!(spec.endpoint, "http://127.0.0.1:8888/search");
            // Worker has NO direct egress — empty allowlist.
            match &entry.policy.net {
                Net::Allowlist(hosts) => {
                    assert!(hosts.is_empty(), "broker-mode worker must have no egress: {hosts:?}")
                }
                other => panic!("expected empty Net::Allowlist, got {other:?}"),
            }
            // No direct-endpoint env leaked to the worker in broker mode.
            assert!(
                entry.policy.env.iter().all(|(k, _)| k != ENDPOINT_ENV),
                "broker-mode worker must not carry the direct endpoint env"
            );
            // broker_uds is set at spawn, not by the manifest.
            assert!(entry.policy.broker_uds.is_none());
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}

#[test]
fn resolve_direct_mode_unchanged_when_use_broker_unset() {
    let get_env = |k: &str| match k {
        BIN_ENV => Some("/opt/web-search".to_string()),
        ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
        _ => None,
    };
    let exists = |_p: &Path| true;
    let allowlist = |_t: &str| Vec::<String>::new();
    let c = ctx(&get_env, &exists, &allowlist);
    match WebSearchManifest.resolve(&c) {
        Resolution::Register(entry) => {
            assert!(entry.broker.is_none());
            match &entry.policy.net {
                Net::Allowlist(hosts) => {
                    assert_eq!(hosts, &vec!["127.0.0.1:8888".to_string()])
                }
                other => panic!("got {other:?}"),
            }
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}

#[test]
fn resolve_injects_max_batch_env_when_set() {
    let get_env = |k: &str| match k {
        BIN_ENV => Some("/opt/web-search".to_string()),
        ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
        "KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES" => Some("5".to_string()),
        _ => None,
    };
    let exists = |_p: &Path| true;
    let allowlist = |_t: &str| vec!["127.0.0.1".to_string()];
    let c = ctx(&get_env, &exists, &allowlist);
    match WebSearchManifest.resolve(&c) {
        Resolution::Register(entry) => {
            assert!(
                entry.policy.env.iter().any(|(k, v)| k == MAX_BATCH_QUERIES_ENV && v == "5"),
                "cap env must be injected when set: {:?}",
                entry.policy.env
            );
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}

#[test]
fn resolve_omits_max_batch_env_when_unset() {
    let get_env = |k: &str| match k {
        BIN_ENV => Some("/opt/web-search".to_string()),
        ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
        _ => None,
    };
    let exists = |_p: &Path| true;
    let allowlist = |_t: &str| vec!["127.0.0.1".to_string()];
    let c = ctx(&get_env, &exists, &allowlist);
    match WebSearchManifest.resolve(&c) {
        Resolution::Register(entry) => {
            // Byte-identical direct-mode env: endpoint + allowlist only.
            assert_eq!(entry.policy.env.len(), 2);
            assert!(entry.policy.env.iter().all(|(k, _)| k != MAX_BATCH_QUERIES_ENV));
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}

#[test]
fn resolve_skips_blank_max_batch_env() {
    let get_env = |k: &str| match k {
        BIN_ENV => Some("/opt/web-search".to_string()),
        ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
        "KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES" => Some("   ".to_string()),
        _ => None,
    };
    let exists = |_p: &Path| true;
    let allowlist = |_t: &str| vec!["127.0.0.1".to_string()];
    let c = ctx(&get_env, &exists, &allowlist);
    match WebSearchManifest.resolve(&c) {
        Resolution::Register(entry) => {
            assert!(entry.policy.env.iter().all(|(k, _)| k != MAX_BATCH_QUERIES_ENV));
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}

fn outcome_label(r: &Resolution) -> &'static str {
    match r {
        Resolution::Register(_) => "Register",
        Resolution::Disabled { .. } => "Disabled",
        Resolution::Misconfigured { .. } => "Misconfigured",
    }
}

#[cfg(target_os = "linux")]
#[test]
fn resolve_uses_direct_microvm_entry_when_opted_in() {
    let get_env = |k: &str| match k {
        BIN_ENV => Some("/opt/web-search".to_string()),
        ENDPOINT_ENV => Some("https://searx.example.org:8888/search".to_string()),
        "KASTELLAN_WEB_SEARCH_USE_MICROVM" => Some("1".to_string()),
        _ => None,
    };
    let exists = |_p: &Path| true;
    let allowlist = |_t: &str| Vec::<String>::new();
    let c = ctx(&get_env, &exists, &allowlist);
    match WebSearchManifest.resolve(&c) {
        Resolution::Register(entry) => {
            assert_eq!(
                entry.sandbox_backend,
                Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm),
                "VM entry emits the FirecrackerVm backend"
            );
            assert!(entry.policy.fs_read.is_empty(), "VM entry has empty fs_read");
            assert!(entry.broker.is_none(), "direct VM entry has no broker");
            // The in-rootfs binary path, NOT the discovered host path.
            assert_eq!(
                entry.binary,
                PathBuf::from("/usr/local/bin/kastellan-worker-web-search")
            );
            // Endpoint host:port allowlist preserved (routable-SearxNG path).
            match &entry.policy.net {
                Net::Allowlist(hosts) => {
                    assert_eq!(hosts, &vec!["searx.example.org:8888".to_string()])
                }
                other => panic!("expected Net::Allowlist, got {other:?}"),
            }
            // Rootfs env forwarded.
            assert!(entry
                .policy
                .env
                .iter()
                .any(|(k, v)| k == "KASTELLAN_MICROVM_ROOTFS" && v == "web-search.ext4"));
            assert!(entry.policy.env.iter().any(|(k, _)| k == "KASTELLAN_MICROVM_DIR"));
            // Worker env mirrors the host direct path: the endpoint plus the
            // host-only allowlist JSON (the worker re-checks `endpoint host ∈
            // allowlist` at startup, so the VM entry must carry it too).
            assert!(entry
                .policy
                .env
                .iter()
                .any(|(k, v)| k == ENDPOINT_ENV && v == "https://searx.example.org:8888/search"));
            assert!(entry.policy.env.iter().any(|(k, v)| k == "KASTELLAN_WEB_SEARCH_ALLOWLIST"
                && v == r#"["searx.example.org"]"#));
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}

#[cfg(target_os = "linux")]
#[test]
fn resolve_uses_broker_microvm_entry_when_both_opted_in() {
    let get_env = |k: &str| match k {
        BIN_ENV => Some("/opt/web-search".to_string()),
        ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
        "KASTELLAN_WEB_SEARCH_USE_MICROVM" => Some("1".to_string()),
        "KASTELLAN_WEB_SEARCH_USE_BROKER" => Some("1".to_string()),
        _ => None,
    };
    let exists = |_p: &Path| true;
    let allowlist = |_t: &str| Vec::<String>::new();
    let c = ctx(&get_env, &exists, &allowlist);
    match WebSearchManifest.resolve(&c) {
        Resolution::Register(entry) => {
            assert_eq!(
                entry.sandbox_backend,
                Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm)
            );
            // Broker declared, carrying the SearxNG endpoint it forwards to.
            let spec = entry.broker.as_ref().expect("broker set in VM broker mode");
            assert_eq!(spec.kind, crate::broker::BrokerKind::Search);
            assert_eq!(spec.endpoint, "http://127.0.0.1:8888/search");
            // Zero direct egress — empty allowlist.
            match &entry.policy.net {
                Net::Allowlist(hosts) => {
                    assert!(hosts.is_empty(), "broker VM worker must have no egress: {hosts:?}")
                }
                other => panic!("expected empty Net::Allowlist, got {other:?}"),
            }
            // No direct endpoint env leaked to the worker in broker mode.
            assert!(entry.policy.env.iter().all(|(k, _)| k != ENDPOINT_ENV));
            assert!(entry.policy.fs_read.is_empty());
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}

#[cfg(target_os = "linux")]
#[test]
fn resolve_vm_entry_still_injects_batch_cap() {
    // The batch cap must survive the VM short-circuit (it applies in the host path
    // after entry construction; the VM branch returns early, so it must thread it
    // too — else a batched search in the VM ignores the operator cap).
    let get_env = |k: &str| match k {
        BIN_ENV => Some("/opt/web-search".to_string()),
        ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
        "KASTELLAN_WEB_SEARCH_USE_MICROVM" => Some("1".to_string()),
        "KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES" => Some("5".to_string()),
        _ => None,
    };
    let exists = |_p: &Path| true;
    let allowlist = |_t: &str| Vec::<String>::new();
    let c = ctx(&get_env, &exists, &allowlist);
    match WebSearchManifest.resolve(&c) {
        Resolution::Register(entry) => {
            assert!(
                entry.policy.env.iter().any(|(k, v)| k == MAX_BATCH_QUERIES_ENV && v == "5"),
                "batch cap must be injected in VM mode: {:?}",
                entry.policy.env
            );
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}
