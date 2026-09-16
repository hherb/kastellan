//! The conversation lookup: the earlier turns of one channel conversation.
//!
//! A channel message becomes a task carrying only its own sentence (#701), so
//! a follow-up has no referent unless something hands it the turns before it.
//! This is that something: one windowed query, and the only place the
//! conversation keys are read out of `tasks.payload`.
//!
//! # Why the anchor is the caller's timestamp and not `now()`
//!
//! The caller passes `before`, which is the new task's `created_at`. A task
//! that suspends on an operator ask and resumes hours later then sees the
//! conversation as it stood when its message arrived, rather than losing turns
//! (and the classification it inherits from them) or gaining turns that
//! finished while it waited. Turns are immutable once terminal, so the same
//! call returns the same rows every time that task runs.
//!
//! # Why this state list
//!
//! [`REPLIED_STATES`] is exactly the set `notify_task_completed` fires on
//! (migration `0005`, widened with `refused` by `0012`), which is the set the
//! outbound pump replies to. So every row this can return is a turn the peer
//! actually received a reply for — which is what makes the rendered `answer`
//! truthful. **If that trigger's list is ever widened, this list moves with
//! it**; `a_crashed_turn_is_returned_and_carries_no_result` is the test that
//! names the coupling.

use sqlx::PgPool;
use sqlx::Row;
use time::OffsetDateTime;

use crate::DbError;

/// One earlier turn of a conversation, exactly as stored.
///
/// Deliberately raw: rendering an answer out of `result` belongs to
/// `core::channel::route::reply_body` (the same function the bus used to
/// deliver that answer), and interpreting `turn_record` belongs to
/// `core::scheduler::conversation`. This crate stays a typed CRUD layer.
#[derive(Clone, Debug)]
pub struct ConversationTurnRow {
    pub task_id: i64,
    pub finished_at: OffsetDateTime,
    pub payload: serde_json::Value,
    pub result: Option<serde_json::Value>,
    pub turn_record: Option<serde_json::Value>,
}

/// The terminal states a channel peer was replied to for. See the module docs:
/// this mirrors `notify_task_completed`.
const REPLIED_STATES: [&str; 7] = [
    "completed", "failed", "cancelled", "blocked", "timed_out", "crashed", "refused",
];

/// Up to `limit` turns of `(channel, peer, conversation)` that finished within
/// `window_hours` before `before`, newest first.
///
/// `exclude_task_id` is the task doing the asking: it must never read itself.
///
/// Keyed on `peer` as well as `conversation` so that if a room ever holds a
/// second paired peer, one peer's turns are never shown to the other.
pub async fn conversation_turns(
    pool: &PgPool,
    channel: &str,
    peer: &str,
    conversation: &str,
    before: OffsetDateTime,
    exclude_task_id: i64,
    window_hours: i64,
    limit: i64,
) -> Result<Vec<ConversationTurnRow>, DbError> {
    let states: Vec<String> = REPLIED_STATES.iter().map(|s| (*s).to_string()).collect();
    let rows = sqlx::query(
        "SELECT id, finished_at, payload, result, turn_record \
           FROM tasks \
          WHERE payload->>'kind' = 'channel' \
            AND payload->>'channel' = $1 \
            AND payload->>'peer' = $2 \
            AND payload->>'conversation' = $3 \
            AND id <> $4 \
            AND state = ANY($5) \
            AND finished_at IS NOT NULL \
            AND finished_at <= $6 \
            AND finished_at >= $6 - make_interval(hours => $7::int) \
          ORDER BY finished_at DESC \
          LIMIT $8",
    )
    .bind(channel)
    .bind(peer)
    .bind(conversation)
    .bind(exclude_task_id)
    .bind(&states)
    .bind(before)
    .bind(i32::try_from(window_hours).unwrap_or(i32::MAX))
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| DbError::Query(format!("tasks conversation_turns: {e}")))?;

    rows.iter()
        .map(|row| {
            Ok(ConversationTurnRow {
                task_id: row
                    .try_get("id")
                    .map_err(|e| DbError::Query(format!("decode tasks.id: {e}")))?,
                finished_at: row
                    .try_get("finished_at")
                    .map_err(|e| DbError::Query(format!("decode tasks.finished_at: {e}")))?,
                payload: row
                    .try_get("payload")
                    .map_err(|e| DbError::Query(format!("decode tasks.payload: {e}")))?,
                result: row
                    .try_get("result")
                    .map_err(|e| DbError::Query(format!("decode tasks.result: {e}")))?,
                turn_record: row
                    .try_get("turn_record")
                    .map_err(|e| DbError::Query(format!("decode tasks.turn_record: {e}")))?,
            })
        })
        .collect()
}
