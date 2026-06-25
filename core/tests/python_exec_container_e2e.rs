//! End-to-end test: the agent core runs python-exec inside the macOS
//! `MacosContainer` micro-VM (Phase 4 container mode) and round-trips
//! `python.exec` through `tool_host::dispatch_with_sink`.
//!
//! Pins what host mode can't on macOS: the `mem_mb: 512` cap is actually
//! ENFORCED by the VM (a >512 MiB allocation is SIGKILLed), and `Net::Deny`
//! + `--network none` contains a socket attempt inside the guest kernel.
//!
//! Uses `dispatch_with_sink` with a no-op audit sink so the test needs no
//! Postgres cluster — the container itself is the only external dependency.
//!
//! `[SKIP]`s cleanly when the `container` CLI / its system service / the
//! `kastellan/python-exec:dev` image are missing. Build the image first:
//!     scripts/workers/python-exec/build-image.sh

#![cfg(target_os = "macos")]

use std::sync::Arc;

use async_trait::async_trait;
use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch_with_sink, spawn_worker, AuditSink, WorkerSpec};
use kastellan_core::workers::python_exec::{container_mode_entry, DEFAULT_IMAGE};
use kastellan_db::DbError;
use kastellan_sandbox::{macos_container::MacosContainer, SandboxBackendKind, SandboxBackends};

/// A no-op audit sink: all inserts succeed immediately without touching any
/// database. Needed because `dispatch_with_sink` requires an `AuditSink` but
/// this test has no PG cluster.
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

/// Skip the test (via early-return) when Apple `container` isn't usable
/// on this host or the python-exec image is absent. Returns `true` when
/// the caller should skip.
fn skip_if_no_container_image() -> bool {
    if let Err(e) = MacosContainer::probe() {
        eprintln!("\n[SKIP] container probe failed: {e}\n");
        return true;
    }
    let listed = std::process::Command::new("container")
        .args(["image", "list"])
        .output();
    let has_image = matches!(
        listed,
        Ok(o) if String::from_utf8_lossy(&o.stdout).contains("python-exec")
    );
    if !has_image {
        eprintln!(
            "\n[SKIP] {DEFAULT_IMAGE} image not present; run \
             scripts/workers/python-exec/build-image.sh\n"
        );
        return true;
    }
    false
}

/// Resolve the container backend for the python-exec image.
fn container_backend() -> Arc<dyn kastellan_sandbox::SandboxBackend> {
    SandboxBackends::default_for_current_os()
        .resolve(Some(SandboxBackendKind::Container), Some(DEFAULT_IMAGE))
}

/// Spawn the worker in the VM, dispatch one `python.exec`, return the result.
///
/// Uses `dispatch_with_sink` + `NoopAuditSink` so no PG cluster is needed.
/// `container_mode_entry` sets `ephemeral_scratch: false` (scratch is the
/// in-VM `/tmp` tmpfs), so no `with_scratch` call.
async fn run_in_container(code: &str) -> serde_json::Value {
    let entry = container_mode_entry(
        std::path::PathBuf::from(
            kastellan_core::workers::python_exec::CONTAINER_WORKER_BIN,
        ),
        DEFAULT_IMAGE.to_string(),
        None,
    );
    let backend = container_backend();
    let program = entry.binary.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &entry.policy,
        program: &program,
        args: &[],
        wall_clock_ms: entry.wall_clock_ms,
    };
    let mut worker = spawn_worker(&*backend, &spec).expect("spawn worker in container");
    let result = dispatch_with_sink(
        &NoopAuditSink,
        &Vault::new(),
        &mut worker,
        "python-exec",
        "python.exec",
        serde_json::json!({ "code": code }),
    )
    .await;
    let _ = worker.close();
    result.expect("dispatch python.exec")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn python_exec_round_trips_through_container() {
    if skip_if_no_container_image() {
        return;
    }
    let out = run_in_container("print('hello-from-microvm')").await;
    let stdout = out["stdout"].as_str().unwrap_or_default();
    assert!(
        stdout.contains("hello-from-microvm"),
        "expected sentinel in stdout, got: {out}"
    );
    assert_eq!(out["exit_code"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn container_enforces_mem_cap() {
    if skip_if_no_container_image() {
        return;
    }
    // Allocate ~900 MiB — above the 512 MiB cap. The VM enforces the cap, so the
    // allocation fails; under macOS Seatbelt host mode it would SUCCEED (Seatbelt
    // has no memory primitive — the parity gap this micro-VM mode closes).
    let code = "x = bytearray(900 * 1024 * 1024); print(len(x))";
    let out = run_in_container(code).await;
    // The cap failure surfaces as a non-zero exit (observed: exit_code 1 with a
    // Python MemoryError traceback) or, if the cgroup OOM killer SIGKILLs the
    // child first, a null exit_code (status.code() is None). Accept either; reject
    // a clean 0 — a 0 would mean the 512 MiB cap was NOT enforced (the Seatbelt gap).
    let exit_indicates_oom = out["exit_code"].is_null()
        || out["exit_code"].as_i64().is_some_and(|c| c != 0);
    assert!(
        exit_indicates_oom,
        "expected an OOM failure exit (non-zero or null), got: {out}"
    );
    let stdout = out["stdout"].as_str().unwrap_or_default();
    assert!(
        !stdout.contains(&(900 * 1024 * 1024).to_string()),
        "the allocation print must not appear — it should be killed first: {out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn container_contains_socket_attempt() {
    if skip_if_no_container_image() {
        return;
    }
    // Net::Deny + --network none: a connect to a public IP cannot succeed in the VM.
    let code = "\
import socket, sys
try:
    s = socket.create_connection(('1.1.1.1', 443), timeout=2)
    print('CONNECTED')
except Exception as e:
    print('blocked', file=sys.stderr)
";
    let out = run_in_container(code).await;
    // Containment guard: a SUCCESSFUL connection prints "CONNECTED" to stdout, so
    // its ABSENCE is the invariant proving egress was denied. We deliberately do
    // NOT assert on exit_code: a denied connect surfaces inconsistently across
    // harness timing — sometimes a caught ENETUNREACH (child exits 0 with a
    // "blocked" stderr), sometimes the child is torn down mid-attempt (exit_code
    // null, empty streams). Both are legitimate "no egress" outcomes; only a real
    // connection would ever print "CONNECTED". Non-vacuity rests on
    // `python_exec_round_trips_through_container`: it proves this same harness
    // faithfully returns the child's stdout, so a connection that truly succeeded
    // could not hide. The result-object check rules out a broken dispatch path.
    assert!(
        out.get("exit_code").is_some(),
        "worker returned no result object — dispatch broken, not contained: {out}"
    );
    let stdout = out["stdout"].as_str().unwrap_or_default();
    assert!(!stdout.contains("CONNECTED"), "network must be denied (no CONNECTED): {out}");
}
