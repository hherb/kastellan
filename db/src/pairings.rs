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
pub async fn is_paired<'e, E>(executor: E, channel: &str, peer: &str) -> Result<bool, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
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
pub async fn insert_pairing<'e, E>(
    executor: E,
    channel: &str,
    peer: &str,
    method: &str,
) -> Result<bool, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
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
