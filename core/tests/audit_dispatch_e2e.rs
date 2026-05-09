//! End-to-end smoke for `tool_host::dispatch` — the chokepoint
//! every Phase 0+ tool call goes through.
//!
//! What this test proves:
//!   1. `dispatch` makes the JSON-RPC call against a sandboxed
//!      shell-exec worker and returns the result verbatim.
//!   2. The same dispatch call writes one row into `audit_log` with
//!      `actor = "tool:shell-exec"`, `action = "<method>"`, and a
//!      payload carrying `req`, `result`, and `ms` fields.
//!   3. A failing call (non-allowlisted argv → POLICY_DENIED) still
//!      lands an audit row, but with `err` instead of `result`.
//!
//! The test brings up its own per-test PG cluster (peer-auth, UDS
//! only) so it can run alongside the operator's installed cluster
//! without colliding. PG bring-up boilerplate mirrors the patterns in
//! `db/tests/postgres_e2e.rs` and `core/tests/supervisor_e2e.rs`.
//!
//! Skips silently with `[SKIP]` lines on hosts without Postgres, a
//! reachable supervisor, the worker binary, or a working sandbox
//! backend. `cargo test -- --nocapture` to see them.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use hhagent_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use hhagent_db::{
    build_initdb_argv, build_postgresql_auto_conf, default_pg_bin_dir_candidates,
    default_socket_dir, find_pg_bin_dir, InitDbOptions, PgConfigOptions,
};
use hhagent_sandbox::{Net, Profile, SandboxBackend, SandboxPolicy};
use hhagent_supervisor::specs::postgres_service_spec;
use hhagent_supervisor::{
    default_probe, default_supervisor, ServiceStatus, Supervisor,
};

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
            return Err(format!("timeout {:?} waiting for {}", timeout, target.display()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Bring up a per-test PG cluster (initdb + auto.conf + supervisor
/// install + start). Returns the connection spec and the cleanup
/// guards. Same shape as the helper in `supervisor_e2e.rs`.
fn bring_up_pg_cluster(
    bin_dir: &Path,
    suffix: &str,
) -> (
    hhagent_db::conn::ConnectSpec,
    (ServiceGuard, PathGuard, PathGuard),
) {
    let postgres = bin_dir.join("postgres");
    let initdb = bin_dir.join("initdb");

    // Short labels here — the cluster socket path
    // `<data_dir>/sockets/.s.PGSQL.5432` must fit in `sockaddr_un.sun_path`
    // (108 bytes on Linux). `unique_temp_root` already appends a
    // pid+nanos suffix; doubling it via the label blows the limit.
    let data_root = unique_temp_root("disp-d");
    let data_guard = PathGuard { path: data_root.clone() };
    let data_dir = data_root.join("data");
    let socket_dir = default_socket_dir(&data_dir);
    let log_dir = unique_temp_root("disp-l");
    std::fs::create_dir_all(&log_dir).expect("create log dir");
    let log_guard = PathGuard { path: log_dir.clone() };

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
    spec.name = format!("hhagent-supervisor-test-pg-dispatch-{suffix}");
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
fn dispatch_writes_audit_row_for_success_and_failure() {
    if skip_if_no_supervisor() {
        return;
    }
    if skip_if_sandbox_unavailable() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };
    let worker = worker_binary();
    if !worker.exists() {
        eprintln!("\n[SKIP] worker binary not built; run cargo build --workspace\n");
        return;
    }

    let suffix = unique_suffix();
    let (conn_spec, _guards) = bring_up_pg_cluster(&bin_dir, &suffix);

    // Dispatch uses `tokio::task::block_in_place` around the
    // synchronous `worker.call`; that requires a multi-thread runtime.
    // `current_thread` would panic at the first dispatch.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime");

    rt.block_on(async {
        // Probe applies migrations 0001 + 0002 + 0003 and writes the
        // bring-up audit row. The dispatch test inserts on top of that
        // baseline.
        hhagent_db::probe::run(
            &conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "purpose": "audit-dispatch"}),
        )
        .await
        .expect("probe run");

        let pool = hhagent_db::pool::connect_runtime_pool(&conn_spec)
            .await
            .expect("connect runtime pool");

        let policy = policy_for_shell_exec(&worker, &[ECHO_PATH]);
        let backend = backend();
        let worker_str = worker.to_string_lossy().into_owned();
        let spec = WorkerSpec {
            policy: &policy,
            program: &worker_str,
            args: &[],
            wall_clock_ms: None,
        };
        let mut sworker = spawn_worker(&*backend, &spec).expect("spawn shell-exec");

        // ---------- success path ----------
        let result = dispatch(
            &pool,
            &mut sworker,
            "shell-exec",
            "shell.exec",
            serde_json::json!({"argv": [ECHO_PATH, "dispatch-ok"]}),
        )
        .await
        .expect("dispatch success");
        assert_eq!(result["exit_code"], 0);
        assert_eq!(result["stdout"].as_str().unwrap().trim_end(), "dispatch-ok");

        // ---------- failure path ----------
        // Non-allowlisted argv → worker returns POLICY_DENIED. The
        // call returns an Err from `dispatch`, but the audit row must
        // still be written.
        let err = dispatch(
            &pool,
            &mut sworker,
            "shell-exec",
            "shell.exec",
            serde_json::json!({"argv": ["/bin/cat", "/etc/passwd"]}),
        )
        .await
        .expect_err("dispatch must propagate worker policy denial");
        assert!(
            err.to_string().contains("-32001"),
            "expected POLICY_DENIED (-32001) in error string: {err}"
        );

        // ---------- audit_log assertions ----------
        // Three rows total: bring-up + success dispatch + failure
        // dispatch. The assertions below pin the *shape* of each
        // dispatch row separately so a refactor that drops the `err`
        // field (or accidentally writes `result` for the failure
        // case) trips the test.
        let rows = sqlx::query_as::<_, (i64, String, String, serde_json::Value)>(
            "SELECT id, actor, action, payload \
             FROM audit_log ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .expect("select audit_log");
        assert_eq!(rows.len(), 3, "expected 3 rows; got {rows:?}");

        // Row 0: bring-up.
        assert_eq!(rows[0].1, "core");
        assert_eq!(rows[0].2, "startup");

        // Row 1: success dispatch — has `result` but no `err`.
        assert_eq!(rows[1].1, "tool:shell-exec");
        assert_eq!(rows[1].2, "shell.exec");
        let p1 = rows[1].3.as_object().expect("payload object");
        assert!(p1.contains_key("req"), "missing req: {:?}", rows[1].3);
        assert!(p1.contains_key("result"), "missing result: {:?}", rows[1].3);
        assert!(p1.contains_key("ms"), "missing ms: {:?}", rows[1].3);
        assert!(!p1.contains_key("err"), "success row must not carry err");

        // Row 2: failure dispatch — has `err` but no `result`.
        assert_eq!(rows[2].1, "tool:shell-exec");
        assert_eq!(rows[2].2, "shell.exec");
        let p2 = rows[2].3.as_object().expect("payload object");
        assert!(p2.contains_key("req"));
        assert!(p2.contains_key("err"), "missing err on failure row: {:?}", rows[2].3);
        assert!(p2.contains_key("ms"));
        assert!(
            !p2.contains_key("result"),
            "failure row must not carry result"
        );
        let err_str = p2.get("err").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            err_str.contains("-32001") || err_str.contains("POLICY_DENIED"),
            "audit err field should mention POLICY_DENIED, got: {err_str}"
        );

        let _ = sworker.close();
        pool.close().await;
    });
}
