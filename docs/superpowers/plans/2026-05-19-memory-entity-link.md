# Memory-write-time entity auto-linker — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** every memory write (L0 seed, L1 promote, future writers) calls `EntityExtractor::extract` on the body and inserts `memory_entities` rows for the resulting entity ids — populating the table that the graph recall lane reads.

**Architecture:** new free function `core::memory::entity_link::link_memory_entities` composes the v2 extractor's `extract` call with the existing `link_memory_to_entities` DB helper, and emits one `memory_linker/entity_link` audit row per attempt. The same `Arc<dyn EntityExtractor>` already constructed in `main.rs` for query-time extraction (PR #91) gets threaded into the L0 seeder, the L1 promoter, and the scheduler runner so write-time and recall-time share the warm GLiNER worker. Failures are degrade-and-warn: memory survives unlinked on extract or link error; quarantine-by-default means linked entities remain invisible to production `graph_search` until the (separate) operator quarantine-review CLI ships.

**Tech Stack:** Rust 2021, tokio, sqlx + PostgreSQL, async_trait, serde_json, thiserror, tracing.

**Spec:** [`docs/superpowers/specs/2026-05-19-memory-entity-link-design.md`](../specs/2026-05-19-memory-entity-link-design.md) (committed `2d8cc2c`).

---

## File map

**Create:**
- `core/src/memory/entity_link.rs` — the new module (~150-200 LOC incl. tests)
- `core/tests/memory_entity_link_e2e.rs` — integration tests (~250-350 LOC)

**Modify:**
- `core/src/memory/mod.rs` — add `pub mod entity_link;`
- `core/src/memory/l0_seed.rs` — widen `seed_l0_from_rules` + `seed_l0_from_file` signatures; widen `L0SeedReport`
- `core/src/memory/l1_promote.rs` — widen `promote_l1` signature; widen `L1WriteOutcome::Inserted` variant
- `core/src/scheduler/runner.rs` — widen `write_l1_promoted_row`, `drain_lane`, `lane_loop`, `spawn_scheduler`
- `core/src/cli_audit.rs` — widen `l1_add_and_audit` (takes extractor; CLI passes NoOp)
- `core/src/bin/hhagent-cli.rs` — construct NoOp + pass to `l1_add_and_audit`
- `core/src/main.rs` — move entity_extractor construction BEFORE L0 seed; pass to `seed_l0_from_file` and `spawn_scheduler`
- `core/tests/memory_l0_seed_e2e.rs` — pass NoOp/Static to writers; +1 new assertion
- `core/tests/memory_l1_promote_e2e.rs` — pass NoOp/Static to writers; +1 new assertion
- `core/tests/scheduler_lanes_e2e.rs` — pass extractor to spawn_scheduler

**Test budget:** +13 tests (workspace 834 → 847). 6 unit + 3 mock-tier e2e + 2 real-model e2e + 2 caller-side e2e extensions.

---

## Conventions

- Every task ends with `cargo test --workspace` green before commit.
- Commit messages follow the in-tree style: `<scope>(<area>): <imperative>` (e.g. `feat(core/memory): entity_link scaffold + payload builder`).
- Source the env first: `source "$HOME/.cargo/env"`.

---

## Task 1: Scaffold `core::memory::entity_link` (types + payload builder + unit tests)

**Files:**
- Create: `core/src/memory/entity_link.rs`
- Modify: `core/src/memory/mod.rs`

This task lands the new module with types, the pure `build_entity_link_payload` helper, the `link_memory_entities` function signature with a stubbed body that returns an obvious-failure error, and 6 unit tests pinning the payload shape + error propagation. The `link_memory_entities` body is filled in by Task 2.

- [ ] **Step 1: Write the failing tests**

Create `core/src/memory/entity_link.rs` with module scaffolding and 6 tests against helpers that don't exist yet:

```rust
//! Memory-write-time entity auto-linker.
//!
//! Compose-op: extract entities from the body of a freshly-written
//! memory, insert `(memory_id, entity_id)` rows into `memory_entities`,
//! and emit a 6-key `memory_linker/entity_link` audit row. The memory
//! row must already be committed when this is called (failure here does
//! NOT roll back the memory write — the caller's posture is
//! degrade-and-warn).
//!
//! ## Why this is a free function, not a trait method
//!
//! See `docs/superpowers/specs/2026-05-19-memory-entity-link-design.md`
//! §2: keeping the `EntityExtractor` trait DB-agnostic and `PgPool`-free
//! is load-bearing for unit tests and future non-Postgres backends.

use std::collections::BTreeMap;

use serde_json::Value;
use sqlx::PgPool;

use crate::entity_extraction::{
    EntityExtractionError, EntityExtractor, EntitySeeds, SeedSource,
};
use hhagent_db::{audit, memories::link_memory_to_entities, DbError};

/// What the auto-linker did, for caller telemetry. Returned on success
/// only; on failure the caller receives [`LinkError`] and decides
/// whether to count it as a degrade.
#[derive(Clone, Debug)]
pub struct LinkOutcome {
    /// Post-`ON CONFLICT DO NOTHING` row count from
    /// [`hhagent_db::memories::link_memory_to_entities`]. May be smaller
    /// than `seeds.ids.len()` when some entities were already linked to
    /// this memory (re-run idempotency path).
    pub n_entities_linked: u64,
    /// Forwarded for caller-side telemetry. The audit row uses
    /// `seeds.ids.len()` as the separate `n_seeds` payload key so
    /// observation-phase SQL sees both bucket counts.
    pub seeds: EntitySeeds,
}

/// Error kinds for the auto-linker.
#[derive(thiserror::Error, Debug)]
pub enum LinkError {
    #[error("entity extraction failed: {0}")]
    Extract(#[from] EntityExtractionError),
    #[error("db error: {0}")]
    Db(#[from] DbError),
}

/// Extract entities from `body` and link them to `memory_id`.
///
/// **Posture: caller-handles-failure.** A `LinkError::Extract` or
/// `LinkError::Db` MUST NOT be treated as a memory-write failure
/// by the caller — the memory row is already committed. Production
/// callers log the error at WARN, increment a degrade counter, and
/// continue. The audit row is written EVEN on failure (with
/// `n_entities_linked = 0` and `seed_source = "none"`) so the
/// observation phase sees every link attempt.
///
/// `layer_label` is a stringly-typed identifier of the calling layer
/// (`"L0"`, `"L1"`, future `"L2"`/`"L3"`/`"L4"`). It goes straight into
/// the audit payload's `layer` key. Stringly avoids a circular dep on
/// `hhagent_db::memories::MemoryLayer` from this module.
///
/// The function calls `extract` unconditionally; the NoOp-extractor
/// case is a path optimisation (empty `seeds.ids` short-circuits at the
/// fast-path in `link_memory_to_entities`) rather than a branch.
pub async fn link_memory_entities(
    extractor: &dyn EntityExtractor,
    pool: &PgPool,
    memory_id: i64,
    layer_label: &'static str,
    body: &str,
) -> Result<LinkOutcome, LinkError> {
    // Task 1 scaffold: stubbed body returns a deliberate panic-equivalent.
    // Task 2 fills in the real implementation. The scaffold exists so
    // Task 1's unit tests (which test `build_entity_link_payload` only)
    // compile against the rest of the module.
    let _ = (extractor, pool, memory_id, layer_label, body);
    unimplemented!("link_memory_entities body lands in Task 2")
}

/// Pure builder: 6 keys, BTreeMap-ordered (matches the convention from
/// `scheduler::audit::build_*_payload`). Unit-tested directly so a
/// future accidental extra/missing key trips the regression pin.
pub(crate) fn build_entity_link_payload(
    memory_id: i64,
    layer_label: &str,
    n_entities_linked: u64,
    n_seeds: u64,
    seed_source: SeedSource,
    model_version: Option<&str>,
) -> Value {
    let mut map: BTreeMap<String, Value> = BTreeMap::new();
    map.insert("memory_id".to_string(), Value::from(memory_id));
    map.insert("layer".to_string(), Value::from(layer_label.to_string()));
    map.insert(
        "n_entities_linked".to_string(),
        Value::from(n_entities_linked),
    );
    map.insert("n_seeds".to_string(), Value::from(n_seeds));
    map.insert(
        "seed_source".to_string(),
        serde_json::to_value(seed_source).expect("snake_case-serializable"),
    );
    map.insert(
        "model_version".to_string(),
        model_version.map(Value::from).unwrap_or(Value::Null),
    );
    Value::Object(map.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity_extraction::SeedSource;

    /// The audit-row payload has exactly 6 keys. Future additions must
    /// touch this test so observation-phase consumers can be informed.
    #[test]
    fn build_payload_keyset_is_exactly_six() {
        let payload =
            build_entity_link_payload(42, "L0", 3, 5, SeedSource::GlinerRelex, Some("multi-v1.0"));
        let obj = payload.as_object().expect("payload is an object");
        let keys: Vec<&String> = obj.keys().collect();
        assert_eq!(
            keys.len(),
            6,
            "expected exactly 6 keys, got {keys:?}",
        );
        // Spelled-out keyset so a renamed key is loud.
        for expected in &[
            "layer",
            "memory_id",
            "model_version",
            "n_entities_linked",
            "n_seeds",
            "seed_source",
        ] {
            assert!(obj.contains_key(*expected), "missing {expected}");
        }
    }

    #[test]
    fn build_payload_with_model_version_carries_string_value() {
        let payload =
            build_entity_link_payload(1, "L1", 2, 2, SeedSource::GlinerRelex, Some("multi-v1.0"));
        assert_eq!(payload["model_version"], Value::from("multi-v1.0"));
        assert_eq!(payload["layer"], Value::from("L1"));
        assert_eq!(payload["memory_id"], Value::from(1));
        assert_eq!(payload["n_entities_linked"], Value::from(2u64));
        assert_eq!(payload["n_seeds"], Value::from(2u64));
    }

    #[test]
    fn build_payload_without_model_version_emits_json_null() {
        let payload = build_entity_link_payload(1, "L0", 0, 0, SeedSource::None, None);
        assert_eq!(payload["model_version"], Value::Null);
        assert_eq!(payload["seed_source"], Value::from("none"));
    }

    #[test]
    fn build_payload_serializes_seed_source_as_snake_case() {
        let gliner = build_entity_link_payload(1, "L0", 0, 0, SeedSource::GlinerRelex, None);
        assert_eq!(gliner["seed_source"], Value::from("gliner_relex"));
        let none = build_entity_link_payload(1, "L0", 0, 0, SeedSource::None, None);
        assert_eq!(none["seed_source"], Value::from("none"));
    }

    #[test]
    fn link_error_extract_variant_carries_source() {
        let underlying = EntityExtractionError::Client("scripted".into());
        let wrapped: LinkError = underlying.into();
        match wrapped {
            LinkError::Extract(e) => {
                // Format the underlying error to prove it round-trips.
                let s = format!("{e}");
                assert!(s.contains("scripted"), "got: {s}");
            }
            _ => panic!("expected LinkError::Extract"),
        }
    }

    #[test]
    fn link_error_db_variant_carries_source() {
        let underlying = DbError::Query("scripted db error".into());
        let wrapped: LinkError = underlying.into();
        match wrapped {
            LinkError::Db(e) => {
                let s = format!("{e}");
                assert!(s.contains("scripted db error"), "got: {s}");
            }
            _ => panic!("expected LinkError::Db"),
        }
    }
}
```

Then add the module to `core/src/memory/mod.rs`. Modify lines 46-50 (the `mod` declarations block):

```rust
mod embed;
pub mod entity_link;
pub mod l0_seed;
pub mod l1_promote;
pub mod layers;
mod recall;
```

- [ ] **Step 2: Run tests to verify they fail (or compile-error on missing module)**

Run:
```bash
source "$HOME/.cargo/env"
cargo test -p hhagent-core --lib memory::entity_link
```

Expected: 6 tests pass (the unit tests exercise pure helpers that ARE implemented; the only stubbed function is `link_memory_entities`, which has no unit-test coverage in this task). Compilation succeeds.

If the 6 tests don't pass, fix the module before continuing.

- [ ] **Step 3: Verify workspace still compiles and passes**

Run:
```bash
cargo test --workspace 2>&1 | tail -5
```

Expected: `test result: ok. 840 passed; 0 failed; 4 ignored; ...` (834 + 6 new = 840).

- [ ] **Step 4: Commit**

```bash
git add core/src/memory/entity_link.rs core/src/memory/mod.rs
git commit -m "$(cat <<'EOF'
feat(core/memory): entity_link scaffold + build_entity_link_payload

Lands the entity_link module surface needed by the upcoming auto-linker
slice: LinkOutcome + LinkError types, the pure 6-key payload builder,
and a stubbed link_memory_entities function (body lands in next commit).
6 unit tests pin the payload keyset and error #[from] propagation.

Spec: docs/superpowers/specs/2026-05-19-memory-entity-link-design.md
EOF
)"
```

---

## Task 2: `link_memory_entities` body + 3 mock-tier integration tests

**Files:**
- Modify: `core/src/memory/entity_link.rs` (fill in the function body, ~50 LOC)
- Create: `core/tests/memory_entity_link_e2e.rs`

This task implements the actual extract-then-link compose op AND lands 3 mock-tier integration tests using `StaticEntityExtractor` against a real per-test PG cluster. These tests are the contract pin: they describe exactly the behaviour callers can rely on.

- [ ] **Step 1: Write the failing integration tests first**

Create `core/tests/memory_entity_link_e2e.rs`:

```rust
//! Integration tests for the memory-write-time entity auto-linker.
//!
//! Two tiers:
//!   * Mock tier (this task) — real per-test PG cluster + StaticEntityExtractor.
//!     Pins the link-row insertion, the audit-row payload, and idempotency.
//!   * Real-model tier (Task 3) — live gliner-relex worker against the
//!     `multi-v1.0` weights. Gated on venv + weights presence (skip-as-pass).
//!
//! All tests use the shared `hhagent-tests-common` PG bring-up helper +
//! the standard skip-without-PG convention.

use std::sync::Arc;

use hhagent_core::entity_extraction::{
    EntitySeeds, NoOpEntityExtractor, SeedSource, StaticEntityExtractor,
};
use hhagent_core::memory::entity_link::{link_memory_entities, LinkError};
use hhagent_db::audit::fetch_since;
use hhagent_db::memories::{seed_meta_memory, MemoryLayer};
use hhagent_db::pool::connect_runtime_pool;

use hhagent_tests_common::{bring_up_pg_cluster, skip_if_no_pg_binaries};

/// Build a Tokio runtime for sync-style tests. Mirrors the convention
/// in `memory_recall_e2e.rs` and `entity_extraction_e2e.rs`.
fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}

/// Helper: insert a quarantined entity manually so tests can return its
/// id from a StaticEntityExtractor. Returns the entity id.
async fn upsert_test_entity(
    pool: &sqlx::PgPool,
    kind: &str,
    name: &str,
) -> i64 {
    use hhagent_db::graph::{Graph, PgGraph};
    let graph = PgGraph::new(pool.clone());
    let e = graph
        .upsert_entity(kind, name, /* embedding */ None)
        .await
        .expect("upsert_entity");
    e.id
}

#[test]
fn link_inserts_memory_entities_rows_and_writes_audit_row() {
    if skip_if_no_pg_binaries() {
        return;
    }
    rt().block_on(async {
        let cluster = bring_up_pg_cluster().await;
        let spec = cluster.spec.clone();
        let pool = connect_runtime_pool(&spec).await.expect("pool");

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
        let rows_before = fetch_since(&pool, 0).await.expect("fetch_since").len();

        let extractor = StaticEntityExtractor::with_ids(vec![e1, e2, e3]);
        let outcome = link_memory_entities(&extractor, &pool, memory_id, "L0",
            "alice took ibuprofen for her headache")
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
        let rows_after = fetch_since(&pool, 0).await.expect("fetch_since");
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
        assert_eq!(obj.len(), 6, "expected 6 keys, got {:?}", obj.keys().collect::<Vec<_>>());
        assert_eq!(payload["memory_id"], memory_id);
        assert_eq!(payload["layer"], "L0");
        assert_eq!(payload["n_entities_linked"], 3u64);
        assert_eq!(payload["n_seeds"], 3u64);
        assert_eq!(payload["seed_source"], "gliner_relex");
        assert_eq!(payload["model_version"], "test");
    });
}

#[test]
fn link_with_noop_extractor_writes_no_rows_but_writes_audit_row() {
    if skip_if_no_pg_binaries() {
        return;
    }
    rt().block_on(async {
        let cluster = bring_up_pg_cluster().await;
        let spec = cluster.spec.clone();
        let pool = connect_runtime_pool(&spec).await.expect("pool");

        let memory_id = seed_meta_memory(
            &pool,
            "the body that no extractor will inspect",
            &serde_json::json!({}),
            None,
        )
        .await
        .expect("seed_meta_memory");

        let rows_before = fetch_since(&pool, 0).await.expect("fetch_since").len();

        let extractor = NoOpEntityExtractor::new();
        let outcome = link_memory_entities(
            &extractor, &pool, memory_id, "L0",
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
        let rows_after = fetch_since(&pool, 0).await.expect("fetch_since");
        assert_eq!(rows_after.len(), rows_before + 1);
        let link_row = rows_after
            .iter()
            .find(|r| r.actor == "memory_linker" && r.action == "entity_link")
            .expect("memory_linker/entity_link row present");
        assert_eq!(link_row.payload["seed_source"], "none");
        assert_eq!(link_row.payload["n_entities_linked"], 0u64);
        assert_eq!(link_row.payload["model_version"], serde_json::Value::Null);
    });
}

#[test]
fn link_is_idempotent_on_rerun_with_same_seeds() {
    if skip_if_no_pg_binaries() {
        return;
    }
    rt().block_on(async {
        let cluster = bring_up_pg_cluster().await;
        let spec = cluster.spec.clone();
        let pool = connect_runtime_pool(&spec).await.expect("pool");

        let e1 = upsert_test_entity(&pool, "person", "bob").await;
        let e2 = upsert_test_entity(&pool, "drug", "aspirin").await;

        let memory_id = seed_meta_memory(
            &pool, "bob took aspirin",
            &serde_json::json!({}), None,
        )
        .await
        .expect("seed");

        let extractor = StaticEntityExtractor::with_ids(vec![e1, e2]);

        // First call: 2 fresh links.
        let out1 = link_memory_entities(&extractor, &pool, memory_id, "L0",
            "bob took aspirin").await.expect("first link");
        assert_eq!(out1.n_entities_linked, 2);

        // Second call: 0 new links (ON CONFLICT DO NOTHING).
        let out2 = link_memory_entities(&extractor, &pool, memory_id, "L0",
            "bob took aspirin").await.expect("second link");
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

        // Both audit rows were written.
        let rows = fetch_since(&pool, 0).await.expect("fetch_since");
        let link_rows: Vec<_> = rows.iter()
            .filter(|r| r.actor == "memory_linker" && r.action == "entity_link")
            .collect();
        assert_eq!(link_rows.len(), 2, "two audit rows even on idempotent rerun");
        // Second row records the 0-link outcome.
        assert_eq!(link_rows[1].payload["n_entities_linked"], 0u64);
        assert_eq!(link_rows[1].payload["n_seeds"], 2u64);
    });
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```bash
cargo test -p hhagent-core --test memory_entity_link_e2e 2>&1 | tail -10
```

Expected: 3 tests FAIL with `unimplemented!()` panic from the stubbed `link_memory_entities`.

- [ ] **Step 3: Fill in `link_memory_entities` body**

Replace the stub in `core/src/memory/entity_link.rs` (the `unimplemented!(...)` line and the `let _ = ...` line above it) with the real implementation:

```rust
pub async fn link_memory_entities(
    extractor: &dyn EntityExtractor,
    pool: &PgPool,
    memory_id: i64,
    layer_label: &'static str,
    body: &str,
) -> Result<LinkOutcome, LinkError> {
    let extract_result = extractor.extract(body).await;

    let (seeds, n_linked) = match extract_result {
        Ok(seeds) => {
            // ON CONFLICT DO NOTHING in link_memory_to_entities makes
            // this idempotent on re-runs; empty seeds short-circuit at
            // the existing fast-path so the NoOp extractor case is
            // essentially free (no SQL issued).
            let n = link_memory_to_entities(pool, memory_id, &seeds.ids).await?;
            (seeds, n)
        }
        Err(e) => {
            // Audit the failed attempt; the audit insert is best-effort
            // (its own error is logged but doesn't shadow the primary
            // extract error). We then propagate the extract error so
            // the caller's `Err` arm runs (warn-log + degrade-counter).
            let payload = build_entity_link_payload(
                memory_id,
                layer_label,
                /* n_entities_linked */ 0,
                /* n_seeds */ 0,
                SeedSource::None,
                None,
            );
            if let Err(audit_err) = audit::insert(
                pool, "memory_linker", "entity_link", payload,
            )
            .await
            {
                tracing::warn!(
                    error = %audit_err, memory_id,
                    "memory_linker degraded-path audit row failed"
                );
            }
            return Err(LinkError::from(e));
        }
    };

    // Success-path audit row.
    let payload = build_entity_link_payload(
        memory_id,
        layer_label,
        n_linked,
        seeds.ids.len() as u64,
        seeds.source,
        seeds.model_version.as_deref(),
    );
    // Best-effort: an audit-insert failure here doesn't roll back the
    // already-committed link rows. Log + continue.
    if let Err(e) = audit::insert(pool, "memory_linker", "entity_link", payload).await {
        tracing::warn!(error = %e, memory_id, "memory_linker audit row failed");
    }

    Ok(LinkOutcome {
        n_entities_linked: n_linked,
        seeds,
    })
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run:
```bash
cargo test -p hhagent-core --test memory_entity_link_e2e 2>&1 | tail -10
```

Expected: `test result: ok. 3 passed; 0 failed; 0 ignored; ...`

- [ ] **Step 5: Verify workspace still passes**

Run:
```bash
cargo test --workspace 2>&1 | tail -5
```

Expected: `test result: ok. 843 passed; 0 failed; 4 ignored; ...` (840 + 3 mock-tier = 843).

- [ ] **Step 6: Commit**

```bash
git add core/src/memory/entity_link.rs core/tests/memory_entity_link_e2e.rs
git commit -m "$(cat <<'EOF'
feat(core/memory/entity_link): link_memory_entities body + mock-tier e2e

Implements the extract → link_memory_to_entities → audit-row compose.
Three mock-tier integration tests using StaticEntityExtractor (real PG,
no GLiNER): happy path (3 entities, 1 audit row, 6-key payload),
NoOp-extractor case (0 rows but audit row still emitted with
seed_source="none"), idempotent re-run (second call returns
n_entities_linked=0, two audit rows total).

Spec: docs/superpowers/specs/2026-05-19-memory-entity-link-design.md
EOF
)"
```

---

## Task 3: Real-model integration tests

**Files:**
- Modify: `core/tests/memory_entity_link_e2e.rs` (+2 tests at the bottom)

Two integration tests against the live `gliner-relex-multi-v1.0` weights. Skip-as-pass without venv + weights, mirroring `core/tests/entity_extraction_e2e.rs`'s tier-2 pattern.

- [ ] **Step 1: Find the existing skip helper used by entity_extraction_e2e**

Read the skip helpers from `core/tests/entity_extraction_e2e.rs`:

```bash
grep -n "skip_if_no\|resolve_worker_script\|resolve_weights" core/tests/entity_extraction_e2e.rs | head -10
```

Expected: helpers like `skip_if_no_gliner_relex_setup()` or equivalent. Note the exact import path and the env-var conventions used (`HHAGENT_GLINER_RELEX_VENV_DIR`, `HHAGENT_GLINER_RELEX_WEIGHTS_DIR`).

- [ ] **Step 2: Add the real-model tests at the end of `memory_entity_link_e2e.rs`**

Append to `core/tests/memory_entity_link_e2e.rs` (verify imports + helper paths against the existing `entity_extraction_e2e.rs` patterns):

```rust
// --- Real-model tier (skip-as-pass without venv + weights) ---

use hhagent_core::entity_extraction::gliner_relex::GlinerRelexExtractor;
use hhagent_core::workers::gliner_relex::Client;
use std::sync::Arc as StdArc;

/// Same skip-helper convention as `core/tests/entity_extraction_e2e.rs` —
/// returns Some(extractor) if the venv + weights are staged, None +
/// stderr [SKIP] line otherwise.
async fn build_real_extractor(pool: &sqlx::PgPool)
    -> Option<StdArc<dyn hhagent_core::entity_extraction::EntityExtractor>>
{
    // Import + reuse the skip helper from entity_extraction_e2e.rs's
    // module — exact name to be verified in Step 1 above (likely
    // `crate::skip_if_no_gliner_relex_setup` or similar).
    //
    // Helper resolves HHAGENT_GLINER_RELEX_VENV_DIR + WEIGHTS_DIR,
    // returns None + prints `[SKIP] memory_entity_link_e2e: ...` when
    // either is missing. The body below assumes that helper exists at
    // the existing path in entity_extraction_e2e.rs — if it lives in a
    // shared module instead (e.g. `tests_common`), import from there.
    //
    // The helper must also build the lifecycle Arc + ToolEntry the
    // GlinerRelexExtractor needs. Copy the construction shape verbatim
    // from `entity_extraction_e2e.rs::build_real_extractor` (existing).
    //
    // (Implementation comment: this body must be filled in by reading
    // the exact helper signature from entity_extraction_e2e.rs and
    // either calling that helper directly via `mod common;` or
    // duplicating its body. The plan defers the exact code to the
    // engineer because the helper's shape isn't visible in this spec
    // document.)
    todo!("fill in by mirroring entity_extraction_e2e.rs::build_real_extractor in Step 1")
}

#[test]
fn link_against_real_extractor_writes_real_entity_ids() {
    if skip_if_no_pg_binaries() {
        return;
    }
    rt().block_on(async {
        let cluster = bring_up_pg_cluster().await;
        let spec = cluster.spec.clone();
        let pool = connect_runtime_pool(&spec).await.expect("pool");

        let Some(extractor) = build_real_extractor(&pool).await else {
            return; // [SKIP] line already printed
        };

        let body = "Dr Smith treats asthma in Mosman.";
        let memory_id = seed_meta_memory(
            &pool, body, &serde_json::json!({}), None,
        )
        .await
        .expect("seed");

        let outcome = link_memory_entities(&*extractor, &pool, memory_id, "L0", body)
            .await
            .expect("real-model link should succeed");

        assert!(outcome.n_entities_linked >= 2,
            "expected ≥2 entity links from the medical sentence, got {}",
            outcome.n_entities_linked);
        assert_eq!(outcome.seeds.source, SeedSource::GlinerRelex);
        assert_eq!(outcome.seeds.model_version.as_deref(), Some("multi-v1.0"));

        // Verify quarantine-by-default: every newly-extracted entity
        // is quarantined, so production graph_search (include_quarantined=false)
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
        let rows = fetch_since(&pool, 0).await.expect("fetch_since");
        let link_row = rows.iter()
            .find(|r| r.actor == "memory_linker" && r.action == "entity_link")
            .expect("memory_linker row");
        assert_eq!(link_row.payload["model_version"], "multi-v1.0");
        assert_eq!(link_row.payload["seed_source"], "gliner_relex");
    });
}

#[test]
fn link_extends_to_l0_seed_path_end_to_end() {
    if skip_if_no_pg_binaries() {
        return;
    }
    rt().block_on(async {
        let cluster = bring_up_pg_cluster().await;
        let spec = cluster.spec.clone();
        let pool = connect_runtime_pool(&spec).await.expect("pool");

        let Some(extractor) = build_real_extractor(&pool).await else {
            return;
        };

        // Two rules, each containing distinct entities.
        let rule1 = "Dr Smith treats asthma in Mosman.";
        let rule2 = "Nurse Jones manages diabetes at Royal North Shore.";

        let mem1 = seed_meta_memory(&pool, rule1, &serde_json::json!({"l0_rule_id":"r1"}), None)
            .await.expect("seed1");
        let mem2 = seed_meta_memory(&pool, rule2, &serde_json::json!({"l0_rule_id":"r2"}), None)
            .await.expect("seed2");

        let o1 = link_memory_entities(&*extractor, &pool, mem1, "L0", rule1)
            .await.expect("link1");
        let o2 = link_memory_entities(&*extractor, &pool, mem2, "L0", rule2)
            .await.expect("link2");

        assert!(o1.n_entities_linked > 0);
        assert!(o2.n_entities_linked > 0);

        // Each memory got its own distinct link set.
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM memory_entities WHERE memory_id IN ($1, $2)",
        )
        .bind(mem1).bind(mem2)
        .fetch_one(&pool).await.expect("count");
        assert!(total >= 4, "expected ≥4 total link rows across both memories, got {total}");
    });
}
```

**Note on the `build_real_extractor` helper:** if `core/tests/entity_extraction_e2e.rs` already contains a usable real-extractor builder, refactor it into a shared `mod common;` or `hhagent-tests-common` helper so this test file can reuse it cleanly. If duplication is the simpler path, duplicate (mark with `// duplicated from entity_extraction_e2e.rs; consider lifting if a third caller appears`).

- [ ] **Step 3: Run the tests to verify they pass (or [SKIP])**

Run:
```bash
cargo test -p hhagent-core --test memory_entity_link_e2e -- --nocapture 2>&1 | tail -15
```

Expected on hosts with venv + weights: 5 passed (3 mock + 2 real-model).
Expected on hosts without: 3 passed + 2 [SKIP] lines on stderr.

- [ ] **Step 4: Verify workspace still passes**

Run:
```bash
cargo test --workspace 2>&1 | tail -5
```

Expected: `test result: ok. 845 passed; 0 failed; 4 ignored; ...` (843 + 2).

- [ ] **Step 5: Commit**

```bash
git add core/tests/memory_entity_link_e2e.rs
git commit -m "$(cat <<'EOF'
test(core/memory_entity_link_e2e): real-model tier (gliner-relex weights)

Two real-model integration tests gated on venv + weights presence:
* link_against_real_extractor_writes_real_entity_ids — medical sentence
  yields ≥2 entity links; quarantine-by-default verified; audit row
  carries model_version=multi-v1.0.
* link_extends_to_l0_seed_path_end_to_end — two distinct L0 rules,
  each with distinct entities, both linked.

Skip-as-pass without venv + weights (mirrors entity_extraction_e2e.rs).

Spec: docs/superpowers/specs/2026-05-19-memory-entity-link-design.md
EOF
)"
```

---

## Task 4: L0 writer widening + main.rs L0 wiring

**Files:**
- Modify: `core/src/memory/l0_seed.rs` (widen `seed_l0_from_rules`, `seed_l0_from_file`; widen `L0SeedReport`)
- Modify: `core/src/main.rs` (move entity_extractor construction before L0 seed; pass to `seed_l0_from_file`)
- Modify: `core/tests/memory_l0_seed_e2e.rs` (update all `seed_l0_from_rules`/`seed_l0_from_file` callers — pass `NoOpEntityExtractor`)

Wider signatures + report fields. All test call-sites get the simplest valid extractor (`NoOpEntityExtractor::new()`); Task 6 adds the one new test that exercises a `StaticEntityExtractor`.

- [ ] **Step 1: Widen `L0SeedReport`**

In `core/src/memory/l0_seed.rs` around line 140 (where the struct is defined), add two purely-additive fields:

```rust
#[derive(Clone, Debug, Default)]
pub struct L0SeedReport {
    /// Number of rules parsed from the source file.
    pub rules_loaded: usize,
    /// Rules whose `(l0_rule_id, body_sha256)` was not yet in
    /// `memories` and were inserted by this run.
    pub new_rows_written: usize,
    /// Rules whose `(l0_rule_id, body_sha256)` already existed; the
    /// loader skipped the insert.
    pub unchanged_skipped: usize,
    /// Path the rules came from (for the audit row + diagnostics).
    pub source_path: PathBuf,
    /// SHA-256 hex of the source file's full byte content (read by
    /// `seed_l0_from_file`). Carries over from the file-level read so
    /// `seed_l0_from_rules` callers can record it for the audit row.
    pub source_sha256: String,
    /// Cumulative `LinkOutcome::n_entities_linked` across all
    /// newly-written rows in this seed pass. Existing rows
    /// (sha256-idempotency skip) don't re-extract.
    pub entities_linked: u64,
    /// Count of newly-written rows where the auto-link step failed
    /// (extract or DB error). The memory body is safe; future relink
    /// tooling can fill in the gap.
    pub link_failures: u32,
}
```

(The exact set of existing fields may differ slightly — preserve them all verbatim and append `entities_linked` + `link_failures`.)

- [ ] **Step 2: Widen `seed_l0_from_rules` signature + body**

Modify the function signature around line 301:

```rust
pub async fn seed_l0_from_rules(
    pool: &PgPool,
    extractor: &dyn crate::entity_extraction::EntityExtractor,
    source_path: &Path,
    source_sha256: &str,
    rules: &[L0Rule],
) -> Result<L0SeedReport, L0Error> {
    let mut report = L0SeedReport {
        rules_loaded: rules.len(),
        new_rows_written: 0,
        unchanged_skipped: 0,
        source_path: source_path.to_path_buf(),
        source_sha256: source_sha256.to_string(),
        entities_linked: 0,
        link_failures: 0,
    };

    for rule in rules {
        let body_sha256 = compute_body_sha256(&rule.body);

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS ( \
                 SELECT 1 FROM memories \
                  WHERE layer = 0 \
                    AND metadata->>'l0_rule_id' = $1 \
                    AND metadata->>'body_sha256' = $2 \
              )",
        )
        .bind(&rule.id)
        .bind(&body_sha256)
        .fetch_one(pool)
        .await
        .map_err(|e| {
            L0Error::Db(DbError::Query(format!(
                "l0 idempotency check ({}): {e}",
                rule.id
            )))
        })?;

        if exists {
            report.unchanged_skipped += 1;
            continue;
        }

        let metadata = build_l0_metadata(&rule.id, &body_sha256, &rule.tags, source_path);
        let memory_id =
            hhagent_db::memories::seed_meta_memory(pool, &rule.body, &metadata, None).await?;
        report.new_rows_written += 1;

        // Auto-link entities. Degrade-and-warn posture — a failure here
        // leaves the memory unlinked but otherwise intact. Production
        // hosts will re-link via the future operator quarantine-review
        // CLI's "relink unlinked memories" subcommand.
        match crate::memory::entity_link::link_memory_entities(
            extractor, pool, memory_id, "L0", &rule.body,
        )
        .await
        {
            Ok(outcome) => {
                report.entities_linked += outcome.n_entities_linked;
            }
            Err(e) => {
                tracing::warn!(
                    error = %e, memory_id, l0_rule_id = %rule.id,
                    "L0 auto-linker degraded; memory survives unlinked"
                );
                report.link_failures += 1;
            }
        }
    }

    Ok(report)
}
```

- [ ] **Step 3: Widen `seed_l0_from_file` signature**

Modify the convenience wrapper around line 354:

```rust
pub async fn seed_l0_from_file(
    pool: &PgPool,
    extractor: &dyn crate::entity_extraction::EntityExtractor,
    path: &Path,
) -> Result<L0SeedReport, L0Error> {
    let content = tokio::fs::read_to_string(path).await.map_err(|e| L0Error::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let source_sha256 = compute_source_sha256(&content);
    let rules = parse_l0_rules(path, &content)?;
    seed_l0_from_rules(pool, extractor, path, &source_sha256, &rules).await
}
```

- [ ] **Step 4: Move `main.rs` entity_extractor construction BEFORE L0 seed + pass it in**

Open `core/src/main.rs`. The current order (lines 60-95 for L0 seed; lines 152-207 for entity_extractor construction) inserts L0 seed BEFORE the extractor exists. Reorder so the entity_extractor block runs first.

Replace lines 60-95 (the prompts + L0 block) by moving them DOWN — and the entity_extractor block (lines ~120-207) UP. Specifically:

Find the block starting:
```rust
    // Load every prompts/*.md, hash, upsert into agent_prompts.
    let prompts_dir = std::env::var("HHAGENT_PROMPTS_DIR")
```

and ending with the L0 seed block ending at:
```rust
    } else {
        info!(path = ?l0_path, "no L0 rules file found, skipping seed");
    }
```

Move this entire block to AFTER the `entity_extractor` construction block (which currently sits between `tool_registry` and `formulator`).

Then modify the L0 seed call line:

```rust
// before
let report = hhagent_core::memory::l0_seed::seed_l0_from_file(&pool, &l0_path)

// after
let report = hhagent_core::memory::l0_seed::seed_l0_from_file(
    &pool, &*entity_extractor, &l0_path,
)
```

**Verify the new order:** (a) `bring_up_database`, (b) pool + audit mirror, (c) crash sweep, (d) router config + Arc, (e) review pipeline, (f) sandbox + lifecycle, (g) `gliner_relex_entry`, (h) tool registry, (i) `entity_extractor`, (j) **prompts load**, (k) **L0 seed (passes `&*entity_extractor`)**, (l) formulator, (m) dispatcher, (n) `spawn_scheduler`.

The reordering does NOT affect any other line because nothing between the original prompts/L0 block and the original entity_extractor block depended on prompts or L0.

- [ ] **Step 5: Update `core/tests/memory_l0_seed_e2e.rs` — all call sites pass `&NoOpEntityExtractor::new()`**

Open `core/tests/memory_l0_seed_e2e.rs`. There are ~10 call sites for `seed_l0_from_rules` and ~2 for `seed_l0_from_file`. Each one needs the extractor argument inserted before `seed_path()`.

At the top of the file, add the import:
```rust
use hhagent_core::entity_extraction::NoOpEntityExtractor;
```

Then for every `seed_l0_from_rules(&pool, seed_path(), ...)`:
```rust
// before
seed_l0_from_rules(&pool, seed_path(), "src-sha-1", &rules)

// after
seed_l0_from_rules(&pool, &NoOpEntityExtractor::new(), seed_path(), "src-sha-1", &rules)
```

And for every `seed_l0_from_file(&pool, &path)`:
```rust
// before
seed_l0_from_file(&pool, &path)

// after
seed_l0_from_file(&pool, &NoOpEntityExtractor::new(), &path)
```

To find every call site:
```bash
grep -n "seed_l0_from_rules\|seed_l0_from_file" core/tests/memory_l0_seed_e2e.rs
```

Edit each line accordingly.

- [ ] **Step 6: Build to surface any remaining call sites**

Run:
```bash
cargo build --workspace --tests 2>&1 | grep -E "error|^warning" | head -30
```

Expected: zero errors. If any remain, they're additional call sites missed in Step 5 — fix them. The signature change is mechanical; expected fix-shape is "insert `&NoOpEntityExtractor::new()` as the second argument".

- [ ] **Step 7: Run workspace tests**

```bash
cargo test --workspace 2>&1 | tail -5
```

Expected: `test result: ok. 845 passed; 0 failed; 4 ignored; ...` (unchanged from Task 3 — no new tests yet; just signature widening that test fixtures already pass).

- [ ] **Step 8: Commit**

```bash
git add core/src/memory/l0_seed.rs core/src/main.rs core/tests/memory_l0_seed_e2e.rs
git commit -m "$(cat <<'EOF'
feat(core/memory/l0_seed): wire entity auto-linker through L0 writers

seed_l0_from_rules + seed_l0_from_file gain `&dyn EntityExtractor`
parameter (immediately after `&pool`). L0SeedReport gains
`entities_linked: u64` + `link_failures: u32` for the auto-link
outcome. Failure posture is degrade-and-warn: extract or link error
logs WARN + counts in report.link_failures + continues (memory body
already committed).

Daemon main.rs is reordered so entity_extractor construction happens
BEFORE the L0 seed block; the same Arc<dyn EntityExtractor> is now
shared between query-time (RouterAgent) and write-time (L0/L1 writers).

Spec: docs/superpowers/specs/2026-05-19-memory-entity-link-design.md
EOF
)"
```

---

## Task 5: L1 writer widening + cascade through scheduler runner + CLI

**Files:**
- Modify: `core/src/memory/l1_promote.rs` (widen `promote_l1`; widen `L1WriteOutcome::Inserted`)
- Modify: `core/src/scheduler/runner.rs` (widen `write_l1_promoted_row`, `drain_lane`, `lane_loop`, `spawn_scheduler`)
- Modify: `core/src/cli_audit.rs` (widen `l1_add_and_audit`)
- Modify: `core/src/bin/hhagent-cli.rs` (construct + pass `NoOpEntityExtractor`)
- Modify: `core/src/main.rs` (pass `entity_extractor.clone()` to `spawn_scheduler`)
- Modify: `core/tests/memory_l1_promote_e2e.rs` (pass `NoOpEntityExtractor` to all `promote_l1` calls; match new `Inserted` shape)
- Modify: `core/tests/scheduler_lanes_e2e.rs` (pass `entity_extractor` to `spawn_scheduler`)

This is the largest cascade in the slice — `promote_l1`'s signature change ripples up through every caller. The TDD pattern here: widen one test to use the new `Inserted { memory_id, link_outcome }` pattern → compile fails → cascade the widening through every call site until it compiles → tests pass.

- [ ] **Step 1: Widen `L1WriteOutcome::Inserted` + `promote_l1` signature**

In `core/src/memory/l1_promote.rs`:

Line ~16 add import:
```rust
use crate::entity_extraction::EntityExtractor;
use crate::memory::entity_link::{link_memory_entities, LinkOutcome};
```

Line 65-74 widen the variant:
```rust
pub enum L1WriteOutcome {
    /// New L1 row inserted at the carried `memory_id`. `link_outcome`
    /// is `Some(_)` on auto-link success (including the NoOp case
    /// where seeds are empty) and `None` on auto-link error (the
    /// memory body is committed; the link step failed and a WARN was
    /// already logged). Operator + agent callers can both ignore the
    /// new field — the variant widening is purely additive at the
    /// match level via `..`.
    Inserted {
        memory_id: i64,
        link_outcome: Option<LinkOutcome>,
    },
    /// A row with the same `body_sha256` already exists at
    /// `layer = 1` (carrying the existing `memory_id`). No new row
    /// was written; no link attempt was made.
    SkippedDuplicate { memory_id: i64 },
}
```

Line ~76-83 update the `memory_id()` accessor:
```rust
impl L1WriteOutcome {
    pub fn memory_id(&self) -> i64 {
        match self {
            L1WriteOutcome::Inserted { memory_id, .. }
            | L1WriteOutcome::SkippedDuplicate { memory_id } => *memory_id,
        }
    }
}
```

Line 187-231 widen `promote_l1`:

```rust
pub async fn promote_l1(
    pool: &PgPool,
    extractor: &dyn EntityExtractor,
    body: &str,
    source: L1Source,
) -> Result<L1WriteOutcome, L1Error> {
    let trimmed = validate_l1_body(body)?;
    let body_sha256 = compute_body_sha256(trimmed);

    let existing: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM memories \
         WHERE layer = $1 AND metadata->>'body_sha256' = $2 \
         LIMIT 1",
    )
    .bind(MemoryLayer::Index.as_db())
    .bind(&body_sha256)
    .fetch_optional(pool)
    .await
    .map_err(|e| L1Error::Db(hhagent_db::DbError::Query(
        format!("promote_l1 EXISTS-check body_sha256={body_sha256}: {e}")
    )))?;

    if let Some(existing_id) = existing {
        return Ok(L1WriteOutcome::SkippedDuplicate { memory_id: existing_id });
    }

    let created_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("rfc3339 format");
    let metadata = build_l1_metadata(&source, &body_sha256, &created_at);

    let new_id = insert_memory_at_layer(
        pool, trimmed, &metadata, None, MemoryLayer::Index,
    )
    .await?;

    // Auto-link entities. Same degrade-and-warn posture as L0:
    // a failure here leaves the L1 row unlinked but otherwise intact.
    let link_outcome = match link_memory_entities(
        extractor, pool, new_id, "L1", trimmed,
    )
    .await
    {
        Ok(outcome) => Some(outcome),
        Err(e) => {
            tracing::warn!(
                error = %e, memory_id = new_id,
                "L1 auto-linker degraded; memory survives unlinked"
            );
            None
        }
    };

    Ok(L1WriteOutcome::Inserted { memory_id: new_id, link_outcome })
}
```

- [ ] **Step 2: Cascade — widen `cli_audit::l1_add_and_audit`**

In `core/src/cli_audit.rs` around line 383:

```rust
pub async fn l1_add_and_audit(
    pool: &PgPool,
    extractor: &dyn crate::entity_extraction::EntityExtractor,
    body: &str,
) -> Result<(crate::memory::l1_promote::L1WriteOutcome, i64), crate::memory::l1_promote::L1Error> {
    use crate::memory::l1_promote::{compute_body_sha256, promote_l1, validate_l1_body, L1Source};

    let trimmed = validate_l1_body(body)?.to_string();
    let source = L1Source::Operator;
    let outcome = promote_l1(pool, extractor, &trimmed, source.clone()).await?;
    let body_sha256 = compute_body_sha256(&trimmed);

    let payload = build_l1_write_payload(&outcome, &source, &body_sha256);
    let audit_id = match hhagent_db::audit::insert(
        pool, CLI_AUDIT_ACTOR, ACTION_L1_ADDED, payload,
    ).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "l1.added audit insert failed (best-effort)");
            0
        }
    };

    Ok((outcome, audit_id))
}
```

Update the compile-pin test at the bottom of `cli_audit.rs::tests` (around line 461-474):

```rust
#[test]
fn l1_add_and_audit_signature_compile_pin() {
    // Compile-only proof of the signature shape. Not run as a test,
    // but cargo build catches signature drift.
    fn _shape_check() {
        async fn _inner(pool: &PgPool, body: &str) {
            use crate::entity_extraction::NoOpEntityExtractor;
            let extractor = NoOpEntityExtractor::new();
            let _ = l1_add_and_audit(pool, &extractor, body).await;
        }
        let _: fn(&PgPool, &str) -> _ = |p, b| Box::pin(_inner(p, b));
    }
}
```

(Adjust to match the existing compile-pin pattern — the existing test may already use a `fn _shape_check` style.)

- [ ] **Step 3: Cascade — widen `runner::write_l1_promoted_row` and its parents**

In `core/src/scheduler/runner.rs`:

Around line 1-30, add the import:
```rust
use crate::entity_extraction::EntityExtractor;
```

Around line 50, widen `spawn_scheduler`:
```rust
pub fn spawn_scheduler(
    pool: PgPool,
    formulator: Arc<dyn PlanFormulator>,
    review: Arc<ChainReviewStage>,
    dispatcher: Arc<dyn StepDispatcher>,
    entity_extractor: Arc<dyn EntityExtractor>,
) -> SchedulerHandle {
    let (tx, rx) = watch::channel(false);

    let fast = tokio::spawn(lane_loop(
        pool.clone(), formulator.clone(), review.clone(), dispatcher.clone(),
        entity_extractor.clone(),
        Lane::Fast, DEFAULT_DEADLINE_FAST_S, DEFAULT_MAX_PLANS_FAST, rx.clone(),
    ));
    let long = tokio::spawn(lane_loop(
        pool, formulator, review, dispatcher,
        entity_extractor,
        Lane::Long, DEFAULT_DEADLINE_LONG_S, DEFAULT_MAX_PLANS_LONG, rx,
    ));

    SchedulerHandle { shutdown: tx, fast, long }
}
```

Around line 70, widen `lane_loop`:
```rust
async fn lane_loop(
    pool: PgPool,
    formulator: Arc<dyn PlanFormulator>,
    review: Arc<ChainReviewStage>,
    dispatcher: Arc<dyn StepDispatcher>,
    entity_extractor: Arc<dyn EntityExtractor>,
    lane: Lane,
    deadline_seconds: i64,
    max_plans: u32,
    mut shutdown: watch::Receiver<bool>,
) {
    // ... existing body unchanged except the two drain_lane calls below
```

Inside `lane_loop`, both `drain_lane(...)` calls need `entity_extractor.clone()` (or `&entity_extractor` if `drain_lane` takes a ref). Update the two call sites at lines ~103 and ~119:

```rust
    drain_lane(
        &pool, formulator.clone(), review.clone(), dispatcher.clone(),
        entity_extractor.clone(),
        lane, deadline_seconds, max_plans, &shutdown,
    ).await;
```

Around line 130, widen `drain_lane`:
```rust
async fn drain_lane(
    pool: &PgPool,
    formulator: Arc<dyn PlanFormulator>,
    review: Arc<ChainReviewStage>,
    dispatcher: Arc<dyn StepDispatcher>,
    entity_extractor: Arc<dyn EntityExtractor>,
    lane: Lane,
    deadline_seconds: i64,
    max_plans: u32,
    shutdown: &watch::Receiver<bool>,
) {
    // ... existing body unchanged except the write_l1_promoted_row call below
```

Inside `drain_lane`, find the `write_l1_promoted_row` call at line ~220:
```rust
        if let Some(insight) = result.terminal_l1_insight.as_deref() {
            write_l1_promoted_row(pool, &*entity_extractor, claimed.id, insight).await;
        }
```

Around line 288, widen `write_l1_promoted_row`:
```rust
async fn write_l1_promoted_row(
    pool: &PgPool,
    extractor: &dyn EntityExtractor,
    task_id: i64,
    insight: &str,
) {
    use crate::memory::l1_promote::{promote_l1, L1Error, L1Source};

    let source = L1Source::AgentRaised { task_id };
    let outcome = match promote_l1(pool, extractor, insight, source.clone()).await {
        Ok(o) => o,
        Err(L1Error::Validation(msg)) => {
            tracing::warn!(
                task_id, error = %msg,
                "agent-raised L1 promotion rejected on validation (skipping audit row)"
            );
            return;
        }
        Err(L1Error::Db(e)) => {
            tracing::warn!(
                task_id, error = %e,
                "agent-raised L1 promotion DB error (skipping audit row)"
            );
            return;
        }
    };

    // ... rest unchanged
```

Around line 593, update the existing signature compile-pin test:
```rust
#[test]
fn write_l1_promoted_row_signature_compile_pin() {
    fn _shape_check() {
        async fn _inner(pool: &PgPool, task_id: i64, insight: &str) {
            use crate::entity_extraction::NoOpEntityExtractor;
            let extractor = NoOpEntityExtractor::new();
            super::write_l1_promoted_row(pool, &extractor, task_id, insight).await
        }
        let _: fn(&PgPool, i64, &str) -> _ = |p, t, i| Box::pin(_inner(p, t, i));
    }
}
```

- [ ] **Step 4: Update `main.rs` to pass `entity_extractor.clone()` to `spawn_scheduler`**

In `core/src/main.rs` around line 232:

```rust
    let scheduler = hhagent_core::scheduler::spawn_scheduler(
        pool.clone(),
        formulator,
        review,
        dispatcher,
        entity_extractor.clone(),
    );
```

- [ ] **Step 5: Update CLI bin to construct + pass NoOp**

In `core/src/bin/hhagent-cli.rs` around line 988-1009:

```rust
async fn memory_l1_add(args: &[String]) -> ExitCode {
    use hhagent_core::cli_audit::l1_add_and_audit;
    use hhagent_core::entity_extraction::NoOpEntityExtractor;
    use hhagent_core::memory::l1_promote::L1WriteOutcome;
    use hhagent_db::pool::connect_runtime_pool;

    let body = match args {
        [b] => b,
        _ => {
            eprintln!("usage: hhagent-cli memory l1 add <body>");
            return ExitCode::from(2);
        }
    };

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    // CLI L1 add path uses NoOp extractor — operator-explicit additions
    // are not auto-linked. A future `hhagent-cli memory relink` subcommand
    // would do batch re-linking against the real extractor.
    let extractor = NoOpEntityExtractor::new();

    match l1_add_and_audit(&pool, &extractor, body).await {
        Ok((L1WriteOutcome::Inserted { memory_id, .. }, _)) => {
            println!("inserted id={memory_id}");
            ExitCode::from(0)
        }
        Ok((L1WriteOutcome::SkippedDuplicate { memory_id }, _)) => {
            println!("skipped_duplicate id={memory_id} (body_sha256 already at layer 1)");
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("memory l1 add: {e}");
            ExitCode::from(1)
        }
    }
}
```

Note the destructuring change: `L1WriteOutcome::Inserted { memory_id }` → `L1WriteOutcome::Inserted { memory_id, .. }` (CLI ignores the link outcome; the println is unchanged).

- [ ] **Step 6: Update `memory_l1_promote_e2e.rs`**

In `core/tests/memory_l1_promote_e2e.rs`:

At the top, add the import:
```rust
use hhagent_core::entity_extraction::NoOpEntityExtractor;
```

For every `promote_l1(...)` call, insert the extractor argument:

```rust
// before
let outcome = promote_l1(&pool, body, source.clone()).await.expect("promote_l1");

// after
let outcome = promote_l1(&pool, &NoOpEntityExtractor::new(), body, source.clone())
    .await.expect("promote_l1");
```

For every `match outcome { L1WriteOutcome::Inserted { memory_id } => ... }`, change the destructuring:

```rust
// before
L1WriteOutcome::Inserted { memory_id } => ...

// after
L1WriteOutcome::Inserted { memory_id, .. } => ...
```

To find all sites:
```bash
grep -n "promote_l1\|L1WriteOutcome::Inserted" core/tests/memory_l1_promote_e2e.rs
```

Also check `l1_add_and_audit` callers — same destructuring change applies.

- [ ] **Step 7: Update `scheduler_lanes_e2e.rs` to pass extractor**

In `core/tests/scheduler_lanes_e2e.rs` around line 455:

```rust
    use hhagent_core::entity_extraction::NoOpEntityExtractor;
    // ...

    let extractor: std::sync::Arc<dyn hhagent_core::entity_extraction::EntityExtractor> =
        std::sync::Arc::new(NoOpEntityExtractor::new());

    let scheduler = spawn_scheduler(
        pool.clone(),
        formulator,
        review,
        dispatcher,
        extractor,
    );
```

- [ ] **Step 8: Build to surface remaining call sites**

Run:
```bash
cargo build --workspace --tests 2>&1 | grep -E "error|^warning" | head -40
```

Expected: zero errors. Any remaining errors are usually missed call sites — the compiler tells you the file:line; insert `&NoOpEntityExtractor::new()` or update the `L1WriteOutcome::Inserted` destructuring to `{ memory_id, .. }`.

- [ ] **Step 9: Run workspace tests**

```bash
cargo test --workspace 2>&1 | tail -5
```

Expected: `test result: ok. 845 passed; 0 failed; 4 ignored; ...` (unchanged — no new tests yet; signature widening only).

- [ ] **Step 10: Commit**

```bash
git add core/src/memory/l1_promote.rs core/src/scheduler/runner.rs \
        core/src/cli_audit.rs core/src/bin/hhagent-cli.rs core/src/main.rs \
        core/tests/memory_l1_promote_e2e.rs core/tests/scheduler_lanes_e2e.rs
git commit -m "$(cat <<'EOF'
feat(core/memory/l1_promote,scheduler/runner): wire auto-linker through L1 writers

Cascade-widens promote_l1 + cli_audit::l1_add_and_audit +
runner::write_l1_promoted_row + drain_lane + lane_loop +
spawn_scheduler all in lockstep so the same Arc<dyn EntityExtractor>
threads from main.rs through the lane runner down to the auto-link
call. L1WriteOutcome::Inserted widens additively with
link_outcome: Option<LinkOutcome>; callers that don't care match { .. }.

CLI memory l1 add path passes NoOpEntityExtractor: operator-explicit
L1 additions are not auto-linked (a future hhagent-cli memory relink
subcommand would handle batch re-linking).

Spec: docs/superpowers/specs/2026-05-19-memory-entity-link-design.md
EOF
)"
```

---

## Task 6: Caller-side e2e extensions (the 2 new assertion tests)

**Files:**
- Modify: `core/tests/memory_l0_seed_e2e.rs` (+1 test)
- Modify: `core/tests/memory_l1_promote_e2e.rs` (+1 test)

Add one new test per caller that uses `StaticEntityExtractor` to pin the auto-link behaviour end-to-end from the writer's surface.

- [ ] **Step 1: Add L0 caller assertion test**

Append to `core/tests/memory_l0_seed_e2e.rs` (after the last existing test):

```rust
#[test]
fn seed_l0_auto_links_entities_via_extractor() {
    if skip_if_no_pg_binaries() {
        return;
    }
    use hhagent_core::entity_extraction::StaticEntityExtractor;
    use hhagent_db::graph::{Graph, PgGraph};

    rt().block_on(async {
        let cluster = bring_up_pg_cluster().await;
        let spec = cluster.spec.clone();
        let pool = connect_runtime_pool(&spec).await.expect("pool");

        // Pre-create three entities; the Static extractor will return their ids.
        let graph = PgGraph::new(pool.clone());
        let e1 = graph.upsert_entity("person", "alice", None).await.expect("e1").id;
        let e2 = graph.upsert_entity("drug", "metformin", None).await.expect("e2").id;
        let e3 = graph.upsert_entity("disease", "diabetes", None).await.expect("e3").id;

        let extractor = StaticEntityExtractor::with_ids(vec![e1, e2, e3]);
        let rules = vec![make_rule("r1", "alice takes metformin for diabetes")];

        let report = seed_l0_from_rules(&pool, &extractor, seed_path(), "sha-link", &rules)
            .await.expect("seed");

        assert_eq!(report.new_rows_written, 1);
        assert_eq!(report.entities_linked, 3,
            "expected 3 entity links via auto-linker");
        assert_eq!(report.link_failures, 0);

        // memory_entities row count for the newly-inserted memory.
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM memory_entities WHERE memory_id IN \
             (SELECT id FROM memories WHERE metadata->>'l0_rule_id' = 'r1')",
        )
        .fetch_one(&pool)
        .await
        .expect("count");
        assert_eq!(count, 3);
    });
}
```

**Verification of import names:** `bring_up_pg_cluster`, `skip_if_no_pg_binaries`, `make_rule`, `seed_path()`, `rt()`, `connect_runtime_pool`, `seed_l0_from_rules` — all should already be in scope from the file's top-of-file imports + helpers. If any aren't, look at the imports at the top and add as needed.

- [ ] **Step 2: Add L1 caller assertion test**

Append to `core/tests/memory_l1_promote_e2e.rs`:

```rust
#[test]
fn promote_l1_inserted_outcome_carries_link_outcome() {
    if skip_if_no_pg_binaries() {
        return;
    }
    use hhagent_core::entity_extraction::StaticEntityExtractor;
    use hhagent_core::memory::l1_promote::{L1Source, L1WriteOutcome};
    use hhagent_db::graph::{Graph, PgGraph};

    rt().block_on(async {
        let cluster = bring_up_pg_cluster().await;
        let spec = cluster.spec.clone();
        let pool = connect_runtime_pool(&spec).await.expect("pool");

        let graph = PgGraph::new(pool.clone());
        let e1 = graph.upsert_entity("person", "carol", None).await.expect("e1").id;
        let e2 = graph.upsert_entity("project", "alpha", None).await.expect("e2").id;

        let extractor = StaticEntityExtractor::with_ids(vec![e1, e2]);

        let outcome = promote_l1(
            &pool, &extractor, "carol leads project alpha",
            L1Source::Operator,
        )
        .await.expect("promote_l1");

        match outcome {
            L1WriteOutcome::Inserted { memory_id, link_outcome } => {
                let link = link_outcome.expect("link_outcome present on Inserted");
                assert_eq!(link.n_entities_linked, 2, "2 entities linked");
                assert_eq!(link.seeds.ids, vec![e1, e2]);
                // memory_entities row check
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM memory_entities WHERE memory_id = $1",
                ).bind(memory_id).fetch_one(&pool).await.expect("count");
                assert_eq!(count, 2);
            }
            other => panic!("expected Inserted, got {other:?}"),
        }
    });
}
```

- [ ] **Step 3: Run workspace tests**

```bash
cargo test --workspace 2>&1 | tail -5
```

Expected: `test result: ok. 847 passed; 0 failed; 4 ignored; ...` (845 + 2 new caller tests = 847; matches the spec's +13 total budget = 834 baseline → 847).

- [ ] **Step 4: Commit**

```bash
git add core/tests/memory_l0_seed_e2e.rs core/tests/memory_l1_promote_e2e.rs
git commit -m "$(cat <<'EOF'
test(core/memory_l0_seed,memory_l1_promote): auto-link e2e pin per writer

One new test per writer using StaticEntityExtractor:
* seed_l0_auto_links_entities_via_extractor — L0SeedReport.entities_linked
  reflects the scripted seed count; memory_entities rows present.
* promote_l1_inserted_outcome_carries_link_outcome — L1WriteOutcome::Inserted
  carries Some(LinkOutcome) with the expected entity count + ids;
  memory_entities rows present.

Workspace test count 845 → 847 (+2), matching the +13 spec budget
across all six tasks (834 → 847).

Spec: docs/superpowers/specs/2026-05-19-memory-entity-link-design.md
EOF
)"
```

---

## Task 7: Final workspace verification + HANDOVER/ROADMAP sync

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

No new code. Confirms the slice is green and updates the two narrative documents.

- [ ] **Step 1: Run the full workspace test suite one final time**

```bash
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | grep -E "test result|warning|\[SKIP\]" | tail -20
```

Expected: zero failures, zero warnings, zero `[SKIP]` lines on the DGX (with venv + weights staged). `847 passed; 0 failed; 4 ignored`.

If anything regressed: fix the regression before continuing.

- [ ] **Step 2: Verify the new module is under the 500-LOC cap**

```bash
wc -l core/src/memory/entity_link.rs core/tests/memory_entity_link_e2e.rs
```

Expected: `entity_link.rs` under ~250 LOC; `memory_entity_link_e2e.rs` under ~400 LOC.

- [ ] **Step 3: Update `docs/devel/handovers/HANDOVER.md`**

Add a "Recently completed (this session, 2026-05-19 — Memory-write-time entity auto-linker)" section above the existing entry, summarising:

- Branch name (suggest `feat/memory-entity-link`).
- Commit chain (Task 1 → Task 6 hashes from `git log --oneline -7`).
- New module `core::memory::entity_link` (~LOC), new test file.
- Cascade: 7 files modified (`l0_seed.rs`, `l1_promote.rs`, `runner.rs`, `cli_audit.rs`, `bin/hhagent-cli.rs`, `main.rs`, three e2e files).
- Workspace test count 834 → 847 (+13).
- What's deliberately NOT in this slice: operator quarantine-review CLI, `hhagent-cli memory relink` backfill subcommand, entities.embedding population, per-link provenance columns.

Bump the header:
- `**Last updated:** 2026-05-19 (Memory-write-time entity auto-linker — branch `feat/memory-entity-link`, N commits, workspace 834 → 847 (+13)...)`
- `**Last commit on main:** <hash of merge or branch tip>`
- `**Session-end verification:** Rust workspace: 847 passed / 0 failed / 4 ignored / 0 warnings / 0 [SKIP]`

Move the "Operator quarantine-review CLI" item up to #1 in the "Next concrete engineering pickups" list (now that the auto-linker has shipped, the quarantine-review CLI is the slice that actually unblocks production graph-lane recall).

- [ ] **Step 4: Update `docs/devel/ROADMAP.md`**

Tick the auto-linker item under the appropriate phase (look for the matching `- [ ]` line; if none yet, add it under Phase 1):

```markdown
- [x] **Memory-write-time entity auto-linker (2026-05-19)** — branch `feat/memory-entity-link`, merged via PR #XX at `<commit>`. New core::memory::entity_link::link_memory_entities compose-op: extract → link_memory_to_entities → audit row. Threaded through seed_l0_from_rules, seed_l0_from_file, promote_l1; cascade through write_l1_promoted_row, drain_lane, lane_loop, spawn_scheduler; CLI memory l1 add path uses NoOp extractor. Workspace 834 → 847 (+13). Spec at docs/superpowers/specs/2026-05-19-memory-entity-link-design.md; plan at docs/superpowers/plans/2026-05-19-memory-entity-link.md.
```

- [ ] **Step 5: Commit the docs**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "$(cat <<'EOF'
docs(handover,roadmap): memory-write-time entity auto-linker — session-end sync

Marks the auto-linker slice complete. Workspace 834 → 847 (+13).
Operator quarantine-review CLI moves to #1 in Next TODO — it's the
slice that actually unblocks production graph-lane recall (every
auto-linked entity is quarantined-by-default).

Spec: docs/superpowers/specs/2026-05-19-memory-entity-link-design.md
Plan: docs/superpowers/plans/2026-05-19-memory-entity-link.md
EOF
)"
```

---

## Self-review

After saving the plan, I reviewed the spec ([`docs/superpowers/specs/2026-05-19-memory-entity-link-design.md`](../specs/2026-05-19-memory-entity-link-design.md)) section-by-section against this plan:

**Spec coverage:**

- §1.1 (Goal) — covered by Task 4 (L0) + Task 5 (L1) wiring.
- §1.2 (Non-goals) — explicitly noted in HANDOVER/ROADMAP update task.
- §1.3 (Locked-in choices) — sync per-write + free function + NoOp-skip all implemented in Task 2's body.
- §2 (Architecture / module layout) — Task 1 creates the file; Task 2-6 modifies the others.
- §3.1 (entity_link.rs public surface) — Task 1 scaffolds, Task 2 fills body.
- §3.2 (L0SeedReport extension) — Task 4 Step 1.
- §3.3 (L1WriteOutcome widening) — Task 5 Step 1.
- §3.4 (Writer signatures) — Task 4 (L0) + Task 5 (L1).
- §4 (Audit row, 6-key payload) — Task 1's `build_entity_link_payload` + Task 2's `link_memory_entities` body + Task 2's mock-tier integration tests (assertion on all 6 keys).
- §5 (Failure isolation + idempotency) — Task 2 body + Task 2 idempotency test + Task 4/5 caller WARN logs.
- §6.1-6.4 (Test breakdown: 6+3+2+2 = 13) — Task 1 (6) + Task 2 (3) + Task 3 (2) + Task 6 (2).
- §6.5 (Test budget +13 → 847) — Task 6 Step 3 verifies.
- §7 (File-size watch) — Task 7 Step 2 verifies.
- §8 (Daemon wiring) — Task 4 Step 4 (move construction) + Task 5 Step 4 (spawn_scheduler arg).
- §9 (Sequencing) — directly drives the 7-task structure here.
- §10 (Plan-phase open questions) — answered during code exploration above; folded into task steps.
- §11 (Locked-in decisions summary) — observed throughout.

**Placeholder scan:** Task 3 Step 2 contains a `todo!()` in the `build_real_extractor` helper, intentionally — the helper's exact code can only be written by reading the existing `entity_extraction_e2e.rs` shape, which is a Step-1 prerequisite explicitly documented in the task. No other placeholders.

**Type consistency:**
- `link_memory_entities` signature is identical in Task 1 (scaffold), Task 2 (body), Task 4 (L0 caller), Task 5 (L1 caller). ✓
- `L1WriteOutcome::Inserted { memory_id, link_outcome }` shape consistent across Task 5 (definition), Task 5 Step 5 (CLI consumer), Task 6 (test assertion). ✓
- `L0SeedReport` field names (`entities_linked`, `link_failures`) consistent across Task 4 Step 1 (definition), Task 4 Step 2 (writer), Task 6 (test). ✓
- `spawn_scheduler` signature consistent across Task 5 Step 3 (definition), Task 5 Step 4 (main.rs caller), Task 5 Step 7 (test caller). ✓

**Scope check:** seven focused tasks, ~one session. Each task is independently committable and unit-of-review-sized.

---

## Plan complete and saved to `docs/superpowers/plans/2026-05-19-memory-entity-link.md`.
