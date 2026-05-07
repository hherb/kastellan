//! End-to-end test: agent core spawns the `shell-exec` worker under the
//! platform's sandbox backend and round-trips a JSON-RPC `shell.exec` call.
//! Phase 0 / 0b verification that everything wires up: sandbox + protocol +
//! tool_host + worker. Runs on both Linux (bwrap) and macOS (Seatbelt).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;

use hhagent_core::tool_host::{spawn_worker, WorkerSpec};
use hhagent_core::workspace::Workspace;
use hhagent_protocol::codes;
use hhagent_sandbox::{Net, Profile, SandboxBackend, SandboxPolicy};

// On Linux /usr/bin/echo exists; on macOS the standalone echo binary lives at
// /bin/echo (there is no /usr/bin/echo on macOS).
#[cfg(target_os = "linux")]
const ECHO_PATH: &str = "/usr/bin/echo";

#[cfg(target_os = "macos")]
const ECHO_PATH: &str = "/bin/echo";

// `cp` is the easiest cross-platform way for shell-exec (no shell
// interpretation, no stdin) to write into the workspace's `out/` dir:
// it just opens the source, opens the dest with `O_CREAT`, copies bytes,
// closes. Path differs the same way `echo` does.
#[cfg(target_os = "linux")]
const CP_PATH: &str = "/usr/bin/cp";

#[cfg(target_os = "macos")]
const CP_PATH: &str = "/bin/cp";

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
        wall_clock_ms: None,
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
        wall_clock_ms: None,
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
        wall_clock_ms: None,
    };
    let mut client = spawn_worker(&*backend, &spec).expect("spawn shell-exec under sandbox");
    let err = client
        .call("does.not.exist", serde_json::json!({}))
        .expect_err("unknown method must error");
    assert!(format!("{err}").contains(&format!("{}", codes::METHOD_NOT_FOUND)));
    let _ = client.close();
}

/// End-to-end check that `Workspace` is wired through the same path used by
/// real workers. The test:
///
/// 1. Creates a `Workspace` under a per-test temp root (so we don't pollute
///    `~/.hhagent/`).
/// 2. Stages a known string at `<ws>/in/source.txt` from the host.
/// 3. Calls `extend_policy` to add `in/`, `out/`, `tmp/` to `policy.fs_write`.
///    On Linux this single call also flows into the worker-side Landlock
///    filter via `derive_lockdown_env`, so host (bwrap bind-mount) and worker
///    (Landlock allow-list) cannot disagree about what the worker may write.
/// 4. Spawns shell-exec under the platform sandbox, allowlisting only `cp`.
/// 5. Calls `shell.exec` to copy `in/source.txt` → `out/dest.txt` *inside* the
///    sandbox. A successful copy proves: (a) bind-mounts cover the workspace,
///    (b) on Linux, Landlock granted write to `out/` and exec for `cp` from
///    `/usr/bin`, (c) seccomp didn't kill any of `cp`'s syscalls.
/// 6. Reads the file back from the host and asserts byte-equality with the
///    staged content.
/// 7. Drops the `Workspace` and asserts the entire `<root>/<task_id>/` tree
///    is gone — proving Drop semantics actually wipe what a worker wrote, not
///    just what `Workspace::new` created.
#[test]
fn workspace_dir_is_writable_during_call_and_wiped_on_drop() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let worker = worker_binary();
    if !worker.exists() {
        eprintln!("[SKIP] worker binary not built");
        return;
    }

    // Per-test root keeps tests independent (so two parallel runs don't
    // collide on the same task id) and avoids touching `~/.hhagent/`.
    let test_root = std::env::temp_dir().join(format!(
        "hhagent-e2e-workspace-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&test_root);

    let ws = Workspace::with_root(&test_root, "task-e2e").expect("create workspace");
    let task_root = ws.root().to_path_buf();
    let source_path = ws.inputs().join("source.txt");
    let dest_path = ws.outputs().join("dest.txt");

    // Stage host-side input. The worker reads it through the bind-mounted
    // `in/` dir (writable here, but `cp` only reads it).
    let payload = b"workspace-e2e-payload\n";
    std::fs::write(&source_path, payload).expect("stage source.txt");

    // Build the policy: worker readable, cp allowlisted, workspace writable.
    let allow_json = serde_json::to_string(&[CP_PATH]).expect("serialize allowlist");
    let mut policy = SandboxPolicy {
        fs_read: vec![worker.clone()],
        fs_write: vec![],
        net: Net::Deny,
        cpu_ms: 5_000,
        mem_mb: 256,
        profile: Profile::WorkerStrict,
        env: vec![("HHAGENT_SHELL_ALLOWLIST".to_string(), allow_json)],
    };
    ws.extend_policy(&mut policy);
    assert_eq!(
        policy.fs_write.len(),
        3,
        "extend_policy must add exactly in/out/tmp"
    );

    let backend = backend();
    let worker_str = worker.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &policy,
        program: &worker_str,
        args: &[],
        wall_clock_ms: None,
    };
    let mut client = spawn_worker(&*backend, &spec).expect("spawn shell-exec under sandbox");

    let result = client
        .call(
            "shell.exec",
            serde_json::json!({
                "argv": [
                    CP_PATH,
                    source_path.to_string_lossy(),
                    dest_path.to_string_lossy(),
                ]
            }),
        )
        .expect("cp round trip");

    assert_eq!(
        result["exit_code"], 0,
        "cp must succeed inside sandbox; got {result}"
    );
    let _ = client.close();

    // Worker is gone. Verify the host can read the artifact `cp` wrote.
    let observed = std::fs::read(&dest_path).expect("worker should have created dest.txt");
    assert_eq!(
        observed, payload,
        "byte-for-byte round-trip from host -> sandboxed worker -> host"
    );
    assert!(task_root.exists(), "task tree should still exist before drop");

    drop(ws);

    assert!(
        !task_root.exists(),
        "Workspace::Drop must recursively wipe the task tree, including files the worker wrote (out/dest.txt)"
    );

    // The Workspace owns only `<root>/<task_id>/`; the test root above it
    // is the test's responsibility to remove.
    let _ = std::fs::remove_dir_all(&test_root);
}
