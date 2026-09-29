//! The `<recalled>` layer gate (issue #785), against a real Postgres.
//!
//! **What this pins.** No recall lane — semantic, lexical, graph or
//! entity-similarity — and not the fused `recall()` either, returns a row
//! from L0 (meta-rules) or L3 (skills). L1, L2 and L4 stay recallable. The
//! rule itself is the pure `MemoryLayer::is_recallable`
//! (`db/src/memories/recall_layers.rs`); this suite proves each lane's SQL
//! actually applies it, and applies it **before** the lane's `LIMIT`.
//!
//! **Why it matters.** L0 and L3 each have their own, gated door into the
//! prompt: `<l0_meta_rules>` loads only the newest version of each rule, and
//! `<skills>` only operator-approved skills. Before #785 the recall lanes
//! read every layer, so an `untrusted` skill — or a superseded meta-rule —
//! could reach the planner through `<recalled>` instead.
//!
//! **How the fixture works.** Six rows: the three *recallable* ones (L1, L2,
//! L4) and three *decoys* (L0, an `untrusted` L3 and a `pinned` L3 — even an
//! approved skill stays out of `<recalled>`). Every lane matches all six,
//! and in every lane each decoy is a **strictly better** match than every
//! recallable row:
//!
//! | Lane | Recallable rows | Decoys |
//! | --- | --- | --- |
//! | semantic | embedding `EMB_RECALLABLE` | embedding = the query (distance 0) |
//! | lexical | the query word once | the query word twice (higher `ts_rank`) |
//! | graph | linked to one seed entity | linked to both seed entities (count 2) |
//! | entity | via `shared` (far from the query) | also via `near` (= the query) |
//!
//! So at a large `k` each lane must return exactly the recallable three —
//! and at `k = 3` too: a filter applied *after* the `LIMIT` would leave
//! nothing, and no filter would leave the decoys. The fixture's own
//! preconditions (six rows embedded, the decoys ranking first when nothing
//! filters them) are asserted before any lane runs, so a drifted fixture
//! fails instead of quietly testing nothing. Asserting exact sets is also
//! the positive control: an empty result fails.
//!
//! The entity lane gets one more case: with a fan-out of **one**, the
//! nearest entity (`near`) links only decoys, so a lane that let it take
//! the fan-out slot would return nothing at all.
//!
//! Skips with a `[SKIP]` line on hosts without Postgres or a reachable
//! supervisor; set `KASTELLAN_PG_REQUIRE_E2E=1` to turn a skip into a failure
//! (the `pg` profile of `scripts/run-e2e-gate.sh` does).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::collections::BTreeSet;

use kastellan_core::memory::{recall, RecallModes, RecallParams};
use kastellan_db::entity_embedding::{entity_similarity_search, set_entity_embedding};
use kastellan_db::graph::{Graph, PgGraph};
use kastellan_db::memories::{
    graph_search, insert_memory, insert_memory_at_layer, lexical_search,
    link_memory_to_entities, seed_meta_memory, semantic_search, vector_literal, MemoryLayer,
};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, text_to_embedding,
    unique_suffix,
};

/// The query word; every fixture body contains it.
const SHARED_WORD: &str = "zephyrgate";

/// Seeds the embedding the recallable rows (and the `shared` entity) carry
/// — deliberately *not* the query's, so the decoys sit closer.
const EMB_RECALLABLE: &str = "rlgate-recallable";

/// Big enough that no lane truncates the six-row fixture: a missing row
/// must mean "filtered", never "cut by the limit".
const K: usize = 50;

/// Exactly the number of recallable rows: the decoys outrank them all, so
/// only a gate applied before the `LIMIT` returns them.
const K_TIGHT: usize = 3;

/// Production's entity fan-out; the fixture has two entities.
const FANOUT: i64 = 64;

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

        // ---- rows: three recallable, three decoys that outrank them ----
        let emb_query = text_to_embedding(SHARED_WORD);
        let emb_recallable = text_to_embedding(EMB_RECALLABLE);
        let recallable_body = |layer: &str| format!("{SHARED_WORD} fixture row for {layer}");
        let decoy_body = |layer: &str| format!("{SHARED_WORD} {SHARED_WORD} decoy row for {layer}");

        let l0 = seed_meta_memory(
            &pool,
            &decoy_body("L0"),
            &serde_json::json!({"l0_rule_id": "rlgate-rule"}),
            Some(&emb_query),
        )
        .await
        .expect("seed L0");
        let at = |layer: MemoryLayer, body: String, emb: Vec<f32>, meta: serde_json::Value| {
            let pool = pool.clone();
            async move {
                insert_memory_at_layer(&pool, &body, &meta, Some(&emb), layer)
                    .await
                    .unwrap_or_else(|e| panic!("insert {layer:?}: {e}"))
            }
        };
        let none = || serde_json::json!({});
        let l1 = at(MemoryLayer::Index, recallable_body("L1"), emb_recallable.clone(), none()).await;
        let l2 = insert_memory(&pool, &recallable_body("L2"), &none(), Some(&emb_recallable))
            .await
            .expect("insert L2");
        let l3_untrusted = at(
            MemoryLayer::Skill,
            decoy_body("L3u"),
            emb_query.clone(),
            serde_json::json!({"trust": "untrusted"}),
        )
        .await;
        let l3_pinned = at(
            MemoryLayer::Skill,
            decoy_body("L3p"),
            emb_query.clone(),
            serde_json::json!({"trust": "pinned"}),
        )
        .await;
        let l4 = at(MemoryLayer::Digest, recallable_body("L4"), emb_recallable.clone(), none()).await;

        let recallable = [l1, l2, l4];
        let decoys = [l0, l3_untrusted, l3_pinned];
        let expected = as_set(&recallable);

        // ---- entities: `shared` links all six, `near` only the decoys ----
        let graph = PgGraph::new(&pool);
        let shared = graph
            .upsert_entity("person", SHARED_WORD, &none())
            .await
            .expect("upsert shared entity");
        let near = graph
            .upsert_entity("person", "rlgate-near", &none())
            .await
            .expect("upsert near entity");
        sqlx::query("UPDATE entities SET quarantine = FALSE")
            .execute(&pool)
            .await
            .expect("unquarantine");
        for (entity, emb) in [(shared, &emb_recallable), (near, &emb_query)] {
            assert!(
                set_entity_embedding(&pool, entity, emb).await.expect("embed entity"),
                "fixture entity {entity} must take its embedding"
            );
        }
        for id in recallable.iter().chain(&decoys) {
            link_memory_to_entities(&pool, *id, &[shared]).await.expect("link shared");
        }
        for id in decoys {
            link_memory_to_entities(&pool, id, &[near]).await.expect("link near");
        }
        let seeds = [shared, near];

        // ---- fixture preconditions: without a gate, the decoys win ----
        let embedded: i64 =
            sqlx::query_scalar("SELECT count(*) FROM memories WHERE embedding IS NOT NULL")
                .fetch_one(&pool)
                .await
                .expect("count embedded");
        assert_eq!(embedded, 6, "fixture: all six rows must be embedded");
        let ungated_semantic: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM memories WHERE embedding IS NOT NULL \
             ORDER BY embedding <=> $1::vector LIMIT 3",
        )
        .bind(vector_literal(&emb_query))
        .fetch_all(&pool)
        .await
        .expect("ungated semantic");
        assert_eq!(as_set(&ungated_semantic), as_set(&decoys), "fixture: decoys nearest");
        let ungated_lexical: Vec<i64> = sqlx::query_scalar(
            "SELECT m.id FROM memories m, plainto_tsquery('simple', $1) q \
             WHERE m.tsv @@ q ORDER BY ts_rank(m.tsv, q) DESC, m.id ASC LIMIT 3",
        )
        .bind(SHARED_WORD)
        .fetch_all(&pool)
        .await
        .expect("ungated lexical");
        assert_eq!(as_set(&ungated_lexical), as_set(&decoys), "fixture: decoys rank first");
        let lexical_matches: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM memories WHERE tsv @@ plainto_tsquery('simple', $1)",
        )
        .bind(SHARED_WORD)
        .fetch_one(&pool)
        .await
        .expect("count lexical matches");
        assert_eq!(lexical_matches, 6, "fixture: every row must match the word");

        // ---- each lane, called directly; every mismatch is reported ----
        let mut failures: Vec<String> = Vec::new();
        let mut check = |label: &str, ids: Vec<i64>| {
            if as_set(&ids) != expected {
                failures.push(format!("{label}: got {ids:?}, want {expected:?}"));
            }
        };

        for k in [K, K_TIGHT] {
            check(&format!("semantic k={k}"), semantic_search(&pool, &emb_query, k).await.expect("semantic"));
            check(&format!("lexical k={k}"), lexical_search(&pool, SHARED_WORD, k).await.expect("lexical"));
            for include_quarantined in [false, true] {
                check(
                    &format!("graph k={k} include_quarantined={include_quarantined}"),
                    graph_search(&pool, &seeds, k, include_quarantined).await.expect("graph"),
                );
                check(
                    &format!("entity k={k} include_quarantined={include_quarantined}"),
                    entity_similarity_search(&pool, &emb_query, FANOUT, k, include_quarantined)
                        .await
                        .expect("entity"),
                );
            }
        }
        // `near` is the nearest entity but links only decoys: it must not
        // take the single fan-out slot.
        check(
            "entity fan-out=1",
            entity_similarity_search(&pool, &emb_query, 1, K, false).await.expect("entity fan-out"),
        );

        // Report the lanes before the fused call: a leaking lane also trips
        // `recall()`'s post-hydration backstop, whose debug-build panic
        // would otherwise pre-empt this per-lane report.
        assert!(failures.is_empty(), "layer gate leaks:\n{}", failures.join("\n"));

        // ---- the fused surface the prompt assembler uses ----
        for k in [K, K_TIGHT] {
            let fused = recall(
                &pool,
                &RecallParams {
                    query_text: Some(SHARED_WORD),
                    query_embedding: Some(&emb_query),
                    seed_entity_ids: Some(&seeds),
                    k,
                    modes: RecallModes::ALL,
                },
            )
            .await
            .expect("recall(ALL)");
            let fused_ids: Vec<i64> = fused.iter().map(|m| m.id).collect();
            assert_eq!(as_set(&fused_ids), expected, "fused recall k={k}");
        }
    });
}
