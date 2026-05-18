//! `entity_kinds` table: which entity categories (kinds) the extractor
//! is allowed to detect. Seeded by migration `0015`. Operator extends
//! via `INSERT INTO entity_kinds`; no automatic widening from the
//! extractor.
//!
//! `KindsCache` holds the result of `SELECT kind FROM entity_kinds` for
//! 60 seconds before a re-fetch — short enough that operator INSERTs
//! propagate to the running daemon without explicit invalidation, long
//! enough that the hot path (every `formulate_plan` call) doesn't
//! re-issue the query.

use crate::DbError;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Cache TTL — 60 seconds.
pub const KINDS_CACHE_TTL: Duration = Duration::from_secs(60);

/// One snapshot of the kinds list and the moment we read it.
#[derive(Clone, Debug)]
pub struct KindsSnapshot {
    pub kinds: Vec<String>,
    pub refreshed_at: Instant,
}

/// Thread-safe TTL cache over `SELECT kind FROM entity_kinds`.
pub struct KindsCache {
    inner: Arc<RwLock<Option<KindsSnapshot>>>,
}

impl KindsCache {
    /// Empty cache; first call to `list_kinds` triggers a refresh.
    pub fn new() -> Self {
        Self { inner: Arc::new(RwLock::new(None)) }
    }

    /// Return the cached kinds list, refreshing it from the database
    /// if the TTL has expired or the cache is empty.
    pub async fn list_kinds(&self, pool: &PgPool) -> Result<Vec<String>, DbError> {
        // Read-lock fast path — covers the common case (cache fresh).
        {
            let guard = self.inner.read().await;
            if let Some(snap) = guard.as_ref() {
                if snap.refreshed_at.elapsed() < KINDS_CACHE_TTL {
                    return Ok(snap.kinds.clone());
                }
            }
        }
        // Write-lock slow path — TTL expired or empty cache.
        let mut guard = self.inner.write().await;
        // Re-check inside write lock — another task may have refreshed
        // while we waited.
        if let Some(snap) = guard.as_ref() {
            if snap.refreshed_at.elapsed() < KINDS_CACHE_TTL {
                return Ok(snap.kinds.clone());
            }
        }
        let kinds = fetch_kinds(pool).await?;
        let snap = KindsSnapshot {
            kinds: kinds.clone(),
            refreshed_at: Instant::now(),
        };
        *guard = Some(snap);
        Ok(kinds)
    }
}

impl Default for KindsCache {
    fn default() -> Self { Self::new() }
}

/// One-shot `SELECT kind FROM entity_kinds ORDER BY kind`. Exposed
/// publicly so `KindsCache` can call it AND so direct integration
/// tests can compare the cached path to the source-of-truth.
pub async fn fetch_kinds(pool: &PgPool) -> Result<Vec<String>, DbError> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT kind FROM entity_kinds ORDER BY kind")
        .fetch_all(pool)
        .await
        .map_err(|e| DbError::Query(format!("fetch_kinds: {e}")))?;
    Ok(rows.into_iter().map(|(k,)| k).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_cache_starts_empty() {
        let c = KindsCache::new();
        // No async path here — just confirm the constructor compiles
        // and the inner Option starts None.
        let guard = c.inner.try_read().expect("uncontended");
        assert!(guard.is_none());
    }

    #[test]
    fn kinds_cache_ttl_is_60s() {
        assert_eq!(KINDS_CACHE_TTL, Duration::from_secs(60));
    }
}
