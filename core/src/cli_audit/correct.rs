//! `tasks correct`: an operator correction to a running task (#922, D11).

use kastellan_db::notebook::{self, EntryKind};
use kastellan_db::tasks;
use kastellan_db::DbError;
use sqlx::PgPool;

use crate::cli_audit::CLI_AUDIT_ACTOR;
use crate::scheduler::notebook::state::CorrectionPayload;

/// The audit action of a correction.
pub const ACTION_TASK_CORRECTED: &str = "task.corrected";

/// Outcome of [`correct_and_audit`].
#[derive(Debug, PartialEq, Eq)]
pub enum CorrectOutcome {
    /// The correction is entry `seq` of the task's notebook. `audited` is
    /// false when its `task.corrected` row could not be written: the CLI
    /// has no log to put that in, so it tells the operator.
    Added { seq: i64, audited: bool },
    /// The task does not exist or is not `running`; nothing was written.
    NotRunning,
}

/// Append `text` to running task `task_id`'s notebook as a `correction`,
/// and write one `actor='cli' action='task.corrected'` row. The next
/// notebook turn shows it to the planner, and it joins the instruction the
/// reviewer sees from then on. The row carries the entry's `seq` and the
/// text's length, never the text: the notebook holds it.
///
/// The state check and the append are not one transaction. A task that
/// finishes in between keeps the correction as a late entry, which the fold
/// allows and nothing reads — the same outcome as typing it a moment later.
/// The audit insert is best-effort (chokepoint posture): the entry has
/// already landed, and a lost row is reported in the outcome.
pub async fn correct_and_audit(pool: &PgPool, task_id: i64, text: &str) -> Result<CorrectOutcome, DbError> {
    match tasks::get(pool, task_id).await? {
        Some(t) if t.state == "running" => {}
        _ => return Ok(CorrectOutcome::NotRunning),
    }
    let entry = crate::scheduler::notebook::state::entry(CorrectionPayload { text: text.to_string() });
    let seq = notebook::append(pool, task_id, EntryKind::Correction, entry).await?;
    let payload = serde_json::json!({"task_id": task_id, "seq": seq, "chars": text.chars().count()});
    let audited = match kastellan_db::audit::insert(pool, CLI_AUDIT_ACTOR, ACTION_TASK_CORRECTED, payload).await {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(task_id, error = %e, "task.corrected audit insert failed (correction already added)");
            false
        }
    };
    Ok(CorrectOutcome::Added { seq, audited })
}
