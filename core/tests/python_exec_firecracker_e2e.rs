//! End-to-end test: the agent core runs python-exec inside a **Linux Firecracker
//! micro-VM** (`SandboxBackendKind::FirecrackerVm`) and round-trips `python.exec`
//! through `tool_host::dispatch_with_sink`.
//!
//! This is the Linux counterpart of `python_exec_container_e2e.rs` (macOS
//! `MacosContainer`). It proves the full chain end to end: the core resolves the
//! firecracker backend → `LinuxFirecracker::spawn_under_policy` writes the VM
//! config and spawns `kastellan-microvm-run` as the worker `Child` →
//! `kastellan-microvm-run` boots Firecracker and connects the guest over hybrid
//! vsock → the guest PID1 `kastellan-microvm-init` accepts the bridge, wires it
//! onto fd 0/1 and execs the unchanged `serve_stdio` worker → JSON-RPC rides the
//! vsock both ways.
//!
//! Pins what bwrap can't, the same way the macOS VM closes the Seatbelt gap:
//! the `mem_mb: 512` cap is enforced by **KVM** (a >512 MiB allocation fails),
//! and `Net::Deny` (no virtio-net device in the VM) contains an outbound socket
//! attempt behind a separate guest kernel.
//!
//! DGX-only / `#[ignore]` by default: needs `/dev/kvm` + `/dev/vhost-vsock`
//! (the one-time `sudo scripts/linux/install-firecracker-vsock.sh`), a built
//! rootfs+kernel (`scripts/workers/microvm/build-rootfs.sh`), firecracker on
//! `$PATH`, and the `kastellan-microvm-run` binary built (`cargo build
//! -p kastellan-microvm-run`). Run with:
//!     cargo test -p kastellan-core --test python_exec_firecracker_e2e -- --ignored --nocapture

#![cfg(target_os = "linux")]

use std::path::PathBuf;

use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch_with_sink, spawn_worker, ToolHostError, WorkerSpec};
use kastellan_core::workers::python_exec::firecracker_mode_entry;
use kastellan_sandbox::linux_firecracker::{LinuxFirecracker};
use kastellan_sandbox::{SandboxBackendKind};
use kastellan_tests_common::microvm::{firecracker_backend, image_dir, skip_if_no_microvm};
use kastellan_tests_common::NoopAuditSink;

/// The rootfs image this suite boots. Passed to the shared
/// `kastellan_tests_common::microvm` helpers, which own the `[SKIP]` wording,
/// the launcher discovery and the `KASTELLAN_MICROVM_DIR` lookup (issue #475).
const VM_ROOTFS: &str = "python-exec.ext4";

/// Spawn the worker in the micro-VM with the given `params_file_max` (forwarded
/// into the guest via the #360 cmdline env token), dispatch one `python.exec`,
/// and return the raw `dispatch_with_sink` result. A worker-side rejection (e.g.
/// an over-ceiling param → `INVALID_PARAMS`) surfaces here as
/// `Err(ToolHostError::Protocol)`, so the over-ceiling differential below can
/// assert on it without panicking. `NoopAuditSink` → no PG needed.
async fn try_dispatch_in_microvm(
    payload: serde_json::Value,
    params_file_max: Option<String>,
) -> Result<serde_json::Value, ToolHostError> {
    let entry = firecracker_mode_entry(
        PathBuf::from("/usr/local/bin/kastellan-worker-python-exec"),
        image_dir(),
        params_file_max,
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
        payload,
    )
    .await;
    let _ = worker.close();
    result
}

/// Convenience: dispatch with the default ceiling and unwrap (the happy-path
/// scenarios expect a successful round-trip).
async fn dispatch_in_microvm(payload: serde_json::Value) -> serde_json::Value {
    try_dispatch_in_microvm(payload, None)
        .await
        .expect("dispatch python.exec")
}

async fn run_in_microvm(code: &str) -> serde_json::Value {
    dispatch_in_microvm(serde_json::json!({ "code": code })).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "needs DGX: /dev/kvm + vhost_vsock + built rootfs + kastellan-microvm-run"]
async fn microvm_round_trip_six_times_seven() {
    if skip_if_no_microvm(VM_ROOTFS) {
        return;
    }
    let out = run_in_microvm("print(6 * 7)").await;
    let stdout = out["stdout"].as_str().unwrap_or_default();
    assert!(
        stdout.contains("42"),
        "expected 42 from the guest, got: {out}"
    );
    assert_eq!(out["exit_code"], 0, "clean exit expected: {out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "needs DGX"]
async fn microvm_enforces_mem_cap() {
    if skip_if_no_microvm(VM_ROOTFS) {
        return;
    }
    // Allocate ~900 MiB in a 512 MiB VM. KVM enforces the cap at the hypervisor
    // (unlike bwrap's cgroup on a shared kernel, this is a separate guest kernel
    // with hard RAM); the allocation fails. Observed: exit_code 1 with a Python
    // MemoryError traceback; a guest OOM-kill of the child would give a null
    // exit_code. Accept either; reject a clean 0 (would mean the cap leaked).
    let code = "x = bytearray(900 * 1024 * 1024); print(len(x))";
    let out = run_in_microvm(code).await;
    let exit_indicates_oom =
        out["exit_code"].is_null() || out["exit_code"].as_i64().is_some_and(|c| c != 0);
    assert!(
        exit_indicates_oom,
        "expected an OOM failure (non-zero or null exit), got: {out}"
    );
    let stdout = out["stdout"].as_str().unwrap_or_default();
    assert!(
        !stdout.contains(&(900 * 1024 * 1024).to_string()),
        "the allocation print must not appear — it should fail first: {out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "needs DGX"]
async fn microvm_net_is_denied() {
    if skip_if_no_microvm(VM_ROOTFS) {
        return;
    }
    // Net::Deny renders the VM with no virtio-net device, so a connect to a
    // public IP cannot succeed. A SUCCESSFUL connection prints "CONNECTED" to
    // stdout; its ABSENCE is the containment invariant. Non-vacuity rests on the
    // round-trip test proving this same harness faithfully relays guest stdout,
    // so a real connection could not hide. (Keep that test paired and live.)
    let code = "\
import socket, sys
try:
    s = socket.create_connection(('1.1.1.1', 443), timeout=2)
    print('CONNECTED')
except Exception:
    print('blocked', file=sys.stderr)
";
    let out = run_in_microvm(code).await;
    assert!(
        out.get("exit_code").is_some(),
        "worker returned no result object — dispatch broken, not contained: {out}"
    );
    let stdout = out["stdout"].as_str().unwrap_or_default();
    assert!(
        !stdout.contains("CONNECTED"),
        "network must be denied (no CONNECTED): {out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "needs DGX"]
async fn microvm_large_params_ride_file_channel() {
    if skip_if_no_microvm(VM_ROOTFS) {
        return;
    }
    // A >64 KiB params payload exceeds the inline env threshold, so the worker
    // takes the FILE channel: it writes `<scratch>/params.json` (the in-VM `/tmp`
    // tmpfs the guest init mounts) and points the child at it via
    // KASTELLAN_PYTHON_PARAMS_FILE. This proves that write path works in-VM (the
    // fail-CLOSED path: write_params_file(...)? would abort the exec on any IO
    // error, surfacing as a non-zero exit here). 100_000 B ≫ 64 KiB, ≪ the 1 MiB
    // default ceiling → the File channel.
    let blob = "A".repeat(100_000);
    let code = concat!(
        "import json, os\n",
        "p = os.environ.get('KASTELLAN_PYTHON_PARAMS_FILE')\n",
        "if p:\n",
        "    with open(p) as f:\n",
        "        params = json.load(f)\n",
        "else:\n",
        "    params = json.loads(os.environ.get('KASTELLAN_PYTHON_PARAMS', '{}'))\n",
        "b = params['blob']\n",
        "print(len(b), b[:4], b[-4:])\n",
    );
    let out = dispatch_in_microvm(serde_json::json!({ "code": code, "params": { "blob": blob } }))
        .await;
    assert_eq!(
        out["exit_code"].as_i64(),
        Some(0),
        "file-channel write to the in-VM tmpfs must succeed; stderr: {}",
        out["stderr"]
    );
    assert_eq!(
        out["stdout"].as_str().unwrap_or_default().trim_end(),
        "100000 AAAA AAAA",
        "agent must read the full 100 KiB payload via the in-VM file channel: {out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "needs DGX"]
async fn microvm_forwarded_params_file_max_is_enforced_in_guest() {
    if skip_if_no_microvm(VM_ROOTFS) {
        return;
    }
    // The #360 differential: prove the operator `KASTELLAN_PYTHON_PARAMS_FILE_MAX`
    // override is LIVE inside the guest (slice 1 left it inert — provisioning-only,
    // never forwarded). With a forwarded ceiling of 80_000 B, a ~100 KiB param now
    // exceeds the file-channel cap and the worker fails closed with INVALID_PARAMS
    // ("params is N bytes; cap is 80000"), surfaced here as a dispatch error.
    //
    // Non-vacuity / negative control: `microvm_large_params_ride_file_channel`
    // sends the SAME 100 KiB payload at the DEFAULT 1 MiB ceiling and it SUCCEEDS.
    // So a rejection here can only mean the forwarded 80_000 ceiling reached the
    // guest — if env-forwarding regresses, the guest falls back to 1 MiB and this
    // dispatch would succeed instead, failing the test.
    let blob = "A".repeat(100_000);
    let code = "print('unreachable')";
    let result = try_dispatch_in_microvm(
        serde_json::json!({ "code": code, "params": { "blob": blob } }),
        Some("80000".to_string()),
    )
    .await;
    let err = result.expect_err(
        "a 100 KiB param under a forwarded 80_000 ceiling must be rejected in-guest \
         (if this is Ok, the ceiling did not reach the guest → #360 regressed)",
    );
    let msg = format!("{err}");
    assert!(
        msg.contains("80000"),
        "rejection must cite the forwarded cap (80000), proving it took effect; got: {msg}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "needs DGX: /dev/kvm + vhost_vsock + built rootfs + kastellan-microvm-run"]
async fn microvm_spawn_leaves_no_orphan_run_dir() {
    if skip_if_no_microvm(VM_ROOTFS) {
        return;
    }
    // #362: a completed micro-VM spawn must leave no per-spawn run-dir behind —
    // the launcher self-cleans its run-dir on graceful exit. The worker/VM exits
    // when this dispatch returns.
    let out = run_in_microvm("print(1)").await;
    assert_eq!(out["exit_code"], 0, "clean exit expected: {out}");

    // A before/after count is racy under parallel tests, so instead assert that
    // every surviving `kastellan-microvm-*` run-dir belongs to a STILL-LIVE
    // launcher (pidfile pid alive). A leaked dir from this finished spawn would
    // carry a dead launcher pid.
    let temp = std::env::temp_dir();
    if let Ok(entries) = std::fs::read_dir(&temp) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.starts_with("kastellan-microvm-") || !path.is_dir() {
                continue;
            }
            if let Ok(pid_str) = std::fs::read_to_string(path.join("launcher.pid")) {
                if let Ok(pid) = pid_str.trim().parse::<u32>() {
                    assert!(
                        std::path::Path::new(&format!("/proc/{pid}")).exists(),
                        "leaked run-dir {path:?}: launcher pid {pid} is dead but dir survived"
                    );
                }
            }
        }
    }
}
