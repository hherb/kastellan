//! End-to-end DB integration coverage for the entity-embedding **backfill** +
//! the **entity-similarity recall lane**
//! ([`kastellan_core::memory::entity_reembed::reembed_entities_null`] +
//! [`kastellan_db::entity_embedding::entity_similarity_search`]).
//!
//! `entities.embedding` is NULL for every row today. The backfill embeds each
//! `"<kind>: <name>"` through the injected `Embedder`; the lane then finds the
//! memories linked to query-similar entities. Scenarios:
//!
//!   1. backfill populates a NULL entity embedding + the lane surfaces its
//!      linked memory;
//!   2. it is idempotent — a re-run embeds nothing;
//!   3. it degrades-and-warns — a failing embed leaves the entity NULL;
//!   4. the lane excludes quarantined entities (privacy invariant).
//!
//! Each scenario brings up its own per-test Postgres cluster. Skips silently
//! with `[SKIP]` lines on hosts without Postgres or a reachable supervisor.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};

use kastellan_core::memory::embedder::Embedder;
use kastellan_core::memory::entity_reembed::reembed_entities_null;
use kastellan_core::memory::reembed::ReembedReport;
use kastellan_db::entity_embedding::{entity_similarity_search, load_unembedded_entities};
use kastellan_db::graph::{Graph, PgGraph};
use kastellan_db::memories::{insert_memory, link_memory_to_entities, EMBEDDING_DIM};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};

/// Test embedder: counts calls, returns a fixed unit vector (or `None`).
struct FakeEmbedder {
    calls: AtomicUsize,
    out: Option<Vec<f32>>,
}
impl FakeEmbedder {
    fn returning(out: Option<Vec<f32>>) -> Self {
        Self { calls: AtomicUsize::new(0), out }
    }
    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}
#[async_trait]
impl Embedder for FakeEmbedder {
    async fn embed_for_storage(&self, _text: &str) -> Option<Vec<f32>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.out.clone()
    }
}

/// A deterministic `EMBEDDING_DIM`-length unit vector: 1.0 in slot 0, else 0.
fn unit_vec_e0() -> Vec<f32> {
    let mut v = vec![0.0f32; EMBEDDING_DIM];
    v[0] = 1.0;
    v
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime")
}

/// Un-quarantine every entity (migration 0015 quarantines new rows by
/// default; the lane filters quarantined rows in production).
async fn unquarantine_all(pool: &sqlx::PgPool) {
    sqlx::query("UPDATE entities SET quarantine = FALSE")
        .execute(pool)
        .await
        .expect("unquarantine all entities");
}

// ---------------------------------------------------------------------------
// Scenario 1 — backfill populates a NULL entity + the lane finds its memory
// ---------------------------------------------------------------------------

#[test]
fn reembed_populates_entity_and_lane_surfaces_linked_memory() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "entre-d",
        "entre-l",
        &format!("kastellan-supervisor-test-pg-entre1-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "entity-reembed-1"}),
        )
        .await
        .expect("probe");

        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // Seed a memory and an entity, link them. The entity has a NULL
        // embedding (nothing populates it yet).
        let mem_id = insert_memory(&pool, "a memory about alice", &serde_json::json!({}), None)
            .await
            .expect("insert memory");
        let graph = PgGraph::new(&pool);
        let alice_id = graph
            .upsert_entity("person", "alice", &serde_json::json!({}))
            .await
            .expect("upsert entity");
        link_memory_to_entities(&pool, mem_id, &[alice_id])
            .await
            .expect("link");
        unquarantine_all(&pool).await;

        // Pre-condition: the entity is unembedded, so the lane finds nothing.
        let before = entity_similarity_search(&pool, &unit_vec_e0(), 64, 10, false)
            .await
            .expect("lane before");
        assert!(before.is_empty(), "no embedded entity yet -> empty lane");

        // Backfill embeds the entity (FakeEmbedder returns the e0 unit vec).
        let embedder = FakeEmbedder::returning(Some(unit_vec_e0()));
        let report = reembed_entities_null(&pool, &embedder).await.expect("reembed");
        assert_eq!(report, ReembedReport { scanned: 1, embedded: 1, skipped: 0 });
        assert_eq!(embedder.call_count(), 1);

        // The lane now surfaces the linked memory for a query near the entity.
        let after = entity_similarity_search(&pool, &unit_vec_e0(), 64, 10, false)
            .await
            .expect("lane after");
        assert!(after.contains(&mem_id), "linked memory surfaces via the entity lane");

        // Nothing left unembedded.
        let remaining = load_unembedded_entities(&pool).await.expect("scan after");
        assert!(remaining.is_empty(), "no NULL-embedding entities remain");

        pool.close().await;
    });
}

// ---------------------------------------------------------------------------
// Scenario 2 — idempotent re-run embeds nothing
// ---------------------------------------------------------------------------

#[test]
fn reembed_entities_is_idempotent_on_rerun() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "entre-d",
        "entre-l",
        &format!("kastellan-supervisor-test-pg-entre2-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "entity-reembed-2"}),
        )
        .await
        .expect("probe");
        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let graph = PgGraph::new(&pool);
        graph
            .upsert_entity("person", "bob", &serde_json::json!({}))
            .await
            .expect("upsert entity");

        let first = FakeEmbedder::returning(Some(unit_vec_e0()));
        let r1 = reembed_entities_null(&pool, &first).await.expect("reembed 1");
        assert_eq!(r1, ReembedReport { scanned: 1, embedded: 1, skipped: 0 });

        // Re-run: the row is no longer NULL, so it is not scanned — embedder
        // never called.
        let second = FakeEmbedder::returning(Some(unit_vec_e0()));
        let r2 = reembed_entities_null(&pool, &second).await.expect("reembed 2");
        assert_eq!(r2, ReembedReport { scanned: 0, embedded: 0, skipped: 0 });
        assert_eq!(second.call_count(), 0, "no double-embed on re-run");

        pool.close().await;
    });
}

// ---------------------------------------------------------------------------
// Scenario 3 — degrade-and-warn: a failing embed leaves the entity NULL
// ---------------------------------------------------------------------------

#[test]
fn reembed_entities_degrades_and_warns_leaving_null() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "entre-d",
        "entre-l",
        &format!("kastellan-supervisor-test-pg-entre3-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "entity-reembed-3"}),
        )
        .await
        .expect("probe");
        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let graph = PgGraph::new(&pool);
        let id = graph
            .upsert_entity("person", "carol", &serde_json::json!({}))
            .await
            .expect("upsert entity");

        // Embedder always returns None: the row is scanned but skipped.
        let embedder = FakeEmbedder::returning(None);
        let report = reembed_entities_null(&pool, &embedder)
            .await
            .expect("reembed degrades, not errors");
        assert_eq!(report, ReembedReport { scanned: 1, embedded: 0, skipped: 1 });
        assert_eq!(embedder.call_count(), 1);

        // The entity is still NULL — it remains in the unembedded scan.
        let remaining = load_unembedded_entities(&pool).await.expect("scan after");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].0, id);

        pool.close().await;
    });
}

// ---------------------------------------------------------------------------
// Scenario 4 — the lane excludes quarantined entities
// ---------------------------------------------------------------------------

#[test]
fn entity_lane_excludes_quarantined_entities() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "entre-d",
        "entre-l",
        &format!("kastellan-supervisor-test-pg-entre4-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "entity-reembed-4"}),
        )
        .await
        .expect("probe");
        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let mem_id = insert_memory(&pool, "a quarantined-entity memory", &serde_json::json!({}), None)
            .await
            .expect("insert memory");
        let graph = PgGraph::new(&pool);
        let dave_id = graph
            .upsert_entity("person", "dave", &serde_json::json!({}))
            .await
            .expect("upsert entity");
        link_memory_to_entities(&pool, mem_id, &[dave_id])
            .await
            .expect("link");
        // Deliberately DO NOT un-quarantine: dave stays quarantine = TRUE.

        // Backfill embeds the quarantined entity (backfill is review-blind).
        let embedder = FakeEmbedder::returning(Some(unit_vec_e0()));
        let report = reembed_entities_null(&pool, &embedder).await.expect("reembed");
        assert_eq!(report, ReembedReport { scanned: 1, embedded: 1, skipped: 0 });

        // Production lane (include_quarantined=false) must NOT surface it.
        let prod = entity_similarity_search(&pool, &unit_vec_e0(), 64, 10, false)
            .await
            .expect("lane prod");
        assert!(!prod.contains(&mem_id), "quarantined entity must not leak its memory");

        // The operator path (include_quarantined=true) DOES see it — proving
        // the row is embedded + linked, only the quarantine filter hid it.
        let op = entity_similarity_search(&pool, &unit_vec_e0(), 64, 10, true)
            .await
            .expect("lane operator");
        assert!(op.contains(&mem_id), "operator path surfaces the quarantined entity's memory");

        pool.close().await;
    });
}
