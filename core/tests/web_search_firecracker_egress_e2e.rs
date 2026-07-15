#![cfg(target_os = "linux")]
//! web-search micro-VM e2e: web-search runs inside a Firecracker VM.
//!
//! Two DGX-only (`#[ignore]`) tests — both need real KVM + vsock + the web-search
//! rootfs (REBUILD via build-web-search-rootfs.sh) + the kastellan-microvm-run
//! RELEASE launcher:
//!
//! * `web_search_vm_reaches_proxy_with_ca_delivered` (DIRECT entry): a host
//!   UnixListener stub stands in for the egress proxy at the worker's proxy_uds; a
//!   force-routed web-search VM boots and one `web.search` is driven through it; we
//!   assert the stub RECEIVES the worker's `CONNECT <searxng-host>:<port>` line.
//!   The worker can only emit CONNECT after loading the in-guest CA, so this single
//!   assertion proves VM boot + force-routing + the vsock relay + CA delivery.
//!   (Mirror of web_research_firecracker_egress_e2e + web_fetch's single-CONNECT gate.)
//!
//! * `brokered_web_search_vm_returns_results_with_zero_egress` (VM x BROKER, live):
//!   drives the real `SingleUseLifecycle::acquire` daemon path — the manager
//!   resolves the VM worker backend from `entry.sandbox_backend = Some(FirecrackerVm)`
//!   and the host search-broker backend from `resolve(None, None)`. The VM worker
//!   holds an EMPTY egress allowlist and reaches a live loopback SearxNG only over
//!   vsock 1026 to the host search-broker; we assert a non-empty `results` array AND
//!   the SearxNG host:port never appears in a worker egress decision (zero direct
//!   egress). Needs a live SearxNG (e.g. 127.0.0.1:8888) + the egress-proxy +
//!   search-broker binaries.
//!
//! Run:
//!
//!     export PATH=$HOME/.local/bin:$PATH
//!     cargo build --release -p kastellan-microvm-run
//!     cargo build --workspace   # egress-proxy + search-broker host binaries
//!     bash scripts/workers/microvm/build-web-search-rootfs.sh
//!     cargo test -p kastellan-core --test web_search_firecracker_egress_e2e -- --ignored --nocapture

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use kastellan_core::broker::{BrokerConfig, BrokerConfigs, BrokerKind};
use kastellan_core::egress::audit::EgressAuditRow;
use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use kastellan_core::worker_lifecycle::force_route::{DecisionSinkFactory, ForceRoutingConfig};
use kastellan_core::worker_lifecycle::{SingleUseLifecycle, WorkerLifecycleManager};
use kastellan_core::workers::web_search::{
    web_search_firecracker_broker_entry, web_search_firecracker_entry,
};
use kastellan_sandbox::linux_firecracker::{FirecrackerImage, LinuxFirecracker};
use kastellan_sandbox::{SandboxBackend, SandboxBackendKind, SandboxBackends};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, skip_if_sandbox_unavailable,
    unique_suffix, workspace_target_binary,
};

/// SearxNG endpoint the DIRECT-entry VM worker searches first. The host part must
/// appear in the worker's CONNECT (host:port), so we pin a non-443 port to make the
/// assertion sharp.
const SEARXNG_ENDPOINT: &str = "https://searx.example.org:8888/search";
/// Default live SearxNG for the broker test (loopback; reached only via the broker).
const DEFAULT_SEARX_ENDPOINT: &str = "http://127.0.0.1:8888/search";

fn image_dir() -> String {
    std::env::var("KASTELLAN_MICROVM_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "/var/lib/kastellan/microvm".to_string())
}

fn firecracker_image() -> FirecrackerImage {
    let dir = PathBuf::from(image_dir());
    FirecrackerImage { kernel_path: dir.join("vmlinux"), rootfs_path: dir.join("web-search.ext4") }
}

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

/// Skip unless a bootable web-search micro-VM is available. Also prepends the
/// `kastellan-microvm-run` build dir to PATH (the Firecracker backend spawns the
/// launcher by bare name; it is off the default SSH PATH — see the memory note
/// `firecracker-e2e-stale-release-launcher`). Idempotent via `Once`.
fn skip_if_no_microvm() -> bool {
    if let Err(e) = LinuxFirecracker::probe(&firecracker_image()) {
        eprintln!("\n[SKIP] firecracker probe failed (need web-search.ext4 + KVM + vsock): {e}\n");
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

fn firecracker_backend() -> Arc<dyn SandboxBackend> {
    SandboxBackends::default_for_current_os().resolve(Some(SandboxBackendKind::FirecrackerVm), None)
}

async fn probe_and_pool(conn_spec: &kastellan_db::conn::ConnectSpec) -> sqlx::PgPool {
    kastellan_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "web-search-firecracker-egress-e2e"}),
    )
    .await
    .expect("probe run");
    kastellan_db::pool::connect_runtime_pool(conn_spec)
        .await
        .expect("connect runtime pool")
}

/// Mint a self-signed CA PEM the in-VM worker trusts as KASTELLAN_EGRESS_PROXY_CA.
/// The worker's make_get fails closed on an unreadable/invalid CA, so a parseable
/// cert is required for it to build ProxyConnectGet and emit CONNECT at all.
fn write_test_ca(path: &std::path::Path) {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
    let key_pair = KeyPair::generate().expect("keypair");
    let mut params = CertificateParams::new(vec!["egress-proxy.test".to_string()]).expect("params");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let cert = params.self_signed(&key_pair).expect("self-signed");
    std::fs::write(path, cert.pem()).expect("write ca.pem");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "DGX-only: real KVM + vsock + web-search rootfs"]
async fn web_search_vm_reaches_proxy_with_ca_delivered() {
    if skip_if_no_microvm() || skip_if_no_supervisor() || skip_if_sandbox_unavailable() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ws-d",
        "ws-l",
        &format!("kastellan-supervisor-test-pg-websearch-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    // Host scratch under /tmp (a share anchor); holds the stub proxy UDS + ca.pem.
    let dir = std::env::temp_dir().join(format!("kastellan-ws-vm-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let uds_path = dir.join("egress.sock");
    let ca_path = dir.join("ca.pem");
    let _ = std::fs::remove_file(&uds_path);
    write_test_ca(&ca_path);

    // Stub "proxy": on accept, read the first request line and report it back, then
    // send a fast 503 so the worker's request fails fast instead of blocking.
    let listener = UnixListener::bind(&uds_path).unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_ok() {
                let _ = tx.send(line.clone());
            }
            let mut w = stream;
            let _ = w.write_all(b"HTTP/1.1 503 stub\r\n\r\n");
        }
    });

    // Force-routed web-search DIRECT VM entry: set proxy_uds + the CA env + CA in
    // fs_read, exactly as rewrite_worker_policy does on the production path.
    //
    // The allowlist MUST include the SearxNG endpoint host: the worker's `from_env`
    // runs `validate_endpoint(endpoint, allowlist)` and fails closed (never serves)
    // if the endpoint host is off-allowlist — so without `searx.example.org` here
    // the worker would never search and never emit CONNECT. In production the
    // endpoint-derived allowlist carries it for the same reason.
    let mut entry = web_search_firecracker_entry(
        PathBuf::from("/usr/local/bin/kastellan-worker-web-search"),
        image_dir(),
        SEARXNG_ENDPOINT,
        &["searx.example.org".to_string()],
    );
    entry.policy.proxy_uds = Some(uds_path.clone());
    entry.policy.env.push((
        "KASTELLAN_EGRESS_PROXY_CA".into(),
        ca_path.to_string_lossy().into_owned(),
    ));
    entry.policy.fs_read.push(ca_path.clone());

    let backend = firecracker_backend();
    let program = entry.binary.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &entry.policy,
        program: &program,
        args: &[],
        wall_clock_ms: entry.wall_clock_ms,
    };
    let mut worker = spawn_worker(&*backend, &spec).expect("spawn web-search in micro-VM");

    // Drive one web.search on a background task; we only need it to attempt egress.
    let search = tokio::spawn(async move {
        let _ = dispatch(
            &pool,
            &Vault::new(),
            &mut worker,
            "web-search",
            "web.search",
            serde_json::json!({ "query": "hello world" }),
        )
        .await;
        (worker, pool)
    });

    let got = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("stub proxy never received the in-VM worker's CONNECT (transport or CA broken)");
    assert!(
        got.starts_with("CONNECT searx.example.org:8888"),
        "expected CONNECT searx.example.org:8888 (the SearxNG search), got {got:?}"
    );

    let (worker, pool) = search.await.expect("search task joins");
    let _ = worker.close();
    pool.close().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Bare host of a URL (for the zero-egress absence check).
fn url_host(endpoint: &str) -> String {
    url::Url::parse(endpoint)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_else(|| "127.0.0.1".to_string())
}

fn egress_proxy_bin_or_skip() -> Option<PathBuf> {
    let p = workspace_target_binary("kastellan-worker-egress-proxy");
    if p.is_file() {
        Some(p)
    } else {
        eprintln!("[SKIP] egress-proxy not built; run `cargo build -p kastellan-worker-egress-proxy`");
        None
    }
}

/// Live manager-level proof (#448 pattern): a VM web-search worker acquired through
/// the real `SingleUseLifecycle::acquire` reaches a live loopback SearxNG ONLY over
/// vsock 1026 to the host search-broker, with an EMPTY egress allowlist. Asserts a
/// non-empty `results` array AND the SearxNG host:port absent from egress decisions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "DGX-only: real KVM + vsock + web-search rootfs + egress proxy + \
            search-broker + live SearxNG. Drives SingleUseLifecycle::acquire for a \
            VM web-search worker; asserts real results with zero direct egress."]
async fn brokered_web_search_vm_returns_results_with_zero_egress() {
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

    let searx_endpoint = std::env::var("KASTELLAN_WEB_SEARCH_ENDPOINT")
        .unwrap_or_else(|_| DEFAULT_SEARX_ENDPOINT.to_string());

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "wsb-d",
        "wsb-l",
        &format!("kastellan-supervisor-test-pg-websearchbroker-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    // The VM broker-mode manifest entry (sandbox_backend = Some(FirecrackerVm),
    // broker = Some(Search), empty Net::Allowlist). Worker runs from the
    // rootfs-baked path; the broker is a host binary.
    let entry = web_search_firecracker_broker_entry(
        PathBuf::from("/usr/local/bin/kastellan-worker-web-search"),
        image_dir(),
        &searx_endpoint,
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

    // The real production manager. It resolves the worker backend from
    // entry.sandbox_backend (FirecrackerVm) AND the sidecar/broker backend from
    // resolve(None, None) (host bwrap) — the #448 behaviour, now for web-search.
    let sandboxes = Arc::new(SandboxBackends::default_for_current_os());
    let mgr = SingleUseLifecycle::with_force_routing(sandboxes, Some(force), broker_configs);

    let mut handle = mgr
        .acquire("web-search", &entry)
        .await
        .expect("acquire a force-routed VM web-search worker through the manager");

    let result = dispatch(
        &pool,
        &Vault::new(),
        handle.worker_mut(),
        "web-search",
        "web.search",
        serde_json::json!({"query": "rust programming language"}),
    )
    .await
    .expect("web.search round trip through the daemon-managed VM worker");

    for line in decisions.lock().unwrap().iter() {
        eprintln!("[egress-decision] {line}");
    }

    // Payoff: real results even though the worker had an EMPTY egress allowlist —
    // the search rode the broker over vsock 1026.
    let results = result["results"].as_array().expect("results must be an array");
    assert!(
        !results.is_empty(),
        "expected a non-empty results array via the broker (VM worker has zero egress); got {result:?}"
    );

    // Zero direct egress: the SearxNG host:port must never appear in a worker egress
    // decision. Match host AND port (loopback host is shared on the DGX; only the
    // port distinguishes SearxNG).
    let searx_url = url::Url::parse(&searx_endpoint).ok();
    let searx_host = url_host(&searx_endpoint);
    let searx_port = searx_url.as_ref().and_then(url::Url::port).unwrap_or(8888);
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
