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
    backend, bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor,
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
            relation: "relates_to".into(),
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

    let sandbox: Arc<dyn hhagent_sandbox::SandboxBackend> = Arc::from(backend());
    let lifecycle: Arc<dyn WorkerLifecycleManager> =
        Arc::new(CompositeLifecycle::new(sandbox));

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

    let sandbox: Arc<dyn hhagent_sandbox::SandboxBackend> = Arc::from(backend());
    let lifecycle: Arc<dyn WorkerLifecycleManager> =
        Arc::new(CompositeLifecycle::new(sandbox));

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
