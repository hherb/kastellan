//! Integration test for hhagent_core::observation::capture::fetch_audit_rows_for_task.
//!
//! Brings up a per-test PG cluster (skips cleanly without it), runs the
//! probe, opens the runtime-role pool, inserts a handful of audit rows
//! by hand (some matching the target task_id, some not), and asserts
//! the helper returns only the matching rows in id-ascending order.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use hhagent_core::observation::capture::{
    fetch_audit_rows_for_task, CapturedAuditRow,
};
use hhagent_tests_common::{
    bring_up_pg_cluster, current_username, pg_bin_dir_or_skip, skip_if_no_supervisor,
    unique_suffix,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn fetch_audit_rows_for_task_filters_by_task_id_in_payload() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "obs-fetch-d",
        "obs-fetch-l",
        &format!("hhagent-supervisor-test-pg-obsfetch-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "obs-fetch-audit"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("pool");

    // Insert three rows for task 100 and two rows for task 200.
    let task_id_target: i64 = 100;
    let task_id_other: i64 = 200;
    for (actor, action, tid) in &[
        ("scheduler", "task.running", task_id_target),
        ("cassandra:chain", "verdict", task_id_target),
        ("scheduler", "task.completed", task_id_target),
        ("scheduler", "task.running", task_id_other),
        ("scheduler", "task.completed", task_id_other),
    ] {
        let payload = serde_json::json!({"task_id": tid, "lane": "fast", "plan_count": 0});
        hhagent_db::audit::insert(&pool, actor, action, payload)
            .await
            .expect("audit insert");
    }

    let fetched = fetch_audit_rows_for_task(&pool, task_id_target)
        .await
        .expect("fetch");
    assert_eq!(fetched.len(), 3, "exactly the 3 target rows");

    // Verify id-ascending order.
    let ids: Vec<i64> = fetched.iter().map(|r| r.id).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "rows must be id-ascending");

    // Verify each row has a parsable RFC 3339 timestamp.
    for r in &fetched {
        let _: time::OffsetDateTime =
            time::OffsetDateTime::parse(&r.ts, &time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| panic!("rfc 3339 parse: {}", r.ts));
        let _ = CapturedAuditRow::clone(r); // shape pin: still cloneable
    }

    // Confirm the helper did NOT pick up the task_id=200 rows.
    let other = fetch_audit_rows_for_task(&pool, task_id_other)
        .await
        .expect("fetch other");
    assert_eq!(other.len(), 2);
    for r in &other {
        let tid = r
            .payload
            .get("task_id")
            .and_then(|v| v.as_i64())
            .expect("task_id");
        assert_eq!(tid, task_id_other);
    }

    let _ = current_username(); // ensure helper is callable; future-proof
    drop(pool);
    drop(cluster);
}
