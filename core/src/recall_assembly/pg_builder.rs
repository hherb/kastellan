//! Production + test implementations of [`super::RecallBuilder`].
//!
//! * [`PgRecallBuilder`] — composes [`crate::memory::embed_query`] +
//!   [`crate::memory::recall`] against a [`sqlx::PgPool`] and a shared
//!   [`hhagent_llm_router::Router`].
//! * [`StaticRecallBuilder`] — returns a fixed [`super::RecalledContext`]
//!   regardless of the query string. Always `pub` (not `cfg(test)`)
//!   so cross-crate integration tests in `core/tests/*.rs` can use it.

use std::sync::Arc;

use async_trait::async_trait;
use hhagent_db::memories::Memory;
use sqlx::PgPool;
use hhagent_llm_router::Router;

use crate::memory::{embed_query, recall, RecallParams};
use super::{sha256_hex, RecallBuilder, RecalledContext, RecallError, L_RECALL_CAP_BYTES};

/// Greedy newest-first cap: walk `rows` in order, push as long as
/// cumulative body bytes stay ≤ `cap_bytes`. The first row that would
/// push cumulative bytes over the cap is dropped and the walk stops —
/// matches the L1 loader's `saturating_add` break idiom in
/// `core::memory::layers::load_l1`.
///
/// Logging follows the L1 precedent's warn-vs-debug split:
/// - A single row whose body alone exceeds the cap emits
///   `tracing::warn!` (operator signal: retire the memory or raise
///   the cap).
/// - Rows dropped because the cap is already full after N kept rows
///   emit `tracing::debug!` (normal exit, not an operator problem).
///
/// Pure helper, no I/O. Doesn't drop later rows that might
/// individually fit — that would risk reorder vs. the RRF-fused
/// order coming out of `recall`.
///
/// The `cap_bytes` parameter is expected to be [`L_RECALL_CAP_BYTES`]
/// in production; tests may pass a smaller value to exercise the cap
/// path without constructing kilobyte-sized bodies.
pub(crate) fn cap_and_split(rows: Vec<Memory>, cap_bytes: usize) -> (Vec<i64>, Vec<String>) {
    let mut ids = Vec::with_capacity(rows.len());
    let mut bodies = Vec::with_capacity(rows.len());
    let mut used: usize = 0;

    for row in rows {
        let next = used.saturating_add(row.body.len());
        if next > cap_bytes {
            // Mirror load_l1's warn-vs-debug split: warn loudly only when
            // a single row alone is over budget (operator can retire it
            // or raise the cap); stay quiet on normal "cap filled after
            // N rows" exits (that's the expected end of the loop, not
            // an operator signal).
            //
            // `ids.is_empty()` is sufficient here: it implies `used == 0`
            // (we only push to both vectors after incrementing `used`),
            // which means `next == row.body.len()`, so the only way
            // `next > cap_bytes` with no rows kept is `row.body.len() >
            // cap_bytes`. The earlier `&& row.body.len() > cap_bytes`
            // guard was redundant.
            if ids.is_empty() {
                tracing::warn!(
                    target: "hhagent::recall_assembly",
                    memory_id = row.id,
                    row_bytes = row.body.len(),
                    cap_bytes,
                    "recall row body alone exceeds cap; dropping this and any remaining recall rows",
                );
            } else {
                tracing::debug!(
                    target: "hhagent::recall_assembly",
                    memory_id = row.id,
                    row_bytes = row.body.len(),
                    used_bytes = used,
                    cap_bytes,
                    "recall cap full; stopping",
                );
            }
            break;
        }
        used = next;
        ids.push(row.id);
        bodies.push(row.body);
    }

    (ids, bodies)
}

/// Production builder. Composes [`embed_query`] + [`recall`] over a
/// shared [`PgPool`] and [`Router`]; caps the rendered bodies via
/// [`cap_and_split`].
///
/// Holds `PgPool` by value (cheap to clone via sqlx's internal `Arc`
/// — matches the [`crate::prompt_assembly::PgSystemPromptBuilder`]
/// convention) and `Router` behind an `Arc` (the same `Arc<Router>`
/// already constructed in `main.rs`).
pub struct PgRecallBuilder {
    pool: PgPool,
    router: Arc<Router>,
}

impl PgRecallBuilder {
    /// Construct a builder pinned to the supplied pool and router.
    pub fn new(pool: PgPool, router: Arc<Router>) -> Self {
        Self { pool, router }
    }
}

#[async_trait]
impl RecallBuilder for PgRecallBuilder {
    async fn build_with_seeds(
        &self,
        query: &str,
        seeds: &[i64],
    ) -> Result<RecalledContext, RecallError> {
        let query_sha256 = sha256_hex(query.as_bytes());

        // Step 1 — turn the query text into an embedding (writes the
        // actor='llm:router' action='embed' audit row internally).
        let emb = embed_query(&self.pool, &self.router, query).await?;

        // Step 2 — fan out lanes. Seeded vs. semantic+lexical-only:
        // choose params shape. RecallParams::new defaults to
        // SEMANTIC_AND_LEXICAL; with_seeds defaults to ALL
        // (semantic+lexical+graph). Both correct for their respective
        // seed-presence cases — no override needed.
        let params = if seeds.is_empty() {
            RecallParams::new(query, &emb)
        } else {
            RecallParams::with_seeds(query, &emb, seeds)
        };
        let rows = recall(&self.pool, &params).await?;

        // Step 3 — byte-cap into the final RecalledContext. cap_and_split
        // builds ids/bodies side-by-side so the new() invariant holds.
        let (ids, bodies) = cap_and_split(rows, L_RECALL_CAP_BYTES);
        Ok(RecalledContext::new(ids, bodies, query_sha256))
    }
}

/// Test-only fixed-context builder.
pub struct StaticRecallBuilder {
    fixed: RecalledContext,
}

impl StaticRecallBuilder {
    /// Empty-context builder. Most tests use this — recall is "off"
    /// and the assembled prompt has no `<recalled>` block.
    pub fn empty() -> Self {
        Self {
            fixed: RecalledContext::empty(),
        }
    }

    /// Construct with an explicit (ids, bodies, query) triple. The
    /// `query_sha256` field is computed automatically so the test
    /// caller doesn't have to hand-hash. Panics if `ids.len() != bodies.len()`
    /// (delegated to [`RecalledContext::new`]).
    pub fn with(ids: Vec<i64>, bodies: Vec<String>, query: &str) -> Self {
        Self {
            fixed: RecalledContext::new(ids, bodies, sha256_hex(query.as_bytes())),
        }
    }
}

#[async_trait]
impl RecallBuilder for StaticRecallBuilder {
    async fn build_with_seeds(
        &self,
        _query: &str,
        _seeds: &[i64],
    ) -> Result<RecalledContext, RecallError> {
        Ok(self.fixed.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hhagent_db::memories::{Memory, MemoryLayer};
    use time::OffsetDateTime;

    fn mem(id: i64, body: &str) -> Memory {
        Memory {
            id,
            body: body.to_string(),
            metadata: serde_json::json!({}),
            layer: MemoryLayer::Stable,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn cap_and_split_empty_input_returns_empty_vectors() {
        let (ids, bodies) = super::cap_and_split(vec![], 4096);
        assert!(ids.is_empty());
        assert!(bodies.is_empty());
    }

    #[test]
    fn cap_and_split_below_cap_keeps_all_rows() {
        let rows = vec![mem(1, "aaa"), mem(2, "bb"), mem(3, "c")];
        let (ids, bodies) = super::cap_and_split(rows, 100);
        assert_eq!(ids, vec![1, 2, 3]);
        assert_eq!(bodies, vec!["aaa", "bb", "c"]);
    }

    #[test]
    fn cap_and_split_drops_oversize_first_row_returns_empty() {
        // Single row 10 bytes, cap 5 bytes → row is dropped entirely.
        let rows = vec![mem(7, "0123456789")];
        let (ids, bodies) = super::cap_and_split(rows, 5);
        assert!(ids.is_empty(), "oversize-first-row must be dropped");
        assert!(bodies.is_empty());
    }

    #[test]
    fn cap_and_split_stops_at_cap_keeping_rows_that_fit() {
        // Row 1 = 4 bytes, row 2 = 4 bytes, cap = 5. Only row 1 fits
        // (after row 1: 4 used, room for 1 byte; row 2 needs 4 more
        // and would exceed cap → dropped). Row 3 would individually
        // fit but the function stops at the first dropped row to
        // preserve RRF-fused order.
        let rows = vec![mem(1, "aaaa"), mem(2, "bbbb"), mem(3, "c")];
        let (ids, bodies) = super::cap_and_split(rows, 5);
        assert_eq!(ids, vec![1], "only the first row fits under the cap");
        assert_eq!(bodies, vec!["aaaa"]);
    }

    #[test]
    fn cap_and_split_exact_cap_keeps_all_rows() {
        // 2 rows of 2 bytes each, total 4 bytes, cap 4 bytes. After
        // row 1: next = 2, not > 4 → keep. After row 2: next = 4,
        // not > 4 → keep. Boundary pin: `>` not `>=` means rows that
        // fill cap_bytes exactly still fit. Mirrors load_l1's
        // cap_bytes inclusivity contract.
        let rows = vec![mem(1, "ab"), mem(2, "cd")];
        let (ids, bodies) = super::cap_and_split(rows, 4);
        assert_eq!(ids, vec![1, 2], "exact-cap fill must keep both rows");
        assert_eq!(bodies, vec!["ab", "cd"]);
    }

    #[tokio::test]
    async fn static_builder_empty_returns_empty_context() {
        let b = StaticRecallBuilder::empty();
        let c = b.build("anything").await.expect("static build never fails");
        assert!(c.is_empty());
        assert_eq!(c.query_sha256.len(), 64);
    }

    #[tokio::test]
    async fn static_builder_with_returns_fixed_context_ignoring_query_arg() {
        let b = StaticRecallBuilder::with(
            vec![1, 2, 3],
            vec!["a".into(), "b".into(), "c".into()],
            "operator query text",
        );
        let c1 = b.build("ignored").await.expect("static build never fails");
        let c2 = b.build("also ignored").await.expect("static build never fails");
        assert_eq!(c1.ids, vec![1, 2, 3]);
        assert_eq!(c1.bodies, vec!["a", "b", "c"]);
        assert_eq!(c2.ids, vec![1, 2, 3], "second call must return identical context");
        // SHA-256 of "operator query text" — locked so a future
        // refactor changing the hash input (e.g. trimming the query)
        // trips this test immediately.
        let mut h = sha2::Sha256::new();
        use sha2::Digest;
        h.update(b"operator query text");
        let expected = format!("{:x}", h.finalize());
        assert_eq!(c1.query_sha256, expected);
    }

    #[test]
    #[should_panic(expected = "ids.len() must equal bodies.len()")]
    fn static_builder_with_panics_on_length_mismatch() {
        let _ = StaticRecallBuilder::with(vec![1, 2], vec!["only one".into()], "q");
    }

    #[tokio::test]
    async fn static_builder_with_empty_vectors_uses_real_query_hash_not_empty_sentinel() {
        // The valid empty-rows-but-real-query case: a recall that
        // returned zero memories for a non-empty query is wire-distinct
        // from a `StaticRecallBuilder::empty()` (or a degraded recall
        // with no query embedded yet). is_empty() returns true for
        // both, but query_sha256 differs — and the audit row carries
        // the distinction. Pinning this prevents a future refactor
        // from collapsing the two cases.
        let b = StaticRecallBuilder::with(vec![], vec![], "q");
        let c = b.build("ignored").await.expect("static build never fails");
        assert!(c.is_empty(), "ids and bodies are empty by construction");
        assert!(c.ids.is_empty());
        assert!(c.bodies.is_empty());
        // Critical: query_sha256 must reflect the supplied query "q",
        // NOT the canonical empty-string sentinel.
        assert_ne!(
            c.query_sha256,
            super::super::RecalledContext::empty().query_sha256,
            "with(vec![], vec![], \"q\") must NOT produce the canonical empty-string sentinel",
        );
        assert_eq!(c.query_sha256, sha256_hex(b"q"),
                   "query_sha256 must equal sha256_hex(b\"q\")");
    }
}
