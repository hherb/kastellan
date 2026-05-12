//! Producer-side submission audit row — end-to-end.
//!
//! What this test pins (against a per-test PG cluster):
//!
//! 1. [`hhagent_core::cli_audit::submit_and_audit`] on `Lane::Fast` and
//!    `Lane::Long`:
//!    * inserts a `pending` row in `tasks` with the input payload,
//!    * writes one `actor='cli' action='task.submitted'` row in
//!      `audit_log` per call with the canonical lifecycle payload
//!      `{task_id, lane, plan_count}` — same shape as the scheduler's
//!      `task.<state>` rows so observation SQL `WHERE action LIKE 'task.%'`
//!      captures the full lifecycle from submit through terminal,
//!    * returns the new task id (same shape as the underlying
//!      `tasks::insert_pending`).
//!
//! ## Why the test exists
//!
//! Before this slice, `hhagent-cli ask` called `tasks::insert_pending`
//! directly and emitted no producer-side audit row — the lifecycle
//! stream visible in `audit_log` started at the scheduler's
//! `task.running` observation on claim. "Submitted but never claimed"
//! gaps were invisible at the SQL layer (e.g. tasks submitted while the
//! scheduler is down), and submit-to-claim latency queries had to join
//! `audit_log.scheduler/task.running.ts` against `tasks.created_at`
//! across two clocks. This row closes that gap.
//!
//! ## Skip semantics
//!
//! Skips silently with `[SKIP]` lines on hosts without Postgres or a
//! reachable supervisor; run `cargo test -- --nocapture` to see them.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use hhagent_core::cli_audit::{submit_and_audit, CLI_AUDIT_ACTOR};
use hhagent_db::tasks::{get, Lane};
use hhagent_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};

/// Headline test for the slice: submitting on both lanes writes exactly
/// one canonical producer-side audit row per call and leaves matching
/// pending rows in the `tasks` table.
#[test]
fn submit_and_audit_emits_producer_task_submitted_row() {
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
        "csa-d",
        "csa-l",
        &format!("hhagent-supervisor-test-pg-csa-{suffix}"),
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
            serde_json::json!({"version": "test", "purpose": "cli-submit-audit"}),
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

        // ── 1. Submit on Lane::Fast ────────────────────────────────────
        let fast_payload =
            serde_json::json!({"instruction": "fast lane task", "kind": "test"});
        let fast_id = submit_and_audit(&pool, Lane::Fast, fast_payload.clone())
            .await
            .expect("submit_and_audit fast");

        // ── 2. Submit on Lane::Long ────────────────────────────────────
        let long_payload =
            serde_json::json!({"instruction": "long lane task", "kind": "test"});
        let long_id = submit_and_audit(&pool, Lane::Long, long_payload.clone())
            .await
            .expect("submit_and_audit long");

        assert_ne!(fast_id, long_id, "two inserts must produce distinct ids");

        // ── 3. Confirm `tasks` table shape ─────────────────────────────
        let fast_task = get(&pool, fast_id).await.expect("get fast").expect("fast task exists");
        assert_eq!(fast_task.state, "pending");
        assert_eq!(fast_task.lane, Lane::Fast);
        assert_eq!(fast_task.plan_count, 0);
        assert_eq!(fast_task.payload, fast_payload, "fast payload round-trip");

        let long_task = get(&pool, long_id).await.expect("get long").expect("long task exists");
        assert_eq!(long_task.state, "pending");
        assert_eq!(long_task.lane, Lane::Long);
        assert_eq!(long_task.plan_count, 0);
        assert_eq!(long_task.payload, long_payload, "long payload round-trip");

        // ── 4. Confirm exactly two new audit rows, with the canonical shape ─
        let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log")
            .fetch_one(&pool)
            .await
            .expect("count audit_log after");
        assert_eq!(
            after - before,
            2,
            "exactly two new audit_log rows from two submit_and_audit calls"
        );

        // Fetch both producer rows ordered by id (= insertion order).
        let rows: Vec<(String, String, serde_json::Value)> = sqlx::query_as(
            "SELECT actor, action, payload \
             FROM audit_log \
             WHERE actor = $1 AND action = 'task.submitted' \
             ORDER BY id ASC",
        )
        .bind(CLI_AUDIT_ACTOR)
        .fetch_all(&pool)
        .await
        .expect("fetch cli_audit submit rows");

        assert_eq!(rows.len(), 2, "exactly two task.submitted rows");

        // First row pins fast-lane payload values; second row pins long-lane.
        for (i, (id, lane_str)) in [(fast_id, "fast"), (long_id, "long")].iter().enumerate() {
            let (actor, action, payload) = &rows[i];
            assert_eq!(actor, CLI_AUDIT_ACTOR);
            assert_eq!(action, "task.submitted");

            assert_eq!(
                payload.get("task_id").and_then(|v| v.as_i64()),
                Some(*id),
                "row {i}: payload.task_id must equal inserted id"
            );
            assert_eq!(
                payload.get("lane").and_then(|v| v.as_str()),
                Some(*lane_str),
                "row {i}: payload.lane must equal the SQL lane string"
            );
            assert_eq!(
                payload.get("plan_count").and_then(|v| v.as_i64()),
                Some(0),
                "row {i}: payload.plan_count must be 0 at submit time"
            );

            // Key-set pin — detects a future accidental extra field.
            let keys: std::collections::BTreeSet<_> = payload
                .as_object()
                .expect("payload is a JSON object")
                .keys()
                .cloned()
                .collect();
            let expected: std::collections::BTreeSet<String> =
                ["task_id", "lane", "plan_count"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
            assert_eq!(keys, expected, "row {i}: cli submit audit payload key set");
        }

        pool.close().await;
    });
}
