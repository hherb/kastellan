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
    fn resolve_registers_union_net_and_injects_env() {
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-research".to_string()),
            ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["searx.example.org".to_string(), ".docs.example.org".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                assert!(matches!(entry.policy.profile, Profile::WorkerNetClient));
                assert_eq!(entry.policy.cpu_ms, 15_000);
                assert_eq!(entry.policy.mem_mb, 512);
                assert_eq!(entry.wall_clock_ms, Some(60_000));
                assert!(entry.policy.fs_read.contains(&PathBuf::from("/etc/resolv.conf")));
                match &entry.policy.net {
                    Net::Allowlist(hosts) => {
                        // endpoint host:443 first, then content docs.example.org:443.
                        assert_eq!(hosts, &vec![
                            "searx.example.org:443".to_string(),
                            "docs.example.org:443".to_string(),
                        ]);
                    }
                    other => panic!("expected Net::Allowlist, got {other:?}"),
                }
                assert_eq!(entry.policy.env[0].0, ENDPOINT_ENV);
                assert_eq!(entry.policy.env[0].1, "https://searx.example.org/search");
                assert_eq!(entry.policy.env[1].0, "KASTELLAN_WEB_RESEARCH_ALLOWLIST");
                assert_eq!(entry.policy.env[1].1, r#"["searx.example.org",".docs.example.org"]"#);
                assert_eq!(entry.policy.env.len(), 2, "no embed env when endpoint unset");
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn resolve_unions_embed_endpoint_into_net_and_injects_env() {
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-research".to_string()),
            ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
            EMBED_ENDPOINT_ENV => Some("http://embed.example.org:11434/v1/embeddings".to_string()),
            _ => None, // EMBED_MODEL_ENV unset -> default model
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec![
            "searx.example.org".to_string(),
            "embed.example.org".to_string(),
            ".docs.example.org".to_string(),
        ];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                match &entry.policy.net {
                    Net::Allowlist(hosts) => {
                        assert!(hosts.iter().any(|h| h == "embed.example.org:11434"),
                            "embed host:port missing from net: {hosts:?}");
                    }
                    other => panic!("expected Net::Allowlist, got {other:?}"),
                }
                let has = |k: &str, v: &str| entry.policy.env.iter().any(|(ek, ev)| ek == k && ev == v);
                assert!(has(EMBED_ENDPOINT_ENV, "http://embed.example.org:11434/v1/embeddings"));
                assert!(has(EMBED_MODEL_ENV, "embeddinggemma"), "default model injected");
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn resolve_broker_mode_drops_embed_host_sets_spec_and_omits_endpoint_env() {
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-research".to_string()),
            ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
            EMBED_ENDPOINT_ENV => Some("http://127.0.0.1:11434/v1/embeddings".to_string()),
            USE_EMBED_BROKER_ENV => Some("1".to_string()),
            _ => None, // EMBED_MODEL_ENV unset -> default model
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| {
            vec![
                "searx.example.org".to_string(),
                ".docs.example.org".to_string(),
            ]
        };
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                // Broker declaration carries the kind + backend endpoint.
                let spec = entry.broker.as_ref().expect("broker set in broker mode");
                assert_eq!(spec.kind, crate::broker::BrokerKind::Embed);
                assert_eq!(spec.endpoint, "http://127.0.0.1:11434/v1/embeddings");
                // Embed host is NOT in the net allowlist (the backend is loopback,
                // but even a routable embed host must be absent — it leaves egress).
                match &entry.policy.net {
                    Net::Allowlist(hosts) => {
                        assert!(
                            hosts.iter().all(|h| !h.starts_with("127.0.0.1")),
                            "embed host must be absent from net: {hosts:?}"
                        );
                        assert_eq!(
                            hosts,
                            &vec![
                                "searx.example.org:443".to_string(),
                                "docs.example.org:443".to_string(),
                            ]
                        );
                    }
                    other => panic!("expected Net::Allowlist, got {other:?}"),
                }
                // The direct embed-endpoint env is NOT injected; the model IS.
                let has_key = |k: &str| entry.policy.env.iter().any(|(ek, _)| ek == k);
                assert!(!has_key(EMBED_ENDPOINT_ENV), "no direct embed endpoint env in broker mode");
                assert!(
                    entry
                        .policy
                        .env
                        .iter()
                        .any(|(k, v)| k == EMBED_MODEL_ENV && v == "embeddinggemma"),
                    "embed model env injected for the worker's BrokeredEmbedder"
                );
                // broker_uds is set at spawn, not the manifest.
                assert!(entry.policy.broker_uds.is_none());
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn resolve_broker_gate_without_embed_endpoint_is_direct() {
        // Gate on but no embed endpoint => nothing to broker => byte-identical to
        // the lexical-only direct entry (broker None, no broker net drop).
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-research".to_string()),
            ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
            USE_EMBED_BROKER_ENV => Some("1".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["searx.example.org".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                assert!(entry.broker.is_none(), "no broker without an embed endpoint");
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
        match WebResearchManifest.resolve(&c) {
            Resolution::Misconfigured { detail } => {
                assert!(detail.contains("kastellan-worker-web-research"), "detail: {detail}");
            }
            other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
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
    fn resolve_vm_broker_drops_embed_host_sets_vm_backend_and_broker_spec() {
        let get_env = |k: &str| match k {
            "KASTELLAN_WEB_RESEARCH_USE_MICROVM" => Some("1".to_string()),
            USE_EMBED_BROKER_ENV => Some("1".to_string()),
            ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
            EMBED_ENDPOINT_ENV => Some("http://127.0.0.1:11434/v1/embeddings".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist =
            |_t: &str| vec!["searx.example.org".to_string(), ".docs.example.org".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                // VM backend AND a broker spec (the two combined).
                assert!(matches!(
                    entry.sandbox_backend,
                    Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm)
                ));
                let spec = entry.broker.as_ref().expect("VM broker mode declares a broker spec");
                assert_eq!(spec.kind, crate::broker::BrokerKind::Embed);
                assert_eq!(spec.endpoint, "http://127.0.0.1:11434/v1/embeddings");
                // Embed host ABSENT from egress; VM fs_read empty.
                assert!(entry.policy.fs_read.is_empty(), "VM fs_read must be empty");
                match &entry.policy.net {
                    Net::Allowlist(hosts) => assert!(
                        hosts.iter().all(|h| !h.starts_with("127.0.0.1")),
                        "embed host must be absent from net: {hosts:?}"
                    ),
                    other => panic!("expected Net::Allowlist, got {other:?}"),
                }
                // Direct embed-endpoint env omitted; model present; broker_uds set at spawn.
                assert!(!entry.policy.env.iter().any(|(k, _)| k == EMBED_ENDPOINT_ENV));
                assert!(entry
                    .policy
                    .env
                    .iter()
                    .any(|(k, v)| k == EMBED_MODEL_ENV && v == "embeddinggemma"));
                assert!(entry.policy.broker_uds.is_none());
            }
            other => panic!("expected Register(VM broker entry), got {}", outcome_label(&other)),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_vm_without_broker_stays_direct_vm_entry() {
        // USE_MICROVM without the broker gate => the existing direct/degrade VM entry.
        let get_env = |k: &str| match k {
            "KASTELLAN_WEB_RESEARCH_USE_MICROVM" => Some("1".to_string()),
            ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["searx.example.org".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                assert!(matches!(
                    entry.sandbox_backend,
                    Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm)
                ));
                assert!(entry.broker.is_none(), "no broker without the gate + endpoint");
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_uses_microvm_entry_when_opted_in() {
        let get_env = |k: &str| match k {
            "KASTELLAN_WEB_RESEARCH_USE_MICROVM" => Some("1".to_string()),
            ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["searx.example.org".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);

        match WebResearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                assert!(matches!(
                    entry.sandbox_backend,
                    Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm)
                ));
                // In-rootfs binary path, not a host-discovered binary.
                assert_eq!(
                    entry.binary,
                    PathBuf::from("/usr/local/bin/kastellan-worker-web-research")
                );
                let env = &entry.policy.env;
                let dir = env.iter().find(|(k, _)| k == "KASTELLAN_MICROVM_DIR").map(|(_, v)| v.as_str());
                assert_eq!(dir, Some("/var/lib/kastellan/microvm"));
            }
            other => panic!("expected Register(VM entry), got {}", outcome_label(&other)),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn firecracker_entry_is_vm_with_empty_fs_read_and_forwarded_env() {
        let allowlist = vec!["searx.example.org".to_string(), ".docs.example.org".to_string()];
        let entry = web_research_firecracker_entry(
            PathBuf::from("/usr/local/bin/kastellan-worker-web-research"),
            "/var/lib/kastellan/microvm".to_string(),
            "https://searx.example.org:8888/search",
            Some("http://embed.example.org:11434/v1/embeddings"),
            None, // default model
            &allowlist,
        );
        // VM backend, net client, no host paths shared in (the CA is added at spawn).
        assert!(matches!(
            entry.sandbox_backend,
            Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm)
        ));
        assert!(matches!(entry.policy.profile, Profile::WorkerNetClient));
        assert!(entry.policy.fs_read.is_empty(), "VM fs_read must be empty (no NIC, no local DNS)");
        assert!(entry.policy.proxy_uds.is_none(), "proxy_uds is set at spawn, not in the manifest");
        assert_eq!(entry.policy.cpu_ms, 15_000);
        assert_eq!(entry.policy.mem_mb, 512);
        assert_eq!(entry.wall_clock_ms, Some(60_000));
        // Union Net::Allowlist: endpoint host:port, embed host:port, content host:443.
        match &entry.policy.net {
            Net::Allowlist(hosts) => {
                assert_eq!(hosts[0], "searx.example.org:8888", "endpoint host:port first");
                assert!(hosts.iter().any(|h| h == "embed.example.org:11434"), "embed host:port present: {hosts:?}");
                assert!(hosts.iter().any(|h| h == "docs.example.org:443"), "content host:443 present: {hosts:?}");
            }
            other => panic!("expected Net::Allowlist, got {other:?}"),
        }
        // Env forwards endpoint + verbatim allowlist + embed endpoint/model + the VM image dir + rootfs.
        let env = &entry.policy.env;
        let get = |k: &str| env.iter().find(|(ek, _)| ek == k).map(|(_, v)| v.as_str());
        assert_eq!(get(ENDPOINT_ENV), Some("https://searx.example.org:8888/search"));
        assert_eq!(get("KASTELLAN_WEB_RESEARCH_ALLOWLIST"), Some(r#"["searx.example.org",".docs.example.org"]"#));
        assert_eq!(get(EMBED_ENDPOINT_ENV), Some("http://embed.example.org:11434/v1/embeddings"));
        assert_eq!(get(EMBED_MODEL_ENV), Some("embeddinggemma"), "default model forwarded");
        assert_eq!(get("KASTELLAN_MICROVM_DIR"), Some("/var/lib/kastellan/microvm"));
        assert_eq!(get("KASTELLAN_MICROVM_ROOTFS"), Some("web-research.ext4"));
    }

    #[test]
    fn resolve_forced_host_localhost_name_searxng_is_misconfigured() {
        // Host mode + force-routing flag + a `localhost`-NAME SearxNG endpoint,
        // no search-broker enabled (USE_SEARCH_BROKER unset): the proxy
        // range-denies what the name resolves to, so this config reaches nothing
        // (#452). With the flag it would register — see
        // `localhost_name_endpoint_with_search_broker_registers`.
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-research".to_string()),
            ENDPOINT_ENV => Some("http://localhost:8888/search".to_string()),
            "KASTELLAN_EGRESS_FORCE_ROUTING" => Some("1".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["localhost".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Misconfigured { detail } => {
                assert!(detail.contains("KASTELLAN_WEB_RESEARCH_ENDPOINT"), "detail: {detail}");
                assert!(detail.contains("127.0.0.1"), "literal-IP remedy missing: {detail}");
                // The worker fail-closes on an off-allowlist endpoint host
                // (the #428 lesson) and SchemeDenies http on non-loopback
                // hosts — the remedy must carry both caveats or it trades one
                // registered-but-dead config for another.
                assert!(detail.contains("tool_allowlists"), "allowlist caveat missing: {detail}");
                assert!(detail.contains("https://"), "https caveat missing: {detail}");
            }
            other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn resolve_forced_host_literal_loopback_searxng_still_registers() {
        // Option-A policy pin (2026-07-16 review): a LITERAL loopback SearxNG
        // endpoint is dialed via the egress proxy's allowlisted-literal
        // carve-out, so it works force-routed and must keep registering.
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-research".to_string()),
            ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
            "KASTELLAN_EGRESS_FORCE_ROUTING" => Some("1".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["127.0.0.1".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Register(_) => {}
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn resolve_forced_host_localhost_name_embed_only_warns_and_registers() {
        // A `localhost`-NAME *embed* endpoint under force-routing degrades
        // ranking but does not break the tool: warn-only, registration
        // proceeds and the entry is unchanged (#429).
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-research".to_string()),
            ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
            EMBED_ENDPOINT_ENV => Some("http://localhost:11434/v1/embeddings".to_string()),
            "KASTELLAN_EGRESS_FORCE_ROUTING" => Some("1".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist =
            |_t: &str| vec!["searx.example.org".to_string(), "localhost".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                // The embed env is still injected — the entry itself is unchanged
                // (the warning is a log line, not a policy change).
                assert!(entry.policy.env.iter().any(|(k, _)| k == EMBED_ENDPOINT_ENV));
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_microvm_localhost_name_searxng_is_misconfigured() {
        // A VM worker force-routes unconditionally: a `localhost`-name SearxNG
        // is dead in VM mode regardless of the host force-routing flag (#452).
        let get_env = |k: &str| match k {
            "KASTELLAN_WEB_RESEARCH_USE_MICROVM" => Some("1".to_string()),
            ENDPOINT_ENV => Some("http://localhost:8888/search".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["localhost".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Misconfigured { detail } => {
                assert!(detail.contains("KASTELLAN_WEB_RESEARCH_ENDPOINT"), "detail: {detail}");
            }
            other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_vm_embed_broker_does_not_rescue_localhost_name_searxng() {
        // The embed-broker carries only embed traffic — it must NOT exempt a
        // `localhost`-name SearxNG endpoint from the #452 guard.
        let get_env = |k: &str| match k {
            "KASTELLAN_WEB_RESEARCH_USE_MICROVM" => Some("1".to_string()),
            USE_EMBED_BROKER_ENV => Some("1".to_string()),
            ENDPOINT_ENV => Some("http://localhost:8888/search".to_string()),
            EMBED_ENDPOINT_ENV => Some("http://127.0.0.1:11434/v1/embeddings".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["localhost".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Misconfigured { .. } => {}
            other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn embed_warning_only_when_forced_unbrokered_and_localhost_name() {
        let localhost_name = Some("http://localhost:11434/v1/embeddings");
        assert!(embed_local_warning(true, false, localhost_name).is_some());
        // Not force-routed: the worker resolves localhost itself, no proxy.
        assert!(embed_local_warning(false, false, localhost_name).is_none());
        // Brokered: the embed-broker reaches the backend over its UDS.
        assert!(embed_local_warning(true, true, localhost_name).is_none());
        // A LITERAL loopback embed endpoint is dialed via the proxy's
        // allowlisted-literal carve-out (it is unioned into net_entries) —
        // never warn about a working config.
        let literal = Some("http://127.0.0.1:11434/v1/embeddings");
        assert!(embed_local_warning(true, false, literal).is_none());
        // Routable or unset endpoint: nothing to warn about.
        let routable = Some("http://embed.example.org:11434/v1/embeddings");
        assert!(embed_local_warning(true, false, routable).is_none());
        assert!(embed_local_warning(true, false, None).is_none());
    }

    #[test]
    fn embed_warning_names_env_remedies_and_allowlist_caveat() {
        let w = embed_local_warning(true, false, Some("http://localhost:11434/v1/embeddings"))
            .expect("should warn");
        assert!(w.contains("KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT"), "warning: {w}");
        assert!(w.contains("KASTELLAN_WEB_RESEARCH_USE_EMBED_BROKER=1"), "warning: {w}");
        assert!(w.contains("127.0.0.1"), "literal-IP remedy missing: {w}");
        // The worker validates the embed host against tool_allowlists and
        // fail-closes the whole worker when missing — the warning must say so
        // or its literal-IP remedy escalates a ranking degradation into a
        // dead tool.
        assert!(w.contains("tool_allowlists"), "allowlist caveat missing: {w}");
        assert!(w.contains("https://"), "https caveat missing: {w}");
    }

    // ------------------------------------------------------------------
    // #464 — search-broker (single-broker XOR) resolve behaviour
    // ------------------------------------------------------------------

    #[test]
    fn both_broker_flags_is_misconfigured() {
        // USE_SEARCH_BROKER=1 + USE_EMBED_BROKER=1 (+ an embed endpoint so the
        // embed flag would otherwise be effective) → Misconfigured naming both
        // envs and the single-broker rule (at most ONE broker socket per worker).
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-research".to_string()),
            ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
            EMBED_ENDPOINT_ENV => Some("https://embed.example.org:11434/v1/embeddings".to_string()),
            USE_SEARCH_BROKER_ENV => Some("1".to_string()),
            USE_EMBED_BROKER_ENV => Some("1".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist =
            |_t: &str| vec!["searx.example.org".to_string(), "embed.example.org".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Misconfigured { detail } => {
                assert!(detail.contains(USE_SEARCH_BROKER_ENV), "detail: {detail}");
                assert!(detail.contains(USE_EMBED_BROKER_ENV), "detail: {detail}");
                assert!(detail.contains("one broker socket"), "single-broker rule missing: {detail}");
            }
            other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn search_broker_entry_has_no_searxng_egress_and_no_endpoint_env() {
        // USE_SEARCH_BROKER=1, loopback SearxNG endpoint, content allowlist: the
        // SearxNG host leaves egress and no endpoint env is injected; the broker
        // spec carries the SearxNG endpoint; content hosts stay in the allowlist.
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-research".to_string()),
            ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
            USE_SEARCH_BROKER_ENV => Some("1".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["127.0.0.1".to_string(), ".docs.example.org".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                let spec = entry.broker.as_ref().expect("broker set in search-broker mode");
                assert_eq!(spec.kind, crate::broker::BrokerKind::Search);
                assert_eq!(spec.endpoint, "http://127.0.0.1:8888/search");
                match &entry.policy.net {
                    Net::Allowlist(hosts) => {
                        // Match host:PORT — the #448 DGX lesson (SearxNG + embed may
                        // share 127.0.0.1; only the port distinguishes them).
                        assert!(
                            !hosts.iter().any(|h| h == "127.0.0.1:8888"),
                            "SearxNG host must be absent from net: {hosts:?}"
                        );
                        assert!(
                            hosts.iter().any(|h| h == "docs.example.org:443"),
                            "content host missing from net: {hosts:?}"
                        );
                    }
                    other => panic!("expected Net::Allowlist, got {other:?}"),
                }
                assert!(
                    entry.policy.env.iter().any(|(k, _)| k == "KASTELLAN_WEB_RESEARCH_ALLOWLIST"),
                    "allowlist env still injected"
                );
                assert!(
                    !entry.policy.env.iter().any(|(k, _)| k == ENDPOINT_ENV),
                    "endpoint env must be omitted in broker mode"
                );
                assert!(
                    !entry.policy.env.iter().any(|(k, _)| k == "KASTELLAN_SEARCH_BROKER_UDS"),
                    "broker UDS is injected at spawn, not by the manifest"
                );
                assert!(entry.policy.broker_uds.is_none(), "broker_uds set at spawn");
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn search_broker_entry_keeps_direct_embed() {
        // Same as above but a direct embed endpoint is set (no embed-broker flag):
        // the embed host stays in Net::Allowlist and the embed env is injected —
        // the search-broker choice leaves the embed path direct (the XOR trade-off).
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-research".to_string()),
            ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
            EMBED_ENDPOINT_ENV => Some("https://embed.example.org:11434/v1/embeddings".to_string()),
            USE_SEARCH_BROKER_ENV => Some("1".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| {
            vec![
                "127.0.0.1".to_string(),
                "embed.example.org".to_string(),
                ".docs.example.org".to_string(),
            ]
        };
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                assert_eq!(
                    entry.broker.as_ref().expect("broker set").kind,
                    crate::broker::BrokerKind::Search
                );
                match &entry.policy.net {
                    Net::Allowlist(hosts) => {
                        assert!(
                            hosts.iter().any(|h| h == "embed.example.org:11434"),
                            "direct embed host must stay in net: {hosts:?}"
                        );
                        assert!(
                            !hosts.iter().any(|h| h == "127.0.0.1:8888"),
                            "SearxNG host must be absent: {hosts:?}"
                        );
                    }
                    other => panic!("expected Net::Allowlist, got {other:?}"),
                }
                let has = |k: &str, v: &str| entry.policy.env.iter().any(|(ek, ev)| ek == k && ev == v);
                assert!(has(EMBED_ENDPOINT_ENV, "https://embed.example.org:11434/v1/embeddings"));
                assert!(has(EMBED_MODEL_ENV, "embeddinggemma"), "default embed model injected");
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn localhost_name_endpoint_with_search_broker_registers() {
        // Force-routing + a `localhost`-NAME SearxNG endpoint + USE_SEARCH_BROKER=1
        // → Register: the #452 guard must NOT fire (the broker holds the route).
        let exists = |_p: &Path| true;
        let allowlist =
            |_t: &str| vec!["searxng.localhost".to_string(), ".docs.example.org".to_string()];

        let with_broker = |k: &str| match k {
            BIN_ENV => Some("/opt/web-research".to_string()),
            ENDPOINT_ENV => Some("http://searxng.localhost:8888/search".to_string()),
            "KASTELLAN_EGRESS_FORCE_ROUTING" => Some("1".to_string()),
            USE_SEARCH_BROKER_ENV => Some("1".to_string()),
            _ => None,
        };
        match WebResearchManifest.resolve(&ctx(&with_broker, &exists, &allowlist)) {
            Resolution::Register(entry) => {
                assert_eq!(
                    entry.broker.as_ref().expect("broker set").kind,
                    crate::broker::BrokerKind::Search
                );
            }
            other => panic!("expected Register with search-broker, got {}", outcome_label(&other)),
        }

        // The SAME env WITHOUT the flag is Misconfigured today (#452) — contrast pin.
        let no_broker = |k: &str| match k {
            BIN_ENV => Some("/opt/web-research".to_string()),
            ENDPOINT_ENV => Some("http://searxng.localhost:8888/search".to_string()),
            "KASTELLAN_EGRESS_FORCE_ROUTING" => Some("1".to_string()),
            _ => None,
        };
        assert!(
            matches!(
                WebResearchManifest.resolve(&ctx(&no_broker, &exists, &allowlist)),
                Resolution::Misconfigured { .. }
            ),
            "localhost-name SearxNG force-routed without the broker must be Misconfigured"
        );
    }

    #[test]
    fn misconfigured_remedy_offers_search_broker() {
        // The forced-localhost Misconfigured detail (no broker flags) now also
        // offers the search-broker alongside the existing literal/https/row pins.
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-research".to_string()),
            ENDPOINT_ENV => Some("http://localhost:8888/search".to_string()),
            "KASTELLAN_EGRESS_FORCE_ROUTING" => Some("1".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["localhost".to_string()];
        match WebResearchManifest.resolve(&ctx(&get_env, &exists, &allowlist)) {
            Resolution::Misconfigured { detail } => {
                assert!(detail.contains(USE_SEARCH_BROKER_ENV), "search-broker remedy missing: {detail}");
                assert!(detail.contains("127.0.0.1"), "literal-IP remedy missing: {detail}");
                assert!(detail.contains("tool_allowlists"), "allowlist caveat missing: {detail}");
                assert!(detail.contains("https://"), "https caveat missing: {detail}");
            }
            other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_uses_vm_search_broker_entry_when_opted_in() {
        // USE_MICROVM=1 + USE_SEARCH_BROKER=1: FirecrackerVm backend, empty fs_read,
        // env carries KASTELLAN_MICROVM_DIR + ROOTFS=web-research.ext4, broker =
        // search(endpoint), Net::Allowlist has no SearxNG entry.
        let get_env = |k: &str| match k {
            "KASTELLAN_WEB_RESEARCH_USE_MICROVM" => Some("1".to_string()),
            USE_SEARCH_BROKER_ENV => Some("1".to_string()),
            ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["127.0.0.1".to_string(), ".docs.example.org".to_string()];
        match WebResearchManifest.resolve(&ctx(&get_env, &exists, &allowlist)) {
            Resolution::Register(entry) => {
                assert!(matches!(
                    entry.sandbox_backend,
                    Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm)
                ));
                assert!(entry.policy.fs_read.is_empty(), "VM entry has empty fs_read");
                let spec = entry.broker.as_ref().expect("broker set");
                assert_eq!(spec.kind, crate::broker::BrokerKind::Search);
                assert_eq!(spec.endpoint, "http://127.0.0.1:8888/search");
                let env = &entry.policy.env;
                assert!(env.iter().any(|(k, _)| k == "KASTELLAN_MICROVM_DIR"));
                assert!(
                    env.iter().any(|(k, v)| k == "KASTELLAN_MICROVM_ROOTFS" && v == "web-research.ext4"),
                    "rootfs env missing: {env:?}"
                );
                match &entry.policy.net {
                    Net::Allowlist(hosts) => assert!(
                        !hosts.iter().any(|h| h == "127.0.0.1:8888"),
                        "SearxNG host absent in VM search-broker mode: {hosts:?}"
                    ),
                    other => panic!("expected Net::Allowlist, got {other:?}"),
                }
            }
            other => panic!("expected Register(VM search-broker entry), got {}", outcome_label(&other)),
        }
    }

