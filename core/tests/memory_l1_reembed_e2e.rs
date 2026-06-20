//! End-to-end DB integration coverage for the L1 embedding **backfill** —
//! [`kastellan_core::memory::l1_reembed::reembed_l1_null`] (issue #325).
//!
//! The forward write path (#324) embeds agent-raised L1 insights on insert,
//! but pre-existing rows and operator-added rows (`memory l1 add` uses a
//! `NoOpEmbedder`) still have `embedding IS NULL` and so are invisible to the
//! semantic recall lane. The backfill scans those rows and (re)embeds each
//! through the same `Embedder` chokepoint. These scenarios prove:
//!
//!   1. backfill populates NULL-embedding L1 rows and `semantic_search` then
//!      finds them;
//!   2. it is idempotent — a re-run embeds nothing (no double-embed);
//!   3. it degrades-and-warns per row — an embed failure (`None`) skips that
//!      row, leaving it NULL, rather than failing the batch;
//!   4. a mixed batch splits exactly — one row embeds while another fails,
//!      yielding `embedded = 1, skipped = 1`.
//!
//! Each scenario brings up its own per-test Postgres cluster. Skips silently
//! with `[SKIP]` lines on hosts without Postgres or a reachable supervisor;
//! `cargo test -- --nocapture` to see skip lines.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use kastellan_core::memory::l1_reembed::{reembed_l1_null, ReembedReport};
use kastellan_db::memories::{
    insert_memory_at_layer, load_unembedded_at_layer, semantic_search, EMBEDDING_DIM, MemoryLayer,
};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use async_trait::async_trait;

use kastellan_core::memory::embedder::Embedder;

/// Test embedder: counts calls and returns a fixed unit vector (or `None`).
/// `None` models the embed-failure degrade path.
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

/// Test embedder driven by a fixed call-ordered sequence of outcomes: the
/// `n`-th `embed_for_storage` returns `outs[n]` (and `None` past the end).
/// Lets a single batch exercise a **mix** of embed success (`Some`) and
/// failure (`None`) to pin the `embedded`/`skipped` split. The scan orders by
/// `id`, so insertion order is call order.
struct SequencedEmbedder {
    calls: AtomicUsize,
    outs: Vec<Option<Vec<f32>>>,
}

impl SequencedEmbedder {
    fn new(outs: Vec<Option<Vec<f32>>>) -> Self {
        Self { calls: AtomicUsize::new(0), outs }
    }
    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Embedder for SequencedEmbedder {
    async fn embed_for_storage(&self, _text: &str) -> Option<Vec<f32>> {
        let i = self.calls.fetch_add(1, Ordering::SeqCst);
        self.outs.get(i).cloned().flatten()
    }
}

/// A deterministic `EMBEDDING_DIM`-length unit vector: 1.0 in slot 0, else 0.
fn unit_vec_e0() -> Vec<f32> {
    let mut v = vec![0.0f32; EMBEDDING_DIM];
    v[0] = 1.0;
    v
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime")
}

/// Insert an L1 row with a NULL embedding and return its id. Mirrors a
/// pre-#324 / operator-added row: at `layer = 1`, no embedding.
async fn insert_null_l1(pool: &sqlx::PgPool, body: &str) -> i64 {
    insert_memory_at_layer(pool, body, &serde_json::json!({}), None, MemoryLayer::Index)
        .await
        .expect("insert NULL-embedding L1 row")
}

// ---------------------------------------------------------------------------
// Scenario 1 — backfill populates NULL L1 rows + semantic_search finds them
// ---------------------------------------------------------------------------

#[test]
fn reembed_populates_null_l1_rows_and_semantic_search_finds_them() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l1re-d",
        "l1re-l",
        &format!("kastellan-supervisor-test-pg-l1re1-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l1-reembed-1"}),
        )
        .await
        .expect("probe");

        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let id_a = insert_null_l1(&pool, "first NULL insight").await;
        let id_b = insert_null_l1(&pool, "second NULL insight").await;

        // Pre-condition: both rows are invisible to the semantic lane.
        let before = semantic_search(&pool, &unit_vec_e0(), 10).await.expect("semantic before");
        assert!(before.is_empty(), "NULL-embedding rows must not surface in semantic search");

        let embedder = FakeEmbedder::returning(Some(unit_vec_e0()));
        let report = reembed_l1_null(&pool, &embedder).await.expect("reembed");

        assert_eq!(
            report,
            ReembedReport { scanned: 2, embedded: 2, skipped: 0 },
            "both NULL L1 rows scanned + embedded, none skipped"
        );
        assert_eq!(embedder.call_count(), 2, "embedder called once per NULL row");

        // The rows now surface in the semantic lane.
        let after = semantic_search(&pool, &unit_vec_e0(), 10).await.expect("semantic after");
        assert!(after.contains(&id_a), "row A now found by semantic search");
        assert!(after.contains(&id_b), "row B now found by semantic search");

        // Nothing left unembedded at L1.
        let remaining = load_unembedded_at_layer(&pool, MemoryLayer::Index)
            .await
            .expect("load_unembedded after");
        assert!(remaining.is_empty(), "no NULL-embedding L1 rows remain");

        pool.close().await;
    });
}

// ---------------------------------------------------------------------------
// Scenario 2 — backfill is idempotent: a re-run embeds nothing
// ---------------------------------------------------------------------------

#[test]
fn reembed_is_idempotent_on_rerun() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l1re-d",
        "l1re-l",
        &format!("kastellan-supervisor-test-pg-l1re2-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l1-reembed-2"}),
        )
        .await
        .expect("probe");

        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        insert_null_l1(&pool, "an insight to embed once").await;

        let first = FakeEmbedder::returning(Some(unit_vec_e0()));
        let r1 = reembed_l1_null(&pool, &first).await.expect("reembed 1");
        assert_eq!(r1, ReembedReport { scanned: 1, embedded: 1, skipped: 0 });

        // Second run: the row is no longer NULL, so it is not even scanned —
        // the embedder is never called (no double-embed).
        let second = FakeEmbedder::returning(Some(unit_vec_e0()));
        let r2 = reembed_l1_null(&pool, &second).await.expect("reembed 2");
        assert_eq!(
            r2,
            ReembedReport { scanned: 0, embedded: 0, skipped: 0 },
            "re-run finds no NULL rows"
        );
        assert_eq!(second.call_count(), 0, "embedder not called on an idempotent re-run");

        pool.close().await;
    });
}

// ---------------------------------------------------------------------------
// Scenario 3 — degrade-and-warn: an embed failure skips the row, keeps NULL
// ---------------------------------------------------------------------------

#[test]
fn reembed_degrades_and_warns_leaving_row_null() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l1re-d",
        "l1re-l",
        &format!("kastellan-supervisor-test-pg-l1re3-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l1-reembed-3"}),
        )
        .await
        .expect("probe");

        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let id = insert_null_l1(&pool, "an insight whose embed fails").await;

        // Embedder always returns None (transient failure / unreachable
        // endpoint). The row is scanned but skipped, not embedded; the batch
        // does not error.
        let embedder = FakeEmbedder::returning(None);
        let report = reembed_l1_null(&pool, &embedder).await.expect("reembed degrades, not errors");

        assert_eq!(
            report,
            ReembedReport { scanned: 1, embedded: 0, skipped: 1 },
            "a failing embed skips the row rather than failing the batch"
        );
        assert_eq!(embedder.call_count(), 1, "embedder attempted exactly once");

        // The row is still NULL — it remains in the unembedded scan.
        let remaining = load_unembedded_at_layer(&pool, MemoryLayer::Index)
            .await
            .expect("load_unembedded after");
        assert_eq!(remaining.len(), 1, "the skipped row stays NULL-embedding");
        assert_eq!(remaining[0].0, id, "the same row is still unembedded");

        pool.close().await;
    });
}

// ---------------------------------------------------------------------------
// Scenario 4 — mixed batch: one row embeds, one fails; the split is exact
// ---------------------------------------------------------------------------

#[test]
fn reembed_mixed_batch_embeds_one_skips_the_other() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l1re-d",
        "l1re-l",
        &format!("kastellan-supervisor-test-pg-l1re4-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l1-reembed-4"}),
        )
        .await
        .expect("probe");

        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // Two NULL rows; the scan visits them in id (= insertion) order.
        let id_ok = insert_null_l1(&pool, "this insight embeds").await;
        let id_fail = insert_null_l1(&pool, "this insight's embed fails").await;

        // First call succeeds, second fails — a genuinely mixed batch.
        let embedder = SequencedEmbedder::new(vec![Some(unit_vec_e0()), None]);
        let report = reembed_l1_null(&pool, &embedder).await.expect("reembed");

        assert_eq!(
            report,
            ReembedReport { scanned: 2, embedded: 1, skipped: 1 },
            "exactly one row embedded, the other skipped"
        );
        assert_eq!(embedder.call_count(), 2, "embedder attempted once per scanned row");

        // The embedded row surfaces in the semantic lane; the failed one does
        // not — and remains the sole NULL-embedding row.
        let after = semantic_search(&pool, &unit_vec_e0(), 10).await.expect("semantic after");
        assert!(after.contains(&id_ok), "embedded row is now found by semantic search");
        assert!(!after.contains(&id_fail), "the skipped row stays out of the semantic lane");

        let remaining = load_unembedded_at_layer(&pool, MemoryLayer::Index)
            .await
            .expect("load_unembedded after");
        assert_eq!(remaining.len(), 1, "only the failed row remains NULL-embedding");
        assert_eq!(remaining[0].0, id_fail, "and it is the row whose embed failed");

        pool.close().await;
    });
}
