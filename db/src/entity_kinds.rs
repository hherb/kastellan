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
use time::OffsetDateTime;
use tokio::sync::RwLock;

/// Cache TTL — 60 seconds.
pub const KINDS_CACHE_TTL: Duration = Duration::from_secs(60);

/// Maximum length (UTF-8 bytes) for a `kind` label. Bounds the size of
/// audit-row payloads and pins the wire-shape footprint. 64 bytes
/// covers every seed value (longest is `'organization'` at 12 bytes)
/// and any plausible operator extension.
///
/// Pinned at exactly 64 to keep parity with
/// [`crate::relation_kinds::MAX_RELATION_KIND_LEN`] — the two tables
/// are intentionally symmetric, so a future widening on one side
/// should be paired with the other.
pub const MAX_ENTITY_KIND_LEN: usize = 64;

/// Maximum length (UTF-8 bytes) for an entity-kind `description`.
/// 2 KiB is long enough for a verbose explanatory paragraph but well
/// short of inflating audit-row size enough to break grep-driven
/// operator workflows. Mirror of
/// [`crate::relation_kinds::MAX_RELATION_KIND_DESCRIPTION_LEN`].
///
/// Issue [#111](https://github.com/hherb/kastellan/issues/111) item 3 —
/// without this cap an operator could store an arbitrarily long
/// description, which would then land verbatim in
/// `audit_log.payload->>'description'`.
pub const MAX_ENTITY_KIND_DESCRIPTION_LEN: usize = 2048;

/// The FK-fallback sentinel kind. Migration 0015's FK on
/// `entities.kind` has `ON DELETE SET DEFAULT` pointing at this row;
/// deleting it would break the FK invariant for any historical row
/// whose original `kind` was already removed. Operator-facing CLIs
/// must refuse to remove it.
///
/// Parallel to [`crate::relation_kinds::RELATION_KIND_UNDEFINED`].
pub const ENTITY_KIND_UNDEFINED: &str = "undefined";

/// Errors that come out of [`add`], [`remove`], and [`list_all`].
///
/// Shape mirrors [`crate::relation_kinds::RelationKindError`] —
/// operator-CLI error surfaces should stay symmetric across the two
/// vocabulary-management subcommands.
#[derive(thiserror::Error, Debug)]
pub enum EntityKindError {
    #[error("entity kind is empty or longer than {MAX_ENTITY_KIND_LEN} bytes")]
    InvalidKind,

    #[error("entity kind contains a NUL byte")]
    KindHasNul,

    #[error("entity kind {ENTITY_KIND_UNDEFINED:?} is the FK fallback and cannot be removed by operator action")]
    RemovalOfUndefinedRejected,

    /// Description exceeded [`MAX_ENTITY_KIND_DESCRIPTION_LEN`]. The
    /// payload carries the offending byte length so the operator sees
    /// exactly how far over the cap they were.
    #[error("entity kind description is {len} bytes; cap is {MAX_ENTITY_KIND_DESCRIPTION_LEN}")]
    DescriptionTooLong { len: usize },

    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// One row in `entity_kinds`. Returned by [`list_all`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityKindEntry {
    pub kind: String,
    pub description: Option<String>,
    pub created_at: OffsetDateTime,
}

/// Validate an entity-kind label.
///
/// Rules:
///   * non-empty,
///   * ≤ [`MAX_ENTITY_KIND_LEN`] UTF-8 bytes,
///   * no NUL byte (Postgres TEXT rejects NULs at the protocol layer,
///     but catching here gives a precise typed error).
///
/// Spaces, multi-word phrases, and arbitrary Unicode are allowed —
/// the seed taxonomy contains entries like `'phone number'`. No
/// charset restriction in the schema; the label flows into JSON-RPC
/// payloads where it is treated as opaque string data.
pub fn validate_entity_kind(kind: &str) -> Result<(), EntityKindError> {
    if kind.is_empty() || kind.len() > MAX_ENTITY_KIND_LEN {
        return Err(EntityKindError::InvalidKind);
    }
    if kind.contains('\0') {
        return Err(EntityKindError::KindHasNul);
    }
    Ok(())
}

/// Validate an entity-kind description.
///
/// Rules:
///   * `None` is always valid (operator may add a kind without
///     describing it),
///   * `Some(d)` where `d.len() <= MAX_ENTITY_KIND_DESCRIPTION_LEN`
///     is valid (including empty `""`),
///   * otherwise returns [`EntityKindError::DescriptionTooLong`]
///     carrying the offending byte length.
///
/// Cap is 2 KiB — see [`MAX_ENTITY_KIND_DESCRIPTION_LEN`] for the
/// motivation.
pub fn validate_entity_kind_description(
    description: Option<&str>,
) -> Result<(), EntityKindError> {
    if let Some(d) = description {
        if d.len() > MAX_ENTITY_KIND_DESCRIPTION_LEN {
            return Err(EntityKindError::DescriptionTooLong { len: d.len() });
        }
    }
    Ok(())
}

/// Add one entity-kind row. Idempotent — returns `Ok(true)` if a row
/// was INSERTed, `Ok(false)` if the kind was already present.
///
/// `description` is stored as `NULL` when `None`. `kind` is validated
/// against [`validate_entity_kind`]; `description` (if `Some`) is
/// validated against [`validate_entity_kind_description`] for the
/// 2 KiB cap — operator-set descriptions land in `audit_log.payload`
/// so an unbounded length would inflate audit rows beyond
/// grep-friendly sizes (Issue
/// [#111](https://github.com/hherb/kastellan/issues/111) item 3).
///
/// **Requires a connection with write privileges on `entity_kinds`**
/// — that's the [`crate::pool::connect_admin_pool`] shape, not the
/// runtime pool. A runtime-role connection will fail with
/// `EntityKindError::Db(...)` carrying a `permission denied` from
/// Postgres (migration 0016 REVOKEs INSERT/UPDATE/DELETE/TRUNCATE
/// from the runtime role).
pub async fn add(
    pool: &PgPool,
    kind: &str,
    description: Option<&str>,
) -> Result<bool, EntityKindError> {
    validate_entity_kind(kind)?;
    validate_entity_kind_description(description)?;
    let rows = sqlx::query(
        "INSERT INTO entity_kinds (kind, description)
         VALUES ($1, $2)
         ON CONFLICT (kind) DO NOTHING",
    )
    .bind(kind)
    .bind(description)
    .execute(pool)
    .await?;
    Ok(rows.rows_affected() == 1)
}

/// Remove one entity-kind row. Idempotent — returns `Ok(true)` if a
/// row was deleted, `Ok(false)` if nothing matched.
///
/// Rejects `kind == ENTITY_KIND_UNDEFINED` up front with a typed
/// error rather than letting Postgres execute the DELETE. The DB-side
/// FK has `ON DELETE SET DEFAULT` pointing at `'undefined'`, so if
/// `'undefined'` itself were deleted the next dependent row update
/// would fail. The CLI surfaces this as a clear "cannot remove
/// fallback" message instead of a confusing FK error on a future
/// unrelated operation.
///
/// **Requires admin-pool privileges** — see [`add`].
pub async fn remove(pool: &PgPool, kind: &str) -> Result<bool, EntityKindError> {
    validate_entity_kind(kind)?;
    if kind == ENTITY_KIND_UNDEFINED {
        return Err(EntityKindError::RemovalOfUndefinedRejected);
    }
    let rows = sqlx::query("DELETE FROM entity_kinds WHERE kind = $1")
        .bind(kind)
        .execute(pool)
        .await?;
    Ok(rows.rows_affected() == 1)
}

/// List every row in `entity_kinds`, ordered by `kind` ASC for
/// deterministic test assertions and stable operator output.
///
/// Works against either pool shape: the runtime role has `SELECT`
/// granted by migration 0015, so the cache refresh path
/// ([`fetch_kinds`]) and operator-CLI `list` can use the same data
/// source.
pub async fn list_all(pool: &PgPool) -> Result<Vec<EntityKindEntry>, EntityKindError> {
    let rows: Vec<(String, Option<String>, OffsetDateTime)> = sqlx::query_as(
        "SELECT kind, description, created_at
         FROM entity_kinds
         ORDER BY kind ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(kind, description, created_at)| EntityKindEntry {
            kind,
            description,
            created_at,
        })
        .collect())
}

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

    // --- validate_entity_kind ----------------------------------------

    #[test]
    fn validate_entity_kind_accepts_seed_shapes() {
        // Single-word kinds.
        validate_entity_kind("person").unwrap();
        validate_entity_kind("organization").unwrap();
        // Multi-word kind with space (an actual 0015 seed).
        validate_entity_kind("phone number").unwrap();
        // Single char (minimum non-empty).
        validate_entity_kind("x").unwrap();
        // Exactly MAX_ENTITY_KIND_LEN bytes — inclusive boundary.
        let max: String = "a".repeat(MAX_ENTITY_KIND_LEN);
        validate_entity_kind(&max).unwrap();
    }

    #[test]
    fn validate_entity_kind_rejects_empty_and_oversize() {
        assert!(matches!(
            validate_entity_kind(""),
            Err(EntityKindError::InvalidKind)
        ));
        let too_long: String = "a".repeat(MAX_ENTITY_KIND_LEN + 1);
        assert!(matches!(
            validate_entity_kind(&too_long),
            Err(EntityKindError::InvalidKind)
        ));
    }

    #[test]
    fn validate_entity_kind_rejects_nul_byte() {
        assert!(matches!(
            validate_entity_kind("bad\0kind"),
            Err(EntityKindError::KindHasNul)
        ));
        assert!(matches!(
            validate_entity_kind("\0"),
            Err(EntityKindError::KindHasNul)
        ));
    }

    // --- constants ---------------------------------------------------

    /// The CLI's "cannot remove" message and the DB-side FK both pin on
    /// `"undefined"`. A rename here without coordinating the migration
    /// would silently break the FK fallback target — pin the literal.
    #[test]
    fn undefined_sentinel_is_literally_undefined() {
        assert_eq!(ENTITY_KIND_UNDEFINED, "undefined");
    }

    /// Symmetric with [`crate::relation_kinds::MAX_RELATION_KIND_LEN`]
    /// — a future widening on one side should be a deliberate paired
    /// edit. Pin the number so the asymmetry is visible.
    #[test]
    fn max_entity_kind_len_is_64() {
        assert_eq!(MAX_ENTITY_KIND_LEN, 64);
    }

    /// Mirror of
    /// [`crate::relation_kinds::tests::max_relation_kind_description_len_is_2048`]
    /// — see the rationale there. Pin the number so a future widening
    /// is a deliberate paired edit.
    #[test]
    fn max_entity_kind_description_len_is_2048() {
        assert_eq!(MAX_ENTITY_KIND_DESCRIPTION_LEN, 2048);
    }

    // --- validate_entity_kind_description ----------------------------
    // Mirror of `crate::relation_kinds::tests::validate_description_*`.

    #[test]
    fn validate_description_accepts_none() {
        validate_entity_kind_description(None).unwrap();
    }

    #[test]
    fn validate_description_accepts_empty() {
        validate_entity_kind_description(Some("")).unwrap();
    }

    #[test]
    fn validate_description_accepts_just_under_cap() {
        let d: String = "a".repeat(MAX_ENTITY_KIND_DESCRIPTION_LEN - 1);
        validate_entity_kind_description(Some(&d)).unwrap();
    }

    #[test]
    fn validate_description_accepts_at_cap() {
        let d: String = "a".repeat(MAX_ENTITY_KIND_DESCRIPTION_LEN);
        validate_entity_kind_description(Some(&d)).unwrap();
    }

    #[test]
    fn validate_description_rejects_one_byte_over_cap() {
        let d: String = "a".repeat(MAX_ENTITY_KIND_DESCRIPTION_LEN + 1);
        match validate_entity_kind_description(Some(&d)) {
            Err(EntityKindError::DescriptionTooLong { len }) => {
                assert_eq!(len, MAX_ENTITY_KIND_DESCRIPTION_LEN + 1);
            }
            other => panic!("expected DescriptionTooLong; got {other:?}"),
        }
    }
}
