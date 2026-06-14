//! End-to-end test: agent core spawns the `python-exec` worker under the
//! platform's real sandbox backend and round-trips `python.exec` calls
//! **through `tool_host::dispatch`** (the sealed chokepoint — see
//! `shell_exec_e2e.rs` for why dispatch and not `worker.call`).
//!
//! What this pins beyond the worker's own `real_python.rs` suite: the
//! **production policy inside the real jail** — `python_exec_entry`'s
//! `Net::Deny` + `Profile::WorkerStrict` actually contain the CPython
//! child (a socket attempt dies), and the explicit
//! `KASTELLAN_LANDLOCK_RW=["/tmp"]` grant really lets code write the
//! jail's ephemeral scratch.
//!
//! `[SKIP]`s cleanly when PG, the supervisor, the sandbox, the worker
//! binary, or a python3 interpreter is missing.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;

use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use kastellan_core::workers::python_exec::{
    interpreter_extra_lib_dirs, python_exec_entry, PYTHON_CANDIDATES,
};
use kastellan_tests_common::{
    backend, bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor,
    skip_if_sandbox_unavailable, unique_suffix, workspace_target_binary, PgCluster,
};

/// The manifest's own per-OS candidate cascade (single source of truth —
/// on macOS that list deliberately excludes the `/usr/bin/python3` xcrun
/// shim, which cannot run inside the jail). Canonicalized like the
/// manifest does, so the framework-layout fs_read derivation sees the
/// real path.
fn find_python() -> Option<PathBuf> {
    for c in PYTHON_CANDIDATES {
        let p = PathBuf::from(c);
        if p.is_file() {
            return Some(std::fs::canonicalize(&p).unwrap_or(p));
        }
    }
    eprintln!("\n[SKIP] no python3 interpreter on this host\n");
    None
}

async fn probe_and_pool(conn_spec: &kastellan_db::conn::ConnectSpec) -> sqlx::PgPool {
    kastellan_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "python-exec-e2e"}),
    )
    .await
    .expect("probe run");
    kastellan_db::pool::connect_runtime_pool(conn_spec)
        .await
        .expect("connect runtime pool")
}

fn dispatch_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime")
}

struct TestEnv {
    cluster: PgCluster,
    worker_path: PathBuf,
    python: PathBuf,
}

fn ready_or_skip() -> Option<TestEnv> {
    if skip_if_no_supervisor() {
        return None;
    }
    if skip_if_sandbox_unavailable() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let worker_path = workspace_target_binary("kastellan-worker-python-exec");
    if !worker_path.exists() {
        eprintln!("\n[SKIP] worker binary not built; run cargo build --workspace\n");
        return None;
    }
    let python = find_python()?;

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "pyx-d",
        "pyx-l",
        &format!("kastellan-supervisor-test-pg-pythonexec-{suffix}"),
    );

    Some(TestEnv {
        cluster,
        worker_path,
        python,
    })
}

/// One jailed `python.exec` dispatch under the **production** policy
/// (`python_exec_entry`), returning the result value.
async fn exec_in_jail(
    pool: &sqlx::PgPool,
    env: &TestEnv,
    code: &str,
) -> Result<serde_json::Value, kastellan_core::tool_host::ToolHostError> {
    // Mirror the manifest: bind the interpreter's out-of-prefix shared-lib dirs
    // (issue #284) so a pyenv/Homebrew-linked interpreter dyld-loads in the jail
    // without a manual KASTELLAN_*_EXTRA_FS_READ. Shares the manifest's seed
    // logic (interpreter_deps) so the two can't drift.
    let interpreter_lib_dirs = interpreter_extra_lib_dirs(
        &env.python,
        &|p| p.exists(),
        &|p| std::fs::canonicalize(p).ok(),
        &kastellan_core::workers::interpreter_deps::resolve_deps_via_tool,
    );
    let entry = python_exec_entry(
        env.worker_path.clone(),
        env.python.clone(),
        interpreter_lib_dirs,
    );
    let backend = backend();
    let worker_str = env.worker_path.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &entry.policy,
        program: &worker_str,
        args: &[],
        wall_clock_ms: None,
    };
    let mut sworker = spawn_worker(&*backend, &spec).expect("spawn python-exec under sandbox");
    let result = dispatch(
        pool,
        &Vault::new(),
        &mut sworker,
        "python-exec",
        "python.exec",
        serde_json::json!({ "code": code }),
    )
    .await;
    let _ = sworker.close();
    result
}

#[test]
fn print_round_trip_through_sandboxed_worker() {
    let env = match ready_or_skip() {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let r = exec_in_jail(&pool, &env, "print(6 * 7)")
            .await
            .expect("python.exec round trip");
        assert_eq!(r["exit_code"], 0, "stderr: {}", r["stderr"]);
        assert_eq!(r["stdout"].as_str().unwrap().trim_end(), "42");
        pool.close().await;
    });
}

#[test]
fn socket_attempt_is_contained_by_the_jail() {
    let env = match ready_or_skip() {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        // Under seccomp `strict` the socket(2) syscall is not in the
        // allow-list → CPython dies with SIGSYS (exit_code null); under
        // Seatbelt the connect is denied → OSError (exit_code 1). Either
        // way: anything but success.
        let r = exec_in_jail(
            &pool,
            &env,
            "import socket\ns = socket.socket()\ns.connect(('127.0.0.1', 9))\nprint('escaped')",
        )
        .await
        .expect("dispatch itself must succeed");
        assert_ne!(r["exit_code"], 0, "socket attempt must not succeed: {r}");
        assert!(
            !r["stdout"].as_str().unwrap_or("").contains("escaped"),
            "network reached from inside the jail: {r}"
        );
        pool.close().await;
    });
}

#[test]
fn scratch_tmp_write_round_trip_inside_jail() {
    // macOS slice #1 has no writable scratch (Seatbelt deny-default with
    // fs_write = []) — the Linux jail's /tmp is an ephemeral tmpfs with
    // an explicit Landlock RW grant. See the design spec §2.3/§5.
    // The gate sits BEFORE ready_or_skip(): on darwin the `env` binding
    // must never exist (unused-variable → clippy -D warnings) and a PG
    // cluster must not be brought up just to be dropped.
    #[cfg(target_os = "macos")]
    {
        eprintln!("\n[SKIP] no writable scratch under Seatbelt in slice #1\n");
    }
    #[cfg(target_os = "linux")]
    {
        let env = match ready_or_skip() {
            Some(e) => e,
            None => return,
        };
        dispatch_runtime().block_on(async {
            let pool = probe_and_pool(&env.cluster.conn_spec).await;
            let code = concat!(
                "import tempfile\n",
                "with tempfile.NamedTemporaryFile('w+', delete=True) as f:\n",
                "    f.write('jail-scratch-ok')\n",
                "    f.flush()\n",
                "    f.seek(0)\n",
                "    print(f.read())\n",
            );
            let r = exec_in_jail(&pool, &env, code)
                .await
                .expect("python.exec round trip");
            assert_eq!(r["exit_code"], 0, "stderr: {}", r["stderr"]);
            assert_eq!(r["stdout"].as_str().unwrap().trim_end(), "jail-scratch-ok");
            pool.close().await;
        });
    }
}
