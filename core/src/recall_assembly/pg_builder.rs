//! Production + test implementations of [`super::RecallBuilder`].
//!
//! * [`PgRecallBuilder`] — composes [`crate::memory::embed_query`] +
//!   [`crate::memory::recall`] against a [`sqlx::PgPool`] and a shared
//!   [`hhagent_llm_router::Router`].
//! * [`StaticRecallBuilder`] — returns a fixed [`super::RecalledContext`]
//!   regardless of the query string. Always `pub` (not `cfg(test)`)
//!   so cross-crate integration tests in `core/tests/*.rs` can use it.

use async_trait::async_trait;

use super::{sha256_hex, RecallBuilder, RecalledContext, RecallError};

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
}
