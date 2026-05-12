//! End-to-end test for crash recovery.
//!
//! One scenario:
//!   back_dated_lease_is_swept_to_crashed — plants a pending row,
//!   claims it (transition → running), back-dates the lease to simulate
//!   a daemon crash that never finalised, runs `tasks::sweep_crashed`,
//!   and asserts the state transitions to 'crashed'. Verifies
//!   idempotency: a second sweep returns 0.
//!
//! Skips silently with `[SKIP]` lines on hosts without Postgres or a
//! reachable supervisor. `cargo test -- --nocapture` to see them.
//!
//! Issue #15 will eventually hoist the bring-up helpers into a shared
//! fixture; until then we copy and adapt the recipe from
//! `core/tests/scheduler_inner_loop_e2e.rs`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

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
/// cleanup guards. Mirrors `bring_up_pg_cluster` in `scheduler_inner_loop_e2e.rs`
/// with a short label to keep socket paths under the 108-byte limit.
fn bring_up_pg_cluster(
    bin_dir: &Path,
    suffix: &str,
) -> (
    hhagent_db::conn::ConnectSpec,
    (ServiceGuard, PathGuard, PathGuard),
) {
    use hhagent_db::{
        build_initdb_argv, build_postgresql_auto_conf, default_socket_dir, InitDbOptions,
        PgConfigOptions,
    };
    use hhagent_supervisor::{default_supervisor, specs::postgres_service_spec, ServiceStatus};

    let postgres = bin_dir.join("postgres");
    let initdb = bin_dir.join("initdb");

    // Short label — socket path must fit in sockaddr_un.sun_path (108 bytes on Linux).
    let data_root = unique_temp_root("crd");
    let data_guard = PathGuard { path: data_root.clone() };
    let data_dir = data_root.join("data");
    let socket_dir = default_socket_dir(&data_dir);
    let log_dir = unique_temp_root("crl");
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
    spec.name = format!("hhagent-sched-test-pg-cr-{suffix}");
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
async fn bring_up_pg(
    label: &str,
) -> Option<(sqlx::PgPool, (ServiceGuard, PathGuard, PathGuard))> {
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
        serde_json::json!({"version": "test", "purpose": "scheduler-crash-recovery"}),
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
// Tests
// ---------------------------------------------------------------------------

/// Simulates a daemon crash by:
/// 1. Inserting a pending task and claiming it (→ running).
/// 2. Back-dating the lease to a time in the past.
/// 3. Calling `sweep_crashed` — expects it to transition the task to 'crashed'.
/// 4. Verifying idempotency: a second sweep returns 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn back_dated_lease_is_swept_to_crashed() {
    let Some((pool, _guards)) = bring_up_pg("crash").await else {
        return; // [SKIP]
    };

    use hhagent_db::tasks::{self, insert_pending, Lane};

    // Insert a task and claim it (pending → running).
    let id = insert_pending(&pool, Lane::Fast, serde_json::json!({}))
        .await
        .unwrap();
    let claimed = tasks::claim_one(&pool, Lane::Fast, 60)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, id, "claim_one should return the task we just inserted");
    assert_eq!(
        tasks::observe_state(&pool, id).await.unwrap(),
        "running",
        "task should be running after claim_one"
    );

    // Simulate "daemon was killed without finalising" by back-dating the lease.
    sqlx::query(
        "UPDATE tasks SET lease_expires_at = now() - interval '1 second' WHERE id = $1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();

    // The next daemon's startup sweep transitions expired-lease running rows to crashed.
    let swept = tasks::sweep_crashed(&pool).await.unwrap();
    assert_eq!(swept.len(), 1, "sweep_crashed should have swept exactly 1 task");
    assert_eq!(swept[0].id, id, "swept row should be the one we back-dated");
    assert_eq!(
        tasks::observe_state(&pool, id).await.unwrap(),
        "crashed",
        "task should be in state 'crashed' after sweep"
    );

    // Idempotent: a second sweep finds nothing to sweep.
    assert!(
        tasks::sweep_crashed(&pool).await.unwrap().is_empty(),
        "second sweep_crashed should return an empty vec (idempotent)"
    );
}

/// Pins the audit-row contract for the startup sweep, as a regression
/// against [`hhagent_core::scheduler::crash_recovery::sweep_and_audit`].
/// Two crashed tasks are planted (one on Fast, one on Long) so the
/// per-row emission and lane preservation are both pinned in one test.
///
/// Asserts:
///   1. `sweep_and_audit` returns the number of recovered rows.
///   2. Each recovered task gets exactly one `audit_log` row with
///      `actor='scheduler'` and `action='task.crashed'`, whose payload
///      is the canonical lifecycle shape `{task_id, lane, plan_count}`
///      (matches `audit::build_lifecycle_payload` — proves the helper
///      is reused, not re-implemented).
///   3. The lane field round-trips per task (Fast → "fast", Long → "long").
///   4. Idempotency: a second call returns 0 and writes no new rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sweep_and_audit_emits_one_task_crashed_row_per_recovered_task() {
    let Some((pool, _guards)) = bring_up_pg("audit").await else {
        return; // [SKIP]
    };

    use hhagent_db::tasks::{self, insert_pending, Lane};

    // ── Plant two running-and-expired tasks on distinct lanes ────────
    async fn plant_expired(pool: &sqlx::PgPool, lane: Lane) -> i64 {
        let id = insert_pending(pool, lane, serde_json::json!({})).await.unwrap();
        tasks::claim_one(pool, lane, 60).await.unwrap().unwrap();
        sqlx::query(
            "UPDATE tasks SET lease_expires_at = now() - interval '1 second' WHERE id = $1",
        )
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
        id
    }
    let fast_id = plant_expired(&pool, Lane::Fast).await;
    let long_id = plant_expired(&pool, Lane::Long).await;

    // Baseline: count audit rows whose actor='scheduler' and action='task.crashed'.
    // The bring-up probe already wrote a 'core'/'startup' row; an earlier test
    // run cannot bleed into this since each test owns its own PG cluster.
    let baseline_crashed_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log \
         WHERE actor = 'scheduler' AND action = 'task.crashed'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(baseline_crashed_rows, 0, "no task.crashed rows before the sweep");

    // ── Act ──────────────────────────────────────────────────────────
    let n = hhagent_core::scheduler::crash_recovery::sweep_and_audit(&pool)
        .await
        .expect("sweep_and_audit");

    // ── Assert state + count ────────────────────────────────────────
    assert_eq!(n, 2, "two expired-lease tasks were planted; both must be swept");
    assert_eq!(tasks::observe_state(&pool, fast_id).await.unwrap(), "crashed");
    assert_eq!(tasks::observe_state(&pool, long_id).await.unwrap(), "crashed");

    // ── Assert audit row count + per-row payload shape ──────────────
    let crashed_rows: Vec<(i64, serde_json::Value)> = sqlx::query_as(
        "SELECT id, payload FROM audit_log \
         WHERE actor = 'scheduler' AND action = 'task.crashed' \
         ORDER BY id ASC",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        crashed_rows.len(),
        2,
        "one task.crashed audit row per recovered task"
    );

    // Map by task_id so the assertion is independent of insertion order.
    let by_id: std::collections::HashMap<i64, &serde_json::Value> = crashed_rows
        .iter()
        .map(|(_, p)| (p["task_id"].as_i64().expect("task_id is integer"), p))
        .collect();

    let fast_payload = by_id.get(&fast_id).expect("audit row for fast task");
    assert_eq!(fast_payload["lane"], "fast", "fast task → lane='fast'");
    assert_eq!(fast_payload["plan_count"], 0, "freshly-claimed: plan_count=0");
    assert_eq!(
        fast_payload.as_object().unwrap().len(),
        3,
        "lifecycle payload has exactly task_id+lane+plan_count, no extras"
    );

    let long_payload = by_id.get(&long_id).expect("audit row for long task");
    assert_eq!(long_payload["lane"], "long", "long task → lane='long'");
    assert_eq!(long_payload["plan_count"], 0);

    // ── Idempotency: a second call sweeps nothing and writes nothing ──
    let second = hhagent_core::scheduler::crash_recovery::sweep_and_audit(&pool)
        .await
        .expect("sweep_and_audit idempotent");
    assert_eq!(second, 0, "second call: nothing to sweep");
    let final_crashed_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log \
         WHERE actor = 'scheduler' AND action = 'task.crashed'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        final_crashed_rows, 2,
        "idempotent second sweep must not write new audit rows"
    );
}
