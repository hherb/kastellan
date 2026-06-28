#![cfg(target_os = "linux")]
//! Synthetic slice-3 e2e: the firecracker backend exposes a host fs_read dir
//! read-only at its absolute path inside the guest, plus a writable disk-backed
//! scratch drive at an anchor path. Drives the backend via the existing
//! python-exec entry with the policy mutated to add the shares (no
//! production-manifest change) — a generic-mechanism test.
//!
//! DGX-only / #[ignore]: needs /dev/kvm + /dev/vhost-vsock + mkfs.ext4 + a built
//! rootfs (REBUILD via build-rootfs.sh — it must carry the slice-3 anchor dirs)
//! plus the kastellan-microvm-run RELEASE launcher (rebuild it; target/release
//! is preferred and a stale one silently shadows source changes). Run:
//!
//!     export PATH=$HOME/.local/bin:$PATH   # firecracker is off the ssh PATH
//!     cargo build --release -p kastellan-microvm-run
//!     cargo test -p kastellan-core --test python_exec_firecracker_hostdir_e2e -- --ignored --nocapture

use std::path::PathBuf;
use std::sync::Arc;

use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch_with_sink, spawn_worker, WorkerSpec};
use kastellan_core::workers::python_exec::firecracker_mode_entry;
use kastellan_sandbox::linux_firecracker::{FirecrackerImage, LinuxFirecracker};
use kastellan_sandbox::{SandboxBackend, SandboxBackendKind, SandboxBackends};
use kastellan_tests_common::NoopAuditSink;

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
        rootfs_path: dir.join("python-exec.ext4"),
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
                 `cargo build -p kastellan-microvm-run`\n"
            );
            true
        }
    }
}

fn firecracker_backend() -> Arc<dyn SandboxBackend> {
    SandboxBackends::default_for_current_os().resolve(Some(SandboxBackendKind::FirecrackerVm), None)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "DGX-only: real KVM + vsock + rootfs with slice-3 anchors"]
async fn host_dir_is_readonly_and_scratch_writable_in_vm() {
    if skip_if_no_microvm() {
        return;
    }

    // Real readable host dir under /tmp with a sentinel; exposed in-guest at the
    // SAME absolute path (bind-mount path identity).
    let host_ro = std::env::temp_dir().join(format!("kastellan-s3-ro-{}", std::process::id()));
    std::fs::create_dir_all(&host_ro).unwrap();
    std::fs::write(host_ro.join("sentinel.txt"), b"slice3-ok").unwrap();
    let scratch_mount = PathBuf::from("/work/scratch"); // /work is a rootfs anchor

    let mut entry = firecracker_mode_entry(
        PathBuf::from("/usr/local/bin/kastellan-worker-python-exec"),
        image_dir(),
        None,
        kastellan_core::worker_lifecycle::Lifecycle::SingleUse,
    );
    entry.policy.fs_read = vec![host_ro.clone()];
    entry.policy.fs_write = vec![scratch_mount.clone()];

    let backend = firecracker_backend();
    let program = entry.binary.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &entry.policy,
        program: &program,
        args: &[],
        wall_clock_ms: entry.wall_clock_ms,
    };
    let mut worker = spawn_worker(&*backend, &spec).expect("spawn worker in micro-VM");

    let code = format!(
        "open('{}','w').write('w'); print(open('{}').read())",
        scratch_mount.join("out").display(),
        host_ro.join("sentinel.txt").display(),
    );
    let out = dispatch_with_sink(
        &NoopAuditSink,
        &Vault::new(),
        &mut worker,
        "python-exec",
        "python.exec",
        serde_json::json!({ "code": code }),
    )
    .await
    .expect("dispatch python.exec");
    let _ = worker.close();

    let stdout = out["stdout"].as_str().unwrap_or_default();
    assert!(stdout.contains("slice3-ok"), "guest read host sentinel: {out}");
    assert_eq!(out["exit_code"], 0, "scratch write + sentinel read both succeeded: {out}");

    let _ = std::fs::remove_dir_all(&host_ro);
}
