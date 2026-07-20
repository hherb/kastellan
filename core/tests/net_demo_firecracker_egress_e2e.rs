//! Slice 5c DGX e2e: a long-lived net-demo worker in a Firecracker VM does its
//! own end-to-end TLS to a host loopback origin through a transparent-tunnel
//! egress sidecar over the slice-4a vsock reverse-channel, serves many calls on
//! one boot, and survives a launcher-SIGKILL respawn (1:1 sidecar+VM).
//!
//! `#[ignore]`: needs `/dev/kvm` + `/dev/vhost-vsock` + `net-demo.ext4` rootfs +
//! the RELEASE launcher (`kastellan-microvm-run`) + the host egress-proxy
//! sidecar. Hermetic (loopback origin, literal-IP allowlist → no real net /
//! DNS), so it is DGX-green. Run on the DGX:
//!
//! ```sh
//! export PATH=$HOME/.local/bin:$PATH
//! cargo build --release -p kastellan-microvm-run
//! cargo build -p kastellan-worker-egress-proxy   # host sidecar (debug ok)
//! ./scripts/workers/microvm/build-net-demo-rootfs.sh
//! cargo test -p kastellan-core --test net_demo_firecracker_egress_e2e -- --ignored --nocapture
//! ```
//!
//! The test proves:
//! - **In-VM end-to-end TLS**: `net.tls_probe{127.0.0.1:<port>}` → `ok:true`,
//!   the in-guest worker completing a TLS handshake to the host loopback origin
//!   through the transparent-tunnel sidecar over the slice-4a vsock channel.
//! - **Many-calls-one-boot**: several `net.stats` calls served by one VM boot.
//! - **1:1 sidecar+VM respawn**: SIGKILL the launcher → `PersistentWorker`
//!   respawns the VM *and* its egress sidecar; `net.tls_probe` succeeds again.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use kastellan_core::egress::persistent_net::{spawn_net_transport, NetTransportSpawn};
use kastellan_core::worker_lifecycle::{PersistentFactory, PersistentTransport, PersistentWorker};
use kastellan_sandbox::{
use kastellan_tests_common::microvm::{firecracker_backend, image_dir, skip_if_no_microvm};
    Net, Profile, SandboxBackend, SandboxBackendKind, SandboxBackends, SandboxPolicy,
};

/// The rootfs image this suite boots. Passed to the shared
/// `kastellan_tests_common::microvm` helpers, which own the `[SKIP]` wording,
/// the launcher discovery and the `KASTELLAN_MICROVM_DIR` lookup (issue #475).
const VM_ROOTFS: &str = "net-demo.ext4";

// ── Firecracker harness helpers (mirrored from kv_demo_firecracker_persistent_e2e.rs) ──

/// The HOST backend (bwrap on Linux) for the egress-proxy sidecar. The sidecar
/// is the real-network egress boundary and must run on the host, not in the VM;
/// only the worker (`firecracker_backend`) runs in the Firecracker VM.
fn host_backend() -> Arc<dyn SandboxBackend> {
    SandboxBackends::default_for_current_os().resolve(None, None)
}

// ── loopback rustls origin (mirrored from net_demo_egress_e2e.rs) ──────────────

/// Loopback self-signed rustls TLS origin — the server-spawn is shared with the
/// hermetic e2e via [`kastellan_tests_common::tls_origin`] (#390); this module
/// only keeps the Firecracker test's distinct cert-PEM destination. The origin
/// binds `127.0.0.1:0`, replies `HTTP/1.1 204 No Content\r\n\r\n` to any request,
/// and runs on the HOST; the real egress-proxy sidecar dials `127.0.0.1:<port>`,
/// so the operator allowlist must carry that literal `127.0.0.1:<port>` (the
/// proxy's SSRF has a literal-IP carve-out for allowlisted addresses — see
/// `egress_force_routing_e2e.rs`).
///
/// The cert PEM is written under `/tmp` (a slice-3 SHARE_ANCHOR) so `extra_ca`'s
/// `fs_read` entry is accepted by `build_launch_plan` and the RO-share
/// materializes it in-guest at the identical path.
mod origin {
    use std::path::PathBuf;

    /// Spawn the shared loopback TLS origin and write its cert PEM under `/tmp`
    /// (a SHARE_ANCHOR): its path is the worker's `extra_ca`, delivered in-guest
    /// at the identical path by the RO share, and kept out of the per-spawn
    /// scratch so it survives respawn. Returns `(port, ca_pem_path)`.
    pub fn spawn_loopback_tls_origin() -> (u16, PathBuf) {
        let (port, cert_pem) =
            kastellan_tests_common::tls_origin::spawn_loopback_tls_origin();
        let ca_path =
            PathBuf::from("/tmp").join(format!("netdemo-{}-ca.pem", std::process::id()));
        std::fs::write(&ca_path, cert_pem.as_bytes()).expect("write origin ca");
        (port, ca_path)
    }
}

/// Mint a unique, shallow scratch dir under `/tmp` per spawn. Shallow on
/// purpose: `spawn_net_transport` binds `<scratch>/egress.sock`, whose path must
/// fit the 104-byte `sockaddr_un.sun_path` guard (`make_worker_scratch_dir`).
/// `/tmp` is short and is a slice-3 SHARE_ANCHOR. A fresh dir per spawn/respawn
/// gives every sidecar a fresh UDS — no stale-socket reuse across a respawn.
fn scratch_root_subdir() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = PathBuf::from("/tmp").join(format!("netdemo-vm-{}-{}", std::process::id(), seq));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

// ── test ──────────────────────────────────────────────────────────────────────

#[test]
#[ignore = "DGX-only: real KVM + vsock + net-demo rootfs + egress-proxy sidecar"]
fn net_demo_tls_probe_through_vm_survives_respawn() {
    if skip_if_no_microvm(VM_ROOTFS) {
        return;
    }

    // The host egress-proxy sidecar binary (debug or release). Skip-as-pass if
    // it isn't built — same discovery as net_demo_egress_e2e.rs.
    let proxy_bin = {
        let target = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("target");
        let candidates = [
            target.join("debug").join("kastellan-worker-egress-proxy"),
            target.join("release").join("kastellan-worker-egress-proxy"),
        ];
        match candidates.into_iter().find(|p| p.is_file()) {
            Some(p) => p,
            None => {
                eprintln!(
                    "[SKIP] egress-proxy not built; run \
                     `cargo build -p kastellan-worker-egress-proxy`"
                );
                return;
            }
        }
    };

    // Loopback self-signed origin (host, real netns); its cert PEM lives under
    // /tmp and becomes the worker's extra_ca. The sidecar dials 127.0.0.1:<port>,
    // so the operator allowlist carries that literal (proxy SSRF literal-IP
    // carve-out permits it; no DNS → the DGX resolver caveat does not apply).
    let (origin_port, ca_path) = origin::spawn_loopback_tls_origin();
    let allow = vec![format!("127.0.0.1:{origin_port}")];
    let backend = firecracker_backend();
    let host_backend = host_backend();

    let factory: PersistentFactory = {
        let backend = Arc::clone(&backend);
        let host_backend = Arc::clone(&host_backend);
        let proxy_bin = proxy_bin.clone();
        let ca_path = ca_path.clone();
        let allow = allow.clone();
        let img = image_dir();
        Box::new(move || {
            // Fresh per-worker scratch (and fresh sidecar UDS) each spawn/respawn.
            let scratch = scratch_root_subdir();
            let base = SandboxPolicy {
                net: Net::Allowlist(allow.clone()),
                profile: Profile::WorkerNetClient,
                mem_mb: 256,
                // KASTELLAN_MICROVM_DIR + KASTELLAN_MICROVM_ROOTFS select the
                // net-demo rootfs (slice-4b resolver). The origin CA is added to
                // fs_read by spawn_net_transport (extra_ca); the worker reads the
                // CA *path* from KASTELLAN_NETDEMO_EXTRA_CA, which we inject here
                // (both are required — env for the read, fs_read for the bind).
                env: vec![
                    ("KASTELLAN_MICROVM_DIR".to_string(), img.clone()),
                    (
                        "KASTELLAN_MICROVM_ROOTFS".to_string(),
                        "net-demo.ext4".to_string(),
                    ),
                    (
                        "KASTELLAN_NETDEMO_EXTRA_CA".to_string(),
                        ca_path.to_string_lossy().into_owned(),
                    ),
                ],
                ..SandboxPolicy::default()
            };
            let params = NetTransportSpawn {
                backend: &*backend,
                sidecar_backend: &*host_backend,
                proxy_bin: &proxy_bin,
                program: "/usr/local/bin/kastellan-worker-net-demo",
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

    let h = PersistentWorker::spawn("net-demo-vm", factory).expect("boot net-demo VM");

    // ── Phase 1: end-to-end TLS through the VM's vsock reverse-channel ─────────
    let probe = h
        .call(
            "net.tls_probe",
            serde_json::json!({"host":"127.0.0.1","port":origin_port}),
        )
        .expect("net.tls_probe");
    assert_eq!(
        probe["ok"], true,
        "in-VM transparent-tunnel TLS must succeed, got {probe}"
    );

    // Many calls on one boot (many-calls-one-boot invariant). Record the boot's
    // high-water `calls_served` so we can prove the post-kill probe is served by a
    // DIFFERENT (freshly-respawned) VM — a pid check is useless here because the
    // worker is PID 1 inside the guest, but `calls_served` is per-process and
    // resets to a low value on a fresh boot.
    let mut pre_kill_calls = 0u64;
    for i in 0..5 {
        let s = h
            .call("net.stats", serde_json::json!({}))
            .unwrap_or_else(|e| panic!("net.stats call {i} failed: {e}"));
        pre_kill_calls = s["calls_served"].as_u64().unwrap_or(0);
    }
    assert!(
        pre_kill_calls >= 5,
        "expected the first boot to have served several calls, got {pre_kill_calls}"
    );

    // ── Phase 2: SIGKILL the launcher → 1:1 sidecar+VM respawn ────────────────
    // NB: `pkill NAME` matches the 15-char-truncated /proc/<pid>/comm, so the
    // 21-char "kastellan-microvm-run" overflows it and matches NOTHING (a silent
    // no-op that would make this a vacuous respawn test). Use `-f` to match the
    // full command line, which reliably kills the launcher.
    eprintln!("[INFO] sending SIGKILL (-f) to kastellan-microvm-run to force VM death");
    let _ = std::process::Command::new("pkill")
        .args(["-9", "-f", "kastellan-microvm-run"])
        .status();

    // The in-flight / first post-kill call is expected to Err while the worker
    // respawns the VM and its sidecar.
    let _ = h.call(
        "net.tls_probe",
        serde_json::json!({"host":"127.0.0.1","port":origin_port}),
    );

    // Bounded retry — VM boot + sidecar respawn on the DGX takes several seconds.
    let mut ok = false;
    for attempt in 0..60 {
        std::thread::sleep(Duration::from_millis(500));
        match h.call(
            "net.tls_probe",
            serde_json::json!({"host":"127.0.0.1","port":origin_port}),
        ) {
            Ok(p) if p["ok"] == true => {
                eprintln!("[INFO] net.tls_probe succeeded on attempt {attempt}");
                ok = true;
                break;
            }
            Ok(p) => eprintln!("[INFO] attempt {attempt}: probe not-ok yet: {p}"),
            Err(e) => eprintln!("[INFO] attempt {attempt}: probe err: {e}"),
        }
    }
    assert!(
        ok,
        "net.tls_probe must succeed again within ~30s after VM+sidecar respawn"
    );

    // Prove the post-kill probe was served by a FRESH VM, not the original one
    // that a no-op kill left alive: a respawned worker's `calls_served` counter
    // has reset and is far below the pre-kill high-water mark.
    let post = h
        .call("net.stats", serde_json::json!({}))
        .expect("post-respawn net.stats");
    let post_calls = post["calls_served"].as_u64().unwrap_or(u64::MAX);
    assert!(
        post_calls < pre_kill_calls,
        "post-respawn calls_served ({post_calls}) must be below the pre-kill \
         high-water ({pre_kill_calls}) — proves a FRESH VM served it, not a \
         still-alive original (a no-op kill would leave the counter climbing)"
    );

    h.shutdown();
    let _ = std::fs::remove_file(&ca_path);
}
