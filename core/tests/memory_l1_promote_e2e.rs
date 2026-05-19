//! End-to-end DB integration coverage for the L1 promotion writer —
//! [`hhagent_core::memory::l1_promote`] (direct path) and the
//! operator-facing wrappers in [`hhagent_core::cli_audit`].
//!
//! Each scenario brings up its own per-test Postgres cluster so seeded
//! rows cannot drift between scenarios.  Skips silently with `[SKIP]`
//! lines on hosts without Postgres or a reachable supervisor;
//! `cargo test -- --nocapture` to see skip lines.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use hhagent_core::cli_audit::{l1_add_and_audit, l1_remove_and_audit};
use hhagent_core::entity_extraction::NoOpEntityExtractor;
use hhagent_core::memory::l1_promote::{promote_l1, list_l1, L1Source, L1WriteOutcome, L1Error};
use hhagent_db::memories::MemoryLayer;
use hhagent_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime")
}

// ---------------------------------------------------------------------------
// Scenario 1 — operator add writes L1 row + audit row with correct key-set
// ---------------------------------------------------------------------------

#[test]
fn operator_add_writes_l1_row_and_audit_row() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l1pa-d",
        "l1pa-l",
        &format!("hhagent-supervisor-test-pg-l1pa-{suffix}"),
    );

    rt().block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l1-promote-a"}),
        )
        .await
        .expect("probe");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let (outcome, _audit_id) = l1_add_and_audit(&pool, &NoOpEntityExtractor::new(), "operator insight one")
            .await
            .expect("l1_add_and_audit");

        // Outcome must be Inserted.
        assert!(
            matches!(outcome, L1WriteOutcome::Inserted { .. }),
            "expected Inserted, got {outcome:?}"
        );

        // list_l1(false) shows exactly 1 row with layer == Index.
        let rows = list_l1(&pool, false).await.expect("list_l1");
        assert_eq!(rows.len(), 1, "exactly 1 L1 row");
        assert_eq!(
            rows[0].layer,
            MemoryLayer::Index,
            "layer must be Index (L1)"
        );

        // audit_log has exactly 1 l1.added row with actor='cli'.
        let all_audit = hhagent_db::audit::fetch_since(&pool, 0, 100)
            .await
            .expect("fetch_since");
        let l1_added: Vec<_> = all_audit
            .iter()
            .filter(|r| r.actor == "cli" && r.action == "l1.added")
            .collect();
        assert_eq!(l1_added.len(), 1, "exactly 1 l1.added audit row");

        // Payload key-set must be exactly {source, action, memory_id, body_sha256}.
        let payload = &l1_added[0].payload;
        let actual_keys: std::collections::BTreeSet<String> =
            payload.as_object().expect("payload object").keys().cloned().collect();
        let expected_keys: std::collections::BTreeSet<String> =
            ["action", "body_sha256", "memory_id", "source"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        assert_eq!(actual_keys, expected_keys, "payload key-set mismatch");

        pool.close().await;
    });
}

// ---------------------------------------------------------------------------
// Scenario 2 — operator add is idempotent on body SHA-256
// ---------------------------------------------------------------------------

#[test]
fn operator_add_is_idempotent_on_body_sha256() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l1pb-d",
        "l1pb-l",
        &format!("hhagent-supervisor-test-pg-l1pb-{suffix}"),
    );

    rt().block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l1-promote-b"}),
        )
        .await
        .expect("probe");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let (first, _) = l1_add_and_audit(&pool, &NoOpEntityExtractor::new(), "X")
            .await
            .expect("first add");
        assert!(
            matches!(first, L1WriteOutcome::Inserted { .. }),
            "first call must be Inserted"
        );

        let (second, _) = l1_add_and_audit(&pool, &NoOpEntityExtractor::new(), "X")
            .await
            .expect("second add");
        assert!(
            matches!(second, L1WriteOutcome::SkippedDuplicate { .. }),
            "second call must be SkippedDuplicate"
        );

        // Only 1 row in the DB.
        let rows = list_l1(&pool, false).await.expect("list_l1");
        assert_eq!(rows.len(), 1, "dedup: only 1 row in DB");

        // 2 audit rows: one action=inserted, one action=skipped_duplicate.
        let all_audit = hhagent_db::audit::fetch_since(&pool, 0, 100)
            .await
            .expect("fetch_since");
        let l1_added: Vec<_> = all_audit
            .iter()
            .filter(|r| r.actor == "cli" && r.action == "l1.added")
            .collect();
        assert_eq!(l1_added.len(), 2, "2 l1.added audit rows");

        let actions: std::collections::HashSet<&str> = l1_added
            .iter()
            .map(|r| r.payload["action"].as_str().expect("action str"))
            .collect();
        assert!(
            actions.contains("inserted"),
            "missing action=inserted row; got {actions:?}"
        );
        assert!(
            actions.contains("skipped_duplicate"),
            "missing action=skipped_duplicate row; got {actions:?}"
        );

        pool.close().await;
    });
}

// ---------------------------------------------------------------------------
// Scenario 3 — invalid bodies are rejected without writing any row
// ---------------------------------------------------------------------------

#[test]
fn operator_add_rejects_invalid_body_with_no_audit_row() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l1pc-d",
        "l1pc-l",
        &format!("hhagent-supervisor-test-pg-l1pc-{suffix}"),
    );

    rt().block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l1-promote-c"}),
        )
        .await
        .expect("probe");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let invalid_bodies = ["", "  ", "has\nnewline", "</l1_insights>"];
        for body in &invalid_bodies {
            let err = l1_add_and_audit(&pool, &NoOpEntityExtractor::new(), body)
                .await
                .expect_err(&format!("expected error for body {body:?}"));
            assert!(
                matches!(err, L1Error::Validation(_)),
                "expected Validation error for {body:?}, got {err:?}"
            );
        }

        // Zero L1 rows written.
        let rows = list_l1(&pool, false).await.expect("list_l1");
        assert_eq!(rows.len(), 0, "no rows written on validation error");

        // Zero audit rows written.
        let all_audit = hhagent_db::audit::fetch_since(&pool, 0, 100)
            .await
            .expect("fetch_since");
        let l1_added_count = all_audit
            .iter()
            .filter(|r| r.actor == "cli" && r.action == "l1.added")
            .count();
        assert_eq!(
            l1_added_count, 0,
            "validation errors must not write audit rows"
        );

        pool.close().await;
    });
}

// ---------------------------------------------------------------------------
// Scenario 4 — operator remove deletes and audits
// ---------------------------------------------------------------------------

#[test]
fn operator_remove_deletes_and_audits() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l1pd-d",
        "l1pd-l",
        &format!("hhagent-supervisor-test-pg-l1pd-{suffix}"),
    );

    rt().block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l1-promote-d"}),
        )
        .await
        .expect("probe");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // Seed one row.
        let (outcome, _) = l1_add_and_audit(&pool, &NoOpEntityExtractor::new(), "row to be removed")
            .await
            .expect("add");
        let memory_id = outcome.memory_id();

        // Remove it.
        let (deleted, _audit_id) = l1_remove_and_audit(&pool, memory_id)
            .await
            .expect("remove");
        assert!(deleted, "deleted must be true");

        // list_l1 no longer shows the row.
        let rows = list_l1(&pool, false).await.expect("list_l1");
        assert_eq!(rows.len(), 0, "row must be gone after remove");

        // Audit log has 1 l1.removed row with {memory_id, deleted: true}.
        let all_audit = hhagent_db::audit::fetch_since(&pool, 0, 100)
            .await
            .expect("fetch_since");
        let l1_removed: Vec<_> = all_audit
            .iter()
            .filter(|r| r.actor == "cli" && r.action == "l1.removed")
            .collect();
        assert_eq!(l1_removed.len(), 1, "exactly 1 l1.removed audit row");
        let payload = &l1_removed[0].payload;
        assert_eq!(
            payload["memory_id"].as_i64().expect("memory_id"),
            memory_id,
            "audit payload memory_id mismatch"
        );
        assert_eq!(
            payload["deleted"].as_bool().expect("deleted bool"),
            true,
            "audit payload deleted must be true"
        );

        pool.close().await;
    });
}

// ---------------------------------------------------------------------------
// Scenario 5 — operator remove refuses to delete a wrong-layer row
// ---------------------------------------------------------------------------

#[test]
fn operator_remove_refuses_wrong_layer() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l1pe-d",
        "l1pe-l",
        &format!("hhagent-supervisor-test-pg-l1pe-{suffix}"),
    );

    rt().block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l1-promote-e"}),
        )
        .await
        .expect("probe");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // Seed an L2 (Stable) row directly through insert_memory (default layer).
        let stable_id = hhagent_db::memories::insert_memory(
            &pool,
            "stable row body",
            &serde_json::json!({}),
            None,
        )
        .await
        .expect("insert stable row");

        // Attempt to remove it via the L1 remove path.
        let (deleted, _audit_id) = l1_remove_and_audit(&pool, stable_id)
            .await
            .expect("remove call must not return Err");
        assert!(!deleted, "must not delete a non-L1 row");

        // The stable row must still be present.
        let remaining = hhagent_db::memories::fetch_by_ids(&pool, &[stable_id])
            .await
            .expect("fetch_by_ids");
        assert_eq!(
            remaining.len(),
            1,
            "stable row must still exist after refused remove"
        );

        // Audit log has 1 l1.removed row with {memory_id, deleted: false}.
        let all_audit = hhagent_db::audit::fetch_since(&pool, 0, 100)
            .await
            .expect("fetch_since");
        let l1_removed: Vec<_> = all_audit
            .iter()
            .filter(|r| r.actor == "cli" && r.action == "l1.removed")
            .collect();
        assert_eq!(l1_removed.len(), 1, "exactly 1 l1.removed audit row");
        let payload = &l1_removed[0].payload;
        assert_eq!(
            payload["memory_id"].as_i64().expect("memory_id"),
            stable_id,
        );
        assert_eq!(
            payload["deleted"].as_bool().expect("deleted bool"),
            false,
            "audit payload deleted must be false for wrong-layer row"
        );

        pool.close().await;
    });
}

// ---------------------------------------------------------------------------
// Scenario 6 — agent-raised promote writes L1 row with task_id metadata
// ---------------------------------------------------------------------------

#[test]
fn agent_raised_promote_l1_writes_l1_row_with_task_id_metadata() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l1pf-d",
        "l1pf-l",
        &format!("hhagent-supervisor-test-pg-l1pf-{suffix}"),
    );

    rt().block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l1-promote-f"}),
        )
        .await
        .expect("probe");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let outcome = promote_l1(
            &pool,
            &NoOpEntityExtractor::new(),
            "shell-exec /bin/echo works",
            L1Source::AgentRaised { task_id: 17 },
        )
        .await
        .expect("promote_l1");

        assert!(
            matches!(outcome, L1WriteOutcome::Inserted { .. }),
            "expected Inserted, got {outcome:?}"
        );

        let rows = list_l1(&pool, false).await.expect("list_l1");
        assert_eq!(rows.len(), 1, "exactly 1 L1 row");

        let meta = rows[0].metadata.as_object().expect("metadata object");
        assert_eq!(
            meta.get("source").and_then(|v| v.as_str()),
            Some("agent_raised"),
            "metadata.source must be agent_raised"
        );
        assert_eq!(
            meta.get("task_id").and_then(|v| v.as_i64()),
            Some(17),
            "metadata.task_id must be 17"
        );

        pool.close().await;
    });
}

// ---------------------------------------------------------------------------
// Scenario 7 — agent-raised deduplicates against existing operator row;
//              the FIRST writer's source is preserved
// ---------------------------------------------------------------------------

#[test]
fn agent_raised_promote_dedups_against_operator_row() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l1pg-d",
        "l1pg-l",
        &format!("hhagent-supervisor-test-pg-l1pg-{suffix}"),
    );

    rt().block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l1-promote-g"}),
        )
        .await
        .expect("probe");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // Operator seeds "shared" first.
        let (op_outcome, _) = l1_add_and_audit(&pool, &NoOpEntityExtractor::new(), "shared")
            .await
            .expect("operator add");
        let op_id = op_outcome.memory_id();

        // Agent-raised call with the same body.
        let agent_outcome = promote_l1(
            &pool,
            &NoOpEntityExtractor::new(),
            "shared",
            L1Source::AgentRaised { task_id: 99 },
        )
        .await
        .expect("promote_l1 agent");

        // Must be SkippedDuplicate carrying the operator's memory_id.
        match agent_outcome {
            L1WriteOutcome::SkippedDuplicate { memory_id } => {
                assert_eq!(
                    memory_id, op_id,
                    "SkippedDuplicate must carry the operator's memory_id"
                );
            }
            other => panic!("expected SkippedDuplicate, got {other:?}"),
        }

        // Only 1 row in the DB.
        let rows = list_l1(&pool, false).await.expect("list_l1");
        assert_eq!(rows.len(), 1, "dedup: only 1 row");

        // The row's metadata source must be "operator" (the FIRST writer).
        let meta = rows[0].metadata.as_object().expect("metadata object");
        assert_eq!(
            meta.get("source").and_then(|v| v.as_str()),
            Some("operator"),
            "first writer's source must be preserved"
        );

        pool.close().await;
    });
}

// ---------------------------------------------------------------------------
// Scenario 8 — list_l1 in-prompt vs all distinguishes at cap boundary
// ---------------------------------------------------------------------------

#[test]
fn list_l1_in_prompt_vs_all_distinguishes_at_cap_boundary() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l1ph-d",
        "l1ph-l",
        &format!("hhagent-supervisor-test-pg-l1ph-{suffix}"),
    );

    rt().block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l1-promote-h"}),
        )
        .await
        .expect("probe");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // Seed 40 rows with distinct bodies.
        for i in 0u32..40 {
            promote_l1(
                &pool,
                &NoOpEntityExtractor::new(),
                &format!("distinct insight row {i:03}"),
                L1Source::AgentRaised { task_id: i64::from(i) },
            )
            .await
            .expect("promote_l1");
        }

        // list_l1(false) → in-prompt slice, capped at 32 rows.
        let in_prompt = list_l1(&pool, false).await.expect("list_l1 false");
        assert!(
            in_prompt.len() <= 32,
            "in-prompt slice must be at most 32 rows, got {}",
            in_prompt.len()
        );

        // list_l1(true) → all rows, uncapped.
        let all = list_l1(&pool, true).await.expect("list_l1 true");
        assert_eq!(all.len(), 40, "all-rows slice must return all 40 rows");

        pool.close().await;
    });
}

// ---------------------------------------------------------------------------
// Task 6 — caller-side e2e: StaticEntityExtractor wired through L1 writer
// ---------------------------------------------------------------------------

/// Verify that `promote_l1` with a `StaticEntityExtractor` returns
/// `L1WriteOutcome::Inserted` carrying a `Some(LinkOutcome)` that reflects
/// the scripted entity count, and that `memory_entities` rows are persisted.
#[test]
fn promote_l1_inserted_outcome_carries_link_outcome() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    use hhagent_core::entity_extraction::StaticEntityExtractor;
    use hhagent_db::graph::{Graph, PgGraph};

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l1el-d",
        "l1el-l",
        &format!("hhagent-supervisor-test-pg-l1el-{suffix}"),
    );

    rt().block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l1-entity-link"}),
        )
        .await
        .expect("probe");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // Pre-create two entities so StaticEntityExtractor's ids resolve via FK.
        let graph = PgGraph::new(&pool);
        let e1 = graph
            .upsert_entity("person", "carol", &serde_json::json!({}))
            .await
            .expect("e1");
        // "concept" is a seeded entity kind in migration 0015.
        let e2 = graph
            .upsert_entity("concept", "alpha", &serde_json::json!({}))
            .await
            .expect("e2");

        let extractor = StaticEntityExtractor::with_ids(vec![e1, e2]);

        let outcome = promote_l1(
            &pool,
            &extractor,
            "carol leads project alpha",
            L1Source::Operator,
        )
        .await
        .expect("promote_l1");

        match outcome {
            L1WriteOutcome::Inserted { memory_id, link_outcome } => {
                let link =
                    link_outcome.expect("link_outcome must be Some on Inserted");
                assert_eq!(link.n_entities_linked, 2, "2 entities linked");
                assert_eq!(link.seeds.ids, vec![e1, e2], "seed ids match");

                // Confirm memory_entities rows were persisted.
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM memory_entities WHERE memory_id = $1",
                )
                .bind(memory_id)
                .fetch_one(&pool)
                .await
                .expect("count");
                assert_eq!(count, 2, "2 memory_entities rows for the inserted memory");
            }
            other => panic!("expected Inserted, got {other:?}"),
        }

        pool.close().await;
    });
}
