//! Cross-platform hermetic e2e (Seatbelt on macOS, bwrap on Linux) for slice 5c's
//! transparent-tunnel long-lived net worker WITHOUT a VM: a net-demo worker under
//! `PersistentWorker`, force-routed through a real transparent-tunnel egress
//! sidecar to a loopback self-signed TLS origin. Proves end-to-end TLS through the
//! tunnel, many-calls-one-boot, and net.crash → 1:1 sidecar+worker respawn.
//!
//! Skip-as-pass if the net-demo / egress-proxy binaries are not built or the
//! default OS sandbox is unavailable.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kastellan_core::egress::persistent_net::{spawn_net_transport, NetTransportSpawn};
use kastellan_core::worker_lifecycle::{PersistentFactory, PersistentTransport, PersistentWorker};
use kastellan_sandbox::{Net, Profile, SandboxBackends, SandboxPolicy};

/// Locate a built worker binary under the workspace target dir; `None` → skip.
fn target_bin(name: &str) -> Option<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.parent().unwrap().join("target"));
    let bin = target.join("debug").join(name);
    if bin.exists() {
        Some(bin)
    } else {
        None
    }
}

/// Loopback self-signed rustls TLS origin — the server-spawn is shared with the
/// Firecracker e2e via [`kastellan_tests_common::tls_origin`] (#390); this module
/// only keeps the hermetic test's distinct cert-PEM destination. The origin binds
/// `127.0.0.1:0`, replies `HTTP/1.1 204 No Content\r\n\r\n` to any request, and
/// runs on the HOST; the real egress-proxy sidecar dials `127.0.0.1:<port>`, so
/// the operator allowlist must carry that literal `127.0.0.1:<port>` (the proxy's
/// SSRF has a literal-IP carve-out for allowlisted addresses — see
/// `egress_force_routing_e2e.rs`).
mod origin {
    use std::path::PathBuf;

    /// Spawn the shared loopback TLS origin and write its cert PEM to a stable
    /// temp file — the worker's `extra_ca`, kept out of the per-spawn scratch so
    /// it survives respawn. Returns `(port, ca_pem_path)`.
    pub fn spawn_loopback_tls_origin() -> (u16, PathBuf) {
        let (port, cert_pem) =
            kastellan_tests_common::tls_origin::spawn_loopback_tls_origin();
        let ca_dir = std::env::temp_dir().join(format!(
            "kastellan-netdemo-5c-ca-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&ca_dir).expect("create ca dir");
        let ca_path = ca_dir.join("origin-ca.pem");
        std::fs::write(&ca_path, cert_pem.as_bytes()).expect("write origin ca");
        (port, ca_path)
    }
}

/// A short scratch root shared across all spawns. Short on purpose: the sidecar
/// binds `<scratch>/egress.sock`, and that path must fit the 104-byte macOS
/// `sockaddr_un.sun_path`. macOS's default `$TMPDIR` is ~50 chars deep and would
/// overflow once nested; `/tmp` exists on both Linux and macOS (same reasoning as
/// `egress_force_routing_e2e.rs::short_scratch_root`).
fn short_scratch_root() -> PathBuf {
    let root = PathBuf::from("/tmp").join(format!("knd5c-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn net_demo_tls_probe_survives_respawn_under_default_backend() {
    let net_demo = match target_bin("kastellan-worker-net-demo") {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] net-demo not built; run `cargo build -p kastellan-worker-net-demo`");
            return;
        }
    };
    let proxy_bin = match target_bin("kastellan-worker-egress-proxy") {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] egress-proxy not built; run `cargo build -p kastellan-worker-egress-proxy`");
            return;
        }
    };
    #[cfg(target_os = "linux")]
    {
        use kastellan_sandbox::linux_bwrap::LinuxBwrap;
        if let Err(e) = LinuxBwrap::probe() {
            eprintln!("[SKIP] bwrap probe failed: {e}");
            return;
        }
    }
    #[cfg(target_os = "macos")]
    {
        use kastellan_sandbox::macos_seatbelt::MacosSeatbelt;
        if let Err(e) = MacosSeatbelt::probe() {
            eprintln!("[SKIP] sandbox-exec probe failed: {e}");
            return;
        }
    }

    // Loopback self-signed origin; its cert becomes the worker's extra_ca. The
    // proxy dials 127.0.0.1:<port>, so the operator allowlist carries that
    // literal (the proxy's SSRF literal-IP carve-out permits it).
    let (origin_port, ca_path) = origin::spawn_loopback_tls_origin();
    let allow = vec![format!("127.0.0.1:{origin_port}")];

    let backends = SandboxBackends::default_for_current_os();
    let backend = backends.resolve(None, None);
    let scratch_root = short_scratch_root();

    // Unique per-spawn scratch counter so every spawn/respawn gets a fresh dir
    // (and a fresh sidecar UDS) — no stale-socket reuse across a respawn.
    let spawn_seq = std::sync::Arc::new(AtomicU64::new(0));

    let factory: PersistentFactory = {
        let net_demo = net_demo.clone();
        let proxy_bin = proxy_bin.clone();
        let ca_path = ca_path.clone();
        let allow = allow.clone();
        let scratch_root = scratch_root.clone();
        let backend = std::sync::Arc::clone(&backend);
        let spawn_seq = std::sync::Arc::clone(&spawn_seq);
        let bin_dir = net_demo.parent().unwrap().to_path_buf();
        Box::new(move || {
            // Fresh per-worker scratch subdir (unique) each spawn/respawn.
            let seq = spawn_seq.fetch_add(1, Ordering::SeqCst);
            let scratch = scratch_root.join(format!("w{seq}"));
            let _ = std::fs::remove_dir_all(&scratch);
            std::fs::create_dir_all(&scratch)?;
            let base = SandboxPolicy {
                net: Net::Allowlist(allow.clone()),
                profile: Profile::WorkerNetClient,
                // Loader needs the bin dir; the origin CA is added to fs_read by
                // spawn_net_transport (extra_ca). The worker reads the CA PATH
                // from env (KASTELLAN_NETDEMO_EXTRA_CA), which we inject here.
                fs_read: vec![bin_dir.clone()],
                cpu_ms: 10_000,
                mem_mb: 256,
                env: vec![(
                    "KASTELLAN_NETDEMO_EXTRA_CA".to_string(),
                    ca_path.to_string_lossy().into_owned(),
                )],
                ..SandboxPolicy::default()
            };
            let params = NetTransportSpawn {
                backend: &*backend,
                sidecar_backend: &*backend,
                proxy_bin: &proxy_bin,
                program: &net_demo.to_string_lossy(),
                args: &[],
                base_policy: base,
                allowlist: &allow,
                worker_name: "net-demo",
                extra_ca: Some(&ca_path),
            };
            let t = spawn_net_transport(&params, &scratch, |_row| {})?;
            Ok(Box::new(t) as Box<dyn PersistentTransport>)
        })
    };

    let h = PersistentWorker::spawn("net-demo-5c", factory)
        .expect("spawn net-demo persistent worker");

    // ── Phase 1: end-to-end TLS through the transparent tunnel ────────────────
    let probe = h
        .call(
            "net.tls_probe",
            serde_json::json!({"host":"127.0.0.1","port":origin_port}),
        )
        .expect("net.tls_probe");
    assert_eq!(
        probe["ok"], true,
        "transparent-tunnel TLS must succeed, got {probe}"
    );

    // Many calls on one boot.
    for i in 0..5 {
        h.call("net.stats", serde_json::json!({}))
            .unwrap_or_else(|e| panic!("stats {i}: {e}"));
    }

    // ── Phase 2: deterministic death → 1:1 sidecar+worker respawn ─────────────
    let crash_result = h.call("net.crash", serde_json::json!({}));
    eprintln!("[INFO] net.crash result: {crash_result:?}");

    let mut ok = false;
    for _ in 0..60 {
        std::thread::sleep(Duration::from_millis(50));
        if let Ok(p) = h.call(
            "net.tls_probe",
            serde_json::json!({"host":"127.0.0.1","port":origin_port}),
        ) {
            if p["ok"] == true {
                ok = true;
                break;
            }
        }
    }
    assert!(
        ok,
        "net.tls_probe must succeed again after 1:1 sidecar+worker respawn"
    );

    h.shutdown();
    let _ = std::fs::remove_dir_all(&scratch_root);
    let _ = std::fs::remove_dir_all(ca_path.parent().unwrap());
}
