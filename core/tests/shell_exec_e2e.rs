//! End-to-end test: agent core spawns the `shell-exec` worker under the
//! Linux bwrap backend and round-trips a JSON-RPC `shell.exec` call. This is
//! the Phase 0 verification that everything wires up: sandbox + protocol +
//! tool_host + worker.

#![cfg(target_os = "linux")]

use std::path::PathBuf;

use hhagent_core::tool_host::{spawn_worker, WorkerSpec};
use hhagent_protocol::codes;
use hhagent_sandbox::{linux_bwrap::LinuxBwrap, Net, Profile, SandboxPolicy};

/// Locate the worker binary. Cargo builds it into the same target dir as
/// this test when `cargo test --workspace` is run; resolve via
/// `CARGO_MANIFEST_DIR` so we don't depend on the working directory.
fn worker_binary() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.parent().unwrap().join("target"));
    target.join("debug").join("hhagent-worker-shell-exec")
}

fn skip_if_no_userns() -> bool {
    if let Err(e) = LinuxBwrap::probe() {
        eprintln!("\n[SKIP] bwrap probe failed: {e}\n");
        return true;
    }
    false
}

/// Build a sandbox policy that lets the shell-exec worker run and exec
/// `/usr/bin/echo`. We expose the worker binary read-only and pass the
/// allowlist via env.
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
    if skip_if_no_userns() {
        return;
    }
    let worker = worker_binary();
    assert!(
        worker.exists(),
        "worker binary not found at {worker:?} — run `cargo build --workspace` first"
    );

    let policy = policy_for_shell_exec(&worker, &["/usr/bin/echo"]);
    let backend = LinuxBwrap::new();
    let worker_str = worker.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &policy,
        program: &worker_str,
        args: &[],
    };
    let mut client = spawn_worker(&backend, &spec).expect("spawn shell-exec under bwrap");

    let result = client
        .call(
            "shell.exec",
            serde_json::json!({"argv": ["/usr/bin/echo", "round-trip-ok"]}),
        )
        .expect("shell.exec round trip");

    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["stdout"].as_str().unwrap().trim_end(), "round-trip-ok");
    let _ = client.close();
}

#[test]
fn argv_outside_allowlist_is_rejected_by_worker_policy() {
    if skip_if_no_userns() {
        return;
    }
    let worker = worker_binary();
    if !worker.exists() {
        eprintln!("[SKIP] worker binary not built");
        return;
    }
    // Allowlist contains echo but the call asks for cat — worker must reject
    // before it even tries to exec.
    let policy = policy_for_shell_exec(&worker, &["/usr/bin/echo"]);
    let backend = LinuxBwrap::new();
    let worker_str = worker.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &policy,
        program: &worker_str,
        args: &[],
    };
    let mut client = spawn_worker(&backend, &spec).expect("spawn shell-exec under bwrap");
    let err = client
        .call(
            "shell.exec",
            serde_json::json!({"argv": ["/usr/bin/cat", "/etc/passwd"]}),
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
    if skip_if_no_userns() {
        return;
    }
    let worker = worker_binary();
    if !worker.exists() {
        eprintln!("[SKIP] worker binary not built");
        return;
    }
    let policy = policy_for_shell_exec(&worker, &["/usr/bin/echo"]);
    let backend = LinuxBwrap::new();
    let worker_str = worker.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &policy,
        program: &worker_str,
        args: &[],
    };
    let mut client = spawn_worker(&backend, &spec).expect("spawn shell-exec under bwrap");
    let err = client
        .call("does.not.exist", serde_json::json!({}))
        .expect_err("unknown method must error");
    assert!(format!("{err}").contains(&format!("{}", codes::METHOD_NOT_FOUND)));
    let _ = client.close();
}
