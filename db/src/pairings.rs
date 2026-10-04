//! Typed helpers for the channel-pairing tables (`pairings` + `pairing_codes`,
//! migration 0018). The channel bus's `DbPeerAuthorizer` reads `is_paired`; the
//! `DbPairingService` consumes a code (`claim_code`) and binds the peer
//! (`insert_pairing`); the operator CLI mints codes (`insert_code`) and revokes
//! (`revoke_pairing`). All SQL lives here.

use serde::{Deserialize, Serialize};
use sqlx::Row;
use time::{Duration, OffsetDateTime};

use crate::DbError;

/// One `pairings` row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pairing {
    pub id: i64,
    pub channel: String,
    pub peer: String,
    pub method: String,
    pub paired_at: OffsetDateTime,
    pub revoked_at: Option<OffsetDateTime>,
}

/// True iff `(channel, peer)` has an active (non-revoked) pairing.
///
/// A key holding a NUL answers `false` without a query: Postgres cannot
/// store a NUL in `text`, so no row can match, and sending it would fail
/// the lookup itself (#818).
pub async fn is_paired<'e, E>(executor: E, channel: &str, peer: &str) -> Result<bool, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    if refuse_nul_key(channel, peer).is_err() {
        return Ok(false);
    }
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pairings \
         WHERE channel = $1 AND peer = $2 AND revoked_at IS NULL)",
    )
    .bind(channel)
    .bind(peer)
    .fetch_one(executor)
    .await
    .map_err(|e| DbError::Query(format!("pairings is_paired: {e}")))?;
    Ok(exists)
}

/// Bind `(channel, peer)` if not already active. Idempotent via the partial
/// unique index (`pairings_active_uniq`). Returns `true` iff a new row was added.
/// A NUL in either key is refused (`refuse_nul_key`).
pub async fn insert_pairing<'e, E>(
    executor: E,
    channel: &str,
    peer: &str,
    method: &str,
) -> Result<bool, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    refuse_nul_key(channel, peer)?;
    let r = sqlx::query(
        "INSERT INTO pairings (channel, peer, method) VALUES ($1, $2, $3) \
         ON CONFLICT (channel, peer) WHERE revoked_at IS NULL DO NOTHING",
    )
    .bind(channel)
    .bind(peer)
    .bind(method)
    .execute(executor)
    .await
    .map_err(|e| DbError::Query(format!("pairings insert: {e}")))?;
    Ok(r.rows_affected() == 1)
}

/// Operator path: revoke the active pairing for `(channel, peer)`. Returns `true`
/// iff a row was revoked. Requires UPDATE privilege (admin connection — the
/// runtime role is REVOKEd from UPDATE on `pairings`).
pub async fn revoke_pairing<'e, E>(executor: E, channel: &str, peer: &str) -> Result<bool, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    refuse_nul_key(channel, peer)?;
    let r = sqlx::query(
        "UPDATE pairings SET revoked_at = now() \
         WHERE channel = $1 AND peer = $2 AND revoked_at IS NULL",
    )
    .bind(channel)
    .bind(peer)
    .execute(executor)
    .await
    .map_err(|e| DbError::Query(format!("pairings revoke: {e}")))?;
    Ok(r.rows_affected() == 1)
}

/// List pairings, newest first. `include_revoked = false` returns only active.
pub async fn list_pairings<'e, E>(
    executor: E,
    include_revoked: bool,
) -> Result<Vec<Pairing>, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows = sqlx::query(
        "SELECT id, channel, peer, method, paired_at, revoked_at FROM pairings \
         WHERE ($1 OR revoked_at IS NULL) \
         ORDER BY paired_at DESC",
    )
    .bind(include_revoked)
    .fetch_all(executor)
    .await
    .map_err(|e| DbError::Query(format!("pairings list: {e}")))?;

    rows.iter()
        .map(|row| {
            Ok(Pairing {
                id: row.try_get("id").map_err(dec("id"))?,
                channel: row.try_get("channel").map_err(dec("channel"))?,
                peer: row.try_get("peer").map_err(dec("peer"))?,
                method: row.try_get("method").map_err(dec("method"))?,
                paired_at: row.try_get("paired_at").map_err(dec("paired_at"))?,
                revoked_at: row.try_get("revoked_at").map_err(dec("revoked_at"))?,
            })
        })
        .collect()
}

/// Insert a pairing directly (operator action — no in-channel handshake), with
/// an optional long-lived token hash. `token_sha256` is `None` for transports
/// that authenticate their own peers (Matrix); `Some(hash)` for email, where
/// the sender must present the plaintext in every message.
pub async fn insert_pairing_with_token<'e, E>(
    executor: E,
    channel: &str,
    peer: &str,
    method: &str,
    token_sha256: Option<&str>,
) -> Result<i64, DbError>
where
    E: sqlx::PgExecutor<'e>,
{
    refuse_nul_key(channel, peer)?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO pairings (channel, peer, method, token_sha256)
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(channel)
    .bind(peer)
    .bind(method)
    .bind(token_sha256)
    .fetch_one(executor)
    .await?;
    Ok(id)
}

/// Token requirement for an ACTIVE pairing.
///
/// Three-state on purpose, and the caller must not collapse it:
/// * `None` — no active pairing (revoked rows included). Not authorized.
/// * `Some(None)` — paired, no token required (Matrix).
/// * `Some(Some(hash))` — paired, and the sender must present this token.
///
/// A key holding a NUL answers `None` without a query, as [`is_paired`]
/// does: Postgres cannot store one, so no row can match.
pub async fn token_hash_for<'e, E>(
    executor: E,
    channel: &str,
    peer: &str,
) -> Result<Option<Option<String>>, DbError>
where
    E: sqlx::PgExecutor<'e>,
{
    if refuse_nul_key(channel, peer).is_err() {
        return Ok(None);
    }
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT token_sha256 FROM pairings
          WHERE channel = $1 AND peer = $2 AND revoked_at IS NULL",
    )
    .bind(channel)
    .bind(peer)
    .fetch_optional(executor)
    .await?;
    Ok(row.map(|(h,)| h))
}

/// Refuse a NUL in a pairing's `(channel, peer)` key, before any SQL (#818).
///
/// Refused, never escaped: these are identities, and a peer `a\0` stored as
/// `a␀` would then match a different, real peer `a␀` ([`crate::nul`]). Every
/// writer calls this so a NUL key fails as a typed `NulRefused` naming the
/// column, rather than as an opaque Postgres encoding error.
fn refuse_nul_key(channel: &str, peer: &str) -> Result<(), DbError> {
    crate::nul::refuse_nul_in_text("pairings.channel", channel)?;
    crate::nul::refuse_nul_in_text("pairings.peer", peer)
}

fn dec(col: &'static str) -> impl Fn(sqlx::Error) -> DbError {
    move |e| DbError::Query(format!("decode pairings.{col}: {e}"))
}

/// Operator path: mint a pending pairing code (store only its SHA-256), valid for
/// `ttl_minutes`. Requires INSERT privilege (admin connection — runtime is
/// REVOKEd from INSERT on `pairing_codes`). Returns the new row id.
pub async fn insert_code<'e, E>(
    executor: E,
    code_sha256: &str,
    label: Option<&str>,
    ttl_minutes: i64,
) -> Result<i64, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    // Operator text, so a record by the rule — but an operator typing a NUL
    // into a label is a mistake to report, not a value worth rewriting.
    if let Some(label) = label {
        crate::nul::refuse_nul_in_text("pairing_codes.label", label)?;
    }
    let expires_at = OffsetDateTime::now_utc() + Duration::minutes(ttl_minutes);
    let row = sqlx::query(
        "INSERT INTO pairing_codes (code_sha256, label, expires_at) \
         VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(code_sha256)
    .bind(label)
    .bind(expires_at)
    .fetch_one(executor)
    .await
    .map_err(|e| DbError::Query(format!("pairing_codes insert: {e}")))?;
    row.try_get::<i64, _>("id")
        .map_err(|e| DbError::Query(format!("decode pairing_codes.id: {e}")))
}

/// True iff at least one code is currently claimable (unconsumed + unexpired).
/// The bus uses this as a cheap gate so the pairing carve-out stays inert when no
/// code is pending.
pub async fn any_active_code<'e, E>(executor: E) -> Result<bool, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pairing_codes \
         WHERE consumed_at IS NULL AND expires_at > now())",
    )
    .fetch_one(executor)
    .await
    .map_err(|e| DbError::Query(format!("pairing_codes any_active: {e}")))?;
    Ok(exists)
}

/// Atomically claim a code by its SHA-256: single-use + unexpired. The conditional
/// UPDATE makes two racing claims mutually exclusive (only one sees
/// `consumed_at IS NULL`). Returns `true` iff this call consumed the code.
pub async fn claim_code<'e, E>(
    executor: E,
    code_sha256: &str,
    consumed_by: &str,
) -> Result<bool, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    // Refused rather than escaped: `consumed_by` is `"{channel}/{peer}"`,
    // and the `insert_pairing` that follows in the same transaction refuses
    // that peer anyway — failing here keeps the code unconsumed.
    crate::nul::refuse_nul_in_text("pairing_codes.consumed_by", consumed_by)?;
    let r = sqlx::query(
        "UPDATE pairing_codes SET consumed_at = now(), consumed_by = $2 \
         WHERE code_sha256 = $1 AND consumed_at IS NULL AND expires_at > now()",
    )
    .bind(code_sha256)
    .bind(consumed_by)
    .execute(executor)
    .await
    .map_err(|e| DbError::Query(format!("pairing_codes claim: {e}")))?;
    Ok(r.rows_affected() == 1)
}
