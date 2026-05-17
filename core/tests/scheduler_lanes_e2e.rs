//! End-to-end test for the two-lane concurrent scheduler.
//!
//! One scenario:
//!   two_lanes_run_concurrently — two pending tasks (one per lane),
//!   spawn the real `scheduler::spawn_scheduler`, expect both
//!   `tasks_completed` NOTIFY rows within 1.7s (proves concurrency).
//!
//! Skips silently with `[SKIP]` lines on hosts without Postgres or a
//! reachable supervisor. `cargo test -- --nocapture` to see them.
//!
//! Issue #15 will eventually hoist the bring-up helpers into a shared
//! fixture; until then we copy and adapt the recipe from
//! `core/tests/scheduler_inner_loop_e2e.rs`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hhagent_core::cassandra::review::{ChainReviewStage, NoopReviewStage};
use hhagent_core::cassandra::types::{DataClass, Plan, PlannedStep};
use hhagent_core::scheduler::agent::{AgentError, FormulationMeta, PlanFormulator};
use hhagent_core::scheduler::inner_loop::{StepDispatcher, StepOutcome, TaskContext};
use hhagent_core::scheduler::spawn_scheduler;
use hhagent_db::tasks::{insert_pending, Lane};
use sqlx::postgres::PgListener;

// ---------------------------------------------------------------------------
// Bring-up boilerplate (adapted from core/tests/scheduler_inner_loop_e2e.rs)
// Issue #15: hoist to a shared fixture once Phase 3 tests land.
// ---------------------------------------------------------------------------

fn skip_if_no_supervisor() -> bool {
    match hhagent_supervisor::default_probe() {
        Ok(()) => false,
        Err(e) => {
            eprintln!("\n[SKIP] supervisor probe failed: {e}\n");
            true
        }
    }
}

fn pg_bin_dir_or_skip() -> Option<PathBuf> {
    match hhagent_db::find_pg_bin_dir(&hhagent_db::default_pg_bin_dir_candidates()) {
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
    sup: Box<dyn hhagent_supervisor::Supervisor>,
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

fn wait_for_status<F: Fn(hhagent_supervisor::ServiceStatus) -> bool>(
    sup: &dyn hhagent_supervisor::Supervisor,
    name: &str,
    predicate: F,
    timeout: Duration,
) -> Result<hhagent_supervisor::ServiceStatus, String> {
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

/// Bring up a per-test PG cluster. Returns the connection spec and
/// cleanup guards. Mirrors `bring_up_pg_cluster` in `scheduler_inner_loop_e2e.rs`.
fn bring_up_pg_cluster(
    bin_dir: &Path,
    suffix: &str,
) -> (
    hhagent_db::conn::ConnectSpec,
    (ServiceGuard, PathGuard, PathGuard),
) {
    use hhagent_db::{
        build_initdb_argv, build_postgresql_auto_conf, default_socket_dir,
        InitDbOptions, PgConfigOptions,
    };
    use hhagent_supervisor::{default_supervisor, specs::postgres_service_spec, ServiceStatus};

    let postgres = bin_dir.join("postgres");
    let initdb = bin_dir.join("initdb");

    // Short label — socket path must fit in sockaddr_un.sun_path (108 bytes on Linux).
    let data_root = unique_temp_root("lnd");
    let data_guard = PathGuard { path: data_root.clone() };
    let data_dir = data_root.join("data");
    let socket_dir = default_socket_dir(&data_dir);
    let log_dir = unique_temp_root("lnl");
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
    spec.name = format!("hhagent-sched-test-pg-ln-{suffix}");
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

/// Async helper: bring up a PG cluster, run migrations, return pool +
/// guards. Returns `None` when PG or supervisor is unavailable (skip).
async fn bring_up_pg(label: &str) -> Option<(sqlx::PgPool, (ServiceGuard, PathGuard, PathGuard))> {
    if skip_if_no_supervisor() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let suffix = format!("{}-{}", label, unique_suffix());
    // bring_up_pg_cluster is sync (spawns initdb, uses systemd/launchd).
    // ServiceGuard holds Box<dyn Supervisor> which is not Send, so we
    // cannot use spawn_blocking. Use block_in_place instead — it yields
    // the async worker thread for the duration of the blocking call
    // without requiring the return value to be Send.
    let (conn_spec, guards) =
        tokio::task::block_in_place(|| bring_up_pg_cluster(&bin_dir, &suffix));

    hhagent_db::probe::run(
        &conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "scheduler-lanes"}),
    )
    .await
    .ok()?;

    let pool = hhagent_db::pool::connect_runtime_pool(&conn_spec)
        .await
        .ok()?;

    // Single guard tuple so all three Drop impls run in declaration order
    // (ServiceGuard first to stop PG, then PathGuards to remove dirs).
    Some((pool, guards))
}

// ---------------------------------------------------------------------------
// Scripted stubs
// ---------------------------------------------------------------------------

/// Returns plans from per-task queues. Keyed by `ctx.task_id`.
/// Out-of-script reads return `AgentError::Decode` to make bugs loud.
struct ScriptedFormulator {
    per_task: Mutex<HashMap<i64, VecDeque<Plan>>>,
}

impl ScriptedFormulator {
    fn new_per_task(scripts: Vec<(i64, Vec<Plan>)>) -> Self {
        Self {
            per_task: Mutex::new(
                scripts
                    .into_iter()
                    .map(|(id, plans)| (id, plans.into()))
                    .collect(),
            ),
        }
    }
}

#[async_trait]
impl PlanFormulator for ScriptedFormulator {
    async fn formulate_plan(
        &self,
        ctx: &TaskContext,
    ) -> Result<(Plan, FormulationMeta), AgentError> {
        let mut map = self.per_task.lock().unwrap();
        let queue = map.get_mut(&ctx.task_id).ok_or(AgentError::Decode {
            detail: format!("no script for task_id {}", ctx.task_id),
            raw: "".into(),
        })?;
        let plan = queue.pop_front().ok_or(AgentError::Decode {
            detail: format!("script exhausted for task_id {}", ctx.task_id),
            raw: "".into(),
        })?;
        Ok((
            plan,
            FormulationMeta {
                prompt_name: "agent_planner".into(),
                prompt_sha256: "test".into(),
                llm_model: "test-model".into(),
                llm_backend: "local".into(),
                latency_ms: 1,
                retry_count: 0,
                assembled_prompt_sha256: "test-assembled-sha".into(),
                l0_count: 0,
                l1_count: 0,
                recalled_memory_ids: Vec::new(),
                recall_count: 0,
                recall_query_sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
            },
        ))
    }
}

/// A dispatcher that sleeps for a fixed delay before returning
/// `StepOutcome::Ok`. Ignores `step.tool` and `step.method`.
struct SleepyDispatcher {
    delay: Duration,
}

#[async_trait]
impl StepDispatcher for SleepyDispatcher {
    async fn dispatch_step(&self, _step: &PlannedStep) -> StepOutcome {
        tokio::time::sleep(self.delay).await;
        StepOutcome::Ok(serde_json::json!("done"))
    }
}

// ---------------------------------------------------------------------------
// Plan-factory helpers
// ---------------------------------------------------------------------------

fn task_complete_plan(body: &str) -> Plan {
    Plan {
        context: "c".into(),
        decision: "task_complete".into(),
        rationale: "done".into(),
        steps: vec![],
        result: Some(serde_json::json!({"kind": "text", "body": body})),
        data_ceiling: DataClass::Public,
        refused: None,
        floor_request: None,
        l1_insight: None,
    }
}

fn one_step_plan(tool: &str, method: &str) -> Plan {
    Plan {
        context: "c".into(),
        decision: "act".into(),
        rationale: "r".into(),
        steps: vec![PlannedStep {
            tool: tool.into(),
            method: method.into(),
            parameters: serde_json::json!({}),
            returns: "x".into(),
            done_when: "x".into(),
            classification: DataClass::Public,
        }],
        result: None,
        data_ceiling: DataClass::Public,
        refused: None,
        floor_request: None,
        l1_insight: None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Two pending tasks (one per lane) must complete within 1.7 s when
/// running concurrently. Each task has one ~1 s sleeping step, so the
/// sequential time would be ~2 s+. Receiving both `tasks_completed`
/// NOTIFYs within 1.7 s proves the two lane loops ran in parallel.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_lanes_run_concurrently() {
    let Some((pool, _guards)) = bring_up_pg("ln").await else {
        return; // [SKIP]
    };

    // Subscribe BEFORE inserting to avoid the race where both tasks
    // complete before the listener is set up.
    let mut listener = PgListener::connect_with(&pool)
        .await
        .expect("PgListener connect");
    listener
        .listen("tasks_completed")
        .await
        .expect("LISTEN tasks_completed");

    // Insert one task per lane.
    let id_fast = insert_pending(
        &pool,
        Lane::Fast,
        serde_json::json!({"instruction": "fast-task", "max_plans": 3}),
    )
    .await
    .unwrap();

    let id_long = insert_pending(
        &pool,
        Lane::Long,
        serde_json::json!({"instruction": "long-task", "max_plans": 3}),
    )
    .await
    .unwrap();

    // Each task's script: one sleeping step, then task_complete.
    // The SleepyDispatcher will sleep ~1 s per step.
    let formulator = Arc::new(ScriptedFormulator::new_per_task(vec![
        (
            id_fast,
            vec![
                one_step_plan("sleep", "doit"),
                task_complete_plan("fast-done"),
            ],
        ),
        (
            id_long,
            vec![
                one_step_plan("sleep", "doit"),
                task_complete_plan("long-done"),
            ],
        ),
    ]));

    let review = Arc::new(ChainReviewStage::new(vec![Arc::new(NoopReviewStage)]));

    // ~1 s sleep per step; two tasks in parallel → both done ~1 s.
    let dispatcher = Arc::new(SleepyDispatcher {
        delay: Duration::from_millis(800),
    });

    let scheduler = spawn_scheduler(
        pool.clone(),
        formulator,
        review,
        dispatcher,
    );

    // Wait for both `tasks_completed` NOTIFYs.
    // Measure wall-clock time from scheduler start; both must arrive
    // within 1.7 s (concurrent) vs. ≥1.6 s serial (two × 800 ms steps).
    let t0 = Instant::now();

    let mut completed = std::collections::HashSet::new();
    while completed.len() < 2 {
        let n = tokio::time::timeout(Duration::from_secs(10), listener.recv())
            .await
            .expect("two_lanes_run_concurrently: timed out waiting for tasks_completed")
            .unwrap();
        let id: i64 = n.payload().parse().unwrap();
        completed.insert(id);
    }

    let elapsed = t0.elapsed();
    eprintln!("[lanes_e2e] both tasks completed in {elapsed:.2?}");

    // Shutdown the scheduler before dropping guards.
    scheduler.shutdown().await;

    assert!(completed.contains(&id_fast), "fast task ({id_fast}) not in completed set");
    assert!(completed.contains(&id_long), "long task ({id_long}) not in completed set");

    // If serial: ≥1.6 s. If concurrent: ≈0.8 s. Allow 1.7 s headroom.
    assert!(
        elapsed < Duration::from_millis(1700),
        "expected both tasks to complete within 1.7 s (concurrently), but elapsed={elapsed:.2?}",
    );
}
