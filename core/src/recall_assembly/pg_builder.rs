//! Production + test implementations of [`super::RecallBuilder`].
//!
//! * [`PgRecallBuilder`] — composes [`crate::memory::embed_query`] +
//!   [`crate::memory::recall`] against a [`sqlx::PgPool`] and a shared
//!   [`hhagent_llm_router::Router`].
//! * [`StaticRecallBuilder`] — returns a fixed [`super::RecalledContext`]
//!   regardless of the query string. Always `pub` (not `cfg(test)`)
//!   so cross-crate integration tests in `core/tests/*.rs` can use it.

use async_trait::async_trait;
use hhagent_db::memories::Memory;

use super::{sha256_hex, RecallBuilder, RecalledContext, RecallError};

/// Greedy newest-first cap: walk `rows` in order, push as long as
/// cumulative body bytes stay ≤ `cap_bytes`. The first row that
/// would push cumulative bytes over the cap is dropped (with a
/// `tracing::warn!`) and the walk stops — matches the L1 loader's
/// `saturating_add` break idiom in `core::memory::layers::load_l1`.
///
/// Pure helper, no I/O. Doesn't drop later rows that might
/// individually fit — that would risk reorder vs. the RRF-fused
/// order coming out of `recall`. Operators see the dropped id in
/// logs and can either retire the oversized memory or raise the cap.
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
            tracing::warn!(
                target: "hhagent::recall_assembly",
                memory_id = row.id,
                row_bytes = row.body.len(),
                used_bytes = used,
                cap_bytes,
                "recall row exceeds cap; dropping this and any remaining recall rows",
            );
            break;
        }
        used = next;
        ids.push(row.id);
        bodies.push(row.body);
    }

    (ids, bodies)
}

/// Production builder. Body lands in Task 5; the constructor + struct
/// are declared here so the trait impl compiles.
pub struct PgRecallBuilder {
    // Fields land in Task 5 with the body. Keep the struct private
    // to-be-revealed; only `new` is public surface today.
    _placeholder: (),
}

impl PgRecallBuilder {
    /// **Task 5 will replace this** with a real constructor taking
    /// `(PgPool, Arc<Router>)`. Stubbed today so module shape compiles.
    pub fn new() -> Self {
        Self { _placeholder: () }
    }
}

impl Default for PgRecallBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RecallBuilder for PgRecallBuilder {
    async fn build(&self, _query: &str) -> Result<RecalledContext, RecallError> {
        // Task 5 replaces this body. Today: empty context so the
        // module compiles and degrade-and-warn callers behave sanely
        // if the stub is reached (it should not be — `main.rs` wires
        // the real impl in Task 8, which lands together with the
        // Task 5 body).
        Ok(RecalledContext::empty())
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
    /// caller doesn't have to hand-hash. Panics if `ids.len() != bodies.len()`.
    pub fn with(ids: Vec<i64>, bodies: Vec<String>, query: &str) -> Self {
        assert_eq!(
            ids.len(),
            bodies.len(),
            "StaticRecallBuilder::with: ids.len() must equal bodies.len()",
        );
        Self {
            fixed: RecalledContext {
                ids,
                bodies,
                query_sha256: sha256_hex(query.as_bytes()),
            },
        }
    }
}

#[async_trait]
impl RecallBuilder for StaticRecallBuilder {
    async fn build(&self, _query: &str) -> Result<RecalledContext, RecallError> {
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
