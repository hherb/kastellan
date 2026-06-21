//! End-to-end DB integration coverage for the **forward entity
//! embed-on-insert** path
//! ([`kastellan_core::entity_extraction::gliner_relex::upsert_entities_and_relations`]
//! with an injected [`Embedder`]).
//!
//! PR #335 shipped the entity-embedding *backfill* + the entity-similarity
//! recall lane, but new entities written by the upsert still landed with
//! `embedding IS NULL` until a manual `entities reembed` run. This path embeds
//! the entities the upsert just *created*, so a freshly-seen entity is
//! immediately searchable via the lane. Scenarios:
//!
//!   1. a brand-new entity is embedded on insert + the lane surfaces its
//!      linked memory (no backfill run);
//!   2. a conflict-hit (already-existing) entity is NOT re-embedded — the
//!      backfill owns still-NULL existing rows (the #324/#325 split);
//!   3. degrade-and-warn: an embedder that declines leaves the row NULL and
//!      the upsert still succeeds.
//!
//! Each scenario brings up its own per-test Postgres cluster. Skips silently
//! with `[SKIP]` lines on hosts without Postgres or a reachable supervisor.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};

use kastellan_core::entity_extraction::gliner_relex::upsert_entities_and_relations;
use kastellan_core::memory::embedder::Embedder;
use kastellan_core::workers::gliner_relex::{Entity, ExtractResponse};
use kastellan_db::entity_embedding::entity_similarity_search;
use kastellan_db::memories::{insert_memory, link_memory_to_entities, EMBEDDING_DIM};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix, PgCluster,
};

/// Test embedder: counts calls, returns a fixed vector (or `None`).
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

/// One entity, no triples — the minimal merged extractor response.
fn one_entity(text: &str, label: &str) -> ExtractResponse {
    ExtractResponse {
        entities: vec![Entity {
            text: text.into(),
            label: label.into(),
            start: 0,
            end: text.len() as u32,
            score: 0.99,
        }],
        triples: vec![],
    }
}

async fn bring_up_pg(label: &str) -> Option<(PgCluster, sqlx::PgPool)> {
    if skip_if_no_supervisor() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        &format!("fe-{label}-d"),
        &format!("fe-{label}-l"),
        &format!("kastellan-supervisor-test-pg-fwd-embed-{label}-{suffix}"),
    );
    kastellan_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"purpose": format!("entity-forward-embed-{label}")}),
    )
    .await
    .expect("probe run");
    let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("connect runtime pool");
    Some((cluster, pool))
}

/// True iff entity `id` has a non-NULL embedding.
async fn entity_is_embedded(pool: &sqlx::PgPool, id: i64) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT embedding IS NOT NULL FROM entities WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("embedding null-check")
}

// ---------------------------------------------------------------------------
// Scenario 1 — a new entity is embedded on insert + the lane finds its memory
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_entity_is_embedded_on_insert_and_lane_surfaces_linked_memory() {
    let Some((_cluster, pool)) = bring_up_pg("surface").await else {
        return;
    };

    let embedder = FakeEmbedder::returning(Some(unit_vec_e0()));
    let merged = one_entity("Carol", "person");
    let outcome = upsert_entities_and_relations(&pool, &merged, &embedder)
        .await
        .expect("upsert");

    // The upsert created the entity and embedded it on the way out.
    assert_eq!(outcome.n_entities_upserted_new, 1);
    assert_eq!(embedder.call_count(), 1, "the one new entity is embedded once");
    let entity_id = outcome.entity_ids[0];
    assert!(entity_is_embedded(&pool, entity_id).await, "embedding populated on insert");

    // Link a memory and (entities are born quarantined) approve it, then the
    // lane surfaces the linked memory for a query near the entity — no
    // `entities reembed` backfill was ever run.
    let mem_id = insert_memory(&pool, "a memory about carol", &serde_json::json!({}), None)
        .await
        .expect("insert memory");
    link_memory_to_entities(&pool, mem_id, &[entity_id]).await.expect("link");
    sqlx::query("UPDATE entities SET quarantine = FALSE")
        .execute(&pool)
        .await
        .expect("unquarantine");

    let hits = entity_similarity_search(&pool, &unit_vec_e0(), 64, 10, false)
        .await
        .expect("lane");
    assert!(hits.contains(&mem_id), "linked memory surfaces via the entity lane");

    pool.close().await;
}

// ---------------------------------------------------------------------------
// Scenario 2 — a conflict-hit entity is NOT re-embedded (backfill owns it)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conflict_hit_entity_is_not_reembedded() {
    let Some((_cluster, pool)) = bring_up_pg("conflict").await else {
        return;
    };

    let embedder = FakeEmbedder::returning(Some(unit_vec_e0()));
    let merged = one_entity("Dave", "person");

    // First upsert: the entity is created and embedded.
    let out1 = upsert_entities_and_relations(&pool, &merged, &embedder)
        .await
        .expect("upsert 1");
    assert_eq!(out1.n_entities_upserted_new, 1);
    assert_eq!(embedder.call_count(), 1);

    // Second upsert of the SAME entity: a conflict hit. The forward path
    // only embeds rows it just created, so the embedder is not called again.
    let out2 = upsert_entities_and_relations(&pool, &merged, &embedder)
        .await
        .expect("upsert 2");
    assert_eq!(out2.n_entities_upserted_new, 0, "conflict hit creates no new row");
    assert_eq!(out2.entity_ids[0], out1.entity_ids[0], "same id resolved");
    assert_eq!(
        embedder.call_count(),
        1,
        "conflict-hit entity is not re-embedded (still the backfill's job)"
    );

    pool.close().await;
}

// ---------------------------------------------------------------------------
// Scenario 3 — degrade-and-warn: a declining embedder leaves the row NULL
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declined_embed_leaves_entity_null_and_upsert_succeeds() {
    let Some((_cluster, pool)) = bring_up_pg("decline").await else {
        return;
    };

    // Embedder always returns None (a NoOp, or a soft-failed RouterEmbedder).
    let embedder = FakeEmbedder::returning(None);
    let merged = one_entity("Erin", "person");
    let outcome = upsert_entities_and_relations(&pool, &merged, &embedder)
        .await
        .expect("upsert still succeeds despite declined embed");

    assert_eq!(outcome.n_entities_upserted_new, 1);
    assert_eq!(embedder.call_count(), 1, "embed was attempted");
    assert!(
        !entity_is_embedded(&pool, outcome.entity_ids[0]).await,
        "declined embed leaves the row NULL (backfill will catch it later)"
    );

    pool.close().await;
}
