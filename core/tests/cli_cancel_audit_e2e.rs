//! Producer-side cancellation audit row — end-to-end.
//!
//! What this test pins (against a per-test PG cluster):
//!
//! 1. [`hhagent_core::cli_audit::cancel_and_audit`] on a `pending` task:
//!    * flips `tasks.state` to `cancelled`,
//!    * writes **two** new `actor='cli'` rows in `audit_log`:
//!      - one `action='task.cancelled'` with the canonical lifecycle
//!        payload `{task_id, lane, plan_count}` — same shape as the
//!        scheduler's `task.<state>` rows so observation-phase SQL can
//!        `WHERE action LIKE 'task.%'` and capture both producer intent
//!        and scheduler observation,
//!      - one `action='task.finalize'` with the canonical 9-key summary
//!        payload, `state='cancelled'`, `started_at: null` (the task was
//!        never claimed), and zero counters/duration (the task ran zero
//!        plan iterations and zero step dispatches — these are *known*
//!        zeros, not the unknowable-null shape used for crashed tasks).
//!    * returns `CancelOutcome::Cancelled(Task)` with the freshly-updated
//!      row data so the caller can display it.
//!
//! 2. [`hhagent_core::cli_audit::cancel_and_audit`] on an already-terminal
//!    task (here: a task that has just been cancelled): returns
//!    `CancelOutcome::NotCancellable` and writes no new audit row.
//!
//! 3. [`hhagent_core::cli_audit::cancel_and_audit`] on a `running` task
//!    (one already claimed by the scheduler) writes ONLY the
//!    `task.cancelled` producer row — NOT a producer `task.finalize`.
//!    The scheduler's inner-loop `observe_state` poll will write its own
//!    `actor='scheduler' action='task.finalize'` row later; a producer
//!    finalize would double-count the finalize stream. The discriminator
//!    is `task.started_at.is_none()` — true iff the task was never
//!    claimed (set by `claim_one` only).
//!
//! ## Why the test exists
//!
//! Before this slice, `hhagent-cli tasks cancel` of a `pending` task
//! flipped the row via the `tasks_cancelled` NOTIFY trigger but emitted
//! no audit row at all — the scheduler never observed the transition
//! (the task was never claimed), and the CLI itself had no audit shim.
//! Observation-phase SQL asking "which tasks were producer-cancelled
//! before being claimed?" had to fall back to the daemon log (no SQL
//! surface). This row closes that gap.
//!
//! ## Skip semantics
//!
//! Skips silently with `[SKIP]` lines on hosts without Postgres or a
//! reachable supervisor; run `cargo test -- --nocapture` to see them.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use hhagent_core::cli_audit::{cancel_and_audit, CancelOutcome, CLI_AUDIT_ACTOR};
use hhagent_db::tasks::{insert_pending, observe_state, Lane};
use hhagent_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};

/// Cancelling a `pending` task writes one canonical producer-side audit
/// row and flips the row's state. Headline happy-path test for the slice.
#[test]
fn cancel_pending_task_writes_one_cli_audit_row() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "cca-d",
        "cca-l",
        &format!("hhagent-supervisor-test-pg-cca-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-thread tokio runtime");

    rt.block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "purpose": "cli-cancel-audit"}),
        )
        .await
        .expect("probe run");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("connect runtime pool");

        // Snapshot audit_log size before the test so we can assert the
        // exact delta (the probe step has already written 1 row).
        let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log")
            .fetch_one(&pool)
            .await
            .expect("count audit_log");

        // ── 1. Insert a pending task ──────────────────────────────────
        let id = insert_pending(
            &pool,
            Lane::Long,
            serde_json::json!({"instruction": "ignore me; will be cancelled"}),
        )
        .await
        .expect("insert_pending");

        // ── 2. Cancel via the producer-side helper ────────────────────
        let outcome = cancel_and_audit(&pool, id)
            .await
            .expect("cancel_and_audit");

        // Outcome shape: Cancelled(task) with the post-update row data.
        let task = match outcome {
            CancelOutcome::Cancelled(t) => t,
            CancelOutcome::NotCancellable => {
                panic!("expected Cancelled(_), got NotCancellable for fresh pending task")
            }
        };
        assert_eq!(task.id, id, "outcome row id must match");
        assert_eq!(task.state, "cancelled", "post-update state must be 'cancelled'");
        assert_eq!(task.lane, Lane::Long, "lane round-trip");
        assert_eq!(task.plan_count, 0, "fresh pending task has plan_count=0");

        // ── 3. Confirm DB state ───────────────────────────────────────
        assert_eq!(
            observe_state(&pool, id).await.expect("observe_state"),
            "cancelled",
            "DB state must agree with returned row"
        );

        // ── 4. Confirm exactly TWO new audit rows ─────────────────────
        // One `task.cancelled` lifecycle row + one `task.finalize`
        // summary row. The producer finalize closes the gap where the
        // scheduler will never observe a never-claimed task, so
        // observation-phase SQL on `action='task.finalize'` previously
        // undercounted by exactly the producer-cancelled-pending
        // population.
        let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log")
            .fetch_one(&pool)
            .await
            .expect("count audit_log after");
        assert_eq!(
            after - before,
            2,
            "exactly two new audit_log rows from cancel_and_audit of a pending task"
        );

        // ── 4a. Pin the `task.cancelled` lifecycle row shape ─────────
        let lifecycle: (String, String, serde_json::Value) = sqlx::query_as(
            "SELECT actor, action, payload \
             FROM audit_log \
             WHERE actor = $1 AND action = 'task.cancelled' \
             ORDER BY id DESC LIMIT 1",
        )
        .bind(CLI_AUDIT_ACTOR)
        .fetch_one(&pool)
        .await
        .expect("fetch cli/task.cancelled row");

        assert_eq!(lifecycle.0, CLI_AUDIT_ACTOR);
        assert_eq!(lifecycle.1, "task.cancelled");

        let lp = lifecycle.2;
        assert_eq!(
            lp.get("task_id").and_then(|v| v.as_i64()),
            Some(id),
            "lifecycle.task_id must equal inserted id"
        );
        assert_eq!(
            lp.get("lane").and_then(|v| v.as_str()),
            Some("long"),
            "lifecycle.lane must equal Lane::Long.as_sql()"
        );
        assert_eq!(
            lp.get("plan_count").and_then(|v| v.as_i64()),
            Some(0),
            "lifecycle.plan_count must be 0 for a fresh pending task"
        );

        // Key-set pin — same BTreeSet check pattern used elsewhere.
        let keys: std::collections::BTreeSet<_> = lp
            .as_object()
            .expect("lifecycle payload is a JSON object")
            .keys()
            .cloned()
            .collect();
        let expected: std::collections::BTreeSet<String> = ["task_id", "lane", "plan_count"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(keys, expected, "cli/task.cancelled payload key set");

        // ── 4b. Pin the `task.finalize` summary row shape ─────────────
        let finalize: (String, String, serde_json::Value) = sqlx::query_as(
            "SELECT actor, action, payload \
             FROM audit_log \
             WHERE actor = $1 AND action = 'task.finalize' \
             ORDER BY id DESC LIMIT 1",
        )
        .bind(CLI_AUDIT_ACTOR)
        .fetch_one(&pool)
        .await
        .expect("fetch cli/task.finalize row");

        assert_eq!(finalize.0, CLI_AUDIT_ACTOR);
        assert_eq!(finalize.1, "task.finalize");

        let fp = finalize.2;
        assert_eq!(
            fp.get("task_id").and_then(|v| v.as_i64()),
            Some(id),
            "finalize.task_id"
        );
        assert_eq!(
            fp.get("lane").and_then(|v| v.as_str()),
            Some("long"),
            "finalize.lane"
        );
        assert_eq!(
            fp.get("state").and_then(|v| v.as_str()),
            Some("cancelled"),
            "finalize.state pins to 'cancelled' for producer-cancel"
        );
        assert_eq!(fp.get("plan_count").and_then(|v| v.as_i64()), Some(0));
        // Counters are KNOWN zero (task never ran), not unknowable —
        // distinct from the crashed-task finalize where they are null.
        assert_eq!(
            fp.get("total_llm_calls").and_then(|v| v.as_i64()),
            Some(0),
            "producer-cancel finalize.total_llm_calls is known zero (task never ran)"
        );
        assert_eq!(
            fp.get("total_dispatch_calls").and_then(|v| v.as_i64()),
            Some(0),
            "producer-cancel finalize.total_dispatch_calls is known zero"
        );
        assert_eq!(
            fp.get("total_duration_ms").and_then(|v| v.as_i64()),
            Some(0),
            "producer-cancel finalize.total_duration_ms is known zero"
        );
        // `started_at: null` is the wire signal "task was never claimed."
        assert!(
            fp.get("started_at").map(|v| v.is_null()).unwrap_or(false),
            "finalize.started_at must be JSON null for producer-cancelled pending task"
        );
        // `finished_at` is the cancel-time `now()` — a non-empty string.
        assert!(
            fp.get("finished_at").and_then(|v| v.as_str()).is_some(),
            "finalize.finished_at must be a non-null RFC 3339 string"
        );

        // 9-key payload-shape pin (matches build_finalize_payload).
        let fkeys: std::collections::BTreeSet<_> = fp
            .as_object()
            .expect("finalize payload is a JSON object")
            .keys()
            .cloned()
            .collect();
        let fexpected: std::collections::BTreeSet<String> = [
            "task_id",
            "lane",
            "state",
            "plan_count",
            "total_llm_calls",
            "total_dispatch_calls",
            "total_duration_ms",
            "started_at",
            "finished_at",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(fkeys, fexpected, "cli/task.finalize payload key set");

        pool.close().await;
    });
}

/// Cancelling an already-terminal task is a no-op at both the SQL layer
/// and the audit layer. `cancel_and_audit` returns `NotCancellable` and
/// writes no row. The second call after the first cancel is the
/// canonical idempotency check.
#[test]
fn cancel_already_terminal_task_writes_no_audit_row() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ccab-d",
        "ccab-l",
        &format!("hhagent-supervisor-test-pg-ccab-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-thread tokio runtime");

    rt.block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "purpose": "cli-cancel-audit-idempotent"}),
        )
        .await
        .expect("probe run");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("connect runtime pool");

        let id = insert_pending(&pool, Lane::Fast, serde_json::json!({"instruction": "x"}))
            .await
            .expect("insert_pending");

        // First cancel: succeeds, writes one row.
        let first = cancel_and_audit(&pool, id).await.expect("first cancel");
        assert!(
            matches!(first, CancelOutcome::Cancelled(_)),
            "first call must be Cancelled"
        );

        let after_first: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log")
            .fetch_one(&pool)
            .await
            .expect("count audit_log after first");

        // Second cancel on the same id: already terminal → NotCancellable,
        // no SQL UPDATE, no audit row.
        let second = cancel_and_audit(&pool, id).await.expect("second cancel");
        assert!(
            matches!(second, CancelOutcome::NotCancellable),
            "second call on already-cancelled task must be NotCancellable"
        );

        let after_second: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log")
            .fetch_one(&pool)
            .await
            .expect("count audit_log after second");

        assert_eq!(
            after_second, after_first,
            "second cancel must not write any audit row"
        );

        // A nonexistent task id is also NotCancellable with no row written.
        let bogus = cancel_and_audit(&pool, 999_999_999)
            .await
            .expect("cancel nonexistent");
        assert!(
            matches!(bogus, CancelOutcome::NotCancellable),
            "nonexistent id must be NotCancellable"
        );
        let after_bogus: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log")
            .fetch_one(&pool)
            .await
            .expect("count audit_log after bogus");
        assert_eq!(
            after_bogus, after_second,
            "cancel of nonexistent id must not write any audit row"
        );

        pool.close().await;
    });
}

/// Cancelling a `running` task (already claimed by the scheduler) writes
/// the producer `task.cancelled` lifecycle row but NOT a producer
/// `task.finalize` row. Rationale: the scheduler's inner-loop
/// `observe_state` poll will write its own `actor='scheduler'
/// action='task.finalize'` row when it sees the new state, and a
/// producer finalize would double-count the finalize stream.
///
/// The discriminator inside [`hhagent_core::cli_audit::cancel_and_audit`]
/// is `task.started_at.is_none()` — true iff the task was never claimed
/// (set by `claim_one` only). This test plants a running task by calling
/// `claim_one` directly (no real scheduler needed; the discriminator is
/// purely DB-state-driven) and asserts exactly ONE new audit row results.
#[test]
fn cancel_running_task_does_not_write_producer_finalize() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ccar-d",
        "ccar-l",
        &format!("hhagent-supervisor-test-pg-ccar-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-thread tokio runtime");

    rt.block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "purpose": "cli-cancel-audit-running"}),
        )
        .await
        .expect("probe run");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("connect runtime pool");

        // Insert a pending task and claim it ourselves (no real scheduler
        // needed — the discriminator is just `tasks.started_at IS NOT NULL`).
        let id = insert_pending(
            &pool,
            Lane::Fast,
            serde_json::json!({"instruction": "claimed-then-cancelled"}),
        )
        .await
        .expect("insert_pending");

        let claimed = hhagent_db::tasks::claim_one(&pool, Lane::Fast, 60)
            .await
            .expect("claim_one")
            .expect("claimed task");
        assert_eq!(claimed.id, id, "claim_one must return our planted task");
        assert!(
            claimed.started_at.is_some(),
            "claim_one must set started_at"
        );

        let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log")
            .fetch_one(&pool)
            .await
            .expect("count audit_log before cancel");

        // Producer-cancel the running task.
        let outcome = cancel_and_audit(&pool, id).await.expect("cancel running");
        let task = match outcome {
            CancelOutcome::Cancelled(t) => t,
            CancelOutcome::NotCancellable => {
                panic!("expected Cancelled(_) for running task")
            }
        };
        assert_eq!(task.state, "cancelled");
        assert!(
            task.started_at.is_some(),
            "post-cancel task still carries started_at from claim_one"
        );

        // Exactly ONE new audit row — only the `task.cancelled` lifecycle.
        // The producer skips finalize because the scheduler will emit
        // its own when its observe_state poll sees the new state.
        let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log")
            .fetch_one(&pool)
            .await
            .expect("count audit_log after cancel");
        assert_eq!(
            after - before,
            1,
            "running-cancel must write exactly 1 producer audit row (lifecycle only, no finalize)"
        );

        // Concrete check: zero producer finalize rows in the whole log.
        let finalize_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM audit_log \
             WHERE actor = 'cli' AND action = 'task.finalize'",
        )
        .fetch_one(&pool)
        .await
        .expect("count cli/finalize rows");
        assert_eq!(
            finalize_count, 0,
            "no cli/task.finalize rows for a running-task cancel — scheduler will emit its own"
        );

        // Lifecycle row is there as before.
        let lifecycle_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM audit_log \
             WHERE actor = 'cli' AND action = 'task.cancelled'",
        )
        .fetch_one(&pool)
        .await
        .expect("count cli/task.cancelled rows");
        assert_eq!(lifecycle_count, 1, "exactly one cli/task.cancelled row");

        pool.close().await;
    });
}
