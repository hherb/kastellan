//! `task.follow`: stream one task's notebook to the client as it fills in,
//! then say how the task ended.
//!
//! The loop, once per wake-up:
//!
//! 1. read the task's state;
//! 2. send every notebook entry after the last one sent (`notebook.entry`);
//! 3. if the state is terminal (and, for a cancel, the notebook has closed —
//!    below), send `task.finished` and stop;
//! 4. if the task just parked on an operator ask, send
//!    `task.awaiting_operator` (once per parking);
//! 5. sleep until a NOTIFY for this task or the next poll tick.
//!
//! Reading the state **before** the entries is what makes step 3 safe when
//! the planner ends the task itself: it appends its last entry before it
//! marks the task terminal, so once that state is visible every entry is too.
//!
//! A cancel is the exception: `task.cancel` sets `cancelled` first, and the
//! planner writes its `cancelled` entry (after the turn in flight, which may
//! write one more) only when it next looks. So a follow that sees
//! `cancelled` on a task with a notebook keeps going until that entry has
//! been sent, for at most its cancel grace (the session's is
//! [`CANCEL_GRACE`]) — a planner that died never writes it. A task cancelled
//! before its first entry, or followed with a zero grace (`ask`, which
//! prints no `cancelled` entry), finishes at once; anything its planner
//! writes later is in `task.status`.
//!
//! The NOTIFYs (`task_notebook`, `tasks_completed`) only make it prompt; the
//! poll tick is what makes it correct. One [`Listener`] per process receives
//! them for every follow (#987), so following many tasks costs one
//! connection, not one each. `PgListener` drops notifications
//! while it reconnects, and nothing notifies a task parking in
//! `awaiting_operator` (`running -> awaiting_operator` fires neither
//! `tasks_completed` nor `tasks_resumed`), so a follow that trusted the
//! NOTIFYs alone could miss an entry or wait forever.
//!
//! A follow that fails (the database went away) sends `task.follow_failed`
//! with the last `seq` it sent; the client resumes with `after_seq`.

use std::time::Duration;

use kastellan_db::notebook::{self, Entry};
use kastellan_db::tasks::{self, reply_claim::REPLIED_STATES, Task};
use serde_json::{json, Value};
use sqlx::postgres::PgListener;
use sqlx::PgPool;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::mpsc::Sender;

use crate::ask::SUSPEND_POLL_SECS;

/// The `tasks_completed` NOTIFY channel (migration 0005, trigger since 0012):
/// payload is the id.
const COMPLETED_CHANNEL: &str = "tasks_completed";

/// How long a session's follow of a cancelled task waits for the planner's
/// `cancelled` entry: one model turn, generously.
pub(super) const CANCEL_GRACE: Duration = Duration::from_secs(120);

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
/// the scheduler's reason for a task that did not complete, as `ask` prints
/// it ([`crate::ask::ended_detail`], #923); either is `null` when the result
/// has none. `result` is the whole result, for a kind neither covers.
pub(super) fn finished_notification(t: &Task) -> Value {
    let field = |key: &str| t.result.as_ref().and_then(|r| r.get(key)).and_then(Value::as_str);
    let answer = match field("kind") {
        Some("text") => field("body"),
        _ => None,
    };
    let detail = match answer {
        Some(_) => None,
        None => crate::ask::ended_detail(t.result.as_ref()),
    };
    notification(
        "task.finished",
        json!({
            "task_id": t.id,
            "state": t.state,
            "answer": answer,
            "detail": detail,
            "result": t.result,
        }),
    )
}

/// The task a NOTIFY on `channel` with `payload` concerns, if any. The
/// notebook's payload is `"<task_id>:<seq>"` (0028), the completion's the
/// bare id (0005).
pub(super) fn woken_task(channel: &str, payload: &str) -> Option<i64> {
    match channel {
        notebook::NOTIFY_CHANNEL => payload.split_once(':')?.0.parse().ok(),
        COMPLETED_CHANNEL => payload.parse().ok(),
        _ => None,
    }
}

/// How many wake-ups may queue for one follow. A follow that falls further
/// behind re-reads its task, which is all a wake-up asks of it anyway.
const WAKE_QUEUE: usize = 64;

/// The one `LISTEN` connection every follow in this process shares (#987).
/// A background task receives each NOTIFY and broadcasts the id of the task
/// it concerns; each follow keeps the ids of its own task. Dropping the
/// `Listener` stops that task and frees the connection.
pub(crate) struct Listener {
    wakes: broadcast::Sender<i64>,
    pump: tokio::task::JoinHandle<()>,
}

impl Listener {
    /// Connect and `LISTEN` on both channels. Call it before a follow's
    /// first read (and [`Listener::subscribe`] before spawning the follow),
    /// so a NOTIFY landing in between is not missed.
    pub(crate) async fn connect(pool: &PgPool) -> Result<Self, String> {
        let mut listener = PgListener::connect_with(pool).await.map_err(|e| format!("listener connect: {e}"))?;
        listener
            .listen_all([notebook::NOTIFY_CHANNEL, COMPLETED_CHANNEL])
            .await
            .map_err(|e| format!("listen: {e}"))?;
        let (wakes, _) = broadcast::channel(WAKE_QUEUE);
        let tx = wakes.clone();
        let pump = tokio::spawn(async move {
            // Whether the last `recv` failed, so each change is said once.
            let mut degraded = false;
            loop {
                match listener.recv().await {
                    Ok(n) => {
                        if degraded {
                            eprintln!("kastellan-cli: notify listener recovered; follows are woken promptly again");
                            degraded = false;
                        }
                        // No follow subscribed is not an error: nobody to wake.
                        if let Some(id) = woken_task(n.channel(), n.payload()) {
                            let _ = tx.send(id);
                        }
                    }
                    // sqlx reconnects (and re-LISTENs) inside `recv` when the
                    // connection is lost, so an `Err` here is a reconnect
                    // that failed (the database is down) or an error sqlx
                    // keeps the connection through, which can repeat for
                    // good. Either way the NOTIFYs are lost and every follow
                    // falls back to its poll tick: still correct, only
                    // slower, so it is said on stderr (stdout is the
                    // client's) rather than failing the follows. The wait
                    // keeps a database that is down from a hot retry loop.
                    Err(e) => {
                        if !degraded {
                            eprintln!(
                                "kastellan-cli: notify listener failed ({e}); follows poll every {SUSPEND_POLL_SECS} s until it recovers"
                            );
                            degraded = true;
                        }
                        tokio::time::sleep(Duration::from_secs(SUSPEND_POLL_SECS)).await
                    }
                }
            }
        });
        Ok(Self { wakes, pump })
    }

    /// A receiver of every wake-up from now on, for one follow.
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<i64> {
        self.wakes.subscribe()
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

/// Follow `task_id` from `after_seq`, sending to `out` until the task ends,
/// the follow fails, or the client is gone (`out` closed). `wakes` is this
/// follow's [`Listener::subscribe`], taken before it was spawned. A
/// cancelled task waits at most `cancel_grace` for its planner's
/// `cancelled` entry.
pub(crate) async fn follow(
    pool: PgPool,
    wakes: broadcast::Receiver<i64>,
    task_id: i64,
    after_seq: i64,
    cancel_grace: Duration,
    out: Sender<Value>,
) {
    let mut last_seq = after_seq;
    if let Err(error) = follow_inner(&pool, wakes, task_id, &mut last_seq, cancel_grace, &out).await {
        let params = json!({"task_id": task_id, "last_seq": last_seq, "error": error});
        let _ = out.send(notification("task.follow_failed", params)).await;
    }
}

/// `Err` is the reason the follow stopped early. A closed `out` is not one:
/// the client left, so there is no one to tell.
async fn follow_inner(
    pool: &PgPool,
    mut wakes: broadcast::Receiver<i64>,
    task_id: i64,
    last_seq: &mut i64,
    cancel_grace: Duration,
    out: &Sender<Value>,
) -> Result<(), String> {
    // First tick one period out: the loop's first read needs no tick, and
    // an immediate one would re-read at once for nothing.
    let poll = Duration::from_secs(SUSPEND_POLL_SECS);
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + poll, poll);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut parked = false;
    let mut cancel_deadline = None;
    loop {
        let task = tasks::get(pool, task_id).await.map_err(|e| e.to_string())?.ok_or(format!("task {task_id} disappeared"))?;
        // Decided before the entries are read, like the state: the notebook
        // only grows, so a `cancelled` entry this check saw is sent below.
        let finished = REPLIED_STATES.contains(&task.state.as_str())
            && (task.state != "cancelled" || {
                let deadline = *cancel_deadline.get_or_insert_with(|| tokio::time::Instant::now() + cancel_grace);
                tokio::time::Instant::now() >= deadline || cancel_closed(pool, task_id).await?
            });
        for e in notebook::entries(pool, task_id, *last_seq).await.map_err(|e| e.to_string())? {
            if out.send(entry_notification(&e)).await.is_err() {
                return Ok(());
            }
            *last_seq = e.seq;
        }
        if finished {
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
                w = wakes.recv() => match on_wake(w, task_id) {
                    Wake::Reread => break,
                    Wake::Ignore => {}
                    Wake::ListenerGone => return Err("the listener stopped".into()),
                },
                _ = tick.tick() => break,
                // The client left: stop polling for it.
                _ = out.closed() => return Ok(()),
            }
        }
    }
}

/// What a follow does with one message from its wake-up queue.
#[derive(Debug, PartialEq)]
enum Wake {
    /// Read the task again now.
    Reread,
    /// Another task's wake-up: keep waiting.
    Ignore,
    /// The queue is closed: stop the follow with an error.
    ListenerGone,
}

/// Decide what one wake-up message means for the follow of `task_id`.
fn on_wake(w: Result<i64, RecvError>, task_id: i64) -> Wake {
    match w {
        Ok(id) if id == task_id => Wake::Reread,
        Ok(_) => Wake::Ignore,
        // Fell behind: a missed wake-up may have been this task's.
        Err(RecvError::Lagged(_)) => Wake::Reread,
        // The `Listener` holds a sender itself, so this happens only once it
        // is dropped, when its owner is done with the follows (`session` aborts
        // them first; `ask` has stopped reading them). It is an error so a
        // follow can never wait on a queue nobody fills.
        Err(RecvError::Closed) => Wake::ListenerGone,
    }
}

/// Whether a cancelled task's notebook is closed: it has none (nothing will
/// write one), or its `cancelled` entry is in. Every entry is read, not just
/// the unsent ones: a follow resumed past that entry must not wait for it.
async fn cancel_closed(pool: &PgPool, task_id: i64) -> Result<bool, String> {
    let all = notebook::entries(pool, task_id, 0).await.map_err(|e| e.to_string())?;
    Ok(all.is_empty() || all.iter().any(|e| e.kind == notebook::EntryKind::Cancelled))
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
    fn a_notebook_notify_names_its_task() {
        assert_eq!(woken_task("task_notebook", "9:3"), Some(9));
        assert_eq!(woken_task("task_notebook", "19:3"), Some(19), "the id is the part before the colon");
        assert_eq!(woken_task("task_notebook", "99:9"), Some(99));
        assert_eq!(woken_task("task_notebook", "9"), None, "a payload without its seq is not 0028's");
        assert_eq!(woken_task("task_notebook", "x:3"), None);
    }

    #[test]
    fn a_follow_rereads_on_its_own_wake_or_a_lag_and_ignores_others() {
        assert_eq!(on_wake(Ok(9), 9), Wake::Reread);
        assert_eq!(on_wake(Ok(19), 9), Wake::Ignore, "another task's wake-up");
        assert_eq!(on_wake(Err(RecvError::Lagged(3)), 9), Wake::Reread, "a dropped wake-up may have been ours");
        assert_eq!(on_wake(Err(RecvError::Closed), 9), Wake::ListenerGone);
    }

    #[test]
    fn a_completion_notify_names_its_task() {
        assert_eq!(woken_task("tasks_completed", "9"), Some(9));
        assert_eq!(woken_task("tasks_completed", "90"), Some(90));
        assert_eq!(woken_task("tasks_completed", "9:3"), None, "a completion's payload is the bare id");
        assert_eq!(woken_task("tasks_resumed", "9"), None, "another channel never wakes a follow");
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

    #[test]
    fn a_blocked_or_refused_task_finishes_with_its_reason() {
        let r = json!({"kind": "blocked", "principle": "P1", "reason": "would mail a stranger"});
        assert_eq!(finished_notification(&task("blocked", Some(r)))["params"]["detail"], "would mail a stranger");
        let r = json!({"kind": "refused", "principle": "P2", "reason": "internal", "body": "I can't do that."});
        let n = finished_notification(&task("refused", Some(r)));
        assert_eq!(n["params"]["detail"], "I can't do that.", "a refusal's body is the text meant for the user");
        assert_eq!(n["params"]["answer"], Value::Null);
    }
}
