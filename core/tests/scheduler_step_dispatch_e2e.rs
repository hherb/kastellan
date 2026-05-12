//! End-to-end smoke for the production [`ToolHostStepDispatcher`] — the
//! `StepDispatcher` impl the scheduler's inner loop calls once per
//! `PlannedStep`.
//!
//! This is the regression pin for **Task 3.2.bis** (HANDOVER): until
//! this slice landed, the dispatcher was a `NOT_IMPLEMENTED`
//! placeholder, so the daemon could schedule tasks but never actually
//! invoke a worker. Every assertion below is something the placeholder
//! couldn't satisfy.
//!
//! ## What this test proves
//!
//!   1. **Happy path** — a `PlannedStep` naming an allowlisted argv
//!      results in `StepOutcome::Ok(value)` where `value["exit_code"]`
//!      is 0 and `value["stdout"]` carries the echoed text. Audit row
//!      with `actor = "tool:shell-exec"`, `action = "shell.exec"`,
//!      payload carrying `req`/`result`/`ms`.
//!   2. **Worker-policy denial** — a non-allowlisted argv yields
//!      `StepOutcome::Err { code: "POLICY_DENIED", detail }`. Audit row
//!      with the same actor/action, payload carrying `err` (not `result`).
//!   3. **Unknown-tool path** — a step naming a tool absent from the
//!      registry returns `StepOutcome::Err { code: "UNKNOWN_TOOL", detail }`
//!      and writes a single `actor="scheduler" action="step.unknown_tool"`
//!      audit row (the spawn never happens, so the `tool_host::dispatch`
//!      chokepoint is bypassed — the dispatcher itself is responsible
//!      for the audit insert). The detail names the missing tool.
//!   4. **Spawn-failure path** — a step naming a tool whose `ToolEntry`
//!      carries an invalid policy (relative path in `fs_read`, rejected
//!      up front by the sandbox backend) returns
//!      `StepOutcome::Err { code: "SPAWN_FAILED", detail }` and writes
//!      a single `actor="scheduler" action="step.spawn_failed"` audit
//!      row carrying the sandbox error string.
//!
//! ## How it differs from `audit_dispatch_e2e.rs`
//!
//! That test exercises `tool_host::dispatch` directly (chokepoint
//! correctness). This test exercises the layer one up:
//! `ToolHostStepDispatcher::dispatch_step` calling into `dispatch`,
//! plus the `StepOutcome` mapping and the registry lookup. Together
//! they pin the scheduler's tool path end-to-end.
//!
//! ## Skip behaviour
//!
//! Skips with `[SKIP]` lines on hosts missing Postgres, supervisor,
//! sandbox backend, or the worker binary. macOS hosts without a
//! Postgres install hit the skip cleanly. `cargo test -- --nocapture`
//! to see the skip lines.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hhagent_core::cassandra::types::{DataClass, PlannedStep};
use hhagent_core::scheduler::inner_loop::{StepDispatcher, StepOutcome};
use hhagent_core::scheduler::{shell_exec_entry, ToolEntry, ToolHostStepDispatcher, ToolRegistry};
use hhagent_sandbox::{Net, Profile, SandboxPolicy};
use hhagent_db::{
    build_initdb_argv, build_postgresql_auto_conf, default_pg_bin_dir_candidates,
    default_socket_dir, find_pg_bin_dir, InitDbOptions, PgConfigOptions,
};
use hhagent_sandbox::SandboxBackend;
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
fn sandbox_arc() -> Arc<dyn SandboxBackend> {
    Arc::new(hhagent_sandbox::linux_bwrap::LinuxBwrap::new())
}

#[cfg(target_os = "macos")]
fn sandbox_arc() -> Arc<dyn SandboxBackend> {
    Arc::new(hhagent_sandbox::macos_seatbelt::MacosSeatbelt::new())
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

/// Bring up a per-test PG cluster. Same shape as the helper in
/// `audit_dispatch_e2e.rs`; lifted because the workspace-level
/// `tests-common` crate (issue #15) doesn't exist yet.
fn bring_up_pg_cluster(
    bin_dir: &Path,
    suffix: &str,
) -> (
    hhagent_db::conn::ConnectSpec,
    (ServiceGuard, PathGuard, PathGuard),
) {
    let postgres = bin_dir.join("postgres");
    let initdb = bin_dir.join("initdb");

    // Short labels — the socket path
    // `<data_dir>/sockets/.s.PGSQL.5432` must fit in `sockaddr_un.sun_path`
    // (108 bytes on Linux).
    let data_root = unique_temp_root("step-d");
    let data_guard = PathGuard { path: data_root.clone() };
    let data_dir = data_root.join("data");
    let socket_dir = default_socket_dir(&data_dir);
    let log_dir = unique_temp_root("step-l");
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
    spec.name = format!("hhagent-supervisor-test-pg-stepdisp-{suffix}");
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

fn step(tool: &str, method: &str, params: serde_json::Value) -> PlannedStep {
    PlannedStep {
        tool: tool.into(),
        method: method.into(),
        parameters: params,
        returns: "stdout".into(),
        done_when: "exit_code == 0".into(),
        classification: DataClass::Public,
    }
}

#[test]
fn dispatcher_routes_ok_denied_and_unknown_tool_paths() {
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

    // `tool_host::dispatch` uses `block_in_place` around the synchronous
    // `Client::call`; mandatory multi-thread runtime.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime");

    rt.block_on(async {
        // Probe applies migrations and writes the bring-up audit row.
        hhagent_db::probe::run(
            &conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "purpose": "scheduler-step-dispatch"}),
        )
        .await
        .expect("probe run");

        let pool = hhagent_db::pool::connect_runtime_pool(&conn_spec)
            .await
            .expect("connect runtime pool");

        // Registry: register shell-exec with ECHO_PATH allowlisted, plus
        // `broken-tool` whose policy carries a relative `fs_read` path —
        // both `LinuxBwrap::spawn_under_policy` and
        // `MacosSeatbelt::spawn_under_policy` reject this up-front with
        // `SandboxError::Backend`, so dispatching against `broken-tool`
        // gives us a deterministic SPAWN_FAILED trigger without depending
        // on a missing binary (which would race the worker's early exit
        // and surface as IO_ERROR/PROTOCOL_ERROR instead).
        let mut registry = ToolRegistry::new();
        registry.insert(
            "shell-exec",
            shell_exec_entry(worker.clone(), &[ECHO_PATH.to_string()]),
        );
        registry.insert(
            "broken-tool",
            ToolEntry {
                binary: worker.clone(),
                policy: SandboxPolicy {
                    // Relative path here is the rejection trigger; both
                    // sandbox backends validate absolute-path-ness before
                    // doing anything else.
                    fs_read: vec![PathBuf::from("relative/path/triggers/rejection")],
                    fs_write: vec![],
                    net: Net::Deny,
                    cpu_ms: 1_000,
                    mem_mb: 32,
                    profile: Profile::WorkerStrict,
                    env: vec![],
                },
                wall_clock_ms: Some(5_000),
            },
        );
        let registry = Arc::new(registry);
        assert_eq!(registry.len(), 2);

        let sandbox = sandbox_arc();
        let dispatcher = ToolHostStepDispatcher::new(
            pool.clone(),
            sandbox,
            registry,
        );

        // ---------- (1) Happy path ----------
        let ok_step = step(
            "shell-exec",
            "shell.exec",
            serde_json::json!({"argv": [ECHO_PATH, "step-ok"]}),
        );
        let outcome = dispatcher.dispatch_step(&ok_step).await;
        let StepOutcome::Ok(value) = &outcome else {
            panic!("expected Ok, got {outcome:?}");
        };
        assert_eq!(value["exit_code"], 0);
        assert_eq!(
            value["stdout"].as_str().expect("stdout is string").trim_end(),
            "step-ok"
        );

        // ---------- (2) Worker-policy denial ----------
        let denied_step = step(
            "shell-exec",
            "shell.exec",
            serde_json::json!({"argv": ["/bin/cat", "/etc/passwd"]}),
        );
        let outcome = dispatcher.dispatch_step(&denied_step).await;
        let StepOutcome::Err { code, detail } = &outcome else {
            panic!("expected Err, got {outcome:?}");
        };
        assert_eq!(code, "POLICY_DENIED",
                   "non-allowlisted argv must map to POLICY_DENIED, not {code}");
        assert!(
            !detail.is_empty(),
            "POLICY_DENIED detail must carry the worker's message"
        );

        // ---------- (3) Unknown tool ----------
        let unknown_step = step(
            "web-fetch",
            "fetch",
            serde_json::json!({"url": "https://example.com"}),
        );
        let outcome = dispatcher.dispatch_step(&unknown_step).await;
        let StepOutcome::Err { code, detail } = &outcome else {
            panic!("expected Err, got {outcome:?}");
        };
        assert_eq!(code, "UNKNOWN_TOOL");
        assert!(
            detail.contains("web-fetch"),
            "UNKNOWN_TOOL detail should name the missing tool, got: {detail}"
        );

        // ---------- (4) Spawn failure (registered tool, invalid policy) -
        // The `broken-tool` entry was registered with a relative path in
        // `fs_read`, which the sandbox backend rejects up front. The
        // dispatcher's spawn path returns `ToolHostError::Sandbox(_)` →
        // SPAWN_FAILED, and (post-slice) writes an audit row.
        let spawn_fail_step = step(
            "broken-tool",
            "shell.exec",
            serde_json::json!({"argv": [ECHO_PATH, "never-runs"]}),
        );
        let outcome = dispatcher.dispatch_step(&spawn_fail_step).await;
        let StepOutcome::Err { code, detail } = &outcome else {
            panic!("expected Err, got {outcome:?}");
        };
        assert_eq!(
            code, "SPAWN_FAILED",
            "relative fs_read must surface as SPAWN_FAILED, not {code}",
        );
        assert!(
            !detail.is_empty(),
            "SPAWN_FAILED detail must carry the sandbox's error message",
        );

        // ---------- audit_log assertions ----------
        // Five rows:
        //   - row 0 — bring-up (`core`/`startup`)
        //   - row 1 — happy-path dispatch (`tool:shell-exec`/`shell.exec`, with `result`)
        //   - row 2 — policy-denied dispatch (`tool:shell-exec`/`shell.exec`, with `err`)
        //   - row 3 — unknown-tool dispatch (`scheduler`/`step.unknown_tool`, no `err`)
        //   - row 4 — spawn-failed dispatch (`scheduler`/`step.spawn_failed`, with `err`)
        //
        // Rows 3 + 4 are the contract for this slice: paths that short-
        // circuit before `tool_host::dispatch` must still leave an audit
        // trail, otherwise an operator triaging "the planner asked for X"
        // or "X never started" has nothing to grep.
        let rows = sqlx::query_as::<_, (i64, String, String, serde_json::Value)>(
            "SELECT id, actor, action, payload FROM audit_log ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .expect("select audit_log");
        assert_eq!(
            rows.len(),
            5,
            "expected 5 rows (bring-up + ok + denied + unknown + spawn_fail); got {rows:?}",
        );

        // Row 0: bring-up.
        assert_eq!(rows[0].1, "core");
        assert_eq!(rows[0].2, "startup");

        // Row 1: happy path — result, no err.
        assert_eq!(rows[1].1, "tool:shell-exec");
        assert_eq!(rows[1].2, "shell.exec");
        let p1 = rows[1].3.as_object().expect("payload object");
        assert!(p1.contains_key("req"));
        assert!(p1.contains_key("result"));
        assert!(p1.contains_key("ms"));
        assert!(!p1.contains_key("err"));

        // Row 2: policy-denied — err, no result.
        assert_eq!(rows[2].1, "tool:shell-exec");
        assert_eq!(rows[2].2, "shell.exec");
        let p2 = rows[2].3.as_object().expect("payload object");
        assert!(p2.contains_key("req"));
        assert!(p2.contains_key("err"));
        assert!(p2.contains_key("ms"));
        assert!(!p2.contains_key("result"));

        // Row 3: unknown-tool — actor=scheduler, action=step.unknown_tool.
        // No `err` field (there is no underlying error; just a missing
        // registration). `tool`+`method`+`req`+`ms` mirror the chokepoint
        // shape so audit consumers don't need a separate parser.
        assert_eq!(rows[3].1, "scheduler");
        assert_eq!(rows[3].2, "step.unknown_tool");
        let p3 = rows[3].3.as_object().expect("payload object");
        assert_eq!(p3.get("tool").and_then(|v| v.as_str()), Some("web-fetch"));
        assert_eq!(p3.get("method").and_then(|v| v.as_str()), Some("fetch"));
        assert!(p3.contains_key("req"));
        assert!(p3.contains_key("ms"));
        assert!(!p3.contains_key("err"),
                "UNKNOWN_TOOL payload must not carry `err`; got {:#}", rows[3].3);

        // Row 4: spawn-failed — actor=scheduler, action=step.spawn_failed,
        // payload carries the sandbox error string under `err`.
        assert_eq!(rows[4].1, "scheduler");
        assert_eq!(rows[4].2, "step.spawn_failed");
        let p4 = rows[4].3.as_object().expect("payload object");
        assert_eq!(p4.get("tool").and_then(|v| v.as_str()), Some("broken-tool"));
        assert_eq!(p4.get("method").and_then(|v| v.as_str()), Some("shell.exec"));
        assert!(p4.contains_key("req"));
        assert!(p4.contains_key("ms"));
        let err_str = p4.get("err").and_then(|v| v.as_str())
            .expect("SPAWN_FAILED payload must carry `err`");
        assert!(
            !err_str.is_empty(),
            "spawn_failed err must be a non-empty sandbox error string",
        );

        pool.close().await;
    });
}
