//! End-to-end: a jailed `web-research` worker reaches SearxNG through a
//! core-spawned **trusted search-broker sidecar** over a bound UDS — with the
//! SearxNG host **removed from the worker's egress allowlist entirely** — while
//! still fetching content pages directly. The search XOR embed slice (#464), the
//! search-side twin of `embed_broker_egress_e2e.rs`.
//!
//! ## Two tests, two postures
//!
//! * `brokered_search_policy_has_broker_uds_and_zero_searxng_egress` (hermetic,
//!   always runs): pins the exact post-rewrite worker policy the live path
//!   depends on — the broker UDS is bound + injected as `KASTELLAN_SEARCH_BROKER_UDS`,
//!   no endpoint env is present, and the SearxNG host is absent from
//!   `Net::Allowlist` while the content host stays. Drives the **real**
//!   `worker_lifecycle::force_route::rewrite_policy_for_broker`, so it guards the
//!   production manifest + rewrite pair against drift (not a local replica).
//! * `brokered_web_research_vm_returns_results_with_zero_egress` (`#[ignore]`,
//!   Linux/DGX-only, live): the real wire. Drives the real
//!   `SingleUseLifecycle::acquire` for a VM web-research worker (mirrors the #451
//!   web-search manager-level test): the VM worker reaches a live loopback SearxNG
//!   ONLY over vsock 1026 to the host search-broker; asserts a parseable research
//!   result AND that the SearxNG host:port never appears in a worker egress
//!   decision (zero direct search egress).
//!
//! `[SKIP]`s cleanly (never fails) when PG, the supervisor, a worker/broker
//! binary, or a working sandbox is missing — same posture as the sibling e2es.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;

use kastellan_core::broker::BrokerKind;
use kastellan_core::worker_lifecycle::force_route::rewrite_policy_for_broker;
use kastellan_core::workers::web_research::web_research_search_broker_entry;
use kastellan_sandbox::Net;

/// Extract the `host:port` authority from a URL, matching how `net_entries`
/// records allowlist entries (so the "SearxNG host absent" check compares like
/// with like). Falls back to the host alone if no explicit port.
fn url_authority(endpoint: &str) -> String {
    let parsed = url::Url::parse(endpoint).expect("parse endpoint URL");
    let host = parsed.host_str().expect("endpoint has a host").to_string();
    match parsed.port() {
        Some(p) => format!("{host}:{p}"),
        None => host,
    }
}

#[test]
fn brokered_search_policy_has_broker_uds_and_zero_searxng_egress() {
    // Hermetic: no spawn, no network — assert the shape of the policy production's
    // `spawn_worker_with_optional_broker` hands the jailed worker in search-broker
    // mode. Endpoint is a loopback literal, but that is irrelevant here: in broker
    // mode the worker never dials it (the broker holds the route).
    let worker = PathBuf::from("/nonexistent/kastellan-worker-web-research");
    let searx = "http://127.0.0.1:8888/search";
    let content = vec!["docs.example.org".to_string()];

    let entry = web_research_search_broker_entry(worker, searx, None, None, &content);

    // The manifest declares the broker backend but leaves the UDS unset — core
    // fills it at spawn time. Simulate that rewrite with a placeholder socket.
    let uds = PathBuf::from("/tmp/search-brokered-test/search.sock");
    let policy = rewrite_policy_for_broker(entry.policy, &uds, BrokerKind::Search);

    // (1) The broker socket is bound into the jail …
    assert_eq!(
        policy.broker_uds.as_deref(),
        Some(uds.as_path()),
        "broker UDS must be bound into the jail"
    );
    // … and (2) `KASTELLAN_SEARCH_BROKER_UDS` (what makes the worker's
    // `choose_search_provider` pick `BrokeredSearchProvider`) is injected with the
    // same path.
    let injected = policy
        .env
        .iter()
        .find(|(k, _)| k == BrokerKind::Search.uds_env())
        .map(|(_, v)| v.as_str());
    assert_eq!(
        injected,
        Some(uds.to_string_lossy().as_ref()),
        "KASTELLAN_SEARCH_BROKER_UDS must be injected pointing at the bound socket"
    );

    // (3) No endpoint env in broker mode — the worker reaches SearxNG only over the
    // UDS, never the endpoint.
    assert!(
        !policy
            .env
            .iter()
            .any(|(k, _)| k == "KASTELLAN_WEB_RESEARCH_ENDPOINT"),
        "broker mode must omit the endpoint env"
    );
    // … and no embed-broker UDS (this is the search broker, not the embed one).
    assert!(
        !policy
            .env
            .iter()
            .any(|(k, _)| k == "KASTELLAN_EMBED_BROKER_UDS"),
        "search-broker mode must not inject the embed-broker UDS"
    );

    // (4) The core #464 property: the SearxNG host is ABSENT from the worker's
    // egress allowlist (a compromised worker cannot reach SearxNG directly — its
    // only path is the trusted broker's UDS), while the content host stays (the
    // worker still fetches pages directly). Match host:PORT — the #448 DGX lesson:
    // SearxNG + embed can share 127.0.0.1; only the port distinguishes them.
    let searx_authority = url_authority(searx);
    match &policy.net {
        Net::Allowlist(entries) => {
            assert!(
                !entries.iter().any(|e| e == &searx_authority),
                "SearxNG host {searx_authority:?} must NOT be in the worker's \
                 Net::Allowlist (broker mode drops it); got {entries:?}"
            );
            assert!(
                entries.iter().any(|e| e == "docs.example.org:443"),
                "content host must stay in the worker's Net::Allowlist; got {entries:?}"
            );
        }
        other => panic!("expected Net::Allowlist in broker mode, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Linux/DGX-only live VM × search-broker test. Gated per-item so the file's
// hermetic pin above still compiles + runs on macOS with no unused imports.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
use std::sync::{Arc, Mutex};

#[cfg(target_os = "linux")]
use kastellan_core::broker::{BrokerConfig, BrokerConfigs};
#[cfg(target_os = "linux")]
use kastellan_core::egress::audit::EgressAuditRow;
#[cfg(target_os = "linux")]
use kastellan_core::secrets::Vault;
#[cfg(target_os = "linux")]
use kastellan_core::tool_host::dispatch;
#[cfg(target_os = "linux")]
use kastellan_core::worker_lifecycle::force_route::{DecisionSinkFactory, ForceRoutingConfig};
#[cfg(target_os = "linux")]
use kastellan_core::worker_lifecycle::{SingleUseLifecycle, WorkerLifecycleManager};
#[cfg(target_os = "linux")]
use kastellan_core::workers::web_research::web_research_firecracker_search_broker_entry;
#[cfg(target_os = "linux")]
use kastellan_sandbox::linux_firecracker::{FirecrackerImage, LinuxFirecracker};
#[cfg(target_os = "linux")]
use kastellan_sandbox::SandboxBackends;
#[cfg(target_os = "linux")]
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, skip_if_sandbox_unavailable,
    unique_suffix, workspace_target_binary,
};

/// Default live SearxNG for the broker test (loopback; reached only via the broker).
#[cfg(target_os = "linux")]
const DEFAULT_SEARX_ENDPOINT: &str = "http://127.0.0.1:8888/search";

#[cfg(target_os = "linux")]
fn image_dir() -> String {
    std::env::var("KASTELLAN_MICROVM_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "/var/lib/kastellan/microvm".to_string())
}

#[cfg(target_os = "linux")]
fn firecracker_image() -> FirecrackerImage {
    let dir = PathBuf::from(image_dir());
    FirecrackerImage {
        kernel_path: dir.join("vmlinux"),
        rootfs_path: dir.join("web-research.ext4"),
    }
}

#[cfg(target_os = "linux")]
fn locate_microvm_run() -> Option<PathBuf> {
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("core has a workspace parent")
        .join("target");
    for profile in ["release", "debug"] {
        let p = target.join(profile).join("kastellan-microvm-run");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Skip unless a bootable web-research micro-VM is available. Also prepends the
/// `kastellan-microvm-run` build dir to PATH (the Firecracker backend spawns the
/// launcher by bare name; it is off the default SSH PATH — see the memory note
/// `firecracker-e2e-stale-release-launcher`). Idempotent via `Once`.
#[cfg(target_os = "linux")]
fn skip_if_no_microvm() -> bool {
    if let Err(e) = LinuxFirecracker::probe(&firecracker_image()) {
        eprintln!("\n[SKIP] firecracker probe failed (need web-research.ext4 + KVM + vsock): {e}\n");
        return true;
    }
    match locate_microvm_run() {
        Some(bin) => {
            use std::sync::Once;
            static PATH_ONCE: Once = Once::new();
            PATH_ONCE.call_once(|| {
                let dir = bin.parent().unwrap().to_path_buf();
                let cur = std::env::var_os("PATH").unwrap_or_default();
                let mut paths = vec![dir];
                paths.extend(std::env::split_paths(&cur));
                let joined = std::env::join_paths(paths).expect("join PATH");
                std::env::set_var("PATH", joined);
            });
            false
        }
        None => {
            eprintln!("\n[SKIP] kastellan-microvm-run not built; run `cargo build --release -p kastellan-microvm-run`\n");
            true
        }
    }
}

#[cfg(target_os = "linux")]
async fn probe_and_pool(conn_spec: &kastellan_db::conn::ConnectSpec) -> sqlx::PgPool {
    kastellan_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "web-research-search-broker-e2e"}),
    )
    .await
    .expect("probe run");
    kastellan_db::pool::connect_runtime_pool(conn_spec)
        .await
        .expect("connect runtime pool")
}

/// Bare host of a URL (for the zero-egress absence check).
#[cfg(target_os = "linux")]
fn url_host(endpoint: &str) -> String {
    url::Url::parse(endpoint)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_else(|| "127.0.0.1".to_string())
}

#[cfg(target_os = "linux")]
fn egress_proxy_bin_or_skip() -> Option<PathBuf> {
    let p = workspace_target_binary("kastellan-worker-egress-proxy");
    if p.is_file() {
        Some(p)
    } else {
        eprintln!("[SKIP] egress-proxy not built; run `cargo build -p kastellan-worker-egress-proxy`");
        None
    }
}

/// Live manager-level proof (#451 pattern, search-broker flavour): a VM
/// web-research worker acquired through the real `SingleUseLifecycle::acquire`
/// reaches a live loopback SearxNG ONLY over vsock 1026 to the host search-broker,
/// with the SearxNG host absent from its egress allowlist. Content pages ride the
/// worker's own (host MITM) egress. Asserts a parseable research result AND the
/// SearxNG host:port absent from egress decisions.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "DGX-only: real KVM + vsock + web-research rootfs + egress proxy + \
            search-broker + live SearxNG. Drives SingleUseLifecycle::acquire for a \
            VM web-research worker; asserts a real result with zero direct search egress."]
async fn brokered_web_research_vm_returns_results_with_zero_egress() {
    if skip_if_no_microvm() || skip_if_no_supervisor() || skip_if_sandbox_unavailable() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let Some(proxy_bin) = egress_proxy_bin_or_skip() else {
        return;
    };
    let broker_bin = workspace_target_binary("kastellan-worker-search-broker");
    if !broker_bin.exists() {
        eprintln!("\n[SKIP] search-broker binary not built; run cargo build --workspace\n");
        return;
    }

    let searx_endpoint = std::env::var("KASTELLAN_WEB_RESEARCH_ENDPOINT")
        .unwrap_or_else(|_| DEFAULT_SEARX_ENDPOINT.to_string());
    // Content allowlist: one live content host. NOT the SearxNG host (it rides the
    // broker) and NOT an embed host (this test runs lexical — no embed backend).
    let content_allowlist = vec!["en.wikipedia.org".to_string()];

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "wrsb-d",
        "wrsb-l",
        &format!("kastellan-supervisor-test-pg-webresearchsearchbroker-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    // VM × search-broker manifest entry (sandbox_backend = Some(FirecrackerVm),
    // broker = Some(Search), SearxNG host dropped from Net::Allowlist, content host
    // kept). No embed endpoint → lexical ranking.
    let entry = web_research_firecracker_search_broker_entry(
        PathBuf::from("/usr/local/bin/kastellan-worker-web-research"),
        image_dir(),
        &searx_endpoint,
        None, // no direct embed endpoint
        None, // default embed model (unused without an embed endpoint)
        &content_allowlist,
    );

    // Capture every egress decision so we can assert zero direct SearxNG egress.
    let decisions: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_src = Arc::clone(&decisions);
    let make_sink: DecisionSinkFactory = Box::new(move || {
        let d = Arc::clone(&sink_src);
        Box::new(move |row: EgressAuditRow| {
            d.lock().unwrap().push(format!("{} {}", row.action, row.payload));
        })
    });
    let force = Arc::new(ForceRoutingConfig::new(
        proxy_bin,
        std::env::temp_dir(),
        make_sink,
        None, // no cert pins
    ));

    // Real host search-broker config (scratch under /tmp so the VMM jail can bind
    // its UDS and the vsock-1026 relay can reach it).
    let broker_configs = BrokerConfigs {
        search: Some(Arc::new(BrokerConfig::new(
            BrokerKind::Search,
            broker_bin,
            std::env::temp_dir(),
        ))),
        ..Default::default()
    };

    // The real production manager: resolves the worker backend from
    // entry.sandbox_backend (FirecrackerVm) AND the sidecar/broker backend from
    // resolve(None, None) (host bwrap) — the #448/#451 behaviour, for web-research.
    let sandboxes = Arc::new(SandboxBackends::default_for_current_os());
    let mgr = SingleUseLifecycle::with_force_routing(sandboxes, Some(force), broker_configs);

    let mut handle = mgr
        .acquire("web-research", &entry)
        .await
        .expect("acquire a force-routed VM web-research worker through the manager");

    let result = dispatch(
        &pool,
        &Vault::new(),
        handle.worker_mut(),
        "web-research",
        "web.research",
        serde_json::json!({"query": "rust programming language", "max_sources": 2}),
    )
    .await
    .expect("web.research round trip through the daemon-managed VM worker");

    for line in decisions.lock().unwrap().iter() {
        eprintln!("[egress-decision] {line}");
    }

    // Payoff: a real research result even though the worker never had a direct
    // route to SearxNG — the search rode the broker over vsock 1026. Content hosts
    // are live-internet, so don't over-pin counts: assert the object parses and its
    // ranking is one of the two legal values (no embed backend here → "lexical").
    assert!(
        result.get("sources").is_some() && result.get("unfetched").is_some(),
        "expected a research result object with sources/unfetched; got {result:?}"
    );
    let ranking = result["ranking"].as_str().unwrap_or("");
    assert!(
        ranking == "hybrid" || ranking == "lexical",
        "ranking must be hybrid or lexical; got {ranking:?} ({result:?})"
    );

    // Zero direct search egress: the SearxNG host:port must never appear in a worker
    // egress decision. Match host AND port (loopback host is shared on the DGX; only
    // the port distinguishes SearxNG).
    let searx_url = url::Url::parse(&searx_endpoint).ok();
    let searx_host = url_host(&searx_endpoint);
    // `port_or_known_default` (not `port`) so a scheme-default endpoint (e.g.
    // `http://host/search`, implicit :80) yields the real port rather than the
    // 8888 fallback — otherwise the needle would check the wrong port and the
    // zero-egress assertion could silently pass. #464 review.
    let searx_port = searx_url
        .as_ref()
        .and_then(url::Url::port_or_known_default)
        .unwrap_or(8888);
    let host_needle = format!("\"host\":\"{searx_host}\"");
    let port_needle = format!("\"port\":{searx_port}");
    let leaked: Vec<_> = decisions
        .lock()
        .unwrap()
        .iter()
        .filter(|d| d.contains(&host_needle) && d.contains(&port_needle))
        .cloned()
        .collect();
    assert!(
        leaked.is_empty(),
        "SearxNG {searx_host}:{searx_port} must be absent from worker egress decisions \
         (search must ride the broker); leaked: {leaked:?}"
    );

    let _ = handle.worker_mut().kill();
    pool.close().await;
}
