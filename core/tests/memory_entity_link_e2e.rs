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

use std::path::PathBuf;
use std::sync::Arc;

use hhagent_core::entity_extraction::{
    EntityExtractor, NoOpEntityExtractor, SeedSource, StaticEntityExtractor,
};
use hhagent_core::memory::entity_link::link_memory_entities;
use hhagent_core::worker_lifecycle::{CompositeLifecycle, WorkerLifecycleManager};
use hhagent_core::workers::gliner_relex::{gliner_relex_entry, Client, GlinerRelexEnv};
use hhagent_core::entity_extraction::gliner_relex::GlinerRelexExtractor;
use hhagent_db::audit::fetch_since;
use hhagent_db::memories::seed_meta_memory;
use hhagent_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor,
    skip_if_sandbox_unavailable, unique_suffix,
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
            &pool,
            &extractor,
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
            &pool,
            &extractor,
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
        let out1 = link_memory_entities(&pool, &extractor, memory_id, "L0", "bob took aspirin")
            .await
            .expect("first link");
        assert_eq!(out1.n_entities_linked, 2);

        // Second call: 0 new links (ON CONFLICT DO NOTHING).
        let out2 = link_memory_entities(&pool, &extractor, memory_id, "L0", "bob took aspirin")
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

/// Regression pin for the audit-row-on-DB-failure invariant. Forcing
/// `link_memory_to_entities` to fail with a FK violation (unknown
/// entity_id) must STILL produce a `memory_linker/entity_link` audit
/// row carrying the real seeds source + `n_seeds > 0` + the failure
/// flag `n_entities_linked = 0`. Without this row, observation-phase
/// SQL would be blind to the post-extract DB-link failure mode.
#[test]
fn link_db_failure_still_writes_audit_row_with_seed_info() {
    if skip_if_no_supervisor() {
        return;
    }

    rt().block_on(async {
        let Some((_cluster, pool)) = bring_up_pg("dberr").await else {
            return;
        };

        let memory_id = seed_meta_memory(
            &pool,
            "body whose entities live nowhere",
            &serde_json::json!({}),
            None,
        )
        .await
        .expect("seed_meta_memory");

        let rows_before = fetch_since(&pool, 0, FETCH_LIMIT)
            .await
            .expect("fetch_since before")
            .len();

        // A bogus entity id that cannot exist in the entities table —
        // forces the FK constraint in memory_entities to reject the
        // INSERT batch, surfacing as DbError::Query.
        let bogus_entity_id: i64 = i64::MAX;
        let extractor = StaticEntityExtractor::with_ids(vec![bogus_entity_id]);

        let err = link_memory_entities(
            &pool,
            &extractor,
            memory_id,
            "L0",
            "body whose entities live nowhere",
        )
        .await
        .expect_err("FK violation must surface as Err");
        match err {
            hhagent_core::memory::entity_link::LinkError::Db(_) => {}
            other => panic!("expected LinkError::Db, got {other:?}"),
        }

        // The audit row MUST still be present, distinguishable from the
        // extract-failure shape: n_seeds = 1 (extract succeeded) and
        // n_entities_linked = 0 (DB link failed).
        let rows_after = fetch_since(&pool, 0, FETCH_LIMIT)
            .await
            .expect("fetch_since after");
        assert_eq!(
            rows_after.len(),
            rows_before + 1,
            "audit row must be written even on DB-link failure",
        );
        let link_row = rows_after
            .iter()
            .find(|r| r.actor == "memory_linker" && r.action == "entity_link")
            .expect("memory_linker/entity_link audit row present");
        assert_eq!(link_row.payload["memory_id"], memory_id);
        assert_eq!(link_row.payload["layer"], "L0");
        assert_eq!(link_row.payload["n_entities_linked"], 0u64);
        assert_eq!(link_row.payload["n_seeds"], 1u64);
        assert_eq!(link_row.payload["seed_source"], "gliner_relex");

        // Observation-phase consumers distinguish "extract failed" from
        // "extract OK + DB link failed" by exactly the n_seeds > 0
        // disjunct. Pin both sides here so a future refactor that
        // accidentally collapses the two paths trips the assertion.
        let n_seeds = link_row.payload["n_seeds"].as_u64().expect("u64");
        let n_linked = link_row.payload["n_entities_linked"]
            .as_u64()
            .expect("u64");
        assert!(
            n_seeds > 0 && n_linked == 0,
            "DB-link-failure audit shape must be (n_seeds > 0, n_entities_linked = 0)",
        );

        pool.close().await;
    });
}

// --- Real-model tier (skip-as-pass without venv + weights) ---
//
// The three helpers below (`resolve_worker_script`, `resolve_weights_dir`,
// `build_real_extractor`) are duplicated from `entity_extraction_e2e.rs`.
// Marker: if a third caller appears, lift them into `hhagent-tests-common`.

/// Resolve the in-tree gliner-relex venv shim path. Returns `None` with
/// a `[SKIP]` print when the path doesn't exist.
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
/// `HHAGENT_GLINER_RELEX_WEIGHTS_DIR`, then `HHAGENT_DATA_DIR`, then
/// `$HOME/.local/share/hhagent`. Skip on missing.
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

/// Build a live `GlinerRelexExtractor` backed by the real gliner-relex worker.
/// Returns `None` (with a `[SKIP]` print) when any precondition is absent:
/// opt-in env-var, sandbox, supervisor, venv shim, or weights dir.
async fn build_real_extractor(pool: &sqlx::PgPool) -> Option<Arc<dyn EntityExtractor>> {
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
    let entry = gliner_relex_entry(&env);
    let sandboxes = Arc::new(hhagent_sandbox::SandboxBackends::default_for_current_os());
    let lifecycle: Arc<dyn WorkerLifecycleManager> =
        Arc::new(CompositeLifecycle::new(sandboxes));
    let client = Client::new(lifecycle, pool.clone(), entry);
    let extractor = GlinerRelexExtractor::new(client, pool.clone());
    Some(Arc::new(extractor))
}

#[test]
fn link_against_real_extractor_writes_real_entity_ids() {
    if skip_if_no_supervisor() {
        return;
    }

    rt().block_on(async {
        let Some((_cluster, pool)) = bring_up_pg("real").await else {
            return;
        };

        let Some(extractor) = build_real_extractor(&pool).await else {
            return; // [SKIP] line already printed by build_real_extractor
        };

        let body = "Dr Smith treats asthma in Mosman.";
        let memory_id = seed_meta_memory(
            &pool,
            body,
            &serde_json::json!({}),
            None,
        )
        .await
        .expect("seed");

        let outcome = link_memory_entities(&pool, &*extractor, memory_id, "L0", body)
            .await
            .expect("real-model link should succeed");

        assert!(
            outcome.n_entities_linked >= 2,
            "expected ≥2 entity links from the medical sentence, got {}",
            outcome.n_entities_linked
        );
        assert_eq!(outcome.seeds.source, SeedSource::GlinerRelex);
        assert_eq!(outcome.seeds.model_version.as_deref(), Some("multi-v1.0"));

        // Verify quarantine-by-default: every newly-extracted entity is
        // quarantined, so production graph_search (include_quarantined=false)
        // returns zero rows even though the link rows exist.
        let n_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM memory_entities me \
             JOIN entities e ON me.entity_id = e.id \
             WHERE me.memory_id = $1 AND e.quarantine = FALSE",
        )
        .bind(memory_id)
        .fetch_one(&pool)
        .await
        .expect("count unquarantined links");
        assert_eq!(n_rows, 0, "every newly-extracted entity is quarantined by default");

        // Audit row carries model version + the gliner_relex source.
        let rows = fetch_since(&pool, 0, FETCH_LIMIT)
            .await
            .expect("fetch_since");
        let link_row = rows
            .iter()
            .find(|r| r.actor == "memory_linker" && r.action == "entity_link")
            .expect("memory_linker/entity_link audit row present");
        assert_eq!(link_row.payload["model_version"], "multi-v1.0");
        assert_eq!(link_row.payload["seed_source"], "gliner_relex");

        pool.close().await;
    });
}

#[test]
fn link_extends_to_l0_seed_path_end_to_end() {
    if skip_if_no_supervisor() {
        return;
    }

    rt().block_on(async {
        let Some((_cluster, pool)) = bring_up_pg("e2e").await else {
            return;
        };

        let Some(extractor) = build_real_extractor(&pool).await else {
            return; // [SKIP] line already printed by build_real_extractor
        };

        // Two rules, each containing distinct entities.
        let rule1 = "Dr Smith treats asthma in Mosman.";
        let rule2 = "Nurse Jones manages diabetes at Royal North Shore.";

        let mem1 = seed_meta_memory(
            &pool,
            rule1,
            &serde_json::json!({"l0_rule_id": "r1"}),
            None,
        )
        .await
        .expect("seed1");
        let mem2 = seed_meta_memory(
            &pool,
            rule2,
            &serde_json::json!({"l0_rule_id": "r2"}),
            None,
        )
        .await
        .expect("seed2");

        let o1 = link_memory_entities(&pool, &*extractor, mem1, "L0", rule1)
            .await
            .expect("link1");
        let o2 = link_memory_entities(&pool, &*extractor, mem2, "L0", rule2)
            .await
            .expect("link2");

        assert!(o1.n_entities_linked > 0, "first rule produced entity links");
        assert!(o2.n_entities_linked > 0, "second rule produced entity links");

        // Each memory got its own distinct link set.
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM memory_entities WHERE memory_id IN ($1, $2)",
        )
        .bind(mem1)
        .bind(mem2)
        .fetch_one(&pool)
        .await
        .expect("count");
        assert!(
            total >= 4,
            "expected ≥4 total link rows across both memories, got {total}"
        );

        pool.close().await;
    });
}
