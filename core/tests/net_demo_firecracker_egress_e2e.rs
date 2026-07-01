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
use kastellan_sandbox::linux_firecracker::{FirecrackerImage, LinuxFirecracker};
use kastellan_sandbox::{
    Net, Profile, SandboxBackend, SandboxBackendKind, SandboxBackends, SandboxPolicy,
};

// ── Firecracker harness helpers (mirrored from kv_demo_firecracker_persistent_e2e.rs) ──

/// The micro-VM image dir (kernel + rootfs). Matches the backend default and is
/// overridable for a user-local build, exactly like the runtime resolver.
fn image_dir() -> String {
    std::env::var("KASTELLAN_MICROVM_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "/var/lib/kastellan/microvm".to_string())
}

fn firecracker_image() -> FirecrackerImage {
    let dir = PathBuf::from(image_dir());
    FirecrackerImage {
        kernel_path: dir.join("vmlinux"),
        rootfs_path: dir.join("net-demo.ext4"),
    }
}

/// Locate the `kastellan-microvm-run` launcher among the workspace target dirs
/// (release preferred, then debug) and prepend its parent to `$PATH` so the
/// backend's `Command::new("kastellan-microvm-run")` resolves it. Returns the
/// path if found.
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

/// Skip (early-return `true`) when this host can't run the micro-VM: the
/// firecracker probe fails (no firecracker / KVM / vhost-vsock / images) or the
/// launcher binary isn't built. On success, prepend the launcher's dir to
/// `$PATH` exactly once.
fn skip_if_no_microvm() -> bool {
    if let Err(e) = LinuxFirecracker::probe(&firecracker_image()) {
        eprintln!("\n[SKIP] firecracker probe failed: {e}\n");
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
            eprintln!(
                "\n[SKIP] kastellan-microvm-run not built; run \
                 `cargo build --release -p kastellan-microvm-run`\n"
            );
            true
        }
    }
}

fn firecracker_backend() -> Arc<dyn SandboxBackend> {
    SandboxBackends::default_for_current_os().resolve(Some(SandboxBackendKind::FirecrackerVm), None)
}

// ── loopback rustls origin (mirrored from net_demo_egress_e2e.rs) ──────────────

/// Loopback self-signed rustls TLS origin. Reuses the exact rcgen + rustls
/// pattern from the net-demo Task-3 dev harness (`workers/net-demo/src/main.rs`
/// `mod probe_harness`), adapted to serve MANY connections (the initial
/// `net.tls_probe`, then a fresh one after respawn each open a new TLS session):
/// binds `127.0.0.1:0`, replies `HTTP/1.1 204 No Content\r\n\r\n` to any request,
/// and returns `(port, ca_pem_path)`. The origin runs on the HOST; the real
/// egress-proxy sidecar dials `127.0.0.1:<port>`, so the operator allowlist must
/// carry that literal `127.0.0.1:<port>` (the proxy's SSRF has a literal-IP
/// carve-out for allowlisted addresses — see `egress_force_routing_e2e.rs`).
///
/// The cert PEM is written under `/tmp` (a slice-3 SHARE_ANCHOR) so `extra_ca`'s
/// `fs_read` entry is accepted by `build_launch_plan` and the RO-share
/// materializes it in-guest at the identical path.
mod origin {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::thread;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    /// Spawn a multi-connection loopback rustls origin on `127.0.0.1:0` that
    /// answers any request with `204 No Content`. Writes its self-signed cert PEM
    /// under `/tmp` (a SHARE_ANCHOR) and returns `(port, ca_pem_path)`.
    pub fn spawn_loopback_tls_origin() -> (u16, PathBuf) {
        // Self-signed cert with a 127.0.0.1 IP SAN so rustls' server-name (IP)
        // verification against the origin succeeds.
        let ck = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()])
            .expect("generate self-signed cert");
        let cert_pem = ck.cert.pem();
        let cert_der = ck.cert.der().clone();
        let key_der = rustls_pki_types::PrivateKeyDer::Pkcs8(
            rustls_pki_types::PrivatePkcs8KeyDer::from(ck.key_pair.serialize_der()),
        );

        let server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .expect("build server config");
        let acceptor = TlsAcceptor::from(Arc::new(server_config));

        // A current-thread runtime dedicated to the origin. It binds the port
        // synchronously so the caller can read it, then serves connections in a
        // loop (one per probe). Detached — lives for the test's duration.
        let (tx, rx) = std::sync::mpsc::channel::<u16>();
        thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("origin runtime");
            rt.block_on(async move {
                let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind origin");
                let port = listener.local_addr().unwrap().port();
                tx.send(port).unwrap();
                loop {
                    let (tcp, _) = match listener.accept().await {
                        Ok(pair) => pair,
                        Err(_) => break,
                    };
                    let acceptor = acceptor.clone();
                    // Each connection gets its own task so a slow/aborted probe
                    // never blocks the next one.
                    tokio::spawn(async move {
                        let mut tls = match acceptor.accept(tcp).await {
                            Ok(t) => t,
                            Err(_) => return,
                        };
                        let mut buf = [0u8; 1024];
                        let _ = tls.read(&mut buf).await;
                        let _ = tls.write_all(b"HTTP/1.1 204 No Content\r\n\r\n").await;
                        let _ = tls.shutdown().await;
                    });
                }
            });
        });

        let port = rx.recv().expect("origin port");

        // Write the origin cert under /tmp (a slice-3 SHARE_ANCHOR): its path is
        // the worker's extra_ca, delivered in-guest at the identical path by the
        // RO share. Kept out of the per-spawn scratch so it survives respawn.
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
    if skip_if_no_microvm() {
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

    let factory: PersistentFactory = {
        let backend = Arc::clone(&backend);
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
                proxy_bin: &proxy_bin,
                program: "/usr/local/bin/kastellan-worker-net-demo",
                args: &[],
                base_policy: base,
                allowlist: &allow,
                worker_name: "net-demo",
                extra_ca: Some(&ca_path),
            };
            let t = spawn_net_transport(&params, &scratch)?;
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

    // Many calls on one boot (many-calls-one-boot invariant).
    for i in 0..5 {
        h.call("net.stats", serde_json::json!({}))
            .unwrap_or_else(|e| panic!("net.stats call {i} failed: {e}"));
    }

    // ── Phase 2: SIGKILL the launcher → 1:1 sidecar+VM respawn ────────────────
    eprintln!("[INFO] sending SIGKILL to kastellan-microvm-run to force VM death");
    let _ = std::process::Command::new("pkill")
        .args(["-9", "kastellan-microvm-run"])
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

    h.shutdown();
    let _ = std::fs::remove_file(&ca_path);
}
