//! End-to-end test for the inner loop with a scripted-router stub.
//!
//! Four scenarios:
//!   (a) one-plan happy path: agent emits task_complete, loop returns
//!       Completed.
//!   (b) tool-fail-then-recover: plan 1's first step fails, agent
//!       sees the error in plan 2 and emits task_complete.
//!   (c) plan-iteration-cap exhausted: agent emits 3 non-terminal
//!       plans, loop returns Failed with the cap message.
//!   (d) cancel mid-execution: while plan is executing steps, the
//!       test plants `state='cancelled'`, loop returns Cancelled.
//!
//! Each scenario asserts the final Outcome is the expected variant.
//!
//! Skips silently with `[SKIP]` lines on hosts without Postgres or a
//! reachable supervisor. `cargo test -- --nocapture` to see them.
//!
//! Issue #15 will eventually hoist the bring-up helpers into a shared
//! fixture; until then we copy and adapt the recipe from
//! `core/tests/audit_dispatch_e2e.rs`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::sync::{Arc, Mutex};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hhagent_core::cassandra::review::{ChainReviewStage, NoopReviewStage};
use hhagent_core::cassandra::types::{DataClass, Plan, PlannedStep};
use hhagent_core::scheduler::agent::{AgentError, FormulationMeta, PlanFormulator};
use hhagent_core::scheduler::inner_loop::{
    run_to_terminal, Outcome, StepDispatcher, StepOutcome, TaskContext,
};
use hhagent_db::tasks::{self, insert_pending, Lane};

// ---------------------------------------------------------------------------
// Bring-up boilerplate (adapted from core/tests/audit_dispatch_e2e.rs)
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
/// cleanup guards. Mirrors `bring_up_pg_cluster` in `audit_dispatch_e2e.rs`
/// with a short label to keep socket paths under the 108-byte limit.
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
    let data_root = unique_temp_root("ilp-d");
    let data_guard = PathGuard { path: data_root.clone() };
    let data_dir = data_root.join("data");
    let socket_dir = default_socket_dir(&data_dir);
    let log_dir = unique_temp_root("ilp-l");
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
    spec.name = format!("hhagent-sched-test-pg-ilp-{suffix}");
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
        serde_json::json!({"version": "test", "purpose": "scheduler-inner-loop"}),
    )
    .await
    .ok()?;

    let pool = hhagent_db::pool::connect_runtime_pool(&conn_spec)
        .await
        .ok()?;

    // Single guard tuple so all three Drop impls run in declaration order
    // (ServiceGuard first to stop PG, then PathGuards to remove dirs).
    // A flat 4-tuple destructure would invert the order via reverse-LIFO
    // local drops and PG would still be writing to the data dir while it
    // gets remove_dir_all'd.
    Some((pool, guards))
}

// ---------------------------------------------------------------------------
// Scripted stubs
// ---------------------------------------------------------------------------

/// Returns plans from a pre-loaded queue. Out-of-script reads return
/// `AgentError::Decode` to make missing-script bugs loud.
struct ScriptedFormulator {
    script: Mutex<std::collections::VecDeque<Plan>>,
}

impl ScriptedFormulator {
    fn new(script: Vec<Plan>) -> Self {
        Self { script: Mutex::new(script.into()) }
    }
}

#[async_trait]
impl PlanFormulator for ScriptedFormulator {
    async fn formulate_plan(
        &self,
        _ctx: &TaskContext,
    ) -> Result<(Plan, FormulationMeta), AgentError> {
        let plan = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(AgentError::Decode {
                detail: "scripted formulator out of plans".into(),
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
            },
        ))
    }
}

/// Looks up the step in a table; missing keys return a
/// `POLICY_DENIED`-shaped error so unscripted calls are loud.
struct ScriptedDispatcher {
    table: std::collections::HashMap<(String, String), StepOutcome>,
}

#[async_trait]
impl StepDispatcher for ScriptedDispatcher {
    async fn dispatch_step(&self, step: &PlannedStep) -> StepOutcome {
        self.table
            .get(&(step.tool.clone(), step.method.clone()))
            .cloned()
            .unwrap_or(StepOutcome::Err {
                code: "POLICY_DENIED".into(),
                detail: format!("no script for {}::{}", step.tool, step.method),
            })
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
    }
}

fn make_ctx(task_id: i64, max_plans: u32) -> TaskContext {
    TaskContext {
        task_id,
        lane: Lane::Fast,
        instruction: "ping".into(),
        classification_floor: DataClass::Public,
        plans: vec![],
        advisories: vec![],
        blocks: vec![],
        plan_count: 0,
        max_plans,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// (a) Agent emits task_complete on the first plan; loop returns
///     Completed("pong").
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn happy_path_one_plan_returns_completed() {
    let Some((pool, _guards)) = bring_up_pg("ihp").await else {
        return; // [SKIP]
    };

    let id = insert_pending(&pool, Lane::Fast, serde_json::json!({}))
        .await
        .unwrap();
    let _ = tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();

    let formulator = Arc::new(ScriptedFormulator::new(vec![task_complete_plan("pong")]));
    let review = Arc::new(ChainReviewStage::new(vec![Arc::new(NoopReviewStage)]));
    let dispatcher = Arc::new(ScriptedDispatcher { table: Default::default() });

    let result = run_to_terminal(&pool, formulator, review, dispatcher, make_ctx(id, 3))
        .await
        .unwrap();

    match result.outcome {
        Outcome::Completed(v) => assert_eq!(v["body"], "pong"),
        o => panic!("expected Completed, got {:?}", o),
    }
    // Spec §7 counter pin: one terminal plan, zero dispatch.
    assert_eq!(result.plan_count, 1);
    assert_eq!(result.dispatch_count, 0);
}

/// (b) Plan 1 dispatches a step that fails (no entry in dispatcher
///     table); plan 2 emits task_complete; loop returns
///     Completed("recovered").
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_fail_then_recover_returns_completed() {
    let Some((pool, _guards)) = bring_up_pg("itf").await else {
        return; // [SKIP]
    };

    let id = insert_pending(&pool, Lane::Fast, serde_json::json!({}))
        .await
        .unwrap();
    let _ = tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();

    let formulator = Arc::new(ScriptedFormulator::new(vec![
        one_step_plan("does-not-exist", "x"), // dispatcher returns Err
        task_complete_plan("recovered"),
    ]));
    let review = Arc::new(ChainReviewStage::new(vec![Arc::new(NoopReviewStage)]));
    let dispatcher = Arc::new(ScriptedDispatcher { table: Default::default() });

    let result = run_to_terminal(&pool, formulator, review, dispatcher, make_ctx(id, 3))
        .await
        .unwrap();

    match result.outcome {
        Outcome::Completed(v) => assert_eq!(v["body"], "recovered"),
        o => panic!("expected Completed (after recovery), got {:?}", o),
    }
    // Spec §7 counter pin: 2 plans (failing + recovery), 1 dispatch
    // attempt (the failing step under plan 1; plan 2 is terminal).
    assert_eq!(result.plan_count, 2);
    assert_eq!(result.dispatch_count, 1);
}

/// (c) Formulator returns 3 non-terminal plans; cap is 3. After
///     formulating the 3rd plan and failing its step, the 4th
///     iteration's cap-check fires → Failed("plan_iteration_cap_exceeded …").
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_iteration_cap_exhausted_returns_failed() {
    let Some((pool, _guards)) = bring_up_pg("icap").await else {
        return; // [SKIP]
    };

    let id = insert_pending(&pool, Lane::Fast, serde_json::json!({}))
        .await
        .unwrap();
    let _ = tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();

    // Three non-terminal plans (each step fails because the dispatcher
    // table is empty). On iter 4 the cap fires.
    let formulator = Arc::new(ScriptedFormulator::new(vec![
        one_step_plan("never", "a"),
        one_step_plan("never", "a"),
        one_step_plan("never", "a"),
    ]));
    let review = Arc::new(ChainReviewStage::new(vec![Arc::new(NoopReviewStage)]));
    let dispatcher = Arc::new(ScriptedDispatcher { table: Default::default() });

    let result = run_to_terminal(&pool, formulator, review, dispatcher, make_ctx(id, 3))
        .await
        .unwrap();

    match result.outcome {
        Outcome::Failed(s) => assert!(
            s.contains("plan_iteration_cap_exceeded"),
            "expected cap message, got: {s}"
        ),
        o => panic!("expected Failed, got {:?}", o),
    }
    // Spec §7 counter pin: cap=3 plans each ran a failing step.
    assert_eq!(result.plan_count, 3);
    assert_eq!(result.dispatch_count, 3);
}

/// (d) The inner loop is running in a spawned task. While iteration 1
///     is mid-step, the test marks the task cancelled in the DB; the
///     loop detects it at the top of the next iteration and returns
///     Cancelled.
///
/// Synchronisation: the test uses a `BarrierDispatcher` that signals
/// when the first step is being processed and waits for an explicit
/// release. This avoids the timing-race a sleep-based test would have:
/// on fast hardware (DGX-class), 150 ms is enough time for the loop to
/// run iter 1 + iter 2 and complete plan 2 before the cancellation
/// lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_mid_execution_returns_cancelled() {
    use tokio::sync::Notify;

    let Some((pool, _guards)) = bring_up_pg("ican").await else {
        return; // [SKIP]
    };

    let id = insert_pending(&pool, Lane::Fast, serde_json::json!({}))
        .await
        .unwrap();
    let _ = tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();

    // Plan 1 dispatches a step that pauses on the barrier; while it
    // pauses, the test plants state='cancelled'. Plan 2 must NOT run.
    let formulator = Arc::new(ScriptedFormulator::new(vec![
        one_step_plan("ok-tool", "ok-method"),
        task_complete_plan("never seen"),
    ]));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let dispatcher = Arc::new(BarrierDispatcher {
        entered: entered.clone(),
        release: release.clone(),
    });
    let review = Arc::new(ChainReviewStage::new(vec![Arc::new(NoopReviewStage)]));

    let pool2 = pool.clone();
    let h = tokio::spawn(async move {
        run_to_terminal(&pool2, formulator, review, dispatcher, make_ctx(id, 3)).await
    });

    // Wait for the dispatcher to signal that iter 1's step is in flight.
    entered.notified().await;
    // Plant the cancellation while the step is paused on the barrier.
    tasks::mark_cancelled(&pool, id).await.unwrap();
    // Release the step. The for-step `observe_state` poll fires on the
    // next iteration of the step loop (none in this 1-step plan), then
    // the top-of-loop `observe_state` for iter 2 catches the cancellation.
    release.notify_one();

    let result = h.await.unwrap().unwrap();
    assert!(
        matches!(result.outcome, Outcome::Cancelled),
        "expected Cancelled, got: {:?}",
        result.outcome
    );
    // Spec §7 counter pin: plan 1 was formulated and its step ran
    // (paused on the barrier, then completed Ok before the top-of-loop
    // cancellation check fired on iter 2). dispatch_count == 1.
    assert_eq!(result.plan_count, 1);
    assert_eq!(result.dispatch_count, 1);
}

/// Dispatcher that signals on first call, waits for a release, then
/// returns Ok. Used by the cancel-mid-execution test to make the race
/// deterministic.
struct BarrierDispatcher {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl StepDispatcher for BarrierDispatcher {
    async fn dispatch_step(&self, _step: &PlannedStep) -> StepOutcome {
        self.entered.notify_one();
        self.release.notified().await;
        StepOutcome::Ok(serde_json::json!("step-ok"))
    }
}
