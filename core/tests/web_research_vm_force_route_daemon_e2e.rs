#![cfg(target_os = "linux")]
//! #448 — live manager-level proof that the DAEMON force-routes a VM
//! `Net::Allowlist` worker through a HOST egress sidecar + HOST embed broker.
//!
//! Unlike `web_research_firecracker_broker_e2e.rs` (which hand-assembles a
//! `NetWorkerSpawn` with two explicit backends), this drives the real
//! `SingleUseLifecycle::with_force_routing(...).acquire(...)` path: the manager
//! itself resolves the worker backend from `entry.sandbox_backend =
//! Some(FirecrackerVm)` and the sidecar/broker backend from
//! `SandboxBackends::resolve(None, None)` (host default). It is strictly
//! stronger — it proves the daemon's own resolution, not a test fixture's.
//!
//! DGX-only (`#[ignore]`): real KVM + vsock + web-research rootfs + egress
//! proxy + embed broker + live SearxNG + live Ollama (embeddinggemma). Asserts
//! `ranking == "hybrid"` (embed rode vsock 1026 to the host broker) AND the
//! embed host never appears in an egress decision (zero embed egress).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use kastellan_core::broker::{BrokerConfig, BrokerConfigs, BrokerKind};
use kastellan_core::egress::audit::EgressAuditRow;
use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::dispatch;
use kastellan_core::worker_lifecycle::force_route::{DecisionSinkFactory, ForceRoutingConfig};
use kastellan_core::worker_lifecycle::{SingleUseLifecycle, WorkerLifecycleManager};
use kastellan_core::workers::web_research::web_research_firecracker_broker_entry;
use kastellan_sandbox::SandboxBackends;
use kastellan_tests_common::microvm::{image_dir, skip_if_no_microvm};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, skip_if_sandbox_unavailable,
    unique_suffix, workspace_target_binary,
};

/// The rootfs image this suite boots. Passed to the shared
/// `kastellan_tests_common::microvm` helpers, which own the `[SKIP]` wording,
/// the launcher discovery and the `KASTELLAN_MICROVM_DIR` lookup (issue #475).
const VM_ROOTFS: &str = "web-research.ext4";

/// Default SearxNG endpoint (loopback). In force-routed mode the egress proxy
/// reaches it via its literal-IP allowlist carve-out. Override with
/// `KASTELLAN_WEB_RESEARCH_ENDPOINT` if SearxNG lives on a routable host.
const DEFAULT_SEARX_ENDPOINT: &str = "http://127.0.0.1:8888/search";
/// Default embed backend (loopback Ollama). Reached ONLY by the host broker;
/// the worker never has it in egress.
const DEFAULT_EMBED_ENDPOINT: &str = "http://127.0.0.1:11434/v1/embeddings";


fn egress_proxy_bin_or_skip() -> Option<PathBuf> {
    let p = workspace_target_binary("kastellan-worker-egress-proxy");
    if p.is_file() {
        Some(p)
    } else {
        eprintln!("[SKIP] egress-proxy not built; run `cargo build -p kastellan-worker-egress-proxy`");
        None
    }
}

/// Bare host of a URL (for the content allowlist entry + the embed-absence check).
fn url_host(endpoint: &str) -> String {
    url::Url::parse(endpoint)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_else(|| "127.0.0.1".to_string())
}

async fn probe_and_pool(conn_spec: &kastellan_db::conn::ConnectSpec) -> sqlx::PgPool {
    kastellan_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "web-research-vm-force-route-daemon-e2e"}),
    )
    .await
    .expect("probe run");
    kastellan_db::pool::connect_runtime_pool(conn_spec)
        .await
        .expect("connect runtime pool")
}

/// Live full-stack via the daemon manager: a VM web-research worker acquired
/// through `SingleUseLifecycle::acquire` ranks `hybrid` by embedding through the
/// host broker over vsock 1026, while SearxNG + content ride the host MITM
/// egress sidecar over vsock 1025 — with the embed host absent from egress. The
/// manager (not a test fixture) selects the host sidecar/broker backend via
/// `resolve(None, None)` and the VM worker backend via `Some(FirecrackerVm)`.
/// Closes #448.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "DGX-only: real KVM + vsock + web-research rootfs + egress proxy + \
            embed broker + live SearxNG + live embeddinggemma. Drives the real \
            SingleUseLifecycle::acquire path for a VM web-research worker; \
            asserts hybrid ranking with the embed host absent from egress."]
async fn daemon_force_routes_vm_web_research_through_host_sidecar_and_broker() {
    if skip_if_no_microvm(VM_ROOTFS) || skip_if_no_supervisor() || skip_if_sandbox_unavailable() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let Some(proxy_bin) = egress_proxy_bin_or_skip() else {
        return;
    };

    // VM worker runs from the rootfs-baked path; the broker is a host binary.
    let worker_in_guest = "/usr/local/bin/kastellan-worker-web-research";
    let broker_bin = workspace_target_binary("kastellan-worker-embed-broker");
    if !broker_bin.exists() {
        eprintln!("\n[SKIP] embed-broker binary not built; run cargo build --workspace\n");
        return;
    }

    let searx_endpoint = std::env::var("KASTELLAN_WEB_RESEARCH_ENDPOINT")
        .unwrap_or_else(|_| DEFAULT_SEARX_ENDPOINT.to_string());
    let embed_endpoint = std::env::var("KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT")
        .unwrap_or_else(|_| DEFAULT_EMBED_ENDPOINT.to_string());

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "vmfr-d",
        "vmfr-l",
        &format!("kastellan-supervisor-test-pg-vmfr-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    // Content allowlist: SearxNG endpoint host + one content host. NOT the embed
    // host — its only path is the broker over vsock 1026.
    let allowlist = vec![url_host(&searx_endpoint), "en.wikipedia.org".to_string()];

    // The VM broker-mode manifest entry (sandbox_backend = Some(FirecrackerVm),
    // broker = Some(Embed), embed host absent from Net::Allowlist).
    let entry = web_research_firecracker_broker_entry(
        PathBuf::from(worker_in_guest),
        image_dir(),
        &searx_endpoint,
        &embed_endpoint,
        None, // default embed model (embeddinggemma)
        &allowlist,
    );

    // Capture every egress decision so we can assert zero embed egress.
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

    // Real host embed-broker config (scratch under /tmp so the VMM jail can bind
    // its UDS and the vsock-1026 relay can reach it).
    let broker_configs = BrokerConfigs {
        embed: Some(Arc::new(BrokerConfig::new(
            BrokerKind::Embed,
            broker_bin,
            std::env::temp_dir(),
        ))),
        ..Default::default()
    };

    // The real production manager. It resolves the worker backend from
    // entry.sandbox_backend (FirecrackerVm) AND the sidecar/broker backend from
    // resolve(None, None) (host bwrap) — the #448 behaviour under test.
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

    // Print decisions for diagnosability on failure.
    for line in decisions.lock().unwrap().iter() {
        eprintln!("[egress-decision] {line}");
    }

    assert_eq!(
        result["ranking"], "hybrid",
        "expected hybrid ranking via the host broker over vsock 1026 (embed host absent from egress)"
    );

    // Zero embed egress: the embed backend's host:port must never appear in an
    // egress decision. Match host AND port — on the loopback DGX setup SearxNG
    // (127.0.0.1:8888) shares the embed host (127.0.0.1:11434), so a bare-host
    // check would false-positive on SearxNG; only the embed PORT distinguishes it.
    let embed_url = url::Url::parse(&embed_endpoint).ok();
    let embed_host = embed_url
        .as_ref()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let embed_port = embed_url.as_ref().and_then(url::Url::port).unwrap_or(11434);
    let host_needle = format!("\"host\":\"{embed_host}\"");
    let port_needle = format!("\"port\":{embed_port}");
    let leaked: Vec<_> = decisions
        .lock()
        .unwrap()
        .iter()
        .filter(|d| d.contains(&host_needle) && d.contains(&port_needle))
        .cloned()
        .collect();
    assert!(
        leaked.is_empty(),
        "embed backend {embed_host}:{embed_port} must be absent from egress decisions; leaked: {leaked:?}"
    );

    let _ = handle.worker_mut().kill();
}
