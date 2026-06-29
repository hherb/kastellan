//! Slice 5a e2e: the Firecracker VMM (the `kastellan-microvm-run` launcher and
//! the `firecracker` process it spawns) runs inside the project's unprivileged
//! `bwrap` jail + a `systemd-run --user --scope` cgroup — **default-ON** — and a
//! python-exec VM still boots and computes. This proves `/dev/kvm` +
//! `/dev/vhost-vsock` survive the bwrap user namespace (the slice-5a **merge
//! gate**), and that the `KASTELLAN_MICROVM_CONFINE_VMM=0` opt-out still boots
//! bare (no-regression of the `VmmConfinement::None` strategy).
//!
//! The two scenarios are one test run sequentially because they both mutate the
//! process-global `KASTELLAN_MICROVM_CONFINE_VMM` env var, which the backend
//! reads at spawn time — parallel tokio tests would race on it (and an env-lock
//! held across `.await` is not `Send`). This binary has no other test, so the
//! single-test, set-between-phases form is race-free.
//!
//! DGX-only / `#[ignore]`: needs `/dev/kvm` + `/dev/vhost-vsock`, a built
//! rootfs+kernel, firecracker on `$PATH`, the `kastellan-microvm-run` binary
//! built, AND (for the default confined path) the unprivileged-userns AppArmor
//! profile + a `systemd --user` session. Run with:
//!     export PATH=$HOME/.local/bin:$PATH
//!     cargo build --release -p kastellan-microvm-run
//!     cargo test -p kastellan-core --test firecracker_vmm_confinement_e2e -- --ignored --nocapture

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::Arc;

use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch_with_sink, spawn_worker, WorkerSpec};
use kastellan_core::workers::python_exec::firecracker_mode_entry;
use kastellan_sandbox::linux_firecracker::{FirecrackerImage, LinuxFirecracker};
use kastellan_sandbox::{SandboxBackend, SandboxBackendKind, SandboxBackends};
use kastellan_tests_common::NoopAuditSink;

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
/// (release preferred, then debug). The confined path also resolves it via
/// `find_executable` on `$PATH`, so we prepend its dir to `$PATH` below.
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

/// Skip (early-return `true`) when this host can't run the confined micro-VM.
/// Note: with confinement ON (the default), `LinuxFirecracker::probe` ALSO
/// requires a usable bwrap + user cgroup (the slice-5a fail-closed gate), so a
/// host missing the AppArmor profile / systemd-user session SKIPs here rather
/// than failing — exactly the fail-closed contract. Prepends the launcher dir to
/// `$PATH` once so both the launcher and (via the export) firecracker resolve.
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

/// Spawn a python-exec worker in the micro-VM (under whatever confinement the
/// current `KASTELLAN_MICROVM_CONFINE_VMM` env selects) and run one `print(6*7)`.
async fn boot_and_compute() -> serde_json::Value {
    let entry = firecracker_mode_entry(
        PathBuf::from("/usr/local/bin/kastellan-worker-python-exec"),
        image_dir(),
        None,
        kastellan_core::worker_lifecycle::Lifecycle::SingleUse,
    );
    let backend = firecracker_backend();
    let program = entry.binary.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &entry.policy,
        program: &program,
        args: &[],
        wall_clock_ms: entry.wall_clock_ms,
    };
    let mut worker = spawn_worker(&*backend, &spec).expect("spawn worker in micro-VM");
    let result = dispatch_with_sink(
        &NoopAuditSink,
        &Vault::new(),
        &mut worker,
        "python-exec",
        "python.exec",
        serde_json::json!({ "code": "print(6 * 7)" }),
    )
    .await;
    let _ = worker.close();
    result.expect("dispatch python.exec")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "needs DGX: /dev/kvm + vhost_vsock + built rootfs + launcher + bwrap/cgroup"]
async fn confined_default_and_opt_out_both_boot_and_compute() {
    if skip_if_no_microvm() {
        return;
    }

    // Phase 1 — DEFAULT (confinement ON): the VMM runs inside systemd-run+bwrap
    // and the guest still computes 6*7. This is the merge gate: KVM + vsock work
    // through the bwrap user namespace.
    std::env::remove_var("KASTELLAN_MICROVM_CONFINE_VMM");
    let out = boot_and_compute().await;
    assert!(
        out["stdout"].as_str().unwrap_or_default().contains("42"),
        "confined (default) boot must compute 42 — KVM/vsock must survive the bwrap userns: {out}"
    );
    assert_eq!(out["exit_code"], 0, "confined clean exit expected: {out}");

    // Phase 2 — OPT-OUT (bare spawn): KASTELLAN_MICROVM_CONFINE_VMM=0 selects the
    // VmmConfinement::None strategy (today's bare launcher spawn) and still boots.
    std::env::set_var("KASTELLAN_MICROVM_CONFINE_VMM", "0");
    let out = boot_and_compute().await;
    std::env::remove_var("KASTELLAN_MICROVM_CONFINE_VMM");
    assert!(
        out["stdout"].as_str().unwrap_or_default().contains("42"),
        "opt-out (bare) boot must compute 42 — None strategy intact: {out}"
    );
    assert_eq!(out["exit_code"], 0, "opt-out clean exit expected: {out}");
}
