//! The `<recalled>` layer gate (issue #785), against a real Postgres.
//!
//! **What this pins.** No recall lane — semantic, lexical, graph or
//! entity-similarity — and not the fused `recall()` either, returns a row
//! from L0 (meta-rules) or L3 (skills). L1, L2 and L4 stay recallable. The
//! rule itself is the pure `MemoryLayer::is_recallable`
//! (`db/src/memories/recall_layers.rs`); this suite proves each lane's SQL
//! actually applies it.
//!
//! **Why it matters.** L0 and L3 each have their own, gated door into the
//! prompt: `<l0_meta_rules>` loads only the newest version of each rule, and
//! `<skills>` only operator-approved skills. Before #785 the recall lanes
//! read every layer, so an `untrusted` skill — or a superseded meta-rule —
//! could reach the planner through `<recalled>` instead.
//!
//! **How the fixture makes every lane want every row.** One row per layer
//! (two at L3: `untrusted` and `pinned`), and every row gets the *same*
//! body word, the *same* embedding, and a link to the *same* entity (which
//! is un-quarantined and embedded with that same vector). Without the gate,
//! each lane would return all six rows; with it, exactly the three
//! recallable ones. Asserting the exact set is also the positive control: an
//! empty result — a lane that silently ran nothing — fails it.
//!
//! Skips with a `[SKIP]` line on hosts without Postgres or a reachable
//! supervisor; set `KASTELLAN_PG_REQUIRE_E2E=1` to turn a skip into a failure.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::collections::BTreeSet;

use kastellan_core::memory::{recall, RecallModes, RecallParams};
use kastellan_db::entity_embedding::{entity_similarity_search, set_entity_embedding};
use kastellan_db::graph::{Graph, PgGraph};
use kastellan_db::memories::{
    graph_search, insert_memory, insert_memory_at_layer, lexical_search,
    link_memory_to_entities, seed_meta_memory, semantic_search, MemoryLayer,
};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, text_to_embedding,
    unique_suffix,
};

/// The word every fixture body shares, so the lexical lane matches them all.
const SHARED_WORD: &str = "zephyrgate";

/// Big enough that no lane truncates the six-row fixture: a missing row
/// must mean "filtered", never "cut by the limit".
const K: usize = 50;

/// Sort a lane's ranked id-list into a set, so the assertion is about
/// *which* rows came back — the gate's job — not their rank.
fn as_set(ids: &[i64]) -> BTreeSet<i64> {
    ids.iter().copied().collect()
}

#[test]
fn no_recall_lane_returns_l0_or_l3_rows() {
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
        "rlgate-d",
        "rlgate-l",
        &format!("kastellan-supervisor-test-pg-rlgate-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime");

    rt.block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "purpose": "recall-layer-gate"}),
        )
        .await
        .expect("probe run (applies migrations)");
        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("connect runtime pool");

        // ---- one row per layer, all identical to every lane ----
        let emb = text_to_embedding(SHARED_WORD);
        let body = |layer: &str| format!("{SHARED_WORD} fixture row for {layer}");

        let l0 = seed_meta_memory(
            &pool,
            &body("L0"),
            &serde_json::json!({"l0_rule_id": "rlgate-rule"}),
            Some(&emb),
        )
        .await
        .expect("seed L0");
        let at = |layer: MemoryLayer, meta: serde_json::Value| {
            let pool = pool.clone();
            let body = body(&format!("{layer:?}"));
            let emb = emb.clone();
            async move {
                insert_memory_at_layer(&pool, &body, &meta, Some(&emb), layer)
                    .await
                    .unwrap_or_else(|e| panic!("insert {layer:?}: {e}"))
            }
        };
        let l1 = at(MemoryLayer::Index, serde_json::json!({})).await;
        let l2 = insert_memory(&pool, &body("L2"), &serde_json::json!({}), Some(&emb))
            .await
            .expect("insert L2");
        let l3_untrusted = at(MemoryLayer::Skill, serde_json::json!({"trust": "untrusted"})).await;
        let l3_pinned = at(MemoryLayer::Skill, serde_json::json!({"trust": "pinned"})).await;
        let l4 = at(MemoryLayer::Digest, serde_json::json!({})).await;

        // ---- one shared, approved, embedded entity linked to every row ----
        let graph = PgGraph::new(&pool);
        let entity = graph
            .upsert_entity("person", SHARED_WORD, &serde_json::json!({}))
            .await
            .expect("upsert entity");
        sqlx::query("UPDATE entities SET quarantine = FALSE")
            .execute(&pool)
            .await
            .expect("unquarantine");
        assert!(
            set_entity_embedding(&pool, entity, &emb).await.expect("embed entity"),
            "the fixture entity must take the embedding"
        );
        for id in [l0, l1, l2, l3_untrusted, l3_pinned, l4] {
            link_memory_to_entities(&pool, id, &[entity]).await.expect("link");
        }

        let expected = as_set(&[l1, l2, l4]);

        // ---- each lane, called directly ----
        let semantic = semantic_search(&pool, &emb, K).await.expect("semantic");
        assert_eq!(as_set(&semantic), expected, "semantic lane");

        let lexical = lexical_search(&pool, SHARED_WORD, K).await.expect("lexical");
        assert_eq!(as_set(&lexical), expected, "lexical lane");

        let graph_hits = graph_search(&pool, &[entity], K, false).await.expect("graph");
        assert_eq!(as_set(&graph_hits), expected, "graph lane");

        let entity_hits =
            entity_similarity_search(&pool, &emb, 64, K, false).await.expect("entity");
        assert_eq!(as_set(&entity_hits), expected, "entity-similarity lane");

        // ---- the fused surface the prompt assembler uses ----
        let fused = recall(
            &pool,
            &RecallParams {
                query_text: Some(SHARED_WORD),
                query_embedding: Some(&emb),
                seed_entity_ids: Some(&[entity]),
                k: K,
                modes: RecallModes::ALL,
            },
        )
        .await
        .expect("recall(ALL)");
        let fused_ids: Vec<i64> = fused.iter().map(|m| m.id).collect();
        assert_eq!(as_set(&fused_ids), expected, "fused recall");
        assert!(
            fused.iter().all(|m| m.layer.is_recallable()),
            "every hydrated row must be at a recallable layer: {:?}",
            fused.iter().map(|m| (m.id, m.layer)).collect::<Vec<_>>()
        );
    });
}
