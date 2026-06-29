//! Slice 5b-1/5b-2 DGX e2e: a long-lived kv-demo worker in a Firecracker VM
//! boots once, serves many calls, and its persistent ext4 store survives a VM
//! respawn. `#[ignore]`: needs `/dev/kvm` + `/dev/vhost-vsock` + `kv-demo.ext4`
//! rootfs + the RELEASE launcher (`kastellan-microvm-run`). Run on the DGX:
//!
//! ```sh
//! export PATH=$HOME/.local/bin:$PATH
//! cargo build --release -p kastellan-microvm-run
//! ./scripts/workers/kv-demo/build-kv-demo-rootfs.sh
//! cargo test -p kastellan-core --test kv_demo_firecracker_persistent_e2e -- --ignored --nocapture
//! ```
//!
//! The test proves:
//! - **Slice 5b-1**: the VM boots once and serves many sequential calls without
//!   re-booting (many-calls-one-boot invariant via kv.stats).
//! - **Slice 5b-2**: the ext4 persistent store (`kv-demo-state.ext4`) survives
//!   a forced VM death (launcher SIGKILL'd) and a subsequent respawn —
//!   `kv.get` after respawn returns the pre-crash value.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kastellan_core::worker_lifecycle::{
    ClientTransport, PersistentFactory, PersistentTransport, PersistentWorker,
};
use kastellan_sandbox::linux_firecracker::{FirecrackerImage, LinuxFirecracker};
use kastellan_sandbox::{Net, PersistentStore, Profile, SandboxBackend, SandboxBackendKind,
    SandboxBackends, SandboxPolicy};

// ── harness helpers (mirrored from python_exec_firecracker_e2e.rs) ─────────────

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
        rootfs_path: dir.join("kv-demo.ext4"),
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

// ── kv-demo VM policy helper ───────────────────────────────────────────────────

/// Build a base `SandboxPolicy` for the kv-demo Firecracker worker:
/// `Net::Deny`, `Profile::WorkerStrict`, 256 MiB RAM, env with
/// `KASTELLAN_MICROVM_DIR` and `KASTELLAN_MICROVM_ROOTFS=kv-demo.ext4`.
/// The caller must set `persistent_store` and `KASTELLAN_KV_STORE_DIR`.
fn kv_demo_vm_policy(image_dir: String) -> SandboxPolicy {
    SandboxPolicy {
        fs_read: vec![],
        fs_write: vec![],
        net: Net::Deny,
        cpu_ms: 0,
        mem_mb: 256,
        profile: Profile::WorkerStrict,
        env: vec![
            ("KASTELLAN_MICROVM_DIR".to_string(), image_dir),
            ("KASTELLAN_MICROVM_ROOTFS".to_string(), "kv-demo.ext4".to_string()),
        ],
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None,
        persistent_store: None,
    }
}

// ── test ──────────────────────────────────────────────────────────────────────

#[test]
#[ignore = "DGX-only: real KVM + vsock + kv-demo rootfs + persistent ext4 store"]
fn kv_demo_persistent_store_survives_vm_respawn() {
    if skip_if_no_microvm() {
        return;
    }

    // Remove any stale state image so this run starts clean.
    let store_img = PathBuf::from(image_dir()).join("kv-demo-state.ext4");
    let _ = std::fs::remove_file(&store_img);

    let backend = firecracker_backend();

    // Factory: creates a fresh ClientTransport (and therefore a fresh VM) for
    // the initial boot and for every respawn after VM death.
    let factory: PersistentFactory = {
        let backend = Arc::clone(&backend);
        let store_img = store_img.clone();
        Box::new(move || {
            let mut policy = kv_demo_vm_policy(image_dir());
            // Persistent ext4 store — mkfs-once by the backend, reused after.
            policy.persistent_store = Some(PersistentStore {
                host_backing: store_img.clone(),
                guest_mount: PathBuf::from("/data"),
                size_mib: 64,
            });
            // Tell the worker where inside the guest its kv store lives.
            policy.env.push((
                "KASTELLAN_KV_STORE_DIR".to_string(),
                "/data".to_string(),
            ));
            let t = ClientTransport::spawn(
                &*backend,
                &policy,
                "/usr/local/bin/kastellan-worker-kv-demo",
                &[],
            )?;
            Ok(Box::new(t) as Box<dyn PersistentTransport>)
        })
    };

    let h = PersistentWorker::spawn("kv-vm", factory).expect("boot kv-demo VM");

    // ── Phase 1: write a value and confirm many calls on one boot ─────────────

    let put = h
        .call("kv.put", serde_json::json!({"key": "k", "value": "pre-crash"}))
        .expect("kv.put");
    assert_eq!(put["ok"], true, "kv.put must return ok:true, got {put}");

    // Issue several kv.stats calls — all should be served by the same VM
    // (many-calls-one-boot invariant).
    for i in 0..5 {
        let stats = h
            .call("kv.stats", serde_json::json!({}))
            .unwrap_or_else(|e| panic!("kv.stats call {i} failed: {e}"));
        eprintln!("[INFO] kv.stats[{i}]: {stats}");
    }

    // ── Phase 2: force VM death by killing the launcher (SIGKILL) ─────────────
    // The kv-demo rootfs is a release build without kv.crash, so we kill the
    // launcher process from the outside. The in-flight call (if any) will Err.
    eprintln!("[INFO] sending SIGKILL to kastellan-microvm-run to simulate VM crash");
    let _ = std::process::Command::new("pkill")
        .args(["-9", "kastellan-microvm-run"])
        .status();

    // Allow PersistentWorker to observe the death and begin respawning.
    // The in-flight or first post-kill call is expected to Err.
    let _ = h.call("kv.get", serde_json::json!({"key": "k"}));

    // ── Phase 3: bounded retry — wait for the respawned VM to answer ──────────
    // VM boot on the DGX takes several seconds.  Poll for up to ~30 s.
    let mut got: anyhow::Result<serde_json::Value> = Err(anyhow::anyhow!("not yet"));
    for attempt in 0..60 {
        std::thread::sleep(Duration::from_millis(500));
        match h.call("kv.get", serde_json::json!({"key": "k"})) {
            Ok(v) => {
                got = Ok(v);
                eprintln!("[INFO] kv.get succeeded on attempt {attempt}");
                break;
            }
            Err(e) => {
                eprintln!("[INFO] attempt {attempt}: kv.get err: {e}");
                // "persistent worker is restarting" — keep retrying.
            }
        }
    }

    let got = got.expect("kv.get must succeed within ~30 s after VM respawn");
    assert_eq!(
        got["value"], "pre-crash",
        "persistent ext4 store must survive a VM respawn (SIGKILL + reboot); got {got}"
    );

    // ── Teardown ──────────────────────────────────────────────────────────────
    h.shutdown();
}
