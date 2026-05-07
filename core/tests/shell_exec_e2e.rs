//! End-to-end test: agent core spawns the `shell-exec` worker under the
//! platform's sandbox backend and round-trips a JSON-RPC `shell.exec` call.
//! Phase 0 / 0b verification that everything wires up: sandbox + protocol +
//! tool_host + worker. Runs on both Linux (bwrap) and macOS (Seatbelt).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;

use hhagent_core::tool_host::{spawn_worker, WorkerSpec};
use hhagent_protocol::codes;
use hhagent_sandbox::{Net, Profile, SandboxBackend, SandboxPolicy};

// On Linux /usr/bin/echo exists; on macOS the standalone echo binary lives at
// /bin/echo (there is no /usr/bin/echo on macOS).
#[cfg(target_os = "linux")]
const ECHO_PATH: &str = "/usr/bin/echo";

#[cfg(target_os = "macos")]
const ECHO_PATH: &str = "/bin/echo";

#[cfg(target_os = "linux")]
fn skip_if_sandbox_unavailable() -> bool {
    use hhagent_sandbox::linux_bwrap::LinuxBwrap;
    if let Err(e) = LinuxBwrap::probe() {
        eprintln!("\n[SKIP] bwrap probe failed: {e}\n");
        return true;
    }
    false
}

#[cfg(target_os = "macos")]
fn skip_if_sandbox_unavailable() -> bool {
    use hhagent_sandbox::macos_seatbelt::MacosSeatbelt;
    if let Err(e) = MacosSeatbelt::probe() {
        eprintln!("\n[SKIP] sandbox-exec probe failed: {e}\n");
        return true;
    }
    false
}

#[cfg(target_os = "linux")]
fn backend() -> Box<dyn SandboxBackend> {
    Box::new(hhagent_sandbox::linux_bwrap::LinuxBwrap::new())
}

#[cfg(target_os = "macos")]
fn backend() -> Box<dyn SandboxBackend> {
    Box::new(hhagent_sandbox::macos_seatbelt::MacosSeatbelt::new())
}

/// Locate the worker binary. Same path layout on Linux and macOS today —
/// `target/debug/<name>`. This helper exists primarily so the next reader
/// has a single place to edit when production deployment establishes a
/// stable install location for workers.
fn worker_binary() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.parent().unwrap().join("target"));
    target.join("debug").join("hhagent-worker-shell-exec")
}

fn policy_for_shell_exec(worker: &PathBuf, allowlist: &[&str]) -> SandboxPolicy {
    let allow_json = serde_json::to_string(allowlist).unwrap();
    SandboxPolicy {
        fs_read: vec![worker.clone()],
        fs_write: vec![],
        net: Net::Deny,
        cpu_ms: 5_000,
        mem_mb: 256,
        profile: Profile::WorkerStrict,
        env: vec![("HHAGENT_SHELL_ALLOWLIST".to_string(), allow_json)],
    }
}

#[test]
fn echo_round_trip_through_sandboxed_worker() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let worker = worker_binary();
    assert!(
        worker.exists(),
        "worker binary not found at {worker:?} — run `cargo build --workspace` first"
    );

    let policy = policy_for_shell_exec(&worker, &[ECHO_PATH]);
    let backend = backend();
    let worker_str = worker.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &policy,
        program: &worker_str,
        args: &[],
    };
    let mut client = spawn_worker(&*backend, &spec).expect("spawn shell-exec under sandbox");

    let result = client
        .call(
            "shell.exec",
            serde_json::json!({"argv": [ECHO_PATH, "round-trip-ok"]}),
        )
        .expect("shell.exec round trip");

    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["stdout"].as_str().unwrap().trim_end(), "round-trip-ok");
    let _ = client.close();
}

#[test]
fn argv_outside_allowlist_is_rejected_by_worker_policy() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let worker = worker_binary();
    if !worker.exists() {
        eprintln!("[SKIP] worker binary not built");
        return;
    }
    let policy = policy_for_shell_exec(&worker, &[ECHO_PATH]);
    let backend = backend();
    let worker_str = worker.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &policy,
        program: &worker_str,
        args: &[],
    };
    let mut client = spawn_worker(&*backend, &spec).expect("spawn shell-exec under sandbox");
    let err = client
        .call(
            "shell.exec",
            serde_json::json!({"argv": ["/bin/cat", "/etc/master.passwd"]}),
        )
        .expect_err("non-allowlisted argv must be denied");
    let msg = format!("{err}");
    assert!(
        msg.contains(&format!("{}", codes::POLICY_DENIED)),
        "expected POLICY_DENIED ({}), got: {msg}",
        codes::POLICY_DENIED
    );
    let _ = client.close();
}

#[test]
fn unknown_method_yields_method_not_found() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let worker = worker_binary();
    if !worker.exists() {
        eprintln!("[SKIP] worker binary not built");
        return;
    }
    let policy = policy_for_shell_exec(&worker, &[ECHO_PATH]);
    let backend = backend();
    let worker_str = worker.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &policy,
        program: &worker_str,
        args: &[],
    };
    let mut client = spawn_worker(&*backend, &spec).expect("spawn shell-exec under sandbox");
    let err = client
        .call("does.not.exist", serde_json::json!({}))
        .expect_err("unknown method must error");
    assert!(format!("{err}").contains(&format!("{}", codes::METHOD_NOT_FOUND)));
    let _ = client.close();
}
