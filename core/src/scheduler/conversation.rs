//! Conversational continuity for channel tasks (#701).
//!
//! # The defect
//!
//! Every channel message becomes a task carrying only its own sentence. The
//! conversation id on the payload routes the reply and nothing else, and
//! memory recall contributes nothing to a bare follow-up (`recall_count: 0`
//! on both tasks of the measured pair). So "from where to where did the last
//! 3 flight bookings go?" arrives with no referent: measured live on
//! 2026-09-14, where the follow-up searched from scratch and answered about a
//! different booking than the turn it was following up on.
//!
//! # The shape of the fix
//!
//! * [`record`] — what a finishing turn leaves behind: the successful tool
//!   calls it made, and the most sensitive data class it touched. Written to
//!   `tasks.turn_record` by `kastellan_db::tasks::finalize`.
//! * [`view`] — what the next turn reads: a bounded, screened array of earlier
//!   turns, each `{at, user, calls, answer}`.
//! * [`floor`] — the classification a follow-up inherits from those turns.
//!
//! Everything here is **data, never instructions** by the time it reaches the
//! planner, including the user's own earlier messages: the referent is looked
//! up in the block, while the direction comes from the current `instruction`.
//!
//! The calls are carried but the **results are not**. The identifier the next
//! turn needs is the one the last turn *passed* (`{message_id, filename}`), so
//! carrying parameters is both smaller and more useful than carrying results,
//! and no tool output crosses a task boundary.

pub(crate) mod floor;
pub(crate) mod record;
pub(crate) mod view;

use time::OffsetDateTime;

use self::record::TurnRecord;

/// One earlier turn of this conversation, assembled from its stored row.
///
/// `answer` is what `channel::route::reply_body` renders from the task's
/// result — the same pure function the bus used to deliver it — so the planner
/// reads the sentence the user actually saw, including the fixed sentences for
/// a refused, denied or failed task.
#[derive(Clone, Debug)]
pub(crate) struct Turn {
    pub(crate) task_id: i64,
    pub(crate) finished_at: OffsetDateTime,
    pub(crate) user: String,
    pub(crate) answer: String,
    /// `None` for a turn that finished before #701 shipped, or whose stored
    /// record could not be parsed. Such a turn still contributes its text.
    pub(crate) record: Option<TurnRecord>,
}

/// Load the earlier turns of `dest`'s conversation, oldest first.
///
/// `before` is the asking task's `created_at`, not `now()`: see
/// [`kastellan_db::tasks::turns`] for why the anchor matters.
///
/// A row whose stored `turn_record` will not parse yields a turn with no calls
/// and a `warn!` — never an error. The record is a convenience for the next
/// turn, and a schema change must not make a whole conversation unreadable.
/// `a_malformed_stored_record_renders_without_calls` is the positive control
/// proving that arm is reachable *and* that a well-formed record does not take
/// it.
pub(crate) async fn load_conversation(
    pool: &sqlx::PgPool,
    dest: &crate::channel::ask_message::AskDestination,
    task_id: i64,
    before: OffsetDateTime,
) -> Result<Vec<Turn>, kastellan_db::DbError> {
    let rows = kastellan_db::tasks::turns::conversation_turns(
        pool,
        kastellan_db::tasks::turns::ConversationQuery {
            channel: &dest.channel.0,
            peer: &dest.peer.0,
            conversation: &dest.conversation.0,
            before,
            exclude_task_id: task_id,
            window_hours: view::WINDOW_HOURS,
            limit: view::MAX_TURNS,
        },
    )
    .await?;

    let mut turns: Vec<Turn> = rows.into_iter().map(turn_from_row).collect();

    // The query returns newest first; the planner reads oldest first, so the
    // newest turn sits closest to the current instruction.
    turns.reverse();
    Ok(turns)
}

/// One stored row as a [`Turn`]. Split out so the parse-failure arm is
/// reachable from a unit test without a live Postgres.
fn turn_from_row(row: kastellan_db::tasks::turns::ConversationTurnRow) -> Turn {
    let record = match row.turn_record {
        None => None,
        Some(value) => match serde_json::from_value::<TurnRecord>(value) {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::warn!(
                    task_id = row.task_id, error = %e,
                    "stored turn_record did not parse; carrying this turn's text only"
                );
                None
            }
        },
    };
    Turn {
        task_id: row.task_id,
        finished_at: row.finished_at,
        user: row
            .payload
            .get("instruction")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        answer: crate::channel::route::reply_body(row.result.as_ref()),
        record,
    }
}

#[cfg(test)]
mod tests;
