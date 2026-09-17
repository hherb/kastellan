//! The conversation lookup: the earlier turns of one channel conversation.
//!
//! A channel message becomes a task carrying only its own sentence (#701), so
//! a follow-up has no referent unless something hands it the turns before it.
//! This is that something: one windowed query, and the only place the
//! conversation keys are read out of `tasks.payload` **for the conversation
//! lookup**. They are read elsewhere for other purposes — `asks::…` filters
//! `channel`/`peer` when routing an operator ask, and
//! `core::channel::ask_message::destination_from_task_payload` reads all three
//! to build the `AskDestination` this query is then called with.
//!
//! # Why the window is anchored on the caller's timestamp, and open at the top
//!
//! The caller passes `window_anchor`, the new task's `created_at`, and the
//! window reaches back `window_hours` from there. Anchoring the **back edge**
//! on the task's own arrival is what keeps a suspended task whole: one that
//! waits hours on an operator ask and then resumes still sees every turn it
//! saw at first planning, so it can never lose a turn — or the classification
//! it inherits from that turn — by being made to wait.
//!
//! There is deliberately **no upper bound**. An earlier design bounded turns
//! at `finished_at <= window_anchor`, which blinds the case this whole feature
//! exists for: a live turn takes 2.5-4.5 minutes, so a user who types a
//! follow-up while the bot is still working would produce a task whose
//! `created_at` precedes the previous turn's `finished_at` — and that
//! follow-up would see an empty conversation and start from scratch, which is
//! #701 reproducing under its own fix. Dropping the bound means a resumed task
//! may *gain* a turn that finished while it was suspended. That direction is
//! safe: floor inheritance only ever raises, and everything carried reaches
//! the planner as fenced data. Losing a turn is the defect; gaining one is the
//! conversation moving on.
//!
//! # Why this state list
//!
//! [`REPLIED_STATES`] is exactly the set `notify_task_completed` fires on
//! (migration `0005`, widened with `refused` by `0012`), which is the set the
//! outbound pump replies to. So every row this can return is a turn the peer
//! actually received a reply for — which is what makes the rendered `answer`
//! truthful. **If that trigger's list is ever widened, this list moves with
//! it.** `every_replied_to_state_is_a_turn_and_an_unfinished_one_is_not`
//! covers all seven plus a negative — but it hand-copies them as literals, so
//! it catches a NARROWING of this const and is blind to a WIDENING of the SQL
//! trigger, which is the direction that actually loses turns. **The coupling
//! is enforced by review, not by a test**; machine-checking it against
//! `pg_get_functiondef` is #712.

use sqlx::PgPool;
use sqlx::Row;
use time::OffsetDateTime;

use crate::DbError;

/// One earlier turn of a conversation, exactly as stored.
///
/// Note the granularity: a decode failure on ANY column of ANY row fails the
/// whole lookup, which `core::scheduler::conversation::load_for_task` then
/// fails open on. The per-row fail-safe its docs promise ("a schema change
/// must not make a whole conversation unreadable") applies to `turn_from_row`
/// parsing `turn_record`, not to this decode — every column here is a fixed
/// type on a fixed schema, so a failure means the table shape moved, not that
/// one row is odd.
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

/// Which conversation to read, and how much of it.
///
/// A struct rather than seven positional parameters, and that is load-bearing
/// rather than cosmetic: `channel`, `peer` and `conversation` are all `&str`,
/// so a positional call silently survives any two of them being swapped — and
/// swapping `peer` with `conversation` would hand one peer another peer's
/// turns. Named fields make that transposition a compile error. Same reasoning
/// as `ClassificationProvenance` and #545's `AllowlistDecl`.
#[derive(Clone, Copy, Debug)]
pub struct ConversationQuery<'a> {
    pub channel: &'a str,
    pub peer: &'a str,
    pub conversation: &'a str,
    /// The asking task's `created_at`: the window reaches back
    /// `window_hours` from here. See the module docs for why the back edge is
    /// anchored here and why there is no forward edge.
    pub window_anchor: OffsetDateTime,
    /// The task doing the asking. It must never read itself.
    pub exclude_task_id: i64,
    pub window_hours: i64,
    pub limit: i64,
}

/// Up to `q.limit` turns of `(channel, peer, conversation)` that finished no
/// earlier than `q.window_hours` before `q.window_anchor`, newest first.
///
/// `id DESC` breaks a `finished_at` tie so the `LIMIT` cannot cut
/// non-deterministically between two turns that finished in the same
/// microsecond. Effectively unreachable with `now()`, and free.
pub async fn conversation_turns(
    pool: &PgPool,
    q: ConversationQuery<'_>,
) -> Result<Vec<ConversationTurnRow>, DbError> {
    // An out-of-range window is an error, not a fallback. This was
    // `unwrap_or(i32::MAX)`, which silently turns the window into ~245,000
    // years — i.e. removes the only bound this parameter exists to impose,
    // on the argument whose whole job is to keep yesterday's topic out of
    // today's question. Unreachable (`WINDOW_HOURS` is a const 5), which is
    // exactly why the fallback direction has to be the safe one.
    let window_hours = i32::try_from(q.window_hours).map_err(|_| {
        DbError::Query(format!(
            "tasks conversation_turns: window_hours {} out of range",
            q.window_hours,
        ))
    })?;
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
            AND finished_at >= $6 - make_interval(hours => $7::int) \
          ORDER BY finished_at DESC, id DESC \
          LIMIT $8",
    )
    .bind(q.channel)
    .bind(q.peer)
    .bind(q.conversation)
    .bind(q.exclude_task_id)
    .bind(&states)
    .bind(q.window_anchor)
    .bind(window_hours)
    .bind(q.limit)
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
