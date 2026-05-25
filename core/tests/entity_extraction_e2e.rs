//! End-to-end tests for the v2 entity extractor.
//!
//! Two tiers:
//!   - **Mock-client tier** (always runs when PG is available): exercises
//!     [`upsert_entities_and_relations`] + [`build_extract_entities_payload`]
//!     + [`hhagent_db::audit::insert`] directly, without spawning a real
//!     worker. Pins the quarantine + idempotency + case-dedup behaviour
//!     against the live Postgres schema.
//!   - **Real-model tier** (skip-as-pass when worker preconditions
//!     missing): builds the full [`Client`] + [`GlinerRelexExtractor`]
//!     stack against the in-tree gliner-relex venv + on-disk weights and
//!     drives one short + one chunked extraction through the live
//!     model. Audit-row + dispatch-row pins assert the production wiring.
//!
//! Skip-as-pass for every dependency: missing supervisor / Postgres /
//! sandbox / venv / weights all surface as `[SKIP]` lines without
//! failing the test (matching `core/tests/gliner_relex_e2e.rs`'s
//! convention). On the DGX where the venv + weights are staged via
//! `scripts/workers/gliner-relex/install.sh`, the real-model tier
//! exercises real CPU inference end-to-end.
//!
//! See `docs/superpowers/specs/2026-05-19-entity-extraction-v2-gliner-relex-design.md`
//! and `docs/superpowers/plans/2026-05-19-entity-extraction-v2.md`
//! (Task 16) for the design and acceptance criteria.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;
use std::sync::Arc;

use hhagent_core::entity_extraction::gliner_relex::{
    upsert_entities_and_relations, GlinerRelexExtractor,
};
use hhagent_core::entity_extraction::{EntityExtractor, SeedSource};
use hhagent_core::scheduler::ToolEntry;
use hhagent_core::worker_lifecycle::{CompositeLifecycle, WorkerLifecycleManager};
use hhagent_core::workers::gliner_relex::{
    gliner_relex_entry, Client, Entity, ExtractResponse, GlinerRelexEnv, Triple,
    TripleEntity,
};
use hhagent_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor,
    skip_if_sandbox_unavailable, unique_suffix, PgCluster,
};

// ---------------------------------------------------------------------
// Cluster bring-up + skip helpers (mirroring `gliner_relex_e2e.rs`)
// ---------------------------------------------------------------------

/// Bring up a one-shot Postgres cluster + run the schema probe (which
/// applies all migrations including 0015) + open a runtime-role pool.
/// Returns `None` on hosts without `pg_ctl` / a working supervisor —
/// every caller turns that into a `[SKIP]` early return.
async fn bring_up_pg(label: &str) -> Option<(PgCluster, sqlx::PgPool)> {
    if skip_if_no_supervisor() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        &format!("ee-{label}-d"),
        &format!("ee-{label}-l"),
        &format!("hhagent-supervisor-test-pg-extract-{label}-{suffix}"),
    );
    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": format!("entity-extraction-{label}")}),
    )
    .await
    .expect("probe run");
    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("connect runtime pool");
    Some((cluster, pool))
}

/// Resolve the in-tree gliner-relex venv shim path. Returns `None` with
/// a `[SKIP]` print when the path doesn't exist — matches
/// `gliner_relex_e2e.rs::resolve_worker_script`.
fn resolve_worker_script() -> Option<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent")
        .to_path_buf();
    let script = workspace_root
        .join("workers/gliner-relex/.venv/bin/hhagent-worker-gliner-relex");
    if !script.exists() {
        eprintln!(
            "\n[SKIP] gliner-relex venv shim not built at {} — run scripts/workers/gliner-relex/install.sh\n",
            script.display()
        );
        return None;
    }
    Some(script)
}

/// Resolve the `multi-v1.0` weights dir. Honours
/// `HHAGENT_GLINER_RELEX_WEIGHTS_DIR` (when set verbatim — the daemon-
/// style override the run-command for these tests uses), otherwise
/// `HHAGENT_DATA_DIR`, otherwise `$HOME/.local/share/hhagent`. Skip on
/// missing.
fn resolve_weights_dir() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("HHAGENT_GLINER_RELEX_WEIGHTS_DIR") {
        let p = PathBuf::from(explicit);
        if p.is_dir() {
            return Some(p);
        }
        eprintln!(
            "\n[SKIP] HHAGENT_GLINER_RELEX_WEIGHTS_DIR points at {} which isn't a directory\n",
            p.display()
        );
        return None;
    }
    let data_dir = std::env::var("HHAGENT_DATA_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".local/share/hhagent"))
        })?;
    let weights = data_dir.join("workers/gliner-relex/weights/multi-v1.0");
    if !weights.is_dir() {
        eprintln!(
            "\n[SKIP] gliner-relex weights dir missing at {} — run scripts/workers/gliner-relex/install.sh\n",
            weights.display()
        );
        return None;
    }
    Some(weights)
}

/// Build the gliner-relex `ToolEntry` for the real-model tier. Returns
/// `None` (with a `[SKIP]` print) when any of: opt-in env-var off,
/// sandbox unavailable, supervisor unavailable, venv shim missing,
/// weights dir missing.
fn build_real_model_entry() -> Option<ToolEntry> {
    if std::env::var("HHAGENT_GLINER_RELEX_ENABLE").ok().as_deref() != Some("1") {
        eprintln!("\n[SKIP] HHAGENT_GLINER_RELEX_ENABLE != \"1\"\n");
        return None;
    }
    if skip_if_sandbox_unavailable() {
        return None;
    }
    if skip_if_no_supervisor() {
        return None;
    }
    let script = resolve_worker_script()?;
    let weights = resolve_weights_dir()?;
    let venv_dir = script
        .parent()
        .and_then(|bin| bin.parent())
        .expect("script_path is .venv/bin/<bin> — both parent levels must exist")
        .to_path_buf();
    let env = GlinerRelexEnv {
        script_path: script,
        venv_dir,
        weights_dir: weights,
        model_id: "knowledgator/gliner-relex-multi-v1.0".to_string(),
        device: "auto".to_string(),
        use_container_backend: false,
        container_image: None,
    };
    Some(gliner_relex_entry(&env))
}

// ---------------------------------------------------------------------
// Mock-client tier: direct DB-shape pins (no real worker)
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_creates_quarantined_entities() {
    let Some((_cluster, pool)) = bring_up_pg("quar").await else {
        return;
    };

    let merged = ExtractResponse {
        entities: vec![
            Entity {
                text: "Dr Smith".into(),
                label: "person".into(),
                start: 0,
                end: 8,
                score: 0.99,
            },
            Entity {
                text: "asthma".into(),
                label: "disease".into(),
                start: 15,
                end: 21,
                score: 0.95,
            },
        ],
        triples: vec![],
    };
    let outcome = upsert_entities_and_relations(&pool, &merged)
        .await
        .expect("upsert");

    assert_eq!(outcome.entity_ids.len(), 2);
    assert_eq!(outcome.n_entities_upserted_new, 2);
    assert_eq!(outcome.n_relations_inserted, 0);

    let qcount: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM entities WHERE id = ANY($1::bigint[]) AND quarantine = TRUE",
    )
    .bind(&outcome.entity_ids)
    .fetch_one(&pool)
    .await
    .expect("count quarantined");
    assert_eq!(qcount, 2, "newly extracted entities born quarantined");

    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_is_idempotent_on_rerun() {
    let Some((_cluster, pool)) = bring_up_pg("idem").await else {
        return;
    };

    let merged = ExtractResponse {
        entities: vec![
            Entity {
                text: "Alpha".into(),
                label: "concept".into(),
                start: 0,
                end: 5,
                score: 0.9,
            },
            Entity {
                text: "Beta".into(),
                label: "concept".into(),
                start: 10,
                end: 14,
                score: 0.9,
            },
        ],
        triples: vec![Triple {
            head: TripleEntity {
                text: "Alpha".into(),
                r#type: "concept".into(),
                start: 0,
                end: 5,
                entity_idx: 0,
            },
            tail: TripleEntity {
                text: "Beta".into(),
                r#type: "concept".into(),
                start: 10,
                end: 14,
                entity_idx: 1,
            },
            // Use a seeded relation kind (0017's relation_kinds FK
            // requires this). Pre-0017 this was the unseeded `relates_to`;
            // the choice is incidental to what this test pins (idempotent
            // re-insert), so `associated with` (the catch-all seed) is
            // a faithful substitute.
            relation: "associated with".into(),
            score: 0.88,
        }],
    };

    let out1 = upsert_entities_and_relations(&pool, &merged)
        .await
        .expect("first upsert");
    let out2 = upsert_entities_and_relations(&pool, &merged)
        .await
        .expect("second upsert");

    assert_eq!(out1.n_entities_upserted_new, 2);
    assert_eq!(
        out2.n_entities_upserted_new, 0,
        "rerun creates no new entity rows"
    );
    assert_eq!(out1.n_relations_inserted, 1);
    assert_eq!(
        out2.n_relations_inserted, 0,
        "rerun creates no new relation rows"
    );
    assert_eq!(
        out1.entity_ids, out2.entity_ids,
        "ids stable across reruns"
    );

    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_dedup_works_with_case_variants() {
    let Some((_cluster, pool)) = bring_up_pg("dedup").await else {
        return;
    };

    let merged_a = ExtractResponse {
        entities: vec![Entity {
            text: "Dr Smith".into(),
            label: "person".into(),
            start: 0,
            end: 8,
            score: 0.9,
        }],
        triples: vec![],
    };
    let merged_b = ExtractResponse {
        entities: vec![Entity {
            text: "DR SMITH".into(),
            label: "person".into(),
            start: 0,
            end: 8,
            score: 0.9,
        }],
        triples: vec![],
    };
    let out_a = upsert_entities_and_relations(&pool, &merged_a)
        .await
        .expect("a");
    let out_b = upsert_entities_and_relations(&pool, &merged_b)
        .await
        .expect("b");

    assert_eq!(
        out_a.entity_ids, out_b.entity_ids,
        "case-insensitive dedup: both resolve to the same id"
    );

    let display: String = sqlx::query_scalar("SELECT name FROM entities WHERE id = $1")
        .bind(out_a.entity_ids[0])
        .fetch_one(&pool)
        .await
        .expect("display");
    assert_eq!(display, "Dr Smith", "first writer's display preserved");

    pool.close().await;
}

/// Bug-of-omission regression pin: a future edit that replaces the no-op
/// `SET name_norm = entities.name_norm` with e.g. `SET quarantine = TRUE`
/// would silently re-quarantine operator-approved entities on next
/// re-extraction. This test catches that — Issue #90's load-bearing
/// invariant for the operator quarantine-review CLI (PR #93).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_preserves_operator_unquarantine_decision() {
    let Some((_cluster, pool)) = bring_up_pg("preserve-quar").await else {
        return;
    };

    // Seed one entity via the production path — it lands quarantined.
    let merged = ExtractResponse {
        entities: vec![Entity {
            text: "Dr Smith".into(),
            label: "person".into(),
            start: 0,
            end: 8,
            score: 0.99,
        }],
        triples: vec![],
    };
    let out1 = upsert_entities_and_relations(&pool, &merged)
        .await
        .expect("first upsert");
    assert_eq!(out1.entity_ids.len(), 1);
    let entity_id = out1.entity_ids[0];

    // Simulate `hhagent-cli entities approve <id>` — operator approves
    // the entity, flipping quarantine to FALSE.
    sqlx::query("UPDATE entities SET quarantine = FALSE WHERE id = $1")
        .bind(entity_id)
        .execute(&pool)
        .await
        .expect("operator approve simulation");

    // Re-extract the same entity. The upsert path hits ON CONFLICT.
    let out2 = upsert_entities_and_relations(&pool, &merged)
        .await
        .expect("second upsert");
    assert_eq!(out2.n_entities_upserted_new, 0, "no new row created");
    assert_eq!(out2.entity_ids, vec![entity_id], "same id returned");

    // The load-bearing assertion: the no-op SET must not have
    // clobbered the operator's approval.
    let quarantine_after: bool =
        sqlx::query_scalar("SELECT quarantine FROM entities WHERE id = $1")
            .bind(entity_id)
            .fetch_one(&pool)
            .await
            .expect("read back quarantine");
    assert!(
        !quarantine_after,
        "ON CONFLICT path must preserve operator approval (quarantine=FALSE)"
    );

    pool.close().await;
}

/// Mixed-batch counter pin: existing tests cover all-new
/// (`upsert_creates_quarantined_entities`) and all-existing
/// (`upsert_is_idempotent_on_rerun`). This pins the only uncovered
/// case — one new + one pre-existing in the same upsert call. The
/// xmax=0 discriminator in Issue #90's SQL rewrite must increment
/// n_entities_upserted_new on exactly the new row, not both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_counts_new_inserts_correctly_in_mixed_batch() {
    let Some((_cluster, pool)) = bring_up_pg("mixed").await else {
        return;
    };

    // Seed one entity.
    let seeded = ExtractResponse {
        entities: vec![Entity {
            text: "Alpha".into(),
            label: "concept".into(),
            start: 0,
            end: 5,
            score: 0.9,
        }],
        triples: vec![],
    };
    let out_seed = upsert_entities_and_relations(&pool, &seeded)
        .await
        .expect("seed upsert");
    let alpha_id = out_seed.entity_ids[0];

    // Now upsert a mixed batch: same Alpha + fresh Beta.
    let mixed = ExtractResponse {
        entities: vec![
            Entity {
                text: "Alpha".into(),
                label: "concept".into(),
                start: 0,
                end: 5,
                score: 0.9,
            },
            Entity {
                text: "Beta".into(),
                label: "concept".into(),
                start: 10,
                end: 14,
                score: 0.9,
            },
        ],
        triples: vec![],
    };
    let out_mixed = upsert_entities_and_relations(&pool, &mixed)
        .await
        .expect("mixed upsert");

    assert_eq!(out_mixed.entity_ids.len(), 2, "both ids returned");
    assert_eq!(
        out_mixed.entity_ids[0], alpha_id,
        "Alpha keeps its original id (resolved via conflict arm)"
    );
    assert_ne!(
        out_mixed.entity_ids[1], alpha_id,
        "Beta gets a distinct id"
    );
    assert_eq!(
        out_mixed.n_entities_upserted_new, 1,
        "exactly one new row created (Beta); Alpha was pre-existing"
    );

    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extractor_extract_writes_summary_audit_row() {
    let Some((_cluster, pool)) = bring_up_pg("audit").await else {
        return;
    };

    // Narrow audit-shape pin: don't spin up the real worker. Call
    // `build_extract_entities_payload` + `hhagent_db::audit::insert`
    // directly with the same 8-key shape `GlinerRelexExtractor::extract`
    // emits in production.
    let payload = hhagent_core::scheduler::audit::build_extract_entities_payload(
        234, 1, 5, 2, 5, 2, "multi-v1.0", 142,
    );
    hhagent_db::audit::insert(
        &pool,
        "extractor:gliner-relex",
        hhagent_core::scheduler::audit::ACTION_EXTRACT_ENTITIES,
        payload,
    )
    .await
    .expect("audit insert");

    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log \
         WHERE actor='extractor:gliner-relex' AND action='extract_entities'",
    )
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(n, 1);

    pool.close().await;
}

// ---------------------------------------------------------------------
// Real-model tier: skip-as-pass without venv + weights
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn extractor_extract_against_real_worker_returns_seeds() {
    let Some(entry) = build_real_model_entry() else {
        return;
    };
    let Some((_cluster, pool)) = bring_up_pg("real").await else {
        return;
    };

    let sandboxes = Arc::new(hhagent_sandbox::SandboxBackends::default_for_current_os());
    let lifecycle: Arc<dyn WorkerLifecycleManager> =
        Arc::new(CompositeLifecycle::new(sandboxes));

    let client = Client::new(lifecycle, pool.clone(), entry);
    let extractor = GlinerRelexExtractor::new(client, pool.clone());

    let seeds = extractor
        .extract("Dr Smith treats asthma in Mosman.")
        .await
        .expect("extract");

    assert!(!seeds.ids.is_empty(), "real model produces entity ids");
    assert_eq!(seeds.source, SeedSource::GlinerRelex);
    assert_eq!(seeds.model_version.as_deref(), Some("multi-v1.0"));

    // Summary audit row was written by the extractor.
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log \
         WHERE actor='extractor:gliner-relex' AND action='extract_entities'",
    )
    .fetch_one(&pool)
    .await
    .expect("count summary");
    assert_eq!(n, 1);

    // At least one dispatch row from tool_host (one chunk → one
    // `tool:gliner-relex / extract` row).
    let n_dispatch: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log \
         WHERE actor='tool:gliner-relex' AND action='extract'",
    )
    .fetch_one(&pool)
    .await
    .expect("count dispatch");
    assert!(
        n_dispatch >= 1,
        "expected at least one tool:gliner-relex/extract row, got {n_dispatch}"
    );

    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn extractor_chunking_path_against_real_worker() {
    let Some(entry) = build_real_model_entry() else {
        return;
    };
    let Some((_cluster, pool)) = bring_up_pg("chunk").await else {
        return;
    };

    let sandboxes = Arc::new(hhagent_sandbox::SandboxBackends::default_for_current_os());
    let lifecycle: Arc<dyn WorkerLifecycleManager> =
        Arc::new(CompositeLifecycle::new(sandboxes));

    let client = Client::new(lifecycle, pool.clone(), entry);
    let extractor = GlinerRelexExtractor::new(client, pool.clone());

    // Build > 8192-byte input: two halves with distinct entities each.
    // 34 bytes × 250 ≈ 8500 bytes each half → 17 KB total, forcing
    // multiple chunks (worker cap is 8192 bytes; chunk_text uses
    // 7500-byte chunks).
    let part_a = "Dr Smith treats asthma in Mosman. ".repeat(250);
    let part_b = "Dr Jones works at Sydney Hospital. ".repeat(250);
    let long = format!("{part_a}{part_b}");
    assert!(
        long.len() > 8192,
        "test input must exceed worker's 8KiB cap (got {})",
        long.len()
    );

    let seeds = extractor.extract(&long).await.expect("extract long");

    assert!(!seeds.ids.is_empty(), "chunked extraction produced ids");

    // Both halves contributed at least one entity. We dodge model-
    // version-specific assertions about exact label/text choices by
    // searching for the recognizable proper-noun tokens.
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM entities WHERE id = ANY($1::bigint[])",
    )
    .bind(&seeds.ids)
    .fetch_all(&pool)
    .await
    .expect("names");
    let combined = names.join(" ").to_lowercase();
    assert!(
        combined.contains("smith"),
        "first half's entity present in {names:?}"
    );
    assert!(
        combined.contains("jones") || combined.contains("sydney"),
        "second half's entity present in {names:?}"
    );

    // n_chunks in the most-recent summary audit row > 1.
    let payload: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM audit_log \
         WHERE actor='extractor:gliner-relex' AND action='extract_entities' \
         ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("payload");
    let n_chunks = payload["n_chunks"].as_i64().expect("n_chunks key");
    assert!(
        n_chunks > 1,
        "long input must produce > 1 chunk; got n_chunks={n_chunks}"
    );

    pool.close().await;
}

/// Layer B happy-path regression pin: a fresh batch of N=5 unique
/// entities through the batch path produces the same UpsertOutcome
/// shape as Layer A would have (entity_ids in order, n_new = 5,
/// n_relations_inserted = 0).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_batch_happy_path_returns_same_outcome_shape_as_layer_a() {
    let Some((_cluster, pool)) = bring_up_pg("batch-happy").await else {
        return;
    };

    // 5 unique entities, no triples — pure entity-batch exercise.
    let merged = ExtractResponse {
        entities: vec![
            Entity { text: "Alpha".into(),   label: "person".into(),       start: 0, end: 5,  score: 0.99 },
            Entity { text: "Beta".into(),    label: "organization".into(), start: 0, end: 4,  score: 0.99 },
            Entity { text: "Gamma".into(),   label: "person".into(),       start: 0, end: 5,  score: 0.99 },
            Entity { text: "Delta".into(),   label: "place".into(),        start: 0, end: 5,  score: 0.99 },
            Entity { text: "Epsilon".into(), label: "person".into(),       start: 0, end: 7,  score: 0.99 },
        ],
        triples: vec![],
    };
    let out = upsert_entities_and_relations(&pool, &merged)
        .await
        .expect("batch upsert should succeed on fresh batch");

    assert_eq!(out.entity_ids.len(), 5, "one id per input entity");
    assert_eq!(out.n_entities_upserted_new, 5, "every entity is new");
    assert_eq!(out.n_relations_inserted, 0, "no triples → no relations");

    // Verify each id round-trips to the expected (kind, name) pair via
    // a SELECT. This is the load-bearing regression pin for the
    // dispatcher: if try_batch_upsert returns ids in a different
    // order than the input, this assertion fails.
    for (idx, ent) in merged.entities.iter().enumerate() {
        let (kind, name): (String, String) =
            sqlx::query_as("SELECT kind, name FROM entities WHERE id = $1")
                .bind(out.entity_ids[idx])
                .fetch_one(&pool)
                .await
                .expect("SELECT round-trip");
        assert_eq!(&kind, &ent.label, "entity_ids[{idx}] kind mismatch");
        assert_eq!(&name, &ent.text, "entity_ids[{idx}] name mismatch");
    }

    pool.close().await;
}

/// Pins that entity_ids is returned in the original input order even
/// though the unnest batch's RETURNING clause may emit rows in arbitrary
/// order. Layer B's HashMap re-walk preserves order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_batch_preserves_entity_id_order_for_unique_inputs() {
    let Some((_cluster, pool)) = bring_up_pg("batch-order").await else {
        return;
    };

    let merged = ExtractResponse {
        entities: vec![
            Entity { text: "Alpha".into(), label: "person".into(), start: 0, end: 5, score: 0.99 },
            Entity { text: "Beta".into(),  label: "person".into(), start: 0, end: 4, score: 0.99 },
            Entity { text: "Gamma".into(), label: "person".into(), start: 0, end: 5, score: 0.99 },
        ],
        triples: vec![],
    };
    let out = upsert_entities_and_relations(&pool, &merged).await.unwrap();

    // Verify each id resolves to the expected name in input order.
    assert_eq!(out.entity_ids.len(), 3);
    for (idx, expected_name) in ["Alpha", "Beta", "Gamma"].iter().enumerate() {
        let name: String =
            sqlx::query_scalar("SELECT name FROM entities WHERE id = $1")
                .bind(out.entity_ids[idx])
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(&name, expected_name, "entity_ids[{idx}] wrong order");
    }

    pool.close().await;
}

/// Pins that input duplicates resolve to the same id and n_new counts
/// each unique (kind, name_norm) only once, even when the input has
/// duplicates. Matches Layer A's observable behaviour where each
/// per-row upsert of a duplicate hits ON CONFLICT and returns the
/// same id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_batch_dedup_input_returns_same_id_for_duplicates() {
    let Some((_cluster, pool)) = bring_up_pg("batch-dedup").await else {
        return;
    };

    // Input: [Alpha, alpha (same key — dups), Beta]
    let merged = ExtractResponse {
        entities: vec![
            Entity { text: "Alpha".into(), label: "person".into(), start: 0, end: 5, score: 0.99 },
            Entity { text: "alpha".into(), label: "person".into(), start: 0, end: 5, score: 0.99 },
            Entity { text: "Beta".into(),  label: "person".into(), start: 0, end: 4, score: 0.99 },
        ],
        triples: vec![],
    };
    let out = upsert_entities_and_relations(&pool, &merged).await.unwrap();

    assert_eq!(out.entity_ids.len(), 3, "entity_ids has one id per input position");
    assert_eq!(
        out.entity_ids[0], out.entity_ids[1],
        "duplicate inputs (Alpha and alpha) must resolve to the same id"
    );
    assert_ne!(
        out.entity_ids[0], out.entity_ids[2],
        "distinct inputs (Alpha and Beta) must resolve to different ids"
    );
    assert_eq!(
        out.n_entities_upserted_new, 2,
        "duplicate should NOT double-count — exactly 2 new (Alpha, Beta)"
    );
    assert_eq!(out.n_relations_inserted, 0);

    pool.close().await;
}

/// Layer B batch path must preserve operator-approved (quarantine=FALSE)
/// entities just like Layer A. This is the load-bearing invariant the
/// no-op `SET name_norm = entities.name_norm` clause guarantees: ON
/// CONFLICT must not touch the quarantine column. Pinned for N=3 with
/// the approved entity in the middle position (Layer A's existing pin
/// uses N=1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_batch_preserves_operator_unquarantine_decision() {
    let Some((_cluster, pool)) = bring_up_pg("batch-quar").await else {
        return;
    };

    // First pass: insert 3 entities. All land quarantined.
    let merged1 = ExtractResponse {
        entities: vec![
            Entity { text: "Alpha".into(),    label: "person".into(), start: 0, end: 5, score: 0.99 },
            Entity { text: "Dr Smith".into(), label: "person".into(), start: 0, end: 8, score: 0.99 },
            Entity { text: "Gamma".into(),    label: "person".into(), start: 0, end: 5, score: 0.99 },
        ],
        triples: vec![],
    };
    let out1 = upsert_entities_and_relations(&pool, &merged1).await.unwrap();
    assert_eq!(out1.entity_ids.len(), 3);
    let smith_id = out1.entity_ids[1];

    // Operator approves the middle entity via the quarantine-review CLI
    // (simulated as a direct UPDATE).
    sqlx::query("UPDATE entities SET quarantine = FALSE WHERE id = $1")
        .bind(smith_id)
        .execute(&pool)
        .await
        .expect("operator approve simulation");

    // Second pass: re-extract — all three hit ON CONFLICT through the
    // batch path.
    let out2 = upsert_entities_and_relations(&pool, &merged1).await.unwrap();
    assert_eq!(out2.entity_ids, out1.entity_ids, "same ids returned");
    assert_eq!(out2.n_entities_upserted_new, 0, "no new rows on rerun");

    // Load-bearing assertion: the batch path's ON CONFLICT DO UPDATE
    // SET name_norm = entities.name_norm must NOT have clobbered the
    // operator's approval.
    let quarantine_after: bool =
        sqlx::query_scalar("SELECT quarantine FROM entities WHERE id = $1")
            .bind(smith_id)
            .fetch_one(&pool)
            .await
            .expect("read back quarantine");
    assert!(
        !quarantine_after,
        "Layer B batch path must preserve operator unquarantine decision (quarantine=FALSE)"
    );

    // The sibling entities (Alpha, Gamma) should still be quarantined —
    // operator only approved Smith.
    for sibling_id in [out1.entity_ids[0], out1.entity_ids[2]] {
        let q: bool = sqlx::query_scalar("SELECT quarantine FROM entities WHERE id = $1")
            .bind(sibling_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(q, "sibling entity should remain quarantined (operator only approved Smith)");
    }

    pool.close().await;
}
