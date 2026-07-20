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

use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch_with_sink, spawn_worker, WorkerSpec};
use kastellan_core::workers::python_exec::firecracker_mode_entry;
use kastellan_tests_common::microvm::{firecracker_backend, image_dir, skip_if_no_microvm};
use kastellan_tests_common::NoopAuditSink;

/// The rootfs image this suite boots. Passed to the shared
/// `kastellan_tests_common::microvm` helpers, which own the `[SKIP]` wording,
/// the launcher discovery and the `KASTELLAN_MICROVM_DIR` lookup (issue #475).
const VM_ROOTFS: &str = "python-exec.ext4";

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
    if skip_if_no_microvm(VM_ROOTFS) {
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

    // Phase 3 — confined teardown leaves NO unreclaimable husk. Under
    // confinement the run-dir is a bwrap bind-mount point, so the launcher can't
    // rmdir it on graceful exit; it drops a `teardown.done` marker instead and
    // the host-side orphan sweep reclaims the husk. Run one explicit sweep (it's
    // normally lazy — fired at the next spawn) and assert no marked husk survives.
    // Scoping to marker-bearing dirs makes this race-safe against run-dirs from
    // other parallel test binaries (only a FINISHED launcher writes the marker).
    use kastellan_sandbox::linux_firecracker::{
        pid_is_alive, sweep_orphaned_run_dirs, RUN_DIR_PREFIX, TEARDOWN_MARKER_FILE,
    };
    let temp = std::env::temp_dir();
    sweep_orphaned_run_dirs(&temp, pid_is_alive);
    if let Ok(entries) = std::fs::read_dir(&temp) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with(RUN_DIR_PREFIX) {
                assert!(
                    !path.join(TEARDOWN_MARKER_FILE).exists(),
                    "confined teardown husk {path:?} not reclaimed by the sweep"
                );
            }
        }
    }
}
