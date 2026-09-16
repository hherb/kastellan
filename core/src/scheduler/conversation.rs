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

pub(crate) mod record;
