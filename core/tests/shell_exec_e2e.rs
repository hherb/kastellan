//! End-to-end test: agent core spawns the `shell-exec` worker under the
//! platform's sandbox backend and round-trips a JSON-RPC `shell.exec`
//! call **through `tool_host::dispatch`**. Phase 0 / 0b verification
//! that everything wires up: sandbox + protocol + tool_host + worker.
//! Runs on both Linux (bwrap) and macOS (Seatbelt).
//!
//! Why dispatch and not `worker.call(...)` directly: as of Option M
//! (Phase 1 entry), `WorkerCommand` is sealed via `pub(crate)`, so
//! out-of-crate callers — including these integration tests — cannot
//! invoke a worker without going through `dispatch`. The chokepoint
//! invariant (every tool/channel/routine action enters core through
//! `dispatch`) is therefore enforced at compile time, not just by code
//! review. The price of the seal: each test in this file brings up a
//! per-test Postgres cluster so dispatch's audit-log INSERT has
//! somewhere to land. `[SKIP]`s cleanly when PG, the supervisor, the
//! worker binary, or a working sandbox is missing.
//!
//! The PG bring-up boilerplate is duplicated with
//! `core/tests/audit_dispatch_e2e.rs` and `core/tests/supervisor_e2e.rs`
//! — see HANDOVER's open issue #15 for the planned `tests-common`
//! dev-dep crate that will host these helpers in one place.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use hhagent_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use hhagent_core::workspace::Workspace;
use hhagent_db::{
    build_initdb_argv, build_postgresql_auto_conf, default_pg_bin_dir_candidates,
    default_socket_dir, find_pg_bin_dir, InitDbOptions, PgConfigOptions,
};
use hhagent_protocol::codes;
use hhagent_sandbox::{Net, Profile, SandboxBackend, SandboxPolicy};
use hhagent_supervisor::specs::postgres_service_spec;
use hhagent_supervisor::{default_probe, default_supervisor, ServiceStatus, Supervisor};

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

fn skip_if_no_supervisor() -> bool {
    match default_probe() {
        Ok(()) => false,
        Err(e) => {
            eprintln!("\n[SKIP] supervisor probe failed: {e}\n");
            true
        }
    }
}

fn pg_bin_dir_or_skip() -> Option<PathBuf> {
    match find_pg_bin_dir(&default_pg_bin_dir_candidates()) {
        Ok(dir) => Some(dir),
        Err(e) => {
            eprintln!("\n[SKIP] no Postgres install found: {e}\n");
            None
        }
    }
}

fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}", std::process::id(), nanos)
}

fn unique_temp_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hhagent-{}-{}", label, unique_suffix()))
}

fn current_username() -> String {
    if let Some(u) = std::env::var_os("USER") {
        let s = u.to_string_lossy().into_owned();
        if !s.is_empty() {
            return s;
        }
    }
    if let Ok(out) = Command::new("whoami").output() {
        if out.status.success() {
            return String::from_utf8_lossy(&out.stdout).trim().to_string();
        }
    }
    "hhagent".into()
}

struct ServiceGuard {
    sup: Box<dyn Supervisor>,
    name: String,
}
impl Drop for ServiceGuard {
    fn drop(&mut self) {
        let _ = self.sup.stop(&self.name);
        let _ = self.sup.uninstall(&self.name);
    }
}

struct PathGuard {
    path: PathBuf,
}
impl Drop for PathGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn wait_for_status<F: Fn(ServiceStatus) -> bool>(
    sup: &dyn Supervisor,
    name: &str,
    predicate: F,
    timeout: Duration,
) -> Result<ServiceStatus, String> {
    let start = Instant::now();
    let mut last = sup.status(name).map_err(|e| format!("status: {e}"))?;
    loop {
        if predicate(last) {
            return Ok(last);
        }
        if start.elapsed() > timeout {
            return Err(format!("timeout {:?}; last={last:?}", timeout));
        }
        std::thread::sleep(Duration::from_millis(50));
        last = sup.status(name).map_err(|e| format!("status: {e}"))?;
    }
}

fn wait_for_socket(socket_dir: &Path, timeout: Duration) -> Result<(), String> {
    let target = socket_dir.join(".s.PGSQL.5432");
    let start = Instant::now();
    loop {
        if target.exists() {
            return Ok(());
        }
        if start.elapsed() > timeout {
            return Err(format!(
                "timeout {:?} waiting for {}",
                timeout,
                target.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Bring up a per-test PG cluster (initdb + auto.conf + supervisor
/// install + start). Returns the connection spec and the cleanup
/// guards. Same shape as the helper in `audit_dispatch_e2e.rs`.
///
/// Short label prefixes (`shx-d` / `shx-l`) keep the cluster's
/// `<data_dir>/sockets/.s.PGSQL.5432` path under the 108-byte
/// `sockaddr_un.sun_path` limit on Linux.
fn bring_up_pg_cluster(
    bin_dir: &Path,
    suffix: &str,
) -> (
    hhagent_db::conn::ConnectSpec,
    (ServiceGuard, PathGuard, PathGuard),
) {
    let postgres = bin_dir.join("postgres");
    let initdb = bin_dir.join("initdb");

    let data_root = unique_temp_root("shx-d");
    let data_guard = PathGuard {
        path: data_root.clone(),
    };
    let data_dir = data_root.join("data");
    let socket_dir = default_socket_dir(&data_dir);
    let log_dir = unique_temp_root("shx-l");
    std::fs::create_dir_all(&log_dir).expect("create log dir");
    let log_guard = PathGuard {
        path: log_dir.clone(),
    };

    let user = current_username();
    let argv = build_initdb_argv(
        &initdb,
        &InitDbOptions {
            data_dir: data_dir.clone(),
            username: user.clone(),
            ..InitDbOptions::default()
        },
    );
    let out = Command::new(&argv[0])
        .args(&argv[1..])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .output()
        .expect("spawn initdb");
    assert!(
        out.status.success(),
        "initdb failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    std::fs::create_dir(&socket_dir).expect("create socket dir");
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&socket_dir).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&socket_dir, perms).unwrap();
    }
    std::fs::write(
        data_dir.join("postgresql.auto.conf"),
        build_postgresql_auto_conf(&PgConfigOptions {
            socket_dir: socket_dir.clone(),
            ..PgConfigOptions::default()
        }),
    )
    .expect("write postgresql.auto.conf");

    let mut spec = postgres_service_spec(&postgres, &data_dir, &log_dir);
    spec.name = format!("hhagent-supervisor-test-pg-shellexec-{suffix}");
    assert!(spec.name.len() <= 200);
    spec.stdout_log = Some(log_dir.join(format!("{}.out", spec.name)));
    spec.stderr_log = Some(log_dir.join(format!("{}.err", spec.name)));

    let sup = default_supervisor();
    let service_guard = ServiceGuard {
        sup: default_supervisor(),
        name: spec.name.clone(),
    };
    sup.install(&spec).expect("install pg");
    sup.start(&spec.name).expect("start pg");
    wait_for_status(
        sup.as_ref(),
        &spec.name,
        |s| s == ServiceStatus::Active,
        Duration::from_secs(15),
    )
    .expect("pg active");
    wait_for_socket(&socket_dir, Duration::from_secs(15)).expect("pg socket");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        sup.status(&spec.name).unwrap(),
        ServiceStatus::Active,
        "pg flap"
    );

    let conn_spec = hhagent_db::conn::ConnectSpec {
        socket_dir: socket_dir.clone(),
        user: user.clone(),
        database: hhagent_db::conn::DEFAULT_APPLICATION_DB.to_string(),
    };
    (conn_spec, (service_guard, data_guard, log_guard))
}

/// Async helper: run the probe (apply migrations 0001 + 0002 + 0003 +
/// 0004) and open a runtime-role pool. Each shell-exec test calls this
/// inside its tokio runtime block_on. The probe writes one bring-up
/// audit row; the dispatcher writes one row per call on top.
async fn probe_and_pool(
    conn_spec: &hhagent_db::conn::ConnectSpec,
) -> sqlx::PgPool {
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

/// Skip-or-spawn helper for the three "no workspace" tests. Returns
/// `None` if any precondition is missing (PG, supervisor, sandbox,
/// worker binary). On `Some`, the caller has a working tokio runtime,
/// pool, and a freshly-spawned worker plus the cleanup guards (which
/// must outlive the test body).
fn ready_or_skip(
    allowlist: &[&str],
) -> Option<TestEnv> {
    if skip_if_no_supervisor() {
        return None;
    }
    if skip_if_sandbox_unavailable() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let worker_path = worker_binary();
    if !worker_path.exists() {
        eprintln!("\n[SKIP] worker binary not built; run cargo build --workspace\n");
        return None;
    }

    let suffix = unique_suffix();
    let (conn_spec, guards) = bring_up_pg_cluster(&bin_dir, &suffix);

    Some(TestEnv {
        conn_spec,
        worker_path,
        allowlist: allowlist.iter().map(|s| s.to_string()).collect(),
        _guards: guards,
    })
}

/// Owned test scaffolding: owns the cluster guards (so PG stays up
/// until the test body returns), the worker binary path, and the
/// allowlist. Each test constructs the runtime + pool + worker
/// inside its async block from these values.
struct TestEnv {
    conn_spec: hhagent_db::conn::ConnectSpec,
    worker_path: PathBuf,
    allowlist: Vec<String>,
    _guards: (ServiceGuard, PathGuard, PathGuard),
}

#[test]
fn echo_round_trip_through_sandboxed_worker() {
    let env = match ready_or_skip(&[ECHO_PATH]) {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.conn_spec).await;
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
        let pool = probe_and_pool(&env.conn_spec).await;
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
        let pool = probe_and_pool(&env.conn_spec).await;
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
        let pool = probe_and_pool(&env.conn_spec).await;
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
