//! `relation_kinds` table: which relation labels (kinds) the GLiNER
//! relation-extraction pass is allowed to detect. Seeded by migration
//! `0017`. Operator extends via direct `INSERT INTO relation_kinds`;
//! the extractor never widens the vocabulary on its own.
//!
//! `RelationKindsCache` holds the result of `SELECT kind FROM
//! relation_kinds` for 60 seconds before a re-fetch. The cadence
//! mirrors `entity_kinds::KindsCache`:
//!
//!   * short enough that operator INSERTs propagate to the running
//!     daemon without explicit invalidation,
//!   * long enough that the hot path (every `formulate_plan` call)
//!     does not re-issue the query.
//!
//! Pure mirror of [`crate::entity_kinds`] — same struct shape, same
//! constants, same locking pattern. Kept as a sibling module rather
//! than a generic over a single type because the call sites read
//! `entity_kinds` and `relation_kinds` for different purposes (entity
//! labels vs relation labels) and the read-time invariants differ
//! (entities have a `quarantine` flag and a `name_norm` dedup column;
//! relation_kinds has neither). Sharing code by parameterising the
//! table name would obscure that asymmetry.

use crate::DbError;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Cache TTL — 60 seconds. Identical to [`crate::entity_kinds::KINDS_CACHE_TTL`]
/// by design; the two caches are read on the same hot path.
pub const RELATION_KINDS_CACHE_TTL: Duration = Duration::from_secs(60);

/// One snapshot of the relation-kinds list and the moment we read it.
#[derive(Clone, Debug)]
pub struct RelationKindsSnapshot {
    pub kinds: Vec<String>,
    pub refreshed_at: Instant,
}

/// Thread-safe TTL cache over `SELECT kind FROM relation_kinds`.
pub struct RelationKindsCache {
    inner: Arc<RwLock<Option<RelationKindsSnapshot>>>,
}

impl RelationKindsCache {
    /// Empty cache; first call to [`list_kinds`](Self::list_kinds)
    /// triggers a refresh.
    pub fn new() -> Self {
        Self { inner: Arc::new(RwLock::new(None)) }
    }

    /// Return the cached relation-kinds list, refreshing it from the
    /// database when the TTL has expired or the cache is empty.
    ///
    /// Locking shape mirrors [`crate::entity_kinds::KindsCache::list_kinds`]:
    /// a read-lock fast path for the common case (cache fresh), a
    /// write-lock slow path on miss with a re-check inside the write
    /// lock so a second task that just refreshed wins the race
    /// without a second SQL round-trip.
    pub async fn list_kinds(&self, pool: &PgPool) -> Result<Vec<String>, DbError> {
        {
            let guard = self.inner.read().await;
            if let Some(snap) = guard.as_ref() {
                if snap.refreshed_at.elapsed() < RELATION_KINDS_CACHE_TTL {
                    return Ok(snap.kinds.clone());
                }
            }
        }
        let mut guard = self.inner.write().await;
        if let Some(snap) = guard.as_ref() {
            if snap.refreshed_at.elapsed() < RELATION_KINDS_CACHE_TTL {
                return Ok(snap.kinds.clone());
            }
        }
        let kinds = fetch_relation_kinds(pool).await?;
        let snap = RelationKindsSnapshot {
            kinds: kinds.clone(),
            refreshed_at: Instant::now(),
        };
        *guard = Some(snap);
        Ok(kinds)
    }
}

impl Default for RelationKindsCache {
    fn default() -> Self { Self::new() }
}

/// One-shot `SELECT kind FROM relation_kinds ORDER BY kind`. Exposed
/// publicly so [`RelationKindsCache`] can call it and so integration
/// tests can compare the cached path against the source-of-truth
/// without going through the cache.
///
/// Order is by `kind` ascending so test assertions on the returned
/// `Vec<String>` are deterministic regardless of insert order.
pub async fn fetch_relation_kinds(pool: &PgPool) -> Result<Vec<String>, DbError> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT kind FROM relation_kinds ORDER BY kind")
        .fetch_all(pool)
        .await
        .map_err(|e| DbError::Query(format!("fetch_relation_kinds: {e}")))?;
    Ok(rows.into_iter().map(|(k,)| k).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Constructor leaves the cache empty; the first
    /// [`list_kinds`](RelationKindsCache::list_kinds) call must hit the
    /// DB to populate it (pinned indirectly via the integration tests
    /// in `db/tests/postgres_e2e.rs`).
    #[test]
    fn relation_kinds_cache_starts_empty() {
        let c = RelationKindsCache::new();
        let guard = c.inner.try_read().expect("uncontended");
        assert!(guard.is_none());
    }

    /// TTL is pinned at exactly 60 seconds — same as
    /// [`crate::entity_kinds::KINDS_CACHE_TTL`]. A change here would
    /// silently widen the latency window for operator-driven
    /// vocabulary updates to propagate, so the constant is part of
    /// the contract.
    #[test]
    fn relation_kinds_cache_ttl_is_60s() {
        assert_eq!(RELATION_KINDS_CACHE_TTL, Duration::from_secs(60));
    }

    /// Default trait delegates to `new()` so callers that build the
    /// cache via `Default::default()` get the same empty start state.
    #[test]
    fn default_impl_matches_new() {
        let c = RelationKindsCache::default();
        let guard = c.inner.try_read().expect("uncontended");
        assert!(guard.is_none());
    }
}
