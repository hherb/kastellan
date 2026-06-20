# L1 Embedding Population Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Populate L1 insight rows with embeddings so the semantic recall lane stops returning 0 rows.

**Architecture:** A new `Embedder` seam (mirroring the existing `EntityExtractor` seam) is injected into `promote_l1` and called lazily after the dedup check. The agent-raised scheduler path injects a `Router`-backed embedder (delegating to the existing `embed_query`, which already truncates to 256-d and writes the audit row); the operator CLI path injects a `NoOpEmbedder`. Embed failure degrades to a NULL embedding + WARN — the write is never blocked.

**Tech Stack:** Rust, `async_trait`, sqlx/Postgres, `kastellan-llm-router`.

## Global Constraints

- AGPL-3.0; AGPL-compatible deps only. No new dependencies are needed for this work.
- Cross-platform (Linux + macOS); this change is pure-Rust with no OS-gated code.
- Rust core only; no Python, no PyO3.
- `EMBEDDING_DIM = 256`; never hardcode 1024/768. Route all model output through `db::memories::truncate_to_embedding_dim` (done inside the existing `embed_query`).
- Keep files under 500 LOC where feasible.
- Source cargo env first in every shell: `source "$HOME/.cargo/env"`.
- All tests must pass before committing.
- Subagent commits: `git add <specific files>` — NEVER `git add -A` (an untracked `assets/agent_with_the_keys.png` and draft docs must stay out of commits).

---

### Task 1: `Embedder` seam module

**Files:**
- Create: `core/src/memory/embedder.rs`
- Modify: `core/src/memory/mod.rs:46-63` (add `pub mod embedder;` + re-export)
- Test: unit tests inline in `core/src/memory/embedder.rs`

**Interfaces:**
- Consumes: `core::memory::embed::embed_query(pool, router, text) -> Result<Vec<f32>, MemoryError>` (existing); `kastellan_llm_router::Router` (existing).
- Produces:
  - `pub trait Embedder: Send + Sync { async fn embed_for_storage(&self, text: &str) -> Option<Vec<f32>>; }`
  - `pub struct RouterEmbedder { pool: sqlx::PgPool, router: std::sync::Arc<kastellan_llm_router::Router> }` with `pub fn new(pool, router) -> Self`.
  - `pub struct NoOpEmbedder;` with `pub fn new() -> Self` + `Default`.

- [ ] **Step 1: Write the failing tests**

Create `core/src/memory/embedder.rs` with the test module first:

```rust
//! The `Embedder` seam: turns an L1 body into a stored-contract embedding
//! vector. Mirrors the `EntityExtractor` seam — the agent-raised write path
//! injects a real `Router`-backed impl ([`RouterEmbedder`]); the operator
//! CLI path injects a [`NoOpEmbedder`] so its rows stay embedding-free
//! (a future batch-(re)embed workflow handles them).
//!
//! Returning `Option<Vec<f32>>` (not `Result`) means the caller
//! ([`crate::memory::l1_promote::promote_l1`]) cannot conflate "intentional
//! skip" with "embed failure" — both store NULL. The WARN distinction is
//! preserved inside [`RouterEmbedder`], so the write path stays trivial and
//! a flaky local embedder never blocks an insight write.

use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;

use kastellan_llm_router::Router;

use super::embed::embed_query;

/// Async seam: produce a stored-contract embedding (EMBEDDING_DIM-length,
/// unit-norm) for `text`, or `None` to store no embedding.
///
/// `None` covers two cases the caller need not distinguish:
/// - intentional skip ([`NoOpEmbedder`]), and
/// - a soft-failed embed ([`RouterEmbedder`] logs the WARN, returns `None`).
#[async_trait]
pub trait Embedder: Send + Sync {
    async fn embed_for_storage(&self, text: &str) -> Option<Vec<f32>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_embedder_returns_none() {
        let e = NoOpEmbedder::new();
        assert!(e.embed_for_storage("anything").await.is_none());
    }

    /// Object-safety + `&dyn` usage compile-pin (mirrors the trait-pin
    /// tests elsewhere in `core`).
    #[test]
    fn embedder_is_object_safe() {
        fn _takes(_e: &dyn Embedder) {}
        let n = NoOpEmbedder::new();
        _takes(&n);
    }

    /// `RouterEmbedder` degrades to `None` (not a panic, not an error) when
    /// the embedding endpoint is unreachable. Uses a lazily-constructed pool
    /// — the failure path returns before any DB/audit write, so no live
    /// Postgres is required.
    #[tokio::test]
    async fn router_embedder_degrades_to_none_on_transport_error() {
        let mut cfg = kastellan_llm_router::RouterConfig::default();
        // Port 1 is unbound; the embed call fails at transport.
        cfg.embedding_url = "http://127.0.0.1:1/v1/embeddings".to_string();
        let router = Arc::new(Router::new(cfg).expect("router"));
        let pool = PgPool::connect_lazy(
            "postgres://invalid:invalid@127.0.0.1:1/nonexistent",
        )
        .expect("lazy pool");

        let e = RouterEmbedder::new(pool, router);
        assert!(
            e.embed_for_storage("some insight").await.is_none(),
            "unreachable embed endpoint must degrade to None"
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib memory::embedder 2>&1 | tail -20`
Expected: FAIL — `NoOpEmbedder`, `RouterEmbedder` not found (compile error).

- [ ] **Step 3: Write the minimal implementation**

Add the impls above the `#[cfg(test)]` module in `core/src/memory/embedder.rs`:

```rust
/// `Router`-backed embedder for the agent-raised write path. Delegates to
/// [`embed_query`], which already Matryoshka-truncates the model output to
/// `EMBEDDING_DIM` and writes the `actor='llm:router' action='embed'` audit
/// row. On any embed error it logs a WARN and returns `None` (degrade-and-
/// warn — the insight write proceeds with a NULL embedding).
pub struct RouterEmbedder {
    pool: PgPool,
    router: Arc<Router>,
}

impl RouterEmbedder {
    pub fn new(pool: PgPool, router: Arc<Router>) -> Self {
        Self { pool, router }
    }
}

#[async_trait]
impl Embedder for RouterEmbedder {
    async fn embed_for_storage(&self, text: &str) -> Option<Vec<f32>> {
        match embed_query(&self.pool, &self.router, text).await {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(
                    target: "kastellan::memory",
                    error = %e,
                    "L1 embed failed; row will be stored with NULL embedding"
                );
                None
            }
        }
    }
}

/// No-op embedder for the operator CLI path. Always returns `None` so
/// operator-added L1 rows stay embedding-free by design (symmetric with
/// [`crate::entity_extraction::NoOpEntityExtractor`]).
pub struct NoOpEmbedder;

impl NoOpEmbedder {
    pub fn new() -> Self {
        Self
    }
}

impl Default for NoOpEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Embedder for NoOpEmbedder {
    async fn embed_for_storage(&self, _text: &str) -> Option<Vec<f32>> {
        None
    }
}
```

Then wire the module into `core/src/memory/mod.rs`. Add after the existing `mod embed;` line (line 46):

```rust
pub mod embedder;
```

And extend the re-export (after the `pub use embed::{embed_query, MemoryError};` line 63):

```rust
pub use embedder::{Embedder, NoOpEmbedder, RouterEmbedder};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib memory::embedder 2>&1 | tail -20`
Expected: PASS — 3 tests pass (`noop_embedder_returns_none`, `embedder_is_object_safe`, `router_embedder_degrades_to_none_on_transport_error`).

- [ ] **Step 5: Verify clippy is clean**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --all-targets 2>&1 | tail -15`
Expected: no warnings introduced by `embedder.rs`.

- [ ] **Step 6: Commit**

```bash
git add core/src/memory/embedder.rs core/src/memory/mod.rs
git commit -m "feat(memory): add Embedder seam (RouterEmbedder + NoOpEmbedder) (#323)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: `promote_l1` embeds lazily after dedup

**Files:**
- Modify: `core/src/memory/l1_promote.rs` (signature + body of `promote_l1`; doc note at lines 209-213; signature-pin test ~line 443)
- Modify: `core/src/cli_audit.rs` (`l1_add_and_audit` passes `&NoOpEmbedder`)
- Modify: `core/src/scheduler/runner.rs` (`write_l1_promoted_row` body constructs a local `NoOpEmbedder` — TEMPORARY, replaced by the real one in Task 3; signature unchanged here)
- Test: `core/tests/memory_l1_promote_e2e.rs` (FakeEmbedder helper + 3 new tests + mechanical arg updates at the 4 `promote_l1(` call sites)

**Interfaces:**
- Consumes: `Embedder`, `NoOpEmbedder` from Task 1.
- Produces: `promote_l1(pool, extractor: &dyn EntityExtractor, embedder: &dyn Embedder, body: &str, source: L1Source) -> Result<L1WriteOutcome, L1Error>` — the new 5-arg signature every caller now uses.

- [ ] **Step 1: Write the failing e2e tests**

In `core/tests/memory_l1_promote_e2e.rs`, add the import and the `FakeEmbedder` helper near the top (after the existing `use` block):

```rust
use kastellan_core::memory::embedder::{Embedder, NoOpEmbedder};
use kastellan_db::memories::{semantic_search, EMBEDDING_DIM};
use std::sync::atomic::{AtomicUsize, Ordering};
use async_trait::async_trait;

/// Test embedder: counts calls and returns a fixed unit vector (or `None`).
/// `None` models both the NoOp and the embed-failure degrade paths.
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
```

Then add three test functions (place after the existing scenarios; each brings up its own cluster following the established pattern):

```rust
// ---------------------------------------------------------------------------
// Scenario: embed-on-insert — a Some(vec) embedder stores a non-NULL
// embedding and semantic_search returns the row.
// ---------------------------------------------------------------------------
#[test]
fn promote_l1_stores_embedding_and_semantic_search_finds_it() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir, "l1emb-d", "l1emb-l",
        &format!("kastellan-supervisor-test-pg-l1emb-{suffix}"),
    );
    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec, "core", "startup",
            serde_json::json!({"purpose": "l1-embed"}),
        ).await.expect("probe");
        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await.expect("pool");

        let embedder = FakeEmbedder::returning(Some(unit_vec_e0()));
        let outcome = promote_l1(
            &pool, &NoOpEntityExtractor::new(), &embedder,
            "semantically retrievable insight", L1Source::Operator,
        ).await.expect("promote_l1");
        assert!(matches!(outcome, L1WriteOutcome::Inserted { .. }));
        assert_eq!(embedder.call_count(), 1, "embedder called once on insert");

        let hits = semantic_search(&pool, &unit_vec_e0(), 10).await.expect("semantic_search");
        assert_eq!(hits, vec![outcome.memory_id()], "semantic lane returns the embedded row");
    });
}

// ---------------------------------------------------------------------------
// Scenario: lazy on dedup-skip — a duplicate body never triggers an embed.
// ---------------------------------------------------------------------------
#[test]
fn promote_l1_does_not_embed_on_dedup_skip() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir, "l1lazy-d", "l1lazy-l",
        &format!("kastellan-supervisor-test-pg-l1lazy-{suffix}"),
    );
    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec, "core", "startup",
            serde_json::json!({"purpose": "l1-lazy"}),
        ).await.expect("probe");
        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await.expect("pool");

        let embedder = FakeEmbedder::returning(Some(unit_vec_e0()));
        let first = promote_l1(
            &pool, &NoOpEntityExtractor::new(), &embedder,
            "duplicate body", L1Source::Operator,
        ).await.expect("first insert");
        assert!(matches!(first, L1WriteOutcome::Inserted { .. }));

        let second = promote_l1(
            &pool, &NoOpEntityExtractor::new(), &embedder,
            "duplicate body", L1Source::Operator,
        ).await.expect("second insert");
        assert!(matches!(second, L1WriteOutcome::SkippedDuplicate { .. }));
        assert_eq!(embedder.call_count(), 1, "skip path must NOT embed");
    });
}

// ---------------------------------------------------------------------------
// Scenario: degrade-and-warn — a None embedder stores a NULL embedding;
// the row is written, semantic_search skips it, lexical still finds it.
// ---------------------------------------------------------------------------
#[test]
fn promote_l1_stores_null_embedding_when_embedder_returns_none() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir, "l1deg-d", "l1deg-l",
        &format!("kastellan-supervisor-test-pg-l1deg-{suffix}"),
    );
    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec, "core", "startup",
            serde_json::json!({"purpose": "l1-degrade"}),
        ).await.expect("probe");
        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await.expect("pool");

        let embedder = FakeEmbedder::returning(None);
        let outcome = promote_l1(
            &pool, &NoOpEntityExtractor::new(), &embedder,
            "insight with no embedding", L1Source::Operator,
        ).await.expect("promote_l1");
        let id = outcome.memory_id();

        let hits = semantic_search(&pool, &unit_vec_e0(), 10).await.expect("semantic_search");
        assert!(!hits.contains(&id), "NULL-embedding row must be absent from semantic lane");

        let lex = kastellan_db::memories::lexical_search(&pool, "embedding", 10)
            .await.expect("lexical_search");
        assert!(lex.contains(&id), "row still retrievable via lexical lane");
    });
}
```

Also update the **4 existing direct `promote_l1(` call sites** in this file (lines ~450, 526, 599, 680) to pass `&NoOpEmbedder::new()` as the new third argument, e.g.:

```rust
let outcome = promote_l1(
    &pool, &NoOpEntityExtractor::new(), &NoOpEmbedder::new(),
    "agent insight", L1Source::AgentRaised { task_id: 7 },
).await.expect("promote_l1");
```

- [ ] **Step 2: Run the new tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --test memory_l1_promote_e2e 2>&1 | tail -25`
Expected: FAIL to compile — `promote_l1` takes 4 args, not 5 (`embedder` not yet a parameter).

- [ ] **Step 3: Add the `embedder` parameter to `promote_l1`**

In `core/src/memory/l1_promote.rs`, change the signature and body of `promote_l1`. Add the import at the top of the file (with the other `use crate::memory::...` lines):

```rust
use crate::memory::embedder::Embedder;
```

Replace the signature (line ~214) and the insert block. New signature:

```rust
pub async fn promote_l1(
    pool: &PgPool,
    extractor: &dyn EntityExtractor,
    embedder: &dyn Embedder,
    body: &str,
    source: L1Source,
) -> Result<L1WriteOutcome, L1Error> {
```

Keep the validate + EXISTS-check + `SkippedDuplicate` early-return exactly as-is. After the `let metadata = build_l1_metadata(...)` line and before `insert_memory_at_layer`, embed lazily:

```rust
    // Embed AFTER the dedup miss so a duplicate body never triggers an
    // embed call. On embed failure the embedder returns None (it logs the
    // WARN); the row is stored with a NULL embedding rather than blocking
    // the insight write.
    let embedding = embedder.embed_for_storage(trimmed).await;

    let new_id = insert_memory_at_layer(
        pool,
        trimmed,
        &metadata,
        embedding.as_deref(),
        MemoryLayer::Index,
    )
    .await?;
```

Rewrite the stale `**Embedding:**` doc paragraph (lines ~209-213) to:

```rust
/// **Embedding:** populated lazily via the injected [`Embedder`] — but
/// only after the dedup EXISTS-check passes, so a duplicate body never
/// triggers an embed call. The agent-raised path injects a
/// [`crate::memory::RouterEmbedder`] (truncated to `EMBEDDING_DIM`,
/// unit-norm, with an `action='embed'` audit row); the operator CLI path
/// injects a [`crate::memory::NoOpEmbedder`] so operator rows stay
/// embedding-free. On embed failure the row is stored with a NULL
/// embedding (graceful degradation, mirroring the entity auto-linker
/// below). A NULL-embedding row is simply skipped by `semantic_search`
/// (`WHERE embedding IS NOT NULL`); it stays retrievable via the lexical
/// and graph lanes.
```

- [ ] **Step 4: Update the in-crate callers + signature-pin test**

In `core/src/cli_audit.rs`, `l1_add_and_audit` — construct a NoOp embedder and pass it. Add to the `use` line in that function:

```rust
    use crate::memory::embedder::NoOpEmbedder;
```

and change the call:

```rust
    let outcome = promote_l1(pool, extractor, &NoOpEmbedder::new(), &trimmed, source.clone()).await?;
```

In `core/src/scheduler/runner.rs`, `write_l1_promoted_row` (~line 376) — construct a TEMPORARY local NoOp embedder (replaced by the real one in Task 3) so the workspace compiles and the agent path stays non-embedding for now. Change the call (~line 380):

```rust
    // TEMPORARY (Task 3 threads the real RouterEmbedder here): operator-
    // equivalent NoOp keeps the agent path compiling + non-embedding.
    let embedder = crate::memory::embedder::NoOpEmbedder::new();
    let outcome = match promote_l1(pool, extractor, &embedder, insight, source.clone()).await {
```

In `core/src/memory/l1_promote.rs`, update the `promote_l1_signature_compile_pin` test (~line 443) to the new signature:

```rust
        fn _signature_pin<'a>(
            pool: &'a sqlx::PgPool,
            extractor: &'a dyn crate::entity_extraction::EntityExtractor,
            embedder: &'a dyn crate::memory::embedder::Embedder,
            body: &'a str,
            source: L1Source,
        ) -> impl std::future::Future<Output = Result<L1WriteOutcome, L1Error>> + 'a {
            promote_l1(pool, extractor, embedder, body, source)
        }
```

- [ ] **Step 5: Run the full affected suite to verify it passes**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --test memory_l1_promote_e2e 2>&1 | tail -30`
Expected: PASS — the 3 new scenarios pass (or print `[SKIP]` lines if no PG; on this Mac PG is configured, so they run); existing scenarios still pass.

Also run the lib unit tests touched:

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib memory::l1_promote 2>&1 | tail -15`
Expected: PASS including `promote_l1_signature_compile_pin`.

- [ ] **Step 6: Verify the whole workspace compiles + clippy clean**

Run: `source "$HOME/.cargo/env" && cargo build --workspace 2>&1 | tail -5 && cargo clippy --workspace --all-targets 2>&1 | tail -15`
Expected: builds clean (only the pre-existing `sqlx-postgres` future-incompat warning); no new clippy warnings.

- [ ] **Step 7: Commit**

```bash
git add core/src/memory/l1_promote.rs core/src/cli_audit.rs core/src/scheduler/runner.rs core/tests/memory_l1_promote_e2e.rs
git commit -m "feat(memory): promote_l1 embeds L1 rows lazily via Embedder seam (#323)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Thread the real `RouterEmbedder` into the agent-raised path

**Files:**
- Modify: `core/src/scheduler/runner.rs` (`spawn_scheduler`, `lane_loop`, `drain_lane`, `write_l1_promoted_row`, the signature-pin test ~line 820)
- Modify: `core/src/main.rs:325` (build `RouterEmbedder`, pass into `spawn_scheduler`)

**Interfaces:**
- Consumes: `RouterEmbedder` from Task 1; the `promote_l1(…, &dyn Embedder, …)` signature from Task 2.
- Produces: `spawn_scheduler(pool, formulator, review, dispatcher, entity_extractor, embedder: Arc<dyn Embedder>) -> SchedulerHandle` — the daemon builds the real embedder and the agent-raised L1 path now stores embeddings.

- [ ] **Step 1: Thread `Arc<dyn Embedder>` through the scheduler**

In `core/src/scheduler/runner.rs`, add the import (with the other `use crate::...` lines near the top):

```rust
use crate::memory::embedder::Embedder;
```

Add `embedder: Arc<dyn Embedder>` as a parameter to `spawn_scheduler`, `lane_loop`, and `drain_lane`, threading it exactly like `entity_extractor` (clone into both lane spawns; clone into the initial drain and the loop drain). In `spawn_scheduler`:

```rust
pub fn spawn_scheduler(
    pool: PgPool,
    formulator: Arc<dyn PlanFormulator>,
    review: Arc<ChainReviewStage>,
    dispatcher: Arc<dyn StepDispatcher>,
    entity_extractor: Arc<dyn EntityExtractor>,
    embedder: Arc<dyn Embedder>,
) -> SchedulerHandle {
    let (tx, rx) = watch::channel(false);

    let fast = tokio::spawn(lane_loop(
        pool.clone(), formulator.clone(), review.clone(), dispatcher.clone(),
        entity_extractor.clone(), embedder.clone(),
        Lane::Fast, DEFAULT_DEADLINE_FAST_S, DEFAULT_MAX_PLANS_FAST, rx.clone(),
    ));
    let long = tokio::spawn(lane_loop(
        pool, formulator, review, dispatcher,
        entity_extractor, embedder,
        Lane::Long, DEFAULT_DEADLINE_LONG_S, DEFAULT_MAX_PLANS_LONG, rx,
    ));

    SchedulerHandle { shutdown: tx, fast, long }
}
```

Add `embedder: Arc<dyn Embedder>` to `lane_loop`'s parameter list (after `entity_extractor`) and pass `embedder.clone()` into both the initial `drain_lane` call and the loop `drain_lane` call. Add `embedder: Arc<dyn Embedder>` to `drain_lane`'s parameter list (after `entity_extractor`).

- [ ] **Step 2: Replace the temporary NoOp in `write_l1_promoted_row`**

Change `write_l1_promoted_row`'s signature to accept the embedder and drop the Task-2 temporary local NoOp:

```rust
async fn write_l1_promoted_row(
    pool: &PgPool,
    extractor: &dyn EntityExtractor,
    embedder: &dyn Embedder,
    task_id: i64,
    insight: &str,
) {
    use crate::memory::l1_promote::{promote_l1, L1Error, L1Source};

    let source = L1Source::AgentRaised { task_id };
    let outcome = match promote_l1(pool, extractor, embedder, insight, source.clone()).await {
```

In `drain_lane`, update the call site (~line 294) to pass the embedder:

```rust
            write_l1_promoted_row(pool, &*entity_extractor, &*embedder, claimed.id, insight).await;
```

- [ ] **Step 3: Update the `write_l1_promoted_row` signature-pin test**

In `core/src/scheduler/runner.rs` (~line 820), update the pin to the new signature:

```rust
        fn _signature_pin<'a>(
            pool: &'a sqlx::PgPool,
            extractor: &'a crate::entity_extraction::NoOpEntityExtractor,
            embedder: &'a crate::memory::embedder::NoOpEmbedder,
            task_id: i64,
            insight: &'a str,
        ) -> impl std::future::Future<Output = ()> + 'a {
            super::write_l1_promoted_row(pool, extractor, embedder, task_id, insight)
        }
```

- [ ] **Step 4: Build the real `RouterEmbedder` in `main.rs`**

In `core/src/main.rs`, at the `spawn_scheduler` call (~line 325), construct and pass the embedder. `pool` and `router` are both already in scope:

```rust
    let embedder: std::sync::Arc<dyn kastellan_core::memory::Embedder> =
        std::sync::Arc::new(kastellan_core::memory::RouterEmbedder::new(
            pool.clone(),
            router.clone(),
        ));
    let scheduler = kastellan_core::scheduler::spawn_scheduler(
        pool.clone(),
        formulator,
        review,
        dispatcher,
        entity_extractor.clone(),
        embedder,
    );
```

- [ ] **Step 5: Verify the workspace compiles, tests pass, clippy clean**

Run: `source "$HOME/.cargo/env" && cargo build --workspace 2>&1 | tail -5`
Expected: builds clean (only the pre-existing `sqlx-postgres` warning).

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib scheduler::runner 2>&1 | tail -15`
Expected: PASS including `write_l1_promoted_row_signature_compile_pin`.

Run: `source "$HOME/.cargo/env" && cargo clippy --workspace --all-targets -D warnings 2>&1 | tail -15`
Expected: clean (`-D warnings` passes).

- [ ] **Step 6: Commit**

```bash
git add core/src/scheduler/runner.rs core/src/main.rs
git commit -m "feat(memory): wire RouterEmbedder into the agent-raised L1 path (#323)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Full verification + docs

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md` (record the shipped work + the deferred-backfill follow-up)
- Modify: `docs/devel/ROADMAP.md` (mark #323 forward-write-path done; note backfill follow-up)

- [ ] **Step 1: Full affected-suite verification**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --test memory_l1_promote_e2e --test memory_recall_e2e --test embedding_recall_e2e 2>&1 | tail -30`
Expected: PASS (or `[SKIP]` lines where PG isn't configured; on this Mac they run). Capture the pass/skip counts for the HANDOVER.

Run: `source "$HOME/.cargo/env" && cargo clippy --workspace --all-targets -D warnings 2>&1 | tail -5`
Expected: clean.

- [ ] **Step 2: Open the backfill follow-up issue**

```bash
gh issue create --title "Backfill: (re)embed existing NULL-embedding L1 rows (kastellan-cli memory l1 reembed)" \
  --body "Follow-up from #323. The forward write path now embeds L1 rows on insert (agent-raised path), but existing NULL-embedding rows and operator-added rows are not embedded. Add a \`kastellan-cli memory l1 reembed\` subcommand that scans \`layer=1 AND embedding IS NULL\` rows and (re)embeds them through the same RouterEmbedder/embed_query chokepoint. Closes #323 item 2."
```

- [ ] **Step 3: Update HANDOVER + ROADMAP**

Update `docs/devel/handovers/HANDOVER.md`: new "Last updated" header summarizing the L1 embedding-population work (files, the Embedder seam, lazy-after-dedup, degrade-and-warn, agent-embeds/operator-NoOp, deferred backfill issue #), with the verification counts from Step 1. Update `docs/devel/ROADMAP.md`: mark #323's forward write path done; reference the backfill follow-up issue.

- [ ] **Step 4: Commit**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs: record L1 embedding population + backfill follow-up (#323)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

- [ ] **Step 5: Push + open PR**

```bash
git push -u origin feat/l1-embedding-population
gh pr create --base main --title "feat(memory): populate L1 embeddings via Embedder seam (#323)" \
  --body "$(cat <<'BODY'
Wires L1 insight rows to be embedded on insert so the semantic recall lane stops returning 0 rows.

## What
- New `Embedder` seam (`core/src/memory/embedder.rs`) mirroring the `EntityExtractor` seam: `RouterEmbedder` (agent path, delegates to `embed_query` → `truncate_to_embedding_dim` + audit row) and `NoOpEmbedder` (operator path).
- `promote_l1` gains a `&dyn Embedder`, called **lazily after the dedup check** (duplicates never embed); embed failure → NULL embedding + WARN (write never blocked).
- Real `RouterEmbedder` threaded from `main.rs` through `spawn_scheduler` to the agent-raised L1 path; operator CLI stays NoOp.

## Tests
- Unit: NoOp returns None, object-safety pin, RouterEmbedder degrade-to-None on transport error.
- PG-e2e: embed-on-insert (+ `semantic_search` finds it), lazy-on-dedup-skip, degrade-and-warn (NULL embedding, lexical still finds it).

## Deferred
Backfill / `kastellan-cli memory l1 reembed` of existing NULL-embedding + operator rows → tracked follow-up (closes #323 item 2 later).

Closes #323 (forward write path).

🤖 Generated with [Claude Code](https://claude.com/claude-code)
BODY
)"
```

---

## Self-Review

**Spec coverage:**
- Decision 1 (L1 only) → Tasks 2–3 (only `promote_l1` / its agent path embed). ✔
- Decision 2 (Embedder seam, lazy, degrade-and-warn) → Task 1 (seam) + Task 2 (lazy-after-dedup, degrade test). ✔
- Decision 3 (agent embeds, operator NoOp, backfill deferred) → Task 3 (RouterEmbedder agent path), Task 2 (operator NoOp), Task 4 Step 2 (deferred issue). ✔
- "Route through `truncate_to_embedding_dim`" → satisfied via `RouterEmbedder` → `embed_query` (which truncates). ✔
- No `db`-crate change → confirmed; only `core` files touched. ✔
- All spec "Files touched" rows map to a task. ✔

**Placeholder scan:** No TBD/TODO/"handle edge cases"/vague steps; every code step shows the code. ✔

**Type consistency:** `embed_for_storage(&self, text: &str) -> Option<Vec<f32>>` used identically across embedder.rs, the FakeEmbedder, and promote_l1. `promote_l1(pool, extractor, embedder, body, source)` arg order consistent across Task 2 (definition), the 4 e2e call sites, cli_audit, runner, and both signature-pin tests. `spawn_scheduler` embedder param order (after `entity_extractor`) consistent between runner.rs and main.rs. ✔
