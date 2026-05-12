//! Producer-side cancellation audit row — end-to-end.
//!
//! What this test pins (against a per-test PG cluster):
//!
//! 1. [`hhagent_core::cli_audit::cancel_and_audit`] on a `pending` task:
//!    * flips `tasks.state` to `cancelled`,
//!    * writes exactly one `actor='cli' action='task.cancelled'` row in
//!      `audit_log` with the canonical lifecycle payload
//!      `{task_id, lane, plan_count}` — same shape as the scheduler's
//!      `task.<state>` rows so observation-phase SQL can `WHERE action
//!      LIKE 'task.%'` and capture both producer intent and scheduler
//!      observation,
//!    * returns `CancelOutcome::Cancelled(Task)` with the freshly-updated
//!      row data so the caller can display it.
//!
//! 2. [`hhagent_core::cli_audit::cancel_and_audit`] on an already-terminal
//!    task (here: a task that has just been cancelled): returns
//!    `CancelOutcome::NotCancellable` and writes no new audit row.
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

        // ── 4. Confirm exactly one new audit row, with the canonical shape ─
        let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log")
            .fetch_one(&pool)
            .await
            .expect("count audit_log after");
        assert_eq!(
            after - before,
            1,
            "exactly one new audit_log row from cancel_and_audit"
        );

        let row: (String, String, serde_json::Value) = sqlx::query_as(
            "SELECT actor, action, payload \
             FROM audit_log \
             WHERE actor = $1 AND action = 'task.cancelled' \
             ORDER BY id DESC LIMIT 1",
        )
        .bind(CLI_AUDIT_ACTOR)
        .fetch_one(&pool)
        .await
        .expect("fetch cli_audit row");

        assert_eq!(row.0, CLI_AUDIT_ACTOR);
        assert_eq!(row.1, "task.cancelled");

        // Payload shape — exact key set + values.
        let payload = row.2;
        assert_eq!(
            payload.get("task_id").and_then(|v| v.as_i64()),
            Some(id),
            "payload.task_id must equal inserted id"
        );
        assert_eq!(
            payload.get("lane").and_then(|v| v.as_str()),
            Some("long"),
            "payload.lane must equal Lane::Long.as_sql()"
        );
        assert_eq!(
            payload.get("plan_count").and_then(|v| v.as_i64()),
            Some(0),
            "payload.plan_count must be 0 for a fresh pending task"
        );

        // Key-set pin — same BTreeSet check pattern used elsewhere in
        // the codebase. Detects a future accidental extra field.
        let keys: std::collections::BTreeSet<_> = payload
            .as_object()
            .expect("payload is a JSON object")
            .keys()
            .cloned()
            .collect();
        let expected: std::collections::BTreeSet<String> = ["task_id", "lane", "plan_count"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(keys, expected, "cli audit payload key set");

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
