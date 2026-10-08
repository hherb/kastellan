//! The reply claim (#825): which finished channel tasks still owe their peer
//! a reply, and the atomic step that settles one.
//!
//! Every route of a channel reply — from the live `tasks_completed` NOTIFY or
//! from the catch-up sweep, on either bus (#497) — claims the task first. The
//! claim is an `UPDATE … WHERE reply_settled_at IS NULL`, so it has exactly one
//! winner and every other router backs off silently. It is **at-most-once**: a
//! task is settled when its reply is *routed*; a transport failure after that
//! is recorded as `channel.reply_undelivered`, never retried (see the spec's
//! D2 for why not at-least-once).

use sqlx::PgPool;
use sqlx::Row;
use time::OffsetDateTime;

use crate::DbError;

/// The terminal states: exactly the set `notify_task_completed` fires on
/// (migration 0005, widened with `refused` by 0012), which is the set a
/// channel peer is replied to for.
///
/// One Rust copy, shared by the claim, the backlog read and
/// `turns::conversation_turns`. Migration 0027's backfill and partial index
/// hold reviewed SQL copies; `db/tests/reply_claim_e2e.rs` pins the index
/// against this const. **If the trigger's list is ever widened, this list and
/// 0027's move with it** — machine-checking the trigger is #712.
pub const REPLIED_STATES: [&str; 7] = [
    "completed", "failed", "cancelled", "blocked", "timed_out", "crashed", "refused",
];

/// How a reply was settled by the claim. (`backfilled` exists only in
/// migration 0027: no code path settles a task that way.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplyDisposition {
    /// Handed to the owning channel's outbound queue.
    Routed,
    /// A channel task with no routing metadata: there is no one to reply to.
    Unroutable,
}

impl ReplyDisposition {
    /// The stored spelling, matching 0027's CHECK.
    pub fn as_sql(self) -> &'static str {
        match self {
            Self::Routed => "routed",
            Self::Unroutable => "unroutable",
        }
    }
}

/// What the claim's winner learns about the task: enough to tell how late
/// its reply is (`core::channel::catch_up::lateness`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClaimedReply {
    pub created_at: OffsetDateTime,
    pub finished_at: Option<OffsetDateTime>,
}

fn states() -> Vec<String> {
    REPLIED_STATES.iter().map(|s| (*s).to_string()).collect()
}

/// Settle `task_id`'s reply as `d`. `Ok(None)` means someone else already
/// settled it (the other bus, the other path, an earlier sweep) **or** the
/// task has not finished — and a task that has not finished must never be
/// settled, or its real completion would find it already claimed.
pub async fn claim_reply(
    pool: &PgPool,
    task_id: i64,
    d: ReplyDisposition,
) -> Result<Option<ClaimedReply>, DbError> {
    let row = sqlx::query(
        "UPDATE tasks SET reply_settled_at = now(), reply_disposition = $2 \
          WHERE id = $1 AND reply_settled_at IS NULL AND state = ANY($3) \
          RETURNING created_at, finished_at",
    )
    .bind(task_id)
    .bind(d.as_sql())
    .bind(states())
    .fetch_optional(pool)
    .await
    .map_err(|e| DbError::Query(format!("tasks claim_reply: {e}")))?;
    let Some(row) = row else { return Ok(None) };
    Ok(Some(ClaimedReply {
        created_at: row
            .try_get("created_at")
            .map_err(|e| DbError::Query(format!("decode tasks.created_at: {e}")))?,
        finished_at: row
            .try_get("finished_at")
            .map_err(|e| DbError::Query(format!("decode tasks.finished_at: {e}")))?,
    }))
}

/// Up to `limit` ids of finished channel tasks whose reply is unsettled, with
/// `id > after_id`, ascending — so a sweep pages through the backlog oldest
/// first, and a caller that skips an id still moves past it. The predicate is
/// 0027's partial index's, so this reads only the backlog.
pub async fn unsettled_channel_replies(
    pool: &PgPool,
    after_id: i64,
    limit: i64,
) -> Result<Vec<i64>, DbError> {
    sqlx::query_scalar(
        "SELECT id FROM tasks \
          WHERE reply_settled_at IS NULL \
            AND payload->>'kind' = 'channel' \
            AND state = ANY($1) \
            AND id > $2 \
          ORDER BY id \
          LIMIT $3",
    )
    .bind(states())
    .bind(after_id)
    .bind(limit.max(0))
    .fetch_all(pool)
    .await
    .map_err(|e| DbError::Query(format!("tasks unsettled_channel_replies: {e}")))
}
