# Memory-write-time entity auto-linker — design

**Status:** design — implementation pending.
**Date:** 2026-05-19.
**Depends on:** Entity Extraction v2 (PR #91, merged 2026-05-19 at `f12b460`).
**Unblocks:** non-empty results from the graph recall lane in production
(paired with the operator quarantine-review CLI, the natural follow-up
slice).
**See also:**
[`2026-05-19-entity-extraction-v2-gliner-relex-design.md`](2026-05-19-entity-extraction-v2-gliner-relex-design.md)
(read-side; the trait + worker this slice consumes),
[`2026-05-16-l0-seed-loader-design.md`](2026-05-16-l0-seed-loader-design.md)
(the L0 writer this slice retrofits),
[`2026-05-17-l1-promotion-writer-design.md`](2026-05-17-l1-promotion-writer-design.md)
(the L1 writer this slice retrofits).

---

## 1. Goal & non-goals

### 1.1 Goal

Every write into the `memories` table — today the L0 seeder
([`core/src/memory/l0_seed.rs`](../../../core/src/memory/l0_seed.rs):346)
and the L1 promotion writer
([`core/src/memory/l1_promote.rs`](../../../core/src/memory/l1_promote.rs):222),
plus future L2 / L3 / L4 writers — invokes the shared
`EntityExtractor::extract` against the memory body, then inserts
`(memory_id, entity_id)` rows into `memory_entities` for the resulting
ids. This populates the table that
[`db::memories::graph_search`](../../../db/src/memories.rs):469 reads;
combined with the upcoming quarantine-review CLI it makes the recall
graph lane return non-empty results in production for the first time.

### 1.2 Non-goals

Deliberately out of scope for this slice:

- **Operator quarantine-review CLI** — separate slice, the natural
  follow-up. Without it, every newly-extracted entity sits at
  `quarantine = TRUE` and is invisible to `graph_search` (which passes
  `include_quarantined = false` on the production read path).
- **Re-linking already-written memories.** A `kastellan-cli memory relink
  [--layer X] [--id Y]` subcommand or one-shot backfill is a separate
  pickup; pre-existing rows stay unlinked until the operator runs that
  tool.
- **`entities.embedding` population.** Column stays NULL; the
  embedding-similarity entity-resolution lane is its own slice.
- **Per-link provenance columns on `memory_entities`.** The existing
  `scheduler/extract_entities` audit row plus the new
  `memory_linker/entity_link` row (this slice) carry the model
  version, latency, and outcome; no schema change to
  `memory_entities`.
- **L2 / L3 / L4 writers.** Don't exist yet; the API is designed so
  they opt in with a single new function call when they materialise.

### 1.3 Locked-in design choices

Confirmed during brainstorming:

1. **Sync per-write** with **NoOp-skip fast path**. No background
   queue; no asymmetric L0-vs-L1 paths. Extraction latency on the
   L0 batch is bounded (one-shot cost, ~150ms × N_new_rules on first
   install only; idempotency skip on subsequent restarts).
2. **Free-function API** in a new `core::memory::entity_link`
   module. Doesn't widen the `EntityExtractor` trait; doesn't create
   a parallel `core::memory::write` wrapper of `kastellan_db::memories`.

---

## 2. Architecture & module layout

```
core/src/memory/
├── mod.rs           # facade; re-exports stay flat — adds
│                    # `pub mod entity_link;`
├── entity_link.rs   # NEW. ~150-200 LOC incl. tests. Pure compose:
│                    # extract → link_memory_to_entities → audit row.
├── l0_seed.rs       # MODIFIED. seed_l0_from_file gains
│                    # `extractor: &dyn EntityExtractor` parameter;
│                    # report gains `entities_linked` + `link_failures`.
├── l1_promote.rs    # MODIFIED. promote_l1 gains extractor parameter;
│                    # L1WriteOutcome::Inserted gains `link_outcome:
│                    # Option<LinkOutcome>`.
├── embed.rs         # untouched
├── layers.rs        # untouched
└── recall.rs        # untouched
```

Daemon main.rs already constructs the `Arc<dyn EntityExtractor>`
(landed in PR #91's `RouterAgent` wiring + post-cleanup `2cf2a0a`'s
single-resolution refactor). That same `Arc` flows into the writers
unchanged — no new construction, no double-warm.

---

## 3. Public surface

### 3.1 `core::memory::entity_link`

```rust
//! Memory-write-time entity auto-linker.
//!
//! Compose-op: extract entities from the body of a freshly-written
//! memory, insert `(memory_id, entity_id)` rows, and emit a 6-key
//! `memory_linker/entity_link` audit row. The memory row must already
//! be committed when this is called (failure here does NOT roll back
//! the memory write).
//!
//! ## Why this isn't a trait method
//!
//! See `docs/superpowers/specs/2026-05-19-memory-entity-link-design.md`
//! §2: keeping the EntityExtractor trait DB-agnostic and PgPool-free
//! is load-bearing for unit tests and future non-Postgres backends.

use std::collections::BTreeMap;

use serde_json::Value;
use sqlx::PgPool;

use crate::entity_extraction::{
    EntityExtractionError, EntityExtractor, EntitySeeds, SeedSource,
};
use kastellan_db::{audit, memories::link_memory_to_entities, DbError};

/// What the auto-linker actually did, for caller telemetry.
#[derive(Clone, Debug)]
pub struct LinkOutcome {
    /// Post-`ON CONFLICT DO NOTHING` row count. May be smaller than
    /// `seeds.ids.len()` when entities were already linked to this
    /// memory (re-run idempotency case).
    pub n_entities_linked: u64,
    /// Forwarded for caller-side telemetry. The seed list itself; the
    /// audit row uses `seeds.ids.len()` separately as `n_seeds` so
    /// observation-phase SQL can see both bucket counts.
    pub seeds: EntitySeeds,
}

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
/// `layer_label` is a stringly-typed identifier of the calling
/// layer (`"L0"`, `"L1"`, future `"L2"`/`"L3"`/`"L4"`). It goes
/// straight into the audit payload. Keeping it stringly avoids a
/// circular dep from this module on `kastellan_db::memories::MemoryLayer`.
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
            // the existing `if entity_ids.is_empty() { return Ok(0); }`
            // fast path — so the NoOp extractor case is essentially
            // free (no SQL issued).
            let n = link_memory_to_entities(pool, memory_id, &seeds.ids).await?;
            (seeds, n)
        }
        Err(e) => {
            // Audit the failed attempt; the audit insert is
            // best-effort (its own error is logged but doesn't shadow
            // the primary extract error). We then propagate the
            // extract error so the caller's `Err` arm runs (warn-log
            // + report counter bump).
            let payload = build_entity_link_payload(
                memory_id,
                layer_label,
                /* n_entities_linked */ 0,
                /* n_seeds */ 0,
                SeedSource::None,
                None,
            );
            if let Err(audit_err) = audit::insert(
                pool, "memory_linker", "entity_link", &payload,
            ).await {
                tracing::warn!(
                    error = %audit_err, memory_id,
                    "memory_linker degraded-path audit row failed"
                );
            }
            return Err(LinkError::from(e));
        }
    };

    // Success path audit row.
    let payload = build_entity_link_payload(
        memory_id,
        layer_label,
        n_linked,
        seeds.ids.len() as u64,
        seeds.source,
        seeds.model_version.as_deref(),
    );
    // Best-effort: an audit insertion failure here doesn't roll back
    // the link rows (they're already committed). Log + continue.
    if let Err(e) = audit::insert(pool, "memory_linker", "entity_link", &payload).await {
        tracing::warn!(error = %e, memory_id, "memory_linker audit row failed");
    }

    Ok(LinkOutcome { n_entities_linked: n_linked, seeds })
}

/// Pure builder: 6 keys, BTreeMap-ordered (matches the convention
/// from `scheduler::audit::build_*_payload`). Unit-tested directly.
fn build_entity_link_payload(
    memory_id: i64,
    layer_label: &str,
    n_entities_linked: u64,
    n_seeds: u64,
    seed_source: SeedSource,
    model_version: Option<&str>,
) -> Value {
    let mut map = BTreeMap::new();
    map.insert("memory_id".to_string(), Value::from(memory_id));
    map.insert("layer".to_string(), Value::from(layer_label));
    map.insert("n_entities_linked".to_string(), Value::from(n_entities_linked));
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
```

### 3.2 `L0SeedReport` extension (additive)

```rust
// core/src/memory/l0_seed.rs

pub struct L0SeedReport {
    pub new_rows_written: u32,
    pub unchanged_skipped: u32,
    // NEW fields (purely additive):
    /// Cumulative `LinkOutcome::n_entities_linked` across all
    /// newly-written L0 rows in this seed pass. Already-existing rows
    /// (sha256-idempotency skip) don't re-extract.
    pub entities_linked: u64,
    /// Count of rows where the memory write succeeded but the
    /// auto-link step failed (extract or DB). The memory body is
    /// safe; future relink-tooling can fill in the gap.
    pub link_failures: u32,
}
```

Existing callers that match `report.new_rows_written` or
`report.unchanged_skipped` are unaffected. The two new fields appear
in the structured `tracing::info!` line that the seeder already emits
at the end of `seed_l0_from_file`.

### 3.3 `L1WriteOutcome::Inserted` widening (additive)

```rust
// core/src/memory/l1_promote.rs

pub enum L1WriteOutcome {
    Inserted {
        memory_id: i64,
        // NEW: None if auto-link errored (warn already logged); Some
        // on success including the NoOp-extractor empty-link case.
        link_outcome: Option<LinkOutcome>,
    },
    SkippedDuplicate { memory_id: i64 },
}
```

`Plan.l1_insight` → `terminal_l1_insight` → `drain_lane` ledger
existing callers match on `Inserted { memory_id, .. }` and don't read
the link payload today; the widening is purely additive.

### 3.4 Writer signatures

```rust
// before:
pub async fn seed_l0_from_file(
    pool: &PgPool, path: &Path,
) -> Result<L0SeedReport, L0Error>;

// after:
pub async fn seed_l0_from_file(
    pool: &PgPool,
    extractor: &dyn EntityExtractor,
    path: &Path,
) -> Result<L0SeedReport, L0Error>;
```

```rust
// before:
pub async fn promote_l1(
    pool: &PgPool, body: &str, source: L1WriteSource,
) -> Result<L1WriteOutcome, L1Error>;

// after:
pub async fn promote_l1(
    pool: &PgPool,
    extractor: &dyn EntityExtractor,
    body: &str,
    source: L1WriteSource,
) -> Result<L1WriteOutcome, L1Error>;
```

Same trait-object reference pattern as `RouterAgent::new` uses today
(`Arc<dyn EntityExtractor>` upstream, `&dyn EntityExtractor` at the
call boundary). All test fixtures already construct
`NoOpEntityExtractor` or `StaticEntityExtractor`; the migration is
mechanical.

---

## 4. Audit row

One new row per `link_memory_entities` call, success OR failure:

| Field   | Value                                                  |
| ------- | ------------------------------------------------------ |
| actor   | `memory_linker`                                        |
| action  | `entity_link`                                          |
| payload | 6 keys (see below)                                     |

Payload key set (BTreeMap-pinned, alphabetical):

```json
{
  "layer": "L0" | "L1" | ...,
  "memory_id": <i64>,
  "model_version": "multi-v1.0" | null,
  "n_entities_linked": <u64>,
  "n_seeds": <u64>,
  "seed_source": "gliner_relex" | "none"
}
```

**Why a separate row** rather than extending the existing
`scheduler/extract_entities` row written by the extractor:

1. The extractor row is keyed by the dispatch site (`scheduler` actor)
   and the extraction call (it doesn't know the `memory_id`); the
   linker row is keyed by the write site (`memory_linker` actor) and
   the link outcome.
2. Observation-phase SQL "which memories got linked to which
   model-extracted entities" is a one-table scan on `memory_linker`
   rows; joining against per-chunk dispatch rows would be a
   reconstruction.
3. The two telemetry buckets — model-ran-zero (`n_seeds = 0,
   seed_source = "gliner_relex"`) vs. extractor-degraded
   (`seed_source = "none"`) — are visible in the linker row without
   cross-referencing.

**Why `n_seeds` AND `n_entities_linked`** when they're often equal:
they diverge on re-link (idempotent rerun: `n_seeds > 0`,
`n_entities_linked = 0`) — that signal is observation-phase-load-bearing
for detecting unnecessary re-extractions.

---

## 5. Failure isolation & idempotency

### 5.1 Failure modes

| Stage                              | Memory survives? | Caller behaviour                                  |
| ---------------------------------- | ---------------- | ------------------------------------------------- |
| `seed_meta_memory` / `insert_memory_at_layer` fails | no | existing error path runs unchanged |
| `extractor.extract(body)` fails    | yes              | `LinkError::Extract` → caller logs warn + counts in report + continues |
| `link_memory_to_entities` fails    | yes              | `LinkError::Db` → same: warn + count + continue |
| Audit `insert` fails               | yes              | logged at WARN; **does NOT propagate** (rows already exist) |

**Memory write and link are deliberately NOT in a single transaction.**

Two reasons:

1. `extractor.extract` hits the GLiNER worker (100-700ms wall-clock
   in CPU mode). Holding a Postgres transaction across an external RPC
   is poor hygiene.
2. We want the memory body to survive even if extraction fails — a
   wrapping transaction would roll the memory back too.

The link-rows-without-memory-row case is structurally impossible
(memory id comes from `RETURNING id` on the prior INSERT). The
memory-row-without-link-rows case is acceptable and recoverable
(future relink tool).

### 5.2 Idempotency

- **Caller-level sha256 dedup** on L0/L1 is the existing gate;
  `link_memory_entities` is only called for newly-inserted rows on the
  happy path.
- **DB-level dedup** in `link_memory_to_entities` (`ON CONFLICT
  (memory_id, entity_id) DO NOTHING`) makes repeat calls safe but
  ineffective (`n_entities_linked` from the second call is 0).
- **No idempotency on the extract call itself.** Re-extracting the
  same body re-pays the inference cost; the `n_entities_linked = 0`
  signal on the audit row is the operator-visible warning that this
  happened. Future optimisation can short-circuit on a content-hash
  cache; YAGNI for v1.

---

## 6. Testing strategy

Mirrors v2's mock-tier + real-model-tier split.

### 6.1 Unit tests in `core/src/memory/entity_link.rs::tests`

| # | Test                                                                   | Verifies                                          |
| - | ---------------------------------------------------------------------- | ------------------------------------------------- |
| 1 | `build_payload_keyset_is_exactly_six`                                  | BTreeMap-pin, no extra/missing keys               |
| 2 | `build_payload_with_model_version_carries_string_value`                | success-path shape                                |
| 3 | `build_payload_without_model_version_emits_json_null`                  | NoOp-path shape                                   |
| 4 | `build_payload_serializes_seed_source_as_snake_case`                   | matches `EntitySeeds::source` wire form           |
| 5 | `link_error_extract_variant_carries_source`                            | `#[from]` propagation pin                         |
| 6 | `link_error_db_variant_carries_source`                                 | `#[from]` propagation pin                         |

No DB, no extractor. Pure-function regression pins. ~6 tests.

### 6.2 Mock-tier integration tests in `core/tests/memory_entity_link_e2e.rs`

Real PG, `StaticEntityExtractor` returning scripted ids; ~3 tests:

| # | Test                                                                    | Verifies                                                                                                                                |
| - | ----------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| 1 | `link_inserts_memory_entities_rows_and_writes_audit_row`                | Static returns `vec![7,13,42]`; 3 `memory_entities` rows; 1 `memory_linker/entity_link` audit row with all 6 keys; `n_entities_linked=3` |
| 2 | `link_with_noop_extractor_writes_no_rows_but_writes_audit_row`          | `NoOpEntityExtractor` → 0 `memory_entities` rows AND 1 audit row with `seed_source="none"`, `n_seeds=0`, `n_entities_linked=0`           |
| 3 | `link_is_idempotent_on_rerun_with_same_seeds`                            | Call twice; second call returns `n_entities_linked=0`; final `memory_entities` row count unchanged; 2 audit rows present                |

### 6.3 Real-model integration tests in `core/tests/memory_entity_link_e2e.rs`

Gated on GLiNER weights + venv; skip-as-pass without them; ~2 tests:

| # | Test                                                                     | Verifies                                                                                                                                            |
| - | ------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| 4 | `link_against_real_extractor_writes_real_entity_ids`                     | "Dr Smith treats asthma in Mosman." → ≥2 `memory_entities` rows; entity rows quarantined-by-default; audit row carries `seed_source="gliner_relex"`, `model_version=Some("multi-v1.0")` |
| 5 | `link_extends_to_l0_seed_path_end_to_end`                                | `seed_l0_from_file` against a 2-rule fixture file with a real extractor: both rules linked; `report.entities_linked > 0`; `link_failures == 0`                          |

### 6.4 Caller-side test extensions

- `core/tests/memory_l0_seed_e2e.rs`: add 1 assertion using
  `StaticEntityExtractor::with_ids` to pin `report.entities_linked` +
  `memory_entities` row presence.
- `core/tests/memory_l1_promote_e2e.rs`: add 1 assertion that
  `L1WriteOutcome::Inserted` carries `link_outcome: Some(_)` with the
  expected `n_entities_linked` count.

### 6.5 Workspace test-count budget

- Unit (entity_link): +6
- Mock-tier e2e (entity_link): +3
- Real-model e2e (entity_link): +2
- L0/L1 caller extensions: +2
- **Total: +13** → workspace 834 → **847** (target).

### 6.6 Verification gate

`cargo test --workspace` on the DGX with vLLM still owning the GPU
(GLiNER CPU mode): all green, 0 [SKIP]. Real-model tier passes
against the live `multi-v1.0` weights; skip-as-pass on hosts without
them.

---

## 7. File-size watch

`core/src/memory/entity_link.rs` is the only new file. Budget:

- Module-level docs + uses + types: ~40 LOC
- `link_memory_entities` body: ~50 LOC
- `build_entity_link_payload` body: ~30 LOC
- `#[cfg(test)] mod tests`: ~80 LOC
- **Total: ~200 LOC** (well under the 500-LOC soft cap)

Modifications to `l0_seed.rs` and `l1_promote.rs` add ~30-40 LOC each
(extractor parameter, link call, report fields, error-arm warn-log).
Neither approaches the cap.

---

## 8. Daemon wiring

The daemon already constructs the `Arc<dyn EntityExtractor>` in
`core/src/main.rs` (post-PR-#91, post-cleanup `2cf2a0a`). The current
flow:

```rust
// existing
let gliner_entry = build_gliner_relex_entry(...);     // resolved once
let entity_extractor: Arc<dyn EntityExtractor> = match gliner_entry {
    Some(entry) => Arc::new(GlinerRelexExtractor::new(
        Client::new(lifecycle.clone(), pool.clone(), entry),
        pool.clone(),
    )),
    None => Arc::new(NoOpEntityExtractor::new()),
};
```

This Arc currently flows into `RouterAgent::new`. The auto-linker
slice adds two more pass-throughs:

1. **L0 seed call site** (already happens at daemon startup, after
   `db::probe::run` and before `lane runners spawn`):

   ```rust
   // before
   memory::l0_seed::seed_l0_from_file(&pool, l0_path).await?;
   // after
   memory::l0_seed::seed_l0_from_file(&pool, &*entity_extractor, l0_path).await?;
   ```

2. **L1 promotion writer** — the writer is constructed and passed
   into the lane runners. Its existing struct gains an
   `entity_extractor: Arc<dyn EntityExtractor>` field; the writer
   function calls `promote_l1(&pool, &*self.entity_extractor, body, source)`.

Both pass-throughs use the **same Arc** the agent uses → same warm
GLiNER worker pool → no extra cold-spawns.

---

## 9. Sequencing (rough — full TDD-ordered plan in writing-plans phase)

The implementation falls into clean dependency-ordered slices:

1. **`entity_link.rs` skeleton + `build_entity_link_payload` + unit
   tests** (no DB) — pure-function regression pins land first.
2. **`link_memory_entities` body + mock-tier e2e** — real PG bring-up
   in tests; `StaticEntityExtractor` proves the compose.
3. **`L0SeedReport` widening + L0 caller wire-up + L0 caller e2e
   extension** — the simpler of the two writer retrofits (no struct
   widening on the outcome enum).
4. **`L1WriteOutcome::Inserted` widening + L1 caller wire-up + L1
   caller e2e extension** — slightly more invasive (enum-variant
   widening propagates through the drain-lane hook's match arms).
5. **Real-model e2e tier** — last; runs against live weights; serves
   as the production-readiness gate.
6. **Daemon wiring in `main.rs`** — final pass-through; verified
   manually + by the `supervisor_e2e` startup probe.

Total estimate: one focused session (~4-6 hours), test-driven from
step 1.

---

## 10. Open questions for the planner phase

None that block the design. A few that the implementation plan will
need to nail down concretely:

1. **Where exactly in `main.rs` does the L0 seed run today?** The
   plan's Task 6 needs the precise insertion point for the new
   extractor-passthrough argument. Implementation will check via
   `grep` and write the exact diff into the task.
2. **How does the L1 writer struct get constructed today?** Likely a
   plain `PgPool` capture; the plan's Task 4 needs the exact
   constructor signature for adding the extractor field.
3. **Does the L0 seed get called every restart, or only when the
   seed file's sha256 changes?** Affects how visible the per-restart
   extraction latency is to the operator. (Quick read of
   `l0_seed.rs` says per-rule sha256 check, so already-seeded rules
   are skipped — extraction cost only on first install of a given
   rule. Confirm during plan-writing.)

---

## 11. Locked-in decisions (summary)

- **Sync per-write**, no background queue.
- **Free-function API** in new `core::memory::entity_link` module
  (not a trait method, not a writer wrapper).
- **Memory write + link are NOT in one transaction** (separate
  commits; link failure leaves memory unlinked but intact).
- **One audit row per link attempt**, success or failure, 6 keys,
  `actor='memory_linker' action='entity_link'`.
- **Same Arc<dyn EntityExtractor> shared** between query-time
  extraction (RouterAgent) and write-time linking (this slice). Same
  warm GLiNER worker pool.
- **`layer_label: &'static str`** on the API, not a typed enum —
  avoids the circular dep from `core::memory::entity_link` on
  `kastellan_db::memories::MemoryLayer`.
- **NoOp-extractor case is a path optimisation**, not a branch — the
  empty-seeds fast-path in `link_memory_to_entities` makes it
  essentially free without any new conditional logic.
- **Operator quarantine-review CLI remains the natural follow-up**
  for unblocking graph-lane hits in production.
