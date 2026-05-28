//! End-to-end test: agent core spawns the `shell-exec` worker under the
//! platform's sandbox backend and round-trips a JSON-RPC `shell.exec`
//! call **through `tool_host::dispatch`**. Phase 0 / 0b verification
//! that everything wires up: sandbox + protocol + tool_host + worker.
//! Runs on both Linux (bwrap) and macOS (Seatbelt).
//!
//! Why dispatch and not `worker.call(...)` directly: as of Option M
//! (Phase 1 entry), `WorkerCommand`'s constructor and `SupervisedWorker::call`
//! are module-private to `tool_host` (originally `pub(crate)`; tightened to
//! module-private 2026-05-13 via issue #16), so out-of-crate callers —
//! including these integration tests — cannot invoke a worker without
//! going through `dispatch`. The chokepoint invariant (every
//! tool/channel/routine action enters core through `dispatch`) is therefore
//! enforced at compile time, not just by code review. The price of the
//! seal: each test in this file brings up a
//! per-test Postgres cluster so dispatch's audit-log INSERT has
//! somewhere to land. `[SKIP]`s cleanly when PG, the supervisor, the
//! worker binary, or a working sandbox is missing.
//!
//! Bring-up scaffolding (PG cluster, sandbox probe, binary discovery,
//! RAII guards) is hoisted into `hhagent-tests-common` as of issue
//! #15.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;

use hhagent_core::secrets::Vault;
use hhagent_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use hhagent_core::workspace::Workspace;
use hhagent_protocol::codes;
use hhagent_tests_common::{
    backend, bring_up_pg_cluster, pg_bin_dir_or_skip, policy_for_shell_exec, shell_exec_worker_binary,
    skip_if_no_supervisor, skip_if_sandbox_unavailable, unique_suffix, PgCluster,
};

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

/// Async helper: run the probe (apply migrations 0001 + 0002 + 0003 +
/// 0004) and open a runtime-role pool. Each shell-exec test calls this
/// inside its tokio runtime block_on. The probe writes one bring-up
/// audit row; the dispatcher writes one row per call on top.
async fn probe_and_pool(conn_spec: &hhagent_db::conn::ConnectSpec) -> sqlx::PgPool {
    hhagent_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "shell-exec-e2e"}),
    )
    .await
    .expect("probe run");
    hhagent_db::pool::connect_runtime_pool(conn_spec)
        .await
        .expect("connect runtime pool")
}

/// Multi-thread tokio runtime — `dispatch` uses
/// `tokio::task::block_in_place` around the synchronous `worker.call`,
/// which panics on a `current_thread` runtime. One worker thread is
/// enough; tests don't need parallelism inside a single test body.
fn dispatch_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime")
}

/// Owned test scaffolding: the cluster handle (RAII-cleaned-up on
/// drop), the worker binary path, and the allowlist. Each test
/// constructs the runtime + pool + worker inside its async block.
struct TestEnv {
    cluster: PgCluster,
    worker_path: PathBuf,
    allowlist: Vec<String>,
}

/// Skip-or-spawn helper. Returns `None` if any precondition is missing
/// (PG, supervisor, sandbox, worker binary). On `Some`, the caller has
/// a working cluster handle whose `Drop` will tear down the per-test
/// service + temp dirs when the test body returns.
fn ready_or_skip(allowlist: &[&str]) -> Option<TestEnv> {
    if skip_if_no_supervisor() {
        return None;
    }
    if skip_if_sandbox_unavailable() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let worker_path = shell_exec_worker_binary();
    if !worker_path.exists() {
        eprintln!("\n[SKIP] worker binary not built; run cargo build --workspace\n");
        return None;
    }

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "shx-d",
        "shx-l",
        &format!("hhagent-supervisor-test-pg-shellexec-{suffix}"),
    );

    Some(TestEnv {
        cluster,
        worker_path,
        allowlist: allowlist.iter().map(|s| s.to_string()).collect(),
    })
}

#[test]
fn echo_round_trip_through_sandboxed_worker() {
    let env = match ready_or_skip(&[ECHO_PATH]) {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let allowlist: Vec<&str> = env.allowlist.iter().map(String::as_str).collect();
        let policy = policy_for_shell_exec(&env.worker_path, &allowlist);
        let backend = backend();
        let worker_str = env.worker_path.to_string_lossy().into_owned();
        let spec = WorkerSpec {
            policy: &policy,
            program: &worker_str,
            args: &[],
            wall_clock_ms: None,
        };
        let mut sworker = spawn_worker(&*backend, &spec).expect("spawn shell-exec under sandbox");

        let result = dispatch(
            &pool,
            &Vault::new(),
            &mut sworker,
            "shell-exec",
            "shell.exec",
            serde_json::json!({"argv": [ECHO_PATH, "round-trip-ok"]}),
        )
        .await
        .expect("shell.exec round trip");

        assert_eq!(result["exit_code"], 0);
        assert_eq!(
            result["stdout"].as_str().unwrap().trim_end(),
            "round-trip-ok"
        );

        let _ = sworker.close();
        pool.close().await;
    });
}

#[test]
fn argv_outside_allowlist_is_rejected_by_worker_policy() {
    let env = match ready_or_skip(&[ECHO_PATH]) {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let allowlist: Vec<&str> = env.allowlist.iter().map(String::as_str).collect();
        let policy = policy_for_shell_exec(&env.worker_path, &allowlist);
        let backend = backend();
        let worker_str = env.worker_path.to_string_lossy().into_owned();
        let spec = WorkerSpec {
            policy: &policy,
            program: &worker_str,
            args: &[],
            wall_clock_ms: None,
        };
        let mut sworker = spawn_worker(&*backend, &spec).expect("spawn shell-exec under sandbox");

        let err = dispatch(
            &pool,
            &Vault::new(),
            &mut sworker,
            "shell-exec",
            "shell.exec",
            serde_json::json!({"argv": ["/bin/cat", "/etc/master.passwd"]}),
        )
        .await
        .expect_err("non-allowlisted argv must be denied");

        // `dispatch` wraps the worker's `ClientError` in
        // `ToolHostError::Protocol(...)`; the inner Display still
        // includes the JSON-RPC numeric code, so the same assertion
        // shape from the pre-Option-M version still applies.
        let msg = format!("{err}");
        assert!(
            msg.contains(&format!("{}", codes::POLICY_DENIED)),
            "expected POLICY_DENIED ({}), got: {msg}",
            codes::POLICY_DENIED
        );

        let _ = sworker.close();
        pool.close().await;
    });
}

#[test]
fn unknown_method_yields_method_not_found() {
    let env = match ready_or_skip(&[ECHO_PATH]) {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let allowlist: Vec<&str> = env.allowlist.iter().map(String::as_str).collect();
        let policy = policy_for_shell_exec(&env.worker_path, &allowlist);
        let backend = backend();
        let worker_str = env.worker_path.to_string_lossy().into_owned();
        let spec = WorkerSpec {
            policy: &policy,
            program: &worker_str,
            args: &[],
            wall_clock_ms: None,
        };
        let mut sworker = spawn_worker(&*backend, &spec).expect("spawn shell-exec under sandbox");

        let err = dispatch(
            &pool,
            &Vault::new(),
            &mut sworker,
            "shell-exec",
            "does.not.exist",
            serde_json::json!({}),
        )
        .await
        .expect_err("unknown method must error");

        assert!(
            format!("{err}").contains(&format!("{}", codes::METHOD_NOT_FOUND)),
            "expected METHOD_NOT_FOUND ({}), got: {err}",
            codes::METHOD_NOT_FOUND
        );

        let _ = sworker.close();
        pool.close().await;
    });
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
/// 5. Calls `shell.exec` (via `tool_host::dispatch`) to copy
///    `in/source.txt` → `out/dest.txt` *inside* the sandbox. A successful
///    copy proves: (a) bind-mounts cover the workspace, (b) on Linux,
///    Landlock granted write to `out/` and exec for `cp` from `/usr/bin`,
///    (c) seccomp didn't kill any of `cp`'s syscalls.
/// 6. Reads the file back from the host and asserts byte-equality with the
///    staged content.
/// 7. Drops the `Workspace` and asserts the entire `<root>/<task_id>/` tree
///    is gone — proving Drop semantics actually wipe what a worker wrote, not
///    just what `Workspace::new` created.
#[test]
fn workspace_dir_is_writable_during_call_and_wiped_on_drop() {
    let env = match ready_or_skip(&[CP_PATH]) {
        Some(e) => e,
        None => return,
    };

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

    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let allowlist: Vec<&str> = env.allowlist.iter().map(String::as_str).collect();

        // Build the policy: worker readable, cp allowlisted, workspace writable.
        let mut policy = policy_for_shell_exec(&env.worker_path, &allowlist);
        ws.extend_policy(&mut policy);
        assert_eq!(
            policy.fs_write.len(),
            3,
            "extend_policy must add exactly in/out/tmp"
        );

        let backend = backend();
        let worker_str = env.worker_path.to_string_lossy().into_owned();
        let spec = WorkerSpec {
            policy: &policy,
            program: &worker_str,
            args: &[],
            wall_clock_ms: None,
        };
        let mut sworker = spawn_worker(&*backend, &spec).expect("spawn shell-exec under sandbox");

        let result = dispatch(
            &pool,
            &Vault::new(),
            &mut sworker,
            "shell-exec",
            "shell.exec",
            serde_json::json!({
                "argv": [
                    CP_PATH,
                    source_path.to_string_lossy(),
                    dest_path.to_string_lossy(),
                ]
            }),
        )
        .await
        .expect("cp round trip");

        assert_eq!(
            result["exit_code"], 0,
            "cp must succeed inside sandbox; got {result}"
        );

        let _ = sworker.close();
        pool.close().await;
    });

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
