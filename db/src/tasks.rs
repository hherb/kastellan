//! Typed CRUD against the `tasks` table.
//!
//! All writes go through this module; the scheduler never builds raw
//! SQL. Reads are typed too (no `serde_json::Value` leaking out where
//! a `Task` would do).

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::Row;
use sqlx::postgres::PgRow;
use time::OffsetDateTime;
use time::Duration;

use crate::DbError;

/// The two concurrency lanes. `fast` is the default; `long` is opt-in
/// via the producer (CLI flag, channel adapter default, etc.).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lane {
    Fast,
    Long,
}

impl Lane {
    pub fn as_sql(self) -> &'static str {
        match self {
            Lane::Fast => "fast",
            Lane::Long => "long",
        }
    }

    pub fn from_sql(s: &str) -> Result<Self, DbError> {
        match s {
            "fast" => Ok(Lane::Fast),
            "long" => Ok(Lane::Long),
            other => Err(DbError::Other(format!("unknown lane: {other}"))),
        }
    }
}

/// Default deadlines per lane. Used at claim time when the producer
/// does not pin `payload.deadline_seconds` itself.
pub const DEFAULT_DEADLINE_FAST_S: i64 = 60;
pub const DEFAULT_DEADLINE_LONG_S: i64 = 30 * 60;

/// Default plan-iteration caps per lane. Mirror values in
/// `core::scheduler` so a producer omitting the cap gets the same
/// behaviour as the runner enforces.
pub const DEFAULT_MAX_PLANS_FAST: u32 = 3;
pub const DEFAULT_MAX_PLANS_LONG: u32 = 12;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_round_trips_through_sql_string() {
        assert_eq!(Lane::Fast.as_sql(), "fast");
        assert_eq!(Lane::Long.as_sql(), "long");
        assert_eq!(Lane::from_sql("fast").unwrap(), Lane::Fast);
        assert_eq!(Lane::from_sql("long").unwrap(), Lane::Long);
        assert!(Lane::from_sql("medium").is_err());
    }
}

/// One decoded `tasks` row.
#[derive(Clone, Debug)]
pub struct Task {
    pub id: i64,
    pub state: String,
    pub lane: Lane,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub started_at: Option<OffsetDateTime>,
    pub finished_at: Option<OffsetDateTime>,
    pub lease_expires_at: Option<OffsetDateTime>,
    pub plan_count: i32,
    pub payload: serde_json::Value,
    pub result: Option<serde_json::Value>,
}

/// Insert a fresh `pending` task row. The `tasks_inserted` trigger
/// will fire `pg_notify('tasks_inserted', NEW.id::text)` for any
/// listeners (the lane runner of the matching lane).
pub async fn insert_pending(
    pool: &PgPool,
    lane: Lane,
    payload: serde_json::Value,
) -> Result<i64, DbError> {
    let row = sqlx::query(
        "INSERT INTO tasks (state, lane, payload) \
         VALUES ('pending', $1, $2) \
         RETURNING id",
    )
    .bind(lane.as_sql())
    .bind(&payload)
    .fetch_one(pool)
    .await
    .map_err(|e| DbError::Query(format!("tasks insert: {e}")))?;

    row.try_get::<i64, _>("id")
        .map_err(|e| DbError::Query(format!("decode tasks.id: {e}")))
}

/// Atomically claim the oldest `pending` task on the given lane,
/// transitioning state to `running` and setting `started_at` +
/// `lease_expires_at`. Returns `None` if no pending row exists on
/// that lane.
///
/// Uses `FOR UPDATE SKIP LOCKED` — the standard PG queue idiom — so
/// concurrent callers (different lane runners, or two daemons during
/// a transient overlap) never race over the same row. The per-lane
/// filter is what keeps the two lane runners from ever racing each
/// other.
pub async fn claim_one(
    pool: &PgPool,
    lane: Lane,
    deadline_seconds: i64,
) -> Result<Option<Task>, DbError> {
    let now = OffsetDateTime::now_utc();
    let lease_expires_at = now + Duration::seconds(deadline_seconds);

    let row = sqlx::query(
        "UPDATE tasks \
         SET state = 'running', \
             started_at = now(), \
             updated_at = now(), \
             lease_expires_at = $2 \
         WHERE id = ( \
             SELECT id FROM tasks \
             WHERE lane = $1 AND state = 'pending' \
             ORDER BY created_at ASC \
             LIMIT 1 \
             FOR UPDATE SKIP LOCKED \
         ) \
         RETURNING id, state, lane, created_at, updated_at, started_at, \
                   finished_at, lease_expires_at, plan_count, payload, result",
    )
    .bind(lane.as_sql())
    .bind(lease_expires_at)
    .fetch_optional(pool)
    .await
    .map_err(|e| DbError::Query(format!("tasks claim_one: {e}")))?;

    let Some(row) = row else { return Ok(None) };

    Ok(Some(decode_task_row(&row)?))
}

/// Decode a `tasks` row into a typed `Task`. Used by `claim_one`,
/// `get`, `list`, and any future read function that returns `Task`s.
/// Centralised so a column-rename mistake fails in one place, not
/// many.
fn decode_task_row(row: &PgRow) -> Result<Task, DbError> {
    Ok(Task {
        id: row.try_get("id")
            .map_err(|e| DbError::Query(format!("decode tasks.id: {e}")))?,
        state: row.try_get("state")
            .map_err(|e| DbError::Query(format!("decode tasks.state: {e}")))?,
        lane: Lane::from_sql(
            row.try_get::<&str, _>("lane")
                .map_err(|e| DbError::Query(format!("decode tasks.lane: {e}")))?,
        )?,
        created_at: row.try_get("created_at")
            .map_err(|e| DbError::Query(format!("decode tasks.created_at: {e}")))?,
        updated_at: row.try_get("updated_at")
            .map_err(|e| DbError::Query(format!("decode tasks.updated_at: {e}")))?,
        started_at: row.try_get("started_at")
            .map_err(|e| DbError::Query(format!("decode tasks.started_at: {e}")))?,
        finished_at: row.try_get("finished_at")
            .map_err(|e| DbError::Query(format!("decode tasks.finished_at: {e}")))?,
        lease_expires_at: row.try_get("lease_expires_at")
            .map_err(|e| DbError::Query(format!("decode tasks.lease_expires_at: {e}")))?,
        plan_count: row.try_get("plan_count")
            .map_err(|e| DbError::Query(format!("decode tasks.plan_count: {e}")))?,
        payload: row.try_get("payload")
            .map_err(|e| DbError::Query(format!("decode tasks.payload: {e}")))?,
        result: row.try_get("result")
            .map_err(|e| DbError::Query(format!("decode tasks.result: {e}")))?,
    })
}
