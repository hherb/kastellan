# Memory two-tier write path (`insert_memory_light`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `db::memories::insert_memory_light(executor, body, metadata, layer)` — a deliberately-named, embedding-skipping writer for high-frequency ephemeral rows.

**Architecture:** A thin named delegate to the existing `insert_memory_at_layer` chokepoint with `embedding = None`. No new SQL, no schema change. Inherits the L0 (`MemoryLayer::Meta`) `PolicyViolation` guard for free. The value-add is the intent-signalling name plus a documented degradation contract (lexical + `metadata @>` work; semantic + graph degrade gracefully). Mirrors how `seed_meta_memory` is a named pass-through.

**Tech Stack:** Rust, `sqlx` (Postgres), the `kastellan-db` crate. Integration tests are PG-required and live in `db/tests/postgres_e2e.rs` using the `kastellan-tests-common` bring-up helpers (skip-as-pass without PG).

**Spec:** [`docs/devel/specs/2026-06-07-memory-light-write-path-design.md`](../specs/2026-06-07-memory-light-write-path-design.md)

---

### Task 1: Add `insert_memory_light` + happy-path / L0-rejection test

**Files:**
- Modify: `db/src/memories/write.rs` (add function after `insert_memory_at_layer`, ends at line 161)
- Modify: `db/src/memories.rs:70-73` (add to the `pub use write::{…}` re-export)
- Test: `db/tests/postgres_e2e.rs` (new `#[tokio::test]` near the existing `insert_memory_at_layer_round_trip` at line 1684)

- [ ] **Step 1: Write the failing test**

Append this test to `db/tests/postgres_e2e.rs` (after `insert_memory_at_layer_round_trip`, i.e. after line 1799). It exercises the happy path (row persisted with NULL embedding + correct layer) and the inherited L0 rejection in one cluster, mirroring the existing `insert_memory_at_layer_round_trip` structure:

```rust
/// `insert_memory_light` persists a row with a NULL embedding at the
/// requested non-L0 layer, and inherits the L0 (Meta) PolicyViolation
/// guard from `insert_memory_at_layer`. The rejection short-circuits
/// before any SQL, so it is exercised on the same pool to avoid a
/// second cluster — same pattern as `insert_memory_at_layer_round_trip`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn insert_memory_light_round_trip_and_rejects_l0() {
    use kastellan_db::memories::{insert_memory_light, MemoryLayer};
    use kastellan_tests_common::{
        bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
    };

    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "mr-d",
        "mr-l",
        &format!("kastellan-pg-mlight-round-trip-{suffix}"),
    );

    kastellan_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "memory-light-round-trip"}),
    )
    .await
    .expect("probe");

    let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("pool");

    // Happy path: light insert at L2 (Stable) returns an id; the row has
    // a NULL embedding and the requested layer.
    let id = insert_memory_light(
        &pool,
        "light ephemeral row",
        &serde_json::json!({"ns": "observations"}),
        MemoryLayer::Stable,
    )
    .await
    .expect("insert_memory_light");

    // `layer` is a SMALLINT column, so decode it as i16 (not i32).
    let (embedding_is_null, layer): (bool, i16) =
        sqlx::query_as("SELECT embedding IS NULL, layer FROM memories WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("fetch light row");
    assert!(
        embedding_is_null,
        "insert_memory_light must persist a NULL embedding"
    );
    assert_eq!(layer, 2, "light row must land at the requested layer (L2)");

    // Policy: L0 (Meta) is rejected, inherited from insert_memory_at_layer.
    let rejected = insert_memory_light(
        &pool,
        "l0 via light path (forbidden)",
        &serde_json::json!({}),
        MemoryLayer::Meta,
    )
    .await;
    match rejected {
        Err(kastellan_db::DbError::PolicyViolation(msg)) => {
            assert!(
                msg.contains("L0") && msg.contains("seed_meta_memory"),
                "PolicyViolation must name L0 and the admin path; got: {msg}"
            );
        }
        Err(other) => panic!("expected DbError::PolicyViolation, got {other:?}"),
        Ok(leaked) => panic!("L0 light write must be rejected; got id {leaked}"),
    }

    let l0_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memories WHERE layer = 0")
        .fetch_one(&pool)
        .await
        .expect("count L0 rows");
    assert_eq!(
        l0_count, 0,
        "rejected L0 light write must not leak a row into memories"
    );

    pool.close().await;
}
```

- [ ] **Step 2: Run test to verify it fails**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-db --test postgres_e2e insert_memory_light_round_trip_and_rejects_l0 2>&1 | tail -20
```

Expected: **compile error** — `insert_memory_light` does not exist yet (`cannot find function insert_memory_light in module kastellan_db::memories`). (This is the TDD red state for a Rust API addition; a missing-symbol compile failure is the failing test.)

- [ ] **Step 3: Write the minimal implementation**

In `db/src/memories/write.rs`, immediately after the closing brace of `insert_memory_at_layer` (line 161), add:

```rust
/// Insert a memory row **without** an embedding — the "light" write path
/// for high-frequency, ephemeral data (channel inbound, browser
/// observations, screen capture) that would never be a useful
/// semantic-search target. Skipping the embed call is the whole point;
/// there is deliberately no `embedding` parameter.
///
/// A thin named delegate to [`insert_memory_at_layer`] with
/// `embedding = None` — so it inherits the same single insert chokepoint
/// and the same **L0 ([`MemoryLayer::Meta`]) rejection**
/// ([`DbError::PolicyViolation`]; L0 writes must go through
/// [`seed_meta_memory`]). The value-add is the intent-signalling name,
/// exactly like [`seed_meta_memory`] is a named pass-through.
///
/// # Recall degradation contract
///
/// A light-written row has `embedding IS NULL` and (by caller contract)
/// no `memory_entities` links — entity extraction is a `core`-side step
/// the light path skips. Therefore:
///
/// - **Lexical lane** (full-text on `body`) — works normally; never
///   touches `embedding`.
/// - **`metadata @>` containment** — works normally; embedding-free.
/// - **Semantic lane** — silently skips the row: `semantic_search`
///   filters `WHERE embedding IS NOT NULL`, so a NULL-embedding row
///   degrades gracefully rather than erroring.
/// - **Graph lane** — never surfaces it: with no `memory_entities`
///   links, the 1-hop entity expansion finds nothing.
///
/// This is graceful degradation, not breakage: the row stays retrievable
/// by the two embedding-free lanes.
///
/// `executor` is generic over `sqlx::Executor` so the same helper works
/// against `&PgPool` (production) and `&mut PgConnection` (test setup).
pub async fn insert_memory_light<'e, E>(
    executor: E,
    body: &str,
    metadata: &serde_json::Value,
    layer: MemoryLayer,
) -> Result<i64, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    insert_memory_at_layer(executor, body, metadata, None, layer).await
}
```

Then add `insert_memory_light` to the parent re-export in `db/src/memories.rs:70-73`, keeping the list alphabetical-ish as it already is:

```rust
pub use write::{
    delete_memory_at_layer, insert_memory, insert_memory_at_layer, insert_memory_light,
    link_memory_to_entities, seed_meta_memory, set_skill_trust,
};
```

- [ ] **Step 4: Run test to verify it passes**

```sh
cargo test -p kastellan-db --test postgres_e2e insert_memory_light_round_trip_and_rejects_l0 2>&1 | tail -20
```

Expected: **PASS** with live PG (`KASTELLAN_PG_BIN_DIR` set / DGX), or a `[SKIP]`/early-return with no PG (macOS skip-as-pass). Either is green; it must not FAIL or error-compile.

- [ ] **Step 5: Commit**

```sh
git add db/src/memories/write.rs db/src/memories.rs db/tests/postgres_e2e.rs
git commit -m "feat(db/memories): add insert_memory_light embedding-skipping writer

ROADMAP:130. Thin named delegate to insert_memory_at_layer with
embedding=None; inherits the L0 PolicyViolation guard. Test pins the
happy path (NULL embedding + correct layer) and L0 rejection.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 2: Degradation pin — light row invisible to semantic, visible to lexical + `metadata @>`

**Files:**
- Test: `db/tests/postgres_e2e.rs` (new `#[tokio::test]` after the Task 1 test)

- [ ] **Step 1: Write the failing test**

Append after the Task 1 test in `db/tests/postgres_e2e.rs`. It inserts one normally-embedded row and one light row, then proves the documented contract on live rows:

```rust
/// Degradation contract: a light-written (NULL-embedding) row is absent
/// from `semantic_search` results yet present via the lexical lane and a
/// `metadata @>` containment query. Pins the spec's degradation table on
/// real rows. An embedded control row proves the semantic lane is
/// actually returning results (so the light row's absence is meaningful,
/// not an empty query).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn insert_memory_light_degrades_gracefully_across_lanes() {
    use kastellan_db::memories::{
        insert_memory_at_layer, insert_memory_light, lexical_search, semantic_search,
        MemoryLayer, EMBEDDING_DIM,
    };
    use kastellan_tests_common::{
        bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
    };

    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "mr-d",
        "mr-l",
        &format!("kastellan-pg-mlight-degrade-{suffix}"),
    );

    kastellan_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "memory-light-degrade"}),
    )
    .await
    .expect("probe");

    let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("pool");

    // A full-dimension query/control vector, reused for the embedded
    // control row and the semantic query. `.as_slice()` is explicit so
    // the `Option<&[f32]>` / `&[f32]` parameter types are unambiguous.
    let query_vec = vec![0.1f32; EMBEDDING_DIM];

    // An embedded control row (so semantic_search returns something).
    let embedded_id = insert_memory_at_layer(
        &pool,
        "embedded control row",
        &serde_json::json!({"ns": "control"}),
        Some(query_vec.as_slice()),
        MemoryLayer::Stable,
    )
    .await
    .expect("insert embedded control");

    // The light row carries a distinctive lexeme + namespace metadata.
    let light_id = insert_memory_light(
        &pool,
        "zqxwv distinctive lexeme payload",
        &serde_json::json!({"ns": "observations"}),
        MemoryLayer::Stable,
    )
    .await
    .expect("insert_memory_light");

    // Semantic lane: light row never appears (NULL embedding filtered);
    // the embedded control row does.
    let semantic = semantic_search(&pool, query_vec.as_slice(), 10)
        .await
        .expect("semantic_search");
    assert!(
        !semantic.contains(&light_id),
        "NULL-embedding light row must not appear in semantic_search"
    );
    assert!(
        semantic.contains(&embedded_id),
        "embedded control row must appear in semantic_search (lane is live)"
    );

    // Lexical lane: the distinctive lexeme surfaces the light row.
    let lexical = lexical_search(&pool, "zqxwv", 10)
        .await
        .expect("lexical_search");
    assert!(
        lexical.contains(&light_id),
        "light row must be retrievable via the lexical lane"
    );

    // metadata @> containment: the namespace selector surfaces it.
    let meta_hits: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM memories WHERE metadata @> $1::jsonb",
    )
    .bind(serde_json::json!({"ns": "observations"}))
    .fetch_all(&pool)
    .await
    .expect("metadata containment query");
    assert!(
        meta_hits.contains(&light_id),
        "light row must be retrievable via a metadata @> containment query"
    );

    pool.close().await;
}
```

- [ ] **Step 2: Run test to verify it fails (or passes immediately)**

```sh
cargo test -p kastellan-db --test postgres_e2e insert_memory_light_degrades_gracefully_across_lanes 2>&1 | tail -20
```

Expected: with live PG, **PASS** (Task 1 already added the function, so this test compiles and the contract already holds). With no PG, early-return skip. This task is a *characterization pin* of already-correct behaviour — there is no separate implementation step because the degradation is a property of the existing `semantic_search` filter + the NULL embedding, not new code. If it FAILS, the contract assumption in the spec is wrong — stop and re-examine `semantic_search`/`lexical_search` before forcing the test green.

- [ ] **Step 3: Commit**

```sh
git add db/tests/postgres_e2e.rs
git commit -m "test(db/memories): pin insert_memory_light recall degradation contract

ROADMAP:130. Light (NULL-embedding) row is absent from semantic_search
yet present via the lexical lane and a metadata @> query, with an
embedded control row proving the semantic lane is live.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 3: Full-workspace verification + clippy

**Files:** none (verification only)

- [ ] **Step 1: Run the full workspace test suite**

```sh
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | tail -25
```

Expected: all suites `ok`, 0 failed. On macOS this is skip-as-pass for the PG-required tests (the two new tests early-return); on the DGX with live PG they run for real.

- [ ] **Step 2: Run clippy with the workspace gate**

```sh
cargo clippy --workspace --all-targets --locked -- -D warnings 2>&1 | tail -15
```

Expected: exit 0, no warnings.

- [ ] **Step 3: Confirm the file stays under the 500-LOC cap**

```sh
wc -l db/src/memories/write.rs
```

Expected: still well under 500 (was 289; the addition is ~45 lines incl. docs, landing near ~334).

No commit — this task is pure verification. Any failure here sends you back to the relevant task.

---

## Notes for the implementer

- **No migration, no schema change.** `embedding` is already nullable; `insert_memory_at_layer` already has the NULL-embedding SQL branch.
- **Why Task 2 has no implementation step:** the degradation is an emergent property of `semantic_search`'s existing `WHERE embedding IS NOT NULL` filter ([`db/src/memories/search.rs:51`](../../../db/src/memories/search.rs#L51)) combined with the NULL embedding. The test characterizes it; it does not drive new code.
- **Deferred (do NOT build now):** core-side caller wiring, per-namespace caps + oldest-eviction. These are follow-ups noted in the spec.
- **macOS dev box:** the two integration tests skip-as-pass without `KASTELLAN_PG_BIN_DIR`. To run them for real locally, use the session-local Postgres.app override (see the memory note `postgres-app-bin-paths.md`). Otherwise they are verified on the DGX/CI.
