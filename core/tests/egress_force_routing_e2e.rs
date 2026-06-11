//! End-to-end acceptance for egress slice #2 **live force-routing** (Task 4.6):
//! the host-side coupling `core::egress::net_worker::spawn_forced_net_worker`
//! brings up a per-worker egress-proxy sidecar under the real platform sandbox,
//! force-routes the worker onto it (private netns / Seatbelt deny-outbound +
//! the bound proxy UDS), and tears the pair down 1:1.
//!
//! This is the live twin of the lower-level probes:
//!   - `sandbox/tests/linux_force_routing.rs` proves the kernel barrier with a
//!     hand-built policy via the raw backend;
//!   - `core/tests/egress_proxy_e2e.rs` proves the proxy's allow/block/audit via
//!     `spawn_sidecar` + a host CONNECT client.
//! Here we go through the **production coupling** (`spawn_forced_net_worker` —
//! the path the Task 4.4 auto-flip wires into the live worker-spawn sites) and
//! assert, end to end:
//!   (a) an allowlisted loopback origin round-trips through the coupling's
//!       sidecar;
//!   (c) an off-allowlist CONNECT is blocked with `403`;
//!       + each decision reaches the bundle's `on_decision` ingest sink;
//!       + dropping the worker tears the sidecar down 1:1 (its UDS stops serving);
//!   (b) a force-routed worker's private netns has **no direct route** (Linux);
//!   (d) the live `pg_decision_sink` persists decisions to `audit_log` (PG-gated).
//!
//! `[SKIP]`s cleanly when the sandbox / proxy binary / Postgres are missing —
//! same skip-as-pass posture as `egress_proxy_e2e.rs`. Compiled on Linux + macOS
//! (the coupling is cross-platform); the no-direct-route assertion is Linux-only
//! (`getent` + netns semantics; the macOS Seatbelt twin is `seatbelt_uds_probe`).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kastellan_core::egress::net_worker::spawn_forced_net_worker;
use kastellan_core::tool_host::WorkerSpec;

/// The sidecar binds its UDS at `<scratch>/egress.sock` (the crate-private
/// `egress::spawn::UDS_FILE_NAME`, not reachable from this integration test).
const UDS_FILE_NAME: &str = "egress.sock";
use kastellan_sandbox::{Net, SandboxPolicy};
use kastellan_tests_common::{
    backend, skip_if_sandbox_unavailable, unique_suffix, workspace_target_binary,
};

/// Locate the built proxy binary; `[SKIP]` if absent (mirrors `egress_proxy_e2e`).
fn proxy_binary_or_skip() -> Option<PathBuf> {
    let p = workspace_target_binary("kastellan-worker-egress-proxy");
    p.exists().then_some(p)
}

/// `spawn_forced_net_worker` mints a unique `egress-<pid>-<seq>/` subdir under
/// the scratch root and the sidecar binds `<that>/egress.sock` in it. Exactly
/// one such subdir exists per spawn here, so we resolve the UDS by finding it.
fn minted_uds(scratch_root: &Path) -> PathBuf {
    let sub = std::fs::read_dir(scratch_root)
        .expect("read scratch root")
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().starts_with("egress-"))
        .expect("force-routed spawn must mint an egress-* scratch subdir");
    sub.path().join(UDS_FILE_NAME)
}

/// Create a short `/tmp`-based scratch root and return it. Short on purpose:
/// `spawn_forced_net_worker` nests `<root>/egress-<pid>-<seq>/egress.sock`, and
/// that projected UDS path must fit the 104-byte macOS `sockaddr_un.sun_path`
/// (the default `$TMPDIR` on macOS is ~50 chars deep and overflows once nested).
/// `/tmp` exists on both Linux and macOS.
fn short_scratch_root(tag: &str) -> PathBuf {
    let root = PathBuf::from("/tmp").join(format!("kfr-{tag}"));
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// A minimal force-routable worker policy: `Net::Allowlist` (the only net mode
/// the auto-flip force-routes) over the given allowlist. The coupling rewrites
/// it onto the sidecar UDS before spawn.
fn allowlist_policy(hosts: &[&str]) -> SandboxPolicy {
    SandboxPolicy {
        net: Net::Allowlist(hosts.iter().map(|s| s.to_string()).collect()),
        cpu_ms: 5_000,
        ..SandboxPolicy::default()
    }
}

/// Read the proxy's full CONNECT 200 response head (39 bytes — same length the
/// sibling `egress_proxy_e2e` pins) so subsequent reads see only tunnelled bytes.
fn assert_connect_established(client: &mut UnixStream) {
    let mut head = [0u8; 39];
    client.read_exact(&mut head).expect("read CONNECT 200 head");
    assert!(
        std::str::from_utf8(&head).unwrap().starts_with("HTTP/1.1 200"),
        "expected a 200 tunnel head, got {:?}",
        std::str::from_utf8(&head)
    );
}

/// (a) + (c) + ingest + 1:1 teardown, all through the production coupling.
#[test]
fn forced_coupling_enforces_allowlist_and_ingests_decisions() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let Some(proxy) = proxy_binary_or_skip() else {
        eprintln!("[SKIP] egress-proxy binary not built");
        return;
    };

    // A loopback origin that echoes a token once the client writes (proves the
    // tunnel carries real bytes, not just a 200 head).
    let origin = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin_port = origin.local_addr().unwrap().port();
    let origin_thread = std::thread::spawn(move || {
        if let Ok((mut s, _)) = origin.accept() {
            let mut buf = [0u8; 8];
            let _ = s.read(&mut buf);
            let _ = s.write_all(b"PONG");
        }
    });

    // Use a short `/tmp`-based root: the force-routing nesting
    // `<root>/egress-<pid>-<seq>/egress.sock` must still fit the 104-byte
    // sockaddr_un.sun_path, and macOS's default `$TMPDIR` (~50 chars deep) would
    // overflow once nested. `/tmp` exists on both Linux and macOS.
    let scratch_root = short_scratch_root(&format!("enf-{}", unique_suffix()));

    // Capture every decision the bundle's ingest thread maps from proxy stdout.
    let actions = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = {
        let actions = Arc::clone(&actions);
        move |row: kastellan_core::egress::audit::EgressAuditRow| {
            actions.lock().unwrap().push(row.action);
        }
    };

    let policy = allowlist_policy(&["127.0.0.1"]);
    let spec = WorkerSpec {
        policy: &policy,
        // A long-lived program keeps the worker (and so the sidecar) up while we
        // drive the proxy from the host. The worker itself doesn't use the UDS
        // here — assertion (b) covers the in-jail no-route path separately.
        program: "/usr/bin/sleep",
        args: &["30"],
        wall_clock_ms: None,
    };
    let backend = backend();
    let mut worker = spawn_forced_net_worker(
        backend.as_ref(),
        &proxy,
        &spec,
        &["127.0.0.1".to_string()],
        &scratch_root,
        "web-fetch",
        sink,
    )
    .expect("force-routed worker + sidecar spawn (fail-closed if the proxy is missing)");

    let uds = minted_uds(&scratch_root);

    // (a) Allowed loopback origin round-trips through the coupling's sidecar.
    let mut client = UnixStream::connect(&uds).expect("connect coupling UDS");
    write!(client, "CONNECT 127.0.0.1:{origin_port} HTTP/1.1\r\n\r\n").unwrap();
    assert_connect_established(&mut client);
    client.write_all(b"ping").unwrap();
    let mut echo = [0u8; 4];
    client.read_exact(&mut echo).unwrap();
    assert_eq!(&echo, b"PONG");
    drop(client);
    origin_thread.join().unwrap();

    // (c) Off-allowlist CONNECT is blocked at the boundary with 403.
    let mut bad = UnixStream::connect(&uds).expect("connect coupling UDS (blocked)");
    write!(bad, "CONNECT evil.test:443 HTTP/1.1\r\n\r\n").unwrap();
    let mut resp = String::new();
    let _ = bad.read_to_string(&mut resp);
    assert!(resp.starts_with("HTTP/1.1 403"), "expected 403, got {resp:?}");
    drop(bad);

    // The bundle's ingest thread fed both decisions to our on_decision sink.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        {
            let seen = actions.lock().unwrap();
            let allowed = seen.iter().any(|a| a == "egress.allowed");
            let blocked = seen.iter().any(|a| a == "egress.blocked.allowlist");
            if allowed && blocked {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "ingest sink never saw both decisions; observed {:?}",
            *actions.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // 1:1 teardown: dropping the worker drops its EgressSidecar, which kills the
    // proxy and removes the UDS — a fresh connect must then be refused.
    worker.kill().ok();
    drop(worker);
    let down_deadline = Instant::now() + Duration::from_secs(5);
    while UnixStream::connect(&uds).is_ok() {
        assert!(
            Instant::now() < down_deadline,
            "sidecar kept serving after the worker was dropped (teardown not 1:1)"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let _ = std::fs::remove_dir_all(&scratch_root);
}

/// (b) A force-routed worker's private netns has **no direct route**, proven
/// through the live coupling (not a hand-built policy). `getent hosts` needs DNS,
/// which needs a route; the worker has none but the proxy UDS, so it must fail.
/// Linux-only (`getent` + netns); the macOS twin is `seatbelt_uds_probe`.
#[cfg(target_os = "linux")]
#[test]
fn forced_coupling_worker_has_no_direct_route() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let Some(proxy) = proxy_binary_or_skip() else {
        eprintln!("[SKIP] egress-proxy binary not built");
        return;
    };

    let scratch_root = short_scratch_root(&format!("nr-{}", unique_suffix()));

    let policy = allowlist_policy(&["example.com:443"]);
    let spec = WorkerSpec {
        policy: &policy,
        program: "/usr/bin/getent",
        args: &["hosts", "example.com"],
        wall_clock_ms: None,
    };
    let backend = backend();
    let worker = spawn_forced_net_worker(
        backend.as_ref(),
        &proxy,
        &spec,
        &["example.com:443".to_string()],
        &scratch_root,
        "web-fetch",
        |_row| {},
    )
    .expect("force-routed getent worker + sidecar spawn");

    // `close()` waits for the worker to exit. getent in a private netns can't
    // resolve DNS (no route), so it exits non-zero.
    let status = worker.close().expect("wait force-routed getent worker");
    assert!(
        !status.success(),
        "FORCE-ROUTING LEAK via the live coupling: a force-routed worker reached DNS \
         directly — its only egress must be the proxy UDS. status={status:?}"
    );

    let _ = std::fs::remove_dir_all(&scratch_root);
}

/// (d) The live `pg_decision_sink` persists egress decisions to `audit_log`.
///
/// Exercises the sink closure exactly as the ingest thread drives it (one
/// `EgressAuditRow` per decision), then reads the rows back — proving the
/// proxy → core-stdout-ingest → `audit_log` pipeline the auto-flip relies on.
/// PG-gated: `[SKIP]`s when no Postgres bin dir is available.
#[test]
fn pg_decision_sink_persists_decisions_to_audit_log() {
    let Some(bin_dir) = kastellan_tests_common::pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = kastellan_tests_common::bring_up_pg_cluster(
        &bin_dir,
        "fr-d",
        "fr-l",
        &format!("kastellan-force-route-audit-pg-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build tokio runtime");

    let pool = rt.block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "force-route-audit-e2e"}),
        )
        .await
        .expect("probe");
        kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool")
    });

    // The live sink — same constructor the auto-flip's `from_env` builds.
    let mut sink = kastellan_core::egress::net_worker::pg_decision_sink(pool.clone(), rt.handle().clone());

    // Drive it with the two decision shapes the proxy emits.
    sink(kastellan_core::egress::audit::EgressAuditRow {
        actor: "egress_proxy",
        action: "egress.allowed".to_string(),
        payload: serde_json::json!({"worker": "web-fetch", "host": "127.0.0.1", "port": 8888}),
    });
    sink(kastellan_core::egress::audit::EgressAuditRow {
        actor: "egress_proxy",
        action: "egress.blocked.allowlist".to_string(),
        payload: serde_json::json!({"worker": "web-fetch", "host": "evil.test", "port": 443}),
    });

    let actions: Vec<String> = rt.block_on(async {
        let rows = kastellan_db::audit::fetch_since(&pool, 0, 1000)
            .await
            .expect("fetch audit rows");
        rows.into_iter().map(|r| r.action).collect()
    });

    assert!(
        actions.iter().any(|a| a == "egress.allowed"),
        "pg_decision_sink did not persist egress.allowed; got {actions:?}"
    );
    assert!(
        actions.iter().any(|a| a == "egress.blocked.allowlist"),
        "pg_decision_sink did not persist egress.blocked.allowlist; got {actions:?}"
    );

    rt.block_on(async { pool.close().await });
    // cluster dropped here — RAII teardown.
}
