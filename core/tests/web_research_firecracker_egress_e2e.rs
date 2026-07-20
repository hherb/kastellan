#![cfg(target_os = "linux")]
//! web-research micro-VM e2e: web-research runs inside a Firecracker VM and reaches
//! the host egress proxy over the slice-4a vsock channel.
//!
//! `web_research_vm_reaches_proxy_with_ca_delivered` (hermetic, no real network;
//! still #[ignore] DGX-only — needs real KVM + vsock + the rootfs): a host
//! UnixListener stub stands in for the egress proxy at the worker's proxy_uds; a
//! force-routed web-research VM boots and one `web.research` is driven through it;
//! we assert the stub RECEIVES the worker's `CONNECT <searxng-host>:<port>` line.
//! The worker's first egress is the SearxNG search, and it can only emit CONNECT
//! after loading the in-guest CA (make_get fails closed on an unreadable
//! KASTELLAN_EGRESS_PROXY_CA), so this single assertion proves VM boot +
//! force-routing + the vsock relay + CA delivery. Embed is not exercised (mirrors
//! the web-fetch single-CONNECT gate).
//!
//! DGX-only / #[ignore]: needs /dev/kvm + /dev/vhost-vsock + the web-research rootfs
//! (REBUILD via build-web-research-rootfs.sh) + the kastellan-microvm-run RELEASE
//! launcher. Run:
//!
//!     export PATH=$HOME/.local/bin:$PATH
//!     cargo build --release -p kastellan-microvm-run
//!     bash scripts/workers/microvm/build-web-research-rootfs.sh
//!     cargo test -p kastellan-core --test web_research_firecracker_egress_e2e -- --ignored --nocapture

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use kastellan_core::workers::web_research::web_research_firecracker_entry;
use kastellan_tests_common::microvm::{firecracker_backend, image_dir, skip_if_no_microvm};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, skip_if_sandbox_unavailable,
    unique_suffix,
};

/// The rootfs image this suite boots. Passed to the shared
/// `kastellan_tests_common::microvm` helpers, which own the `[SKIP]` wording,
/// the launcher discovery and the `KASTELLAN_MICROVM_DIR` lookup (issue #475).
const VM_ROOTFS: &str = "web-research.ext4";

/// SearxNG endpoint the VM worker searches first. The host part must appear in the
/// worker's CONNECT (host:port), so we pin a non-443 port to make the assertion sharp.
const SEARXNG_ENDPOINT: &str = "https://searx.example.org:8888/search";


async fn probe_and_pool(conn_spec: &kastellan_db::conn::ConnectSpec) -> sqlx::PgPool {
    kastellan_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "web-research-firecracker-egress-e2e"}),
    )
    .await
    .expect("probe run");
    kastellan_db::pool::connect_runtime_pool(conn_spec)
        .await
        .expect("connect runtime pool")
}

/// Mint a self-signed CA PEM the in-VM worker will trust as KASTELLAN_EGRESS_PROXY_CA.
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
#[ignore = "DGX-only: real KVM + vsock + web-research rootfs"]
async fn web_research_vm_reaches_proxy_with_ca_delivered() {
    if skip_if_no_microvm(VM_ROOTFS) {
        return;
    }
    if skip_if_no_supervisor() {
        return;
    }
    if skip_if_sandbox_unavailable() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "wr-d",
        "wr-l",
        &format!("kastellan-supervisor-test-pg-webresearch-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    // Host scratch under /tmp (a share anchor); holds the stub proxy UDS + ca.pem.
    let dir = std::env::temp_dir().join(format!("kastellan-wr-vm-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let uds_path = dir.join("egress.sock");
    let ca_path = dir.join("ca.pem");
    let _ = std::fs::remove_file(&uds_path);
    write_test_ca(&ca_path);

    // Stub "proxy": on accept, read the first request line and report it back,
    // then send a fast 503 so the worker's request fails fast instead of blocking.
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

    // Force-routed web-research VM entry: set proxy_uds + the CA env + CA in fs_read,
    // exactly as rewrite_worker_policy does on the production path. No embed endpoint.
    //
    // The allowlist MUST include the SearxNG endpoint host: the worker's `from_env`
    // runs `validate_endpoint(endpoint, allowlist)` and fails closed (never serves)
    // if the endpoint host is off-allowlist — so without `searx.example.org` here the
    // worker would never search and never emit CONNECT. In production the operator's
    // `tool_allowlists` row for web-research carries the endpoint host for the same
    // reason (it is what `net_entries` unions into the egress `Net::Allowlist`).
    let mut entry = web_research_firecracker_entry(
        PathBuf::from("/usr/local/bin/kastellan-worker-web-research"),
        image_dir(),
        SEARXNG_ENDPOINT,
        None,
        None,
        &["searx.example.org".to_string(), "example.com".to_string()],
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
    let mut worker = spawn_worker(&*backend, &spec).expect("spawn web-research in micro-VM");

    // Drive one web.research on a background task; we only need it to make the worker
    // attempt egress (the SearxNG search). The assertion is the stub receiving CONNECT.
    let research = tokio::spawn(async move {
        let _ = dispatch(
            &pool,
            &Vault::new(),
            &mut worker,
            "web-research",
            "web.research",
            serde_json::json!({ "query": "hello world", "max_sources": 1 }),
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

    let (worker, pool) = research.await.expect("research task joins");
    let _ = worker.close();
    pool.close().await;
    let _ = std::fs::remove_dir_all(&dir);
}
