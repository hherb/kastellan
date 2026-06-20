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
        // Port 1 is unbound; the embed call fails at transport.
        let cfg = kastellan_llm_router::RouterConfig {
            embedding_url: "http://127.0.0.1:1/v1/embeddings".to_string(),
            ..Default::default()
        };
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
