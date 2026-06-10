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

use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix, PgCluster,
};

/// Async helper: bring up a PG cluster (via the shared
/// [`kastellan_tests_common::bring_up_pg_cluster`]), run migrations,
/// return pool + cluster handle. The `PgCluster` carries the cleanup
/// guards internally and drops them in the right order at end of scope.
/// Returns `None` when PG or supervisor is unavailable (skip).
async fn bring_up_pg(label: &str) -> Option<(sqlx::PgPool, PgCluster)> {
    if skip_if_no_supervisor() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let suffix = format!("{}-{}", label, unique_suffix());
    let service_name = format!("kastellan-sched-test-pg-cr-{suffix}");
    let cluster = tokio::task::block_in_place(|| {
        bring_up_pg_cluster(&bin_dir, "crd", "crl", &service_name)
    });

    kastellan_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "scheduler-crash-recovery"}),
    )
    .await
    .ok()?;

    let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .ok()?;

    Some((pool, cluster))
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
    let Some((pool, _cluster)) = bring_up_pg("crash").await else {
        return; // [SKIP]
    };

    use kastellan_db::tasks::{self, insert_pending, Lane};

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
/// against [`kastellan_core::scheduler::crash_recovery::sweep_and_audit`].
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
///   4. Each recovered task **also** gets exactly one `task.finalize`
///      summary row whose payload carries `state="crashed"`,
///      `total_llm_calls`/`total_dispatch_calls` as JSON `null`
///      (counters died with the previous daemon), and a numeric
///      `total_duration_ms` plus an RFC 3339 `started_at` string
///      because the back-dated task was claimed before the sweep.
///      Observation-phase queries on `action='task.finalize'` now see
///      the crashed-task population that was previously invisible.
///   5. Idempotency: a second call returns 0 and writes no new rows of
///      either family.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sweep_and_audit_emits_one_task_crashed_row_per_recovered_task() {
    let Some((pool, _cluster)) = bring_up_pg("audit").await else {
        return; // [SKIP]
    };

    use kastellan_db::tasks::{self, insert_pending, Lane};

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
    let n = kastellan_core::scheduler::crash_recovery::sweep_and_audit(&pool)
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

    // ── Assert per-task `task.finalize` summary row + shape ──────────
    // Symmetric to the runtime path (drain_lane writes `task.<state>`
    // followed by `task.finalize`): the crash-recovery sweep emits the
    // same pair so observation-phase SQL grouping on
    // `action='task.finalize'` sees crashed tasks too. The counters are
    // JSON `null` (the dead daemon's in-memory tallies are unrecoverable).
    let finalize_rows: Vec<(i64, serde_json::Value)> = sqlx::query_as(
        "SELECT id, payload FROM audit_log \
         WHERE actor = 'scheduler' AND action = 'task.finalize' \
         ORDER BY id ASC",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        finalize_rows.len(),
        2,
        "one task.finalize audit row per recovered task"
    );

    let finalize_by_id: std::collections::HashMap<i64, &serde_json::Value> = finalize_rows
        .iter()
        .map(|(_, p)| (p["task_id"].as_i64().expect("task_id is integer"), p))
        .collect();

    for tid in [fast_id, long_id] {
        let p = finalize_by_id
            .get(&tid)
            .unwrap_or_else(|| panic!("task.finalize row missing for task {tid}"));
        assert_eq!(p["state"], "crashed", "finalize.state pins to 'crashed'");
        assert!(
            p["total_llm_calls"].is_null(),
            "total_llm_calls must be JSON null on crashed-task finalize"
        );
        assert!(
            p["total_dispatch_calls"].is_null(),
            "total_dispatch_calls must be JSON null on crashed-task finalize"
        );
        // Back-dated tasks were claimed before the sweep, so started_at
        // and a numeric duration are present. (A finalize row where the
        // counters are nullable but duration is computed is the wire
        // signal "we know when this task lived, just not what it did".)
        assert!(p["started_at"].is_string(), "started_at present after claim");
        assert!(
            p["total_duration_ms"].is_number(),
            "duration computable when started_at is present"
        );
        // 10 keys, no extras (defends against accidental payload bloat).
        // Issue #50 schema-v2 added `provenance`; the rest are the
        // canonical 9-key shape from `build_finalize_payload`.
        assert_eq!(
            p.as_object().unwrap().len(),
            10,
            "finalize payload has exactly the 10 canonical keys"
        );
        assert_eq!(
            p["provenance"], "crash_recovery",
            "crashed-task finalize provenance must be 'crash_recovery'"
        );
    }

    // ── Idempotency: a second call sweeps nothing and writes nothing ──
    let second = kastellan_core::scheduler::crash_recovery::sweep_and_audit(&pool)
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
        "idempotent second sweep must not write new task.crashed rows"
    );
    let final_finalize_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log \
         WHERE actor = 'scheduler' AND action = 'task.finalize'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        final_finalize_rows, 2,
        "idempotent second sweep must not write new task.finalize rows"
    );
}
