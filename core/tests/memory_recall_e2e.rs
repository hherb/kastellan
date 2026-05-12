//! End-to-end smoke for `memory::recall` — the first non-trivial
//! sqlx query path in `core`, and the first real consumer of the
//! `memories` table.
//!
//! What this test proves against a per-test PG cluster:
//!
//!   1. `db::memories::insert_memory` writes rows with a `vector(1024)`
//!      embedding via the text-cast path; no pgvector Rust crate
//!      required.
//!   2. `db::memories::semantic_search` ranks the embedding-matched
//!      memory first under cosine distance.
//!   3. `db::memories::lexical_search` ranks the lexically-matched
//!      memory first under `ts_rank`.
//!   4. `core::memory::recall(modes = ALL)` fuses the two via RRF and
//!      returns the same memory as top-1 when both lanes vote
//!      consistently for it.
//!
//! ## How the test creates "matching" embeddings without an embedding
//! worker
//!
//! Three memories are seeded with bodies that share no surface words.
//! [`hhagent_tests_common::text_to_embedding`] hashes each body with
//! SHA-256 and uses the digest to seed a deterministic pseudo-random
//! unit vector of length `EMBEDDING_DIM`. Same text → same vector →
//! cosine 1.0; different texts → near-orthogonal vectors.
//!
//! Skips silently with `[SKIP]` lines on hosts without Postgres or a
//! reachable supervisor; `cargo test -- --nocapture` to see them.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use hhagent_core::memory::{recall, RecallModes, RecallParams};
use hhagent_db::memories::{insert_memory, lexical_search, semantic_search, EMBEDDING_DIM};
use hhagent_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, text_to_embedding,
    unique_suffix,
};

const BODY_A: &str = "alpha bravo charlie gathered for the briefing";
const BODY_B: &str = "delta echo foxtrot ran aground at midnight";
const BODY_C: &str = "golf hotel india signaled clear at dawn";

#[test]
fn recall_seeds_three_docs_and_ranks_target_first_per_mode_and_fused() {
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
        "recall-d",
        "recall-l",
        &format!("hhagent-supervisor-test-pg-recall-{suffix}"),
    );

    // recall is async + uses sqlx — needs a real tokio runtime.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime");

    rt.block_on(async {
        // Probe applies migrations 0001 + 0002 + 0003 + 0004 and writes
        // the bring-up audit row.
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "purpose": "memory-recall"}),
        )
        .await
        .expect("probe run");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("connect runtime pool");

        // ---- seed three memories ----
        let emb_a = text_to_embedding(BODY_A);
        let emb_b = text_to_embedding(BODY_B);
        let emb_c = text_to_embedding(BODY_C);
        assert_eq!(emb_a.len(), EMBEDDING_DIM);

        let id_a = insert_memory(&pool, BODY_A, &serde_json::json!({}), Some(&emb_a))
            .await
            .expect("insert A");
        let id_b = insert_memory(&pool, BODY_B, &serde_json::json!({}), Some(&emb_b))
            .await
            .expect("insert B");
        let id_c = insert_memory(&pool, BODY_C, &serde_json::json!({}), Some(&emb_c))
            .await
            .expect("insert C");
        assert_ne!(id_a, id_b);
        assert_ne!(id_b, id_c);

        // ---- semantic-only: target embedding == BODY_A's embedding,
        // so distance 0; the other two rows are ~1.0 distance away.
        let semantic_hits = semantic_search(&pool, &emb_a, 10)
            .await
            .expect("semantic_search");
        assert_eq!(
            semantic_hits.first().copied(),
            Some(id_a),
            "semantic top-1 must be A: {semantic_hits:?}"
        );

        // ---- lexical-only: query "alpha" appears only in BODY_A's
        // tsvector, so the result set has exactly one row.
        let lexical_hits = lexical_search(&pool, "alpha", 10)
            .await
            .expect("lexical_search");
        assert_eq!(
            lexical_hits,
            vec![id_a],
            "lexical for 'alpha' must return only A: {lexical_hits:?}"
        );

        // ---- recall(SEMANTIC_ONLY): equivalent to the lane query
        // through the public surface, hydrated.
        let semantic_only = recall(
            &pool,
            &RecallParams {
                query_text: None,
                query_embedding: Some(&emb_a),
                k: 5,
                modes: RecallModes::SEMANTIC_ONLY,
            },
        )
        .await
        .expect("recall semantic-only");
        assert_eq!(
            semantic_only.first().map(|m| m.id),
            Some(id_a),
            "recall(SEMANTIC_ONLY) top-1 must be A"
        );
        assert_eq!(semantic_only.first().map(|m| m.body.as_str()), Some(BODY_A));

        // ---- recall(LEXICAL_ONLY): only A matches "alpha", so
        // exactly one hydrated result.
        let lexical_only = recall(
            &pool,
            &RecallParams {
                query_text: Some("alpha"),
                query_embedding: None,
                k: 5,
                modes: RecallModes::LEXICAL_ONLY,
            },
        )
        .await
        .expect("recall lexical-only");
        assert_eq!(lexical_only.len(), 1);
        assert_eq!(lexical_only[0].id, id_a);

        // ---- recall(ALL): both lanes vote for A; RRF fuses; top-1
        // must still be A. The two non-matching memories appear in
        // semantic but not lexical, so they share the lower fused
        // score deterministically.
        let fused = recall(
            &pool,
            &RecallParams {
                query_text: Some("alpha"),
                query_embedding: Some(&emb_a),
                k: 5,
                modes: RecallModes::ALL,
            },
        )
        .await
        .expect("recall fused");
        assert!(
            !fused.is_empty(),
            "fused recall returned empty result set"
        );
        assert_eq!(
            fused[0].id, id_a,
            "fused top-1 must be A; got {:?}",
            fused.iter().map(|m| m.id).collect::<Vec<_>>()
        );
        assert_eq!(fused[0].body, BODY_A);

        // The fused list should also include the two semantic-only
        // candidates somewhere below A — proves RRF is fusing rather
        // than intersecting.
        let fused_ids: Vec<i64> = fused.iter().map(|m| m.id).collect();
        assert!(
            fused_ids.contains(&id_b) && fused_ids.contains(&id_c),
            "fused list should include B and C below A; got {fused_ids:?}"
        );

        pool.close().await;
    });
}
