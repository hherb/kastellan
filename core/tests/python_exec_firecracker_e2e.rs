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
use std::sync::Arc;

use async_trait::async_trait;
use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch_with_sink, spawn_worker, AuditSink, WorkerSpec};
use kastellan_core::workers::python_exec::firecracker_mode_entry;
use kastellan_db::DbError;
use kastellan_sandbox::linux_firecracker::{FirecrackerImage, LinuxFirecracker};
use kastellan_sandbox::{SandboxBackend, SandboxBackendKind, SandboxBackends};

/// A no-op audit sink so the test needs no Postgres cluster — the micro-VM is
/// the only external dependency.
struct NoopAuditSink;

#[async_trait]
impl AuditSink for NoopAuditSink {
    async fn insert(
        &self,
        _actor: &str,
        _action: &str,
        _payload: serde_json::Value,
    ) -> Result<i64, DbError> {
        Ok(1)
    }
}

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

/// Spawn the worker in the micro-VM, dispatch one `python.exec` with the given
/// JSON-RPC params object, return the result. Mirrors the container e2e harness:
/// `dispatch_with_sink` + `NoopAuditSink` so no PG is needed.
async fn dispatch_in_microvm(payload: serde_json::Value) -> serde_json::Value {
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
        payload,
    )
    .await;
    let _ = worker.close();
    result.expect("dispatch python.exec")
}

async fn run_in_microvm(code: &str) -> serde_json::Value {
    dispatch_in_microvm(serde_json::json!({ "code": code })).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "needs DGX: /dev/kvm + vhost_vsock + built rootfs + kastellan-microvm-run"]
async fn microvm_round_trip_six_times_seven() {
    if skip_if_no_microvm() {
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
    if skip_if_no_microvm() {
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
    if skip_if_no_microvm() {
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
    if skip_if_no_microvm() {
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
