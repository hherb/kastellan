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

use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch_with_sink, spawn_worker, WorkerSpec};
use kastellan_core::workers::python_exec::{container_mode_entry, DEFAULT_IMAGE};
use kastellan_sandbox::{macos_container::MacosContainer, SandboxBackendKind, SandboxBackends};
use kastellan_tests_common::NoopAuditSink;

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
///
/// Layering note: this resolves the backend directly (like
/// `lifecycle_container_routing_e2e.rs`) rather than threading the entry's
/// `sandbox_backend`/`container_image` through the daemon's spec→backend
/// wiring. That field-mapping is covered separately — the manifest unit tests
/// (`resolve_uses_container_backend_when_flag_set`) assert `container_mode_entry`
/// produces those fields, and `lifecycle_container_routing_e2e.rs` proves the
/// lifecycle manager honors `sandbox_backend == Some(Container)`. This e2e's job
/// is the *runtime* proof: real worker + real VM + the strict policy's flags.
fn container_backend() -> Arc<dyn kastellan_sandbox::SandboxBackend> {
    SandboxBackends::default_for_current_os()
        .resolve(Some(SandboxBackendKind::Container), Some(DEFAULT_IMAGE))
}

/// Spawn the worker in the VM, dispatch one `python.exec` with the given
/// JSON-RPC params object, return the result.
///
/// Uses `dispatch_with_sink` + `NoopAuditSink` so no PG cluster is needed.
/// `container_mode_entry` sets `ephemeral_scratch: false` (scratch is the
/// in-VM `/tmp` tmpfs), so no `with_scratch` call.
async fn dispatch_in_container(payload: serde_json::Value) -> serde_json::Value {
    let entry = container_mode_entry(
        std::path::PathBuf::from(
            kastellan_core::workers::python_exec::CONTAINER_WORKER_BIN,
        ),
        DEFAULT_IMAGE.to_string(),
        None,
        kastellan_core::worker_lifecycle::Lifecycle::SingleUse,
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
        payload,
    )
    .await;
    let _ = worker.close();
    result.expect("dispatch python.exec")
}

/// Convenience: dispatch code-only (no `params`).
async fn run_in_container(code: &str) -> serde_json::Value {
    dispatch_in_container(serde_json::json!({ "code": code })).await
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
    // NOTE: this test's non-vacuity DEPENDS on the round-trip test above staying
    // live (not `#[ignore]`d / removed) — if it ever is, a worker that never ran
    // would also print no "CONNECTED" and this guard would weaken. Keep them paired.
    assert!(
        out.get("exit_code").is_some(),
        "worker returned no result object — dispatch broken, not contained: {out}"
    );
    let stdout = out["stdout"].as_str().unwrap_or_default();
    assert!(!stdout.contains("CONNECTED"), "network must be denied (no CONNECTED): {out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn container_large_param_round_trips_via_file_channel() {
    if skip_if_no_container_image() {
        return;
    }
    // A >64 KiB params payload exceeds the inline env threshold, so the worker
    // takes the FILE channel: it writes `<scratch>/params.json` and points the
    // child at it via KASTELLAN_PYTHON_PARAMS_FILE. In container mode scratch
    // is the in-VM `/tmp` tmpfs (`--tmpfs /tmp`, writable even under `--read-only`)
    // and the worker runs as `nobody`. This proves that write path actually works
    // in the VM — the one fail-CLOSED path host mode covers but container mode did
    // not (`write_params_file(...)?` aborts the whole exec on any IO error, so a
    // tmpfs that `nobody` couldn't write would surface as a non-zero exit here).
    //
    // 100_000 bytes ≫ the 64 KiB inline threshold, ≪ the 1 MiB default file
    // ceiling → the File channel. The agent reads the file when the env var is
    // set, else falls back to the inline var (which would be the "{}" default →
    // KeyError → non-zero exit if the file channel silently failed).
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
    let out = dispatch_in_container(
        serde_json::json!({ "code": code, "params": { "blob": blob } }),
    )
    .await;
    assert_eq!(
        out["exit_code"].as_i64(),
        Some(0),
        "file-channel write to the in-VM tmpfs must succeed as nobody; stderr: {}",
        out["stderr"]
    );
    assert_eq!(
        out["stdout"].as_str().unwrap_or_default().trim_end(),
        "100000 AAAA AAAA",
        "agent must read the full 100 KiB payload via the in-VM file channel: {out}"
    );
}
