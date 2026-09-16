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

/// Most turns of a conversation the planner is shown.
pub(crate) const MAX_TURNS: i64 = 3;

/// How far back a turn may have finished and still count as part of this
/// conversation. Five hours: a follow-up is nearly always minutes later, and a
/// window this size keeps yesterday's topic from steering today's question
/// without needing an explicit reset command.
pub(crate) const WINDOW_HOURS: i64 = 5;

use self::record::TurnRecord;
use crate::cassandra::types::DataClass;

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
    /// record could not be parsed. Such a turn contributes no calls.
    pub(crate) record: Option<TurnRecord>,
    /// The most sensitive class this turn touched, parsed **independently of
    /// `record`** — and the reason that matters is a security one.
    ///
    /// The floor a follow-up inherits comes from here, while the text it is
    /// shown comes from `user`/`answer`. If one field could fail while the
    /// other survived, a turn's *content* would cross the task boundary while
    /// its *classification* did not: a clinical answer rendered into a
    /// follow-up running at `Public`, where rule I2 then admits `Public`
    /// steps against it. Parsing this key on its own means a future change to
    /// the shape of `calls` can cost the calls but never the class.
    ///
    /// `None` only when the stored record is absent (a turn that finished
    /// before migration 0026) or carries no readable `data_class`. Such a turn
    /// is **not shown at all** — see [`view::render`]. A turn we cannot
    /// classify is strictly worse than a turn we do not carry.
    pub(crate) data_class: Option<DataClass>,
}

/// Load the earlier turns of `dest`'s conversation, oldest first.
///
/// `created_at` is the asking task's own arrival time, which anchors the back
/// edge of the window: see [`kastellan_db::tasks::turns`] for why the window is
/// anchored there and open at the top.
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
    created_at: OffsetDateTime,
) -> Result<Vec<Turn>, kastellan_db::DbError> {
    let rows = kastellan_db::tasks::turns::conversation_turns(
        pool,
        kastellan_db::tasks::turns::ConversationQuery {
            channel: &dest.channel.0,
            peer: &dest.peer.0,
            conversation: &dest.conversation.0,
            window_anchor: created_at,
            exclude_task_id: task_id,
            window_hours: WINDOW_HOURS,
            limit: MAX_TURNS,
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
    // The class first, and on its own: see `Turn::data_class` for why it must
    // not share a failure mode with the calls.
    let data_class = row
        .turn_record
        .as_ref()
        .and_then(|v| v.get("data_class"))
        .and_then(|v| serde_json::from_value::<DataClass>(v.clone()).ok());
    if row.turn_record.is_some() && data_class.is_none() {
        tracing::warn!(
            task_id = row.task_id,
            "stored turn_record carries no readable data_class; this turn will not be shown"
        );
    }
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
        data_class,
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

/// Everything a claimed channel task needs from its own conversation.
///
/// Returned as one value rather than a tuple because the three parts must move
/// together: the rendered turns are what the planner reads, the floor is what
/// bounds the steps it may plan against them, and the ids are what the audit
/// row says the plan was built on. Splitting them at the call site is how they
/// drift.
pub(crate) struct LoadedConversation {
    /// Rendered, screened, budgeted turns — oldest first. Empty for a
    /// non-channel task, a first message, or a failed read.
    pub(crate) turns: Vec<serde_json::Value>,
    /// The floor this task should run at, and where that floor came from.
    pub(crate) floor: (DataClass, crate::scheduler::inner_loop::ClassificationFloorSource),
    /// The turns' ids for the audit row: `Some(vec![])` when the lookup found
    /// nothing, **`None` when it failed** — absence and loss must not render
    /// identically.
    pub(crate) task_ids: Option<Vec<i64>>,
    /// Turns the screen blocked, for the forensic rows the caller writes.
    pub(crate) blocks: Vec<view::ConversationBlock>,
}

/// Load, screen, budget and classify a claimed task's conversation.
///
/// Lifted out of `runner::task_exec::run_one`, which is already over the
/// 500-line guidance and where this logic could only be exercised through a
/// live scheduler. Here it is one call with a value in and a value out.
///
/// **A failed read fails open**: the task plans with no conversation, exactly
/// as every channel task did before #701. That is safe precisely because
/// nothing is carried — there is no content whose classification we would be
/// guessing at — and `task_ids: None` records the loss.
pub(crate) async fn load_for_task(
    pool: &sqlx::PgPool,
    task: &kastellan_db::tasks::Task,
    origin: Option<&crate::channel::ask_message::AskDestination>,
    payload_floor: DataClass,
    payload_source: crate::scheduler::inner_loop::ClassificationFloorSource,
) -> LoadedConversation {
    let Some(dest) = origin else {
        return LoadedConversation {
            turns: Vec::new(),
            floor: (payload_floor, payload_source),
            task_ids: Some(Vec::new()),
            blocks: Vec::new(),
        };
    };

    let turns = match load_conversation(pool, dest, task.id, task.created_at).await {
        Ok(turns) => turns,
        Err(e) => {
            tracing::warn!(
                task_id = task.id, error = %e,
                "could not read this conversation's earlier turns; planning without them"
            );
            return LoadedConversation {
                turns: Vec::new(),
                floor: (payload_floor, payload_source),
                task_ids: None,
                blocks: Vec::new(),
            };
        }
    };

    let task_ids: Vec<i64> = turns.iter().map(|t| t.task_id).collect();
    // ⚠️ Inherited from every turn LOADED, before the screen and the budget
    // have their say. Inheriting from the turns that SURVIVED rendering would
    // let one catalogue phrase in an earlier answer both withhold that turn
    // and drop the whole conversation's floor — handing an attacker the choice
    // of what the follow-up may do.
    let floor = floor::inherit_floor(payload_floor, payload_source, &turns);
    let rendered = view::render(&turns, view::CONVERSATION_BUDGET);

    LoadedConversation {
        turns: rendered.turns,
        floor,
        task_ids: Some(task_ids),
        blocks: rendered.blocks,
    }
}

/// The `policy / injection.blocked` payload for a turn this screen withheld.
///
/// Pure, so the shape is testable without a pool: the caller writes the row.
/// Carries the hash and the length, never the screened text.
pub(crate) fn block_audit_payload(
    task_id: i64,
    block: &view::ConversationBlock,
) -> serde_json::Value {
    serde_json::json!({
        "task_id":       task_id,
        "turn_task_id":  block.task_id,
        "score":         block.score,
        "decision":      "block",
        "tier":          view::TIER_CONVERSATION,
        "reason_codes":  block.reason_codes,
        "body_sha256":   block.body_sha256,
        "body_byte_len": block.body_byte_len,
    })
}
