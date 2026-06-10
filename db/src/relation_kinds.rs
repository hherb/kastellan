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
use time::OffsetDateTime;
use tokio::sync::RwLock;

/// Cache TTL — 60 seconds. Identical to [`crate::entity_kinds::KINDS_CACHE_TTL`]
/// by design; the two caches are read on the same hot path.
pub const RELATION_KINDS_CACHE_TTL: Duration = Duration::from_secs(60);

/// Maximum length (UTF-8 bytes) for a `kind` label. Bounds the size of
/// audit-row payloads and pins the wire-shape footprint. 64 bytes
/// covers every seed value (longest is `'contraindicated with'` at 20
/// bytes) and any plausible operator extension.
pub const MAX_RELATION_KIND_LEN: usize = 64;

/// Maximum length (UTF-8 bytes) for a relation-kind `description`.
/// 2 KiB is long enough for a verbose explanatory paragraph but well
/// short of inflating audit-row size enough to break grep-driven
/// operator workflows. Mirror of [`crate::entity_kinds::MAX_ENTITY_KIND_DESCRIPTION_LEN`].
///
/// Issue [#111](https://github.com/hherb/kastellan/issues/111) item 3 —
/// without this cap an operator could store an arbitrarily long
/// description, which would then land verbatim in
/// `audit_log.payload->>'description'`.
pub const MAX_RELATION_KIND_DESCRIPTION_LEN: usize = 2048;

/// The FK-fallback sentinel kind. Migration 0017's FK on
/// `relations.kind` has `ON DELETE SET DEFAULT` pointing at this row;
/// deleting it would break the FK invariant for any historical row
/// whose original `kind` was already removed. Operator-facing CLIs
/// must refuse to remove it.
pub const RELATION_KIND_UNDEFINED: &str = "undefined";

/// Errors that come out of [`add`], [`remove`], and [`list_all`].
#[derive(thiserror::Error, Debug)]
pub enum RelationKindError {
    #[error("relation kind is empty or longer than {MAX_RELATION_KIND_LEN} bytes")]
    InvalidKind,

    #[error("relation kind contains a NUL byte")]
    KindHasNul,

    #[error("relation kind {RELATION_KIND_UNDEFINED:?} is the FK fallback and cannot be removed by operator action")]
    RemovalOfUndefinedRejected,

    /// Description exceeded [`MAX_RELATION_KIND_DESCRIPTION_LEN`]. The
    /// payload carries the offending byte length so the operator sees
    /// exactly how far over the cap they were.
    #[error("relation kind description is {len} bytes; cap is {MAX_RELATION_KIND_DESCRIPTION_LEN}")]
    DescriptionTooLong { len: usize },

    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// One row in `relation_kinds`. Returned by [`list_all`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationKindEntry {
    pub kind: String,
    pub description: Option<String>,
    pub created_at: OffsetDateTime,
}

/// Validate a relation-kind label.
///
/// Rules:
///   * non-empty,
///   * ≤ [`MAX_RELATION_KIND_LEN`] UTF-8 bytes,
///   * no NUL byte (Postgres TEXT rejects NULs at the protocol layer,
///     but catching here gives a precise typed error).
///
/// Spaces, multi-word phrases, and arbitrary Unicode are allowed — the
/// seed vocabulary contains entries like `'has symptom'` and
/// `'contraindicated with'`, and there is no charset restriction in the
/// schema. The label flows into JSON-RPC payloads (the GLiNER worker's
/// `relation_labels` field) where it is treated as opaque string data.
pub fn validate_relation_kind(kind: &str) -> Result<(), RelationKindError> {
    if kind.is_empty() || kind.len() > MAX_RELATION_KIND_LEN {
        return Err(RelationKindError::InvalidKind);
    }
    if kind.contains('\0') {
        return Err(RelationKindError::KindHasNul);
    }
    Ok(())
}

/// Validate a relation-kind description.
///
/// Rules:
///   * `None` is always valid (operator may add a kind without
///     describing it),
///   * `Some(d)` where `d.len() <= MAX_RELATION_KIND_DESCRIPTION_LEN`
///     is valid (including empty `""`),
///   * otherwise returns
///     [`RelationKindError::DescriptionTooLong`] carrying the
///     offending byte length.
///
/// Cap is 2 KiB — see [`MAX_RELATION_KIND_DESCRIPTION_LEN`] for the
/// motivation.
pub fn validate_relation_kind_description(
    description: Option<&str>,
) -> Result<(), RelationKindError> {
    if let Some(d) = description {
        if d.len() > MAX_RELATION_KIND_DESCRIPTION_LEN {
            return Err(RelationKindError::DescriptionTooLong { len: d.len() });
        }
    }
    Ok(())
}

/// Add one relation-kind row. Idempotent — returns `Ok(true)` if a row
/// was INSERTed, `Ok(false)` if the kind was already present.
///
/// `description` is stored as `NULL` when `None`. `kind` is validated
/// against [`validate_relation_kind`]; `description` (if `Some`) is
/// validated against [`validate_relation_kind_description`] for the
/// 2 KiB cap — operator-set descriptions land in `audit_log.payload`
/// so an unbounded length would inflate audit rows beyond
/// grep-friendly sizes (Issue
/// [#111](https://github.com/hherb/kastellan/issues/111) item 3).
///
/// **Requires a connection with write privileges on `relation_kinds`**
/// — that's the [`crate::pool::connect_admin_pool`] shape, not the
/// runtime pool. A runtime-role connection will fail with
/// `RelationKindError::Db(...)` carrying a `permission denied` from
/// Postgres.
pub async fn add(
    pool: &PgPool,
    kind: &str,
    description: Option<&str>,
) -> Result<bool, RelationKindError> {
    validate_relation_kind(kind)?;
    validate_relation_kind_description(description)?;
    let rows = sqlx::query(
        "INSERT INTO relation_kinds (kind, description)
         VALUES ($1, $2)
         ON CONFLICT (kind) DO NOTHING",
    )
    .bind(kind)
    .bind(description)
    .execute(pool)
    .await?;
    Ok(rows.rows_affected() == 1)
}

/// Remove one relation-kind row. Idempotent — returns `Ok(true)` if a
/// row was deleted, `Ok(false)` if nothing matched.
///
/// Rejects `kind == RELATION_KIND_UNDEFINED` up front with a typed
/// error rather than letting Postgres execute the DELETE. The DB-side
/// FK has `ON DELETE SET DEFAULT` pointing at `'undefined'`, so if
/// `'undefined'` itself were deleted the next dependent row update
/// would fail. The CLI surfaces this as a clear "cannot remove
/// fallback" message instead of a confusing FK error on a future
/// unrelated operation.
///
/// **Requires admin-pool privileges** — see [`add`].
pub async fn remove(pool: &PgPool, kind: &str) -> Result<bool, RelationKindError> {
    validate_relation_kind(kind)?;
    if kind == RELATION_KIND_UNDEFINED {
        return Err(RelationKindError::RemovalOfUndefinedRejected);
    }
    let rows = sqlx::query("DELETE FROM relation_kinds WHERE kind = $1")
        .bind(kind)
        .execute(pool)
        .await?;
    Ok(rows.rows_affected() == 1)
}

/// List every row in `relation_kinds`, ordered by `kind` ASC for
/// deterministic test assertions and stable operator output.
///
/// Works against either pool shape: the runtime role has `SELECT`
/// granted by migration 0017, so the daemon can read for cache
/// refreshes ([`fetch_relation_kinds`]) and the CLI's `list` action
/// can use the same runtime pool as the rest of its read-only paths.
pub async fn list_all(pool: &PgPool) -> Result<Vec<RelationKindEntry>, RelationKindError> {
    let rows: Vec<(String, Option<String>, OffsetDateTime)> = sqlx::query_as(
        "SELECT kind, description, created_at
         FROM relation_kinds
         ORDER BY kind ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(kind, description, created_at)| RelationKindEntry {
            kind,
            description,
            created_at,
        })
        .collect())
}

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

    // --- validate_relation_kind --------------------------------------

    #[test]
    fn validate_relation_kind_accepts_seed_shapes() {
        // Single-word kind.
        validate_relation_kind("treats").unwrap();
        // Multi-word kind with spaces (one of the actual 0017 seeds).
        validate_relation_kind("has symptom").unwrap();
        validate_relation_kind("contraindicated with").unwrap();
        // Single char (minimum non-empty).
        validate_relation_kind("x").unwrap();
        // Exactly MAX_RELATION_KIND_LEN bytes — inclusive boundary.
        let max: String = "a".repeat(MAX_RELATION_KIND_LEN);
        validate_relation_kind(&max).unwrap();
    }

    #[test]
    fn validate_relation_kind_rejects_empty_and_oversize() {
        assert!(matches!(
            validate_relation_kind(""),
            Err(RelationKindError::InvalidKind)
        ));
        let too_long: String = "a".repeat(MAX_RELATION_KIND_LEN + 1);
        assert!(matches!(
            validate_relation_kind(&too_long),
            Err(RelationKindError::InvalidKind)
        ));
    }

    #[test]
    fn validate_relation_kind_rejects_nul_byte() {
        assert!(matches!(
            validate_relation_kind("bad\0kind"),
            Err(RelationKindError::KindHasNul)
        ));
        // NUL at the end / beginning still trips the check.
        assert!(matches!(
            validate_relation_kind("\0"),
            Err(RelationKindError::KindHasNul)
        ));
    }

    // --- constants ---------------------------------------------------

    /// The CLI's "cannot remove" message and the DB-side FK both pin on
    /// `"undefined"`. A rename here without coordinating the migration
    /// would silently break the FK fallback target — pin the literal.
    #[test]
    fn undefined_sentinel_is_literally_undefined() {
        assert_eq!(RELATION_KIND_UNDEFINED, "undefined");
    }

    /// The max-length cap is part of the public contract (operator-
    /// visible error message; bounds audit-payload sizes). Pin the
    /// number so a future widening is a deliberate edit.
    #[test]
    fn max_relation_kind_len_is_64() {
        assert_eq!(MAX_RELATION_KIND_LEN, 64);
    }

    /// The description-length cap is part of the public contract too —
    /// it bounds the size of an `audit_log.payload` row carrying a
    /// `relation_kinds.add` event. 2 KiB is enough for a verbose
    /// paragraph but well short of inflating audit-row size enough to
    /// break grep-driven operator workflows. Pin the number so a
    /// future widening is a deliberate edit (Issue
    /// [#111](https://github.com/hherb/kastellan/issues/111) item 3).
    #[test]
    fn max_relation_kind_description_len_is_2048() {
        assert_eq!(MAX_RELATION_KIND_DESCRIPTION_LEN, 2048);
    }

    // --- validate_relation_kind_description --------------------------

    /// `None` is always valid — operators can add a kind without
    /// describing it.
    #[test]
    fn validate_description_accepts_none() {
        validate_relation_kind_description(None).unwrap();
    }

    /// Empty string is valid (semantically equivalent to `None` but
    /// distinguishable on the wire — operator may have explicitly
    /// passed `--description ""`).
    #[test]
    fn validate_description_accepts_empty() {
        validate_relation_kind_description(Some("")).unwrap();
    }

    /// One byte under the cap is accepted.
    #[test]
    fn validate_description_accepts_just_under_cap() {
        let d: String = "a".repeat(MAX_RELATION_KIND_DESCRIPTION_LEN - 1);
        validate_relation_kind_description(Some(&d)).unwrap();
    }

    /// Exactly at the cap is accepted (inclusive boundary).
    #[test]
    fn validate_description_accepts_at_cap() {
        let d: String = "a".repeat(MAX_RELATION_KIND_DESCRIPTION_LEN);
        validate_relation_kind_description(Some(&d)).unwrap();
    }

    /// One byte over the cap is rejected with the typed variant that
    /// carries the offending length so the operator can see exactly
    /// how far over they were.
    #[test]
    fn validate_description_rejects_one_byte_over_cap() {
        let d: String = "a".repeat(MAX_RELATION_KIND_DESCRIPTION_LEN + 1);
        match validate_relation_kind_description(Some(&d)) {
            Err(RelationKindError::DescriptionTooLong { len }) => {
                assert_eq!(len, MAX_RELATION_KIND_DESCRIPTION_LEN + 1);
            }
            other => panic!("expected DescriptionTooLong; got {other:?}"),
        }
    }
}
