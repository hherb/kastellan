//! Integration tests for the memory-write-time entity auto-linker.
//!
//! Two tiers:
//!   * Mock tier (this task) — real per-test PG cluster + StaticEntityExtractor.
//!     Pins the link-row insertion, the audit-row payload, and idempotency.
//!   * Real-model tier (Task 3) — live gliner-relex worker against the
//!     `multi-v1.0` weights. Gated on venv + weights presence (skip-as-pass).
//!
//! All tests use the shared `hhagent-tests-common` PG bring-up helper +
//! the standard skip-without-PG convention (skip_if_no_supervisor +
//! pg_bin_dir_or_skip).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use hhagent_core::entity_extraction::{
    NoOpEntityExtractor, SeedSource, StaticEntityExtractor,
};
use hhagent_core::memory::entity_link::link_memory_entities;
use hhagent_db::audit::fetch_since;
use hhagent_db::memories::seed_meta_memory;
use hhagent_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};

/// Build a Tokio runtime for sync-style tests. Mirrors the convention
/// in `memory_recall_e2e.rs` and `entity_extraction_e2e.rs`.
fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("tokio runtime")
}

/// Helper: insert an entity manually so tests can return its id from a
/// StaticEntityExtractor. Returns the entity id.
async fn upsert_test_entity(pool: &sqlx::PgPool, kind: &str, name: &str) -> i64 {
    use hhagent_db::graph::{Graph, PgGraph};
    let graph = PgGraph::new(pool);
    graph
        .upsert_entity(kind, name, &serde_json::json!({}))
        .await
        .expect("upsert_entity")
}

/// Shared helper: bring up a named PG cluster + run probe + open pool.
/// Returns `None` (with [SKIP]) if supervisor or PG binaries are absent.
async fn bring_up_pg(label: &str) -> Option<(hhagent_tests_common::PgCluster, sqlx::PgPool)> {
    // Must be called OUTSIDE the async block so skip returns the fn.
    // We return None instead of calling skip helpers (they're sync).
    let cluster = {
        let bin_dir = pg_bin_dir_or_skip()?;
        let suffix = unique_suffix();
        bring_up_pg_cluster(
            &bin_dir,
            &format!("mel-{label}-d"),
            &format!("mel-{label}-l"),
            &format!("hhagent-supervisor-test-pg-mel-{label}-{suffix}"),
        )
    };
    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": format!("entity-link-{label}")}),
    )
    .await
    .expect("probe run");
    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("connect runtime pool");
    Some((cluster, pool))
}

/// `fetch_since` requires a limit; use a large cap to get "all rows".
const FETCH_LIMIT: i64 = 10_000;

#[test]
fn link_inserts_memory_entities_rows_and_writes_audit_row() {
    if skip_if_no_supervisor() {
        return;
    }

    rt().block_on(async {
        let Some((_cluster, pool)) = bring_up_pg("ins").await else {
            return;
        };

        // Pre-create three entities so the Static extractor's ids resolve.
        let e1 = upsert_test_entity(&pool, "person", "alice").await;
        let e2 = upsert_test_entity(&pool, "drug", "ibuprofen").await;
        let e3 = upsert_test_entity(&pool, "disease", "headache").await;

        // Insert an L0 memory directly so we have a memory_id to link to.
        let memory_id = seed_meta_memory(
            &pool,
            "alice took ibuprofen for her headache",
            &serde_json::json!({"test": "link_inserts"}),
            None,
        )
        .await
        .expect("seed_meta_memory");

        // Audit-log row count before the link op, for delta calc.
        let rows_before = fetch_since(&pool, 0, FETCH_LIMIT)
            .await
            .expect("fetch_since before")
            .len();

        let extractor = StaticEntityExtractor::with_ids(vec![e1, e2, e3]);
        let outcome = link_memory_entities(
            &extractor,
            &pool,
            memory_id,
            "L0",
            "alice took ibuprofen for her headache",
        )
        .await
        .expect("link should succeed");

        assert_eq!(outcome.n_entities_linked, 3, "expected 3 fresh links");
        assert_eq!(outcome.seeds.ids, vec![e1, e2, e3]);
        assert_eq!(outcome.seeds.source, SeedSource::GlinerRelex);

        // Verify the rows actually landed.
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM memory_entities WHERE memory_id = $1",
        )
        .bind(memory_id)
        .fetch_one(&pool)
        .await
        .expect("count memory_entities");
        assert_eq!(count, 3, "expected 3 memory_entities rows");

        // Verify the audit row.
        let rows_after = fetch_since(&pool, 0, FETCH_LIMIT)
            .await
            .expect("fetch_since after");
        assert_eq!(
            rows_after.len(),
            rows_before + 1,
            "expected exactly one new audit row"
        );
        let link_row = rows_after
            .iter()
            .find(|r| r.actor == "memory_linker" && r.action == "entity_link")
            .expect("memory_linker/entity_link row present");
        let payload = &link_row.payload;
        let obj = payload.as_object().expect("payload object");
        assert_eq!(
            obj.len(),
            6,
            "expected 6 keys, got {:?}",
            obj.keys().collect::<Vec<_>>()
        );
        assert_eq!(payload["memory_id"], memory_id);
        assert_eq!(payload["layer"], "L0");
        assert_eq!(payload["n_entities_linked"], 3u64);
        assert_eq!(payload["n_seeds"], 3u64);
        assert_eq!(payload["seed_source"], "gliner_relex");
        assert_eq!(payload["model_version"], "test");

        pool.close().await;
    });
}

#[test]
fn link_with_noop_extractor_writes_no_rows_but_writes_audit_row() {
    if skip_if_no_supervisor() {
        return;
    }

    rt().block_on(async {
        let Some((_cluster, pool)) = bring_up_pg("noop").await else {
            return;
        };

        let memory_id = seed_meta_memory(
            &pool,
            "the body that no extractor will inspect",
            &serde_json::json!({}),
            None,
        )
        .await
        .expect("seed_meta_memory");

        let rows_before = fetch_since(&pool, 0, FETCH_LIMIT)
            .await
            .expect("fetch_since before")
            .len();

        let extractor = NoOpEntityExtractor::new();
        let outcome = link_memory_entities(
            &extractor,
            &pool,
            memory_id,
            "L0",
            "the body that no extractor will inspect",
        )
        .await
        .expect("link should succeed with NoOp");

        assert_eq!(outcome.n_entities_linked, 0);
        assert!(outcome.seeds.ids.is_empty());
        assert_eq!(outcome.seeds.source, SeedSource::None);

        // memory_entities table should still be empty for this memory_id.
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM memory_entities WHERE memory_id = $1",
        )
        .bind(memory_id)
        .fetch_one(&pool)
        .await
        .expect("count memory_entities");
        assert_eq!(count, 0);

        // But the audit row IS still written so operators can see
        // "daemon ran without GLiNER" in the observation phase.
        let rows_after = fetch_since(&pool, 0, FETCH_LIMIT)
            .await
            .expect("fetch_since after");
        assert_eq!(rows_after.len(), rows_before + 1);
        let link_row = rows_after
            .iter()
            .find(|r| r.actor == "memory_linker" && r.action == "entity_link")
            .expect("memory_linker/entity_link row present");
        assert_eq!(link_row.payload["seed_source"], "none");
        assert_eq!(link_row.payload["n_entities_linked"], 0u64);
        assert_eq!(link_row.payload["model_version"], serde_json::Value::Null);

        pool.close().await;
    });
}

#[test]
fn link_is_idempotent_on_rerun_with_same_seeds() {
    if skip_if_no_supervisor() {
        return;
    }

    rt().block_on(async {
        let Some((_cluster, pool)) = bring_up_pg("idem").await else {
            return;
        };

        let e1 = upsert_test_entity(&pool, "person", "bob").await;
        let e2 = upsert_test_entity(&pool, "drug", "aspirin").await;

        let memory_id = seed_meta_memory(
            &pool,
            "bob took aspirin",
            &serde_json::json!({}),
            None,
        )
        .await
        .expect("seed");

        let extractor = StaticEntityExtractor::with_ids(vec![e1, e2]);

        // First call: 2 fresh links.
        let out1 = link_memory_entities(&extractor, &pool, memory_id, "L0", "bob took aspirin")
            .await
            .expect("first link");
        assert_eq!(out1.n_entities_linked, 2);

        // Second call: 0 new links (ON CONFLICT DO NOTHING).
        let out2 = link_memory_entities(&extractor, &pool, memory_id, "L0", "bob took aspirin")
            .await
            .expect("second link");
        assert_eq!(out2.n_entities_linked, 0);
        assert_eq!(out2.seeds.ids, vec![e1, e2], "seeds still returned");

        // Final count is 2 (no duplicates).
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM memory_entities WHERE memory_id = $1",
        )
        .bind(memory_id)
        .fetch_one(&pool)
        .await
        .expect("count");
        assert_eq!(count, 2);

        // Both audit rows were written (two separate link_memory_entities calls).
        let rows = fetch_since(&pool, 0, FETCH_LIMIT)
            .await
            .expect("fetch_since");
        let link_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.actor == "memory_linker" && r.action == "entity_link")
            .collect();
        assert_eq!(link_rows.len(), 2, "two audit rows even on idempotent rerun");
        // Second row records the 0-link outcome.
        assert_eq!(link_rows[1].payload["n_entities_linked"], 0u64);
        assert_eq!(link_rows[1].payload["n_seeds"], 2u64);

        pool.close().await;
    });
}
