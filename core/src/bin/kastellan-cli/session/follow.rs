//! `task.follow`: stream one task's notebook to the client as it fills in,
//! then say how the task ended.
//!
//! The loop, once per wake-up:
//!
//! 1. read the task's state;
//! 2. send every notebook entry after the last one sent (`notebook.entry`);
//! 3. if the state is terminal, send `task.finished` and stop;
//! 4. if the task just parked on an operator ask, send
//!    `task.awaiting_operator` (once per parking);
//! 5. sleep until a NOTIFY for this task or the next poll tick.
//!
//! Reading the state **before** the entries is what makes step 3 safe: the
//! planner appends its last entry before it marks the task terminal, so once
//! a terminal state is visible every entry written before it is too.
//!
//! The NOTIFYs (`task_notebook`, `tasks_completed`) only make it prompt; the
//! poll tick is what makes it correct. `PgListener` drops notifications
//! while it reconnects, and nothing notifies a task parking in
//! `awaiting_operator` (see `ask.rs`), so a follow that trusted the NOTIFYs
//! alone could miss an entry or wait forever.
//!
//! A follow that fails (the database went away) sends `task.follow_failed`
//! with the last `seq` it sent; the client resumes with `after_seq`.

use kastellan_db::notebook::{self, Entry};
use kastellan_db::tasks::{self, reply_claim::REPLIED_STATES, Task};
use serde_json::{json, Value};
use sqlx::postgres::PgListener;
use sqlx::PgPool;
use tokio::sync::mpsc::Sender;

use crate::ask::SUSPEND_POLL_SECS;

/// The `tasks_completed` NOTIFY channel (migration 0005): payload is the id.
const COMPLETED_CHANNEL: &str = "tasks_completed";

/// A JSON-RPC 2.0 notification: a message with no `id`, which the client
/// does not answer.
pub(super) fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

fn rfc3339(t: time::OffsetDateTime) -> Value {
    t.format(&time::format_description::well_known::Rfc3339).map(Value::String).unwrap_or(Value::Null)
}

/// `notebook.entry {task_id, seq, kind, payload, created_at}` — one row as
/// stored (an operator surface, design C2).
pub(super) fn entry_notification(e: &Entry) -> Value {
    notification(
        "notebook.entry",
        json!({
            "task_id": e.task_id,
            "seq": e.seq,
            "kind": e.kind.as_str(),
            "payload": e.payload,
            "created_at": rfc3339(e.created_at),
        }),
    )
}

/// `task.finished {task_id, state, answer, detail, result}`. `answer` is
/// the text an `ask` would print (a `kind: "text"` result's body), `detail`
/// the scheduler's reason for a task that did not complete (#923); either
/// is `null` when the result has none. `result` is the whole result, for
/// a kind neither covers.
pub(super) fn finished_notification(t: &Task) -> Value {
    let field = |key: &str| t.result.as_ref().and_then(|r| r.get(key)).and_then(Value::as_str);
    let answer = match field("kind") {
        Some("text") => field("body"),
        _ => None,
    };
    notification(
        "task.finished",
        json!({
            "task_id": t.id,
            "state": t.state,
            "answer": answer,
            "detail": field("detail"),
            "result": t.result,
        }),
    )
}

/// Whether a NOTIFY on `channel` with `payload` concerns `task_id`. The
/// notebook's payload is `"<task_id>:<seq>"` (0028), the completion's the
/// bare id (0005).
pub(super) fn wakes(channel: &str, payload: &str, task_id: i64) -> bool {
    let id = task_id.to_string();
    match channel {
        notebook::NOTIFY_CHANNEL => payload.split_once(':').is_some_and(|(t, _)| t == id),
        COMPLETED_CHANNEL => payload == id,
        _ => false,
    }
}

/// Follow `task_id` from `after_seq`, sending to `out` until the task ends,
/// the follow fails, or the client is gone (`out` closed).
pub(super) async fn follow(pool: PgPool, task_id: i64, after_seq: i64, out: Sender<Value>) {
    let mut last_seq = after_seq;
    if let Err(error) = follow_inner(&pool, task_id, &mut last_seq, &out).await {
        let params = json!({"task_id": task_id, "last_seq": last_seq, "error": error});
        let _ = out.send(notification("task.follow_failed", params)).await;
    }
}

/// `Err` is the reason the follow stopped early. A closed `out` is not one:
/// the client left, so there is no one to tell.
async fn follow_inner(pool: &PgPool, task_id: i64, last_seq: &mut i64, out: &Sender<Value>) -> Result<(), String> {
    // LISTEN before the first read, so nothing landing in between is missed.
    let mut listener = PgListener::connect_with(pool).await.map_err(|e| format!("listener connect: {e}"))?;
    listener
        .listen_all([notebook::NOTIFY_CHANNEL, COMPLETED_CHANNEL])
        .await
        .map_err(|e| format!("listen: {e}"))?;
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(SUSPEND_POLL_SECS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut parked = false;
    loop {
        let task = tasks::get(pool, task_id).await.map_err(|e| e.to_string())?.ok_or(format!("task {task_id} disappeared"))?;
        for e in notebook::entries(pool, task_id, *last_seq).await.map_err(|e| e.to_string())? {
            if out.send(entry_notification(&e)).await.is_err() {
                return Ok(());
            }
            *last_seq = e.seq;
        }
        if REPLIED_STATES.contains(&task.state.as_str()) {
            let _ = out.send(finished_notification(&task)).await;
            return Ok(());
        }
        let now_parked = task.state == "awaiting_operator";
        if now_parked && !parked {
            // The question waits in the inbox: `inbox.list` shows it.
            let params = json!({"task_id": task_id});
            if out.send(notification("task.awaiting_operator", params)).await.is_err() {
                return Ok(());
            }
        }
        parked = now_parked;
        // Sleep until something about this task, or the tick.
        loop {
            tokio::select! {
                n = listener.recv() => {
                    let n = n.map_err(|e| format!("listener: {e}"))?;
                    if wakes(n.channel(), n.payload(), task_id) { break; }
                }
                _ = tick.tick() => break,
                // The client left: stop holding a listener connection for it.
                _ = out.closed() => return Ok(()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_db::notebook::EntryKind;
    use kastellan_db::tasks::Lane;

    fn task(state: &str, result: Option<Value>) -> Task {
        let t0 = time::OffsetDateTime::UNIX_EPOCH;
        Task {
            id: 9, state: state.into(), lane: Lane::Fast, created_at: t0, updated_at: t0,
            started_at: None, finished_at: None, lease_expires_at: None, plan_count: 0,
            payload: json!({}), result,
        }
    }

    #[test]
    fn a_notebook_notify_wakes_only_its_own_task() {
        assert!(wakes("task_notebook", "9:3", 9));
        assert!(!wakes("task_notebook", "19:3", 9), "a prefix of another id is not this task");
        assert!(!wakes("task_notebook", "99:9", 9));
        assert!(!wakes("task_notebook", "9", 9), "a payload without its seq is not 0028's");
    }

    #[test]
    fn a_completion_notify_wakes_only_its_own_task() {
        assert!(wakes("tasks_completed", "9", 9));
        assert!(!wakes("tasks_completed", "90", 9));
        assert!(!wakes("tasks_resumed", "9", 9), "another channel never wakes a follow");
    }

    #[test]
    fn an_entry_is_sent_as_stored() {
        let e = Entry {
            task_id: 9, seq: 2, kind: EntryKind::Correction, payload: json!({"text": "other account"}),
            created_at: time::OffsetDateTime::UNIX_EPOCH,
        };
        assert_eq!(
            entry_notification(&e),
            json!({"jsonrpc": "2.0", "method": "notebook.entry", "params": {
                "task_id": 9, "seq": 2, "kind": "correction", "payload": {"text": "other account"},
                "created_at": "1970-01-01T00:00:00Z",
            }})
        );
    }

    #[test]
    fn a_completed_task_finishes_with_its_answer() {
        let n = finished_notification(&task("completed", Some(json!({"kind": "text", "body": "42 invoices"}))));
        assert_eq!(n["method"], "task.finished");
        assert_eq!(n["params"]["state"], "completed");
        assert_eq!(n["params"]["answer"], "42 invoices");
        assert_eq!(n["params"]["detail"], Value::Null);
        assert!(n.get("id").is_none(), "a notification carries no id");
    }

    #[test]
    fn a_failed_task_finishes_with_its_reason_and_no_answer() {
        let r = json!({"kind": "error", "detail": "not a plan", "body": "ignored"});
        let n = finished_notification(&task("failed", Some(r.clone())));
        assert_eq!(n["params"]["answer"], Value::Null, "only a text result is an answer");
        assert_eq!(n["params"]["detail"], "not a plan");
        assert_eq!(n["params"]["result"], r);
        let n = finished_notification(&task("cancelled", None));
        assert_eq!(n["params"]["result"], Value::Null);
    }
}
