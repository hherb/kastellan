//! The session's request/response methods: each parses its params
//! ([`super::params`]), calls the same helper the one-shot CLI command uses,
//! and returns the result as JSON. `task.follow` is not here: it answers at
//! once and then streams, so the session loop runs it (`super::follow`).
//!
//! Errors: bad params are `INVALID_PARAMS` (-32602); a request the state of
//! the world refuses (no such task, not running, already answered) is
//! `OPERATION_FAILED` (-32002) with the reason; a database failure is
//! `INTERNAL_ERROR` (-32603), so a client may retry it.

use kastellan_core::cli_audit::{
    cancel_and_audit, correct_and_audit, submit_and_audit, CancelOutcome, CorrectOutcome,
};
use kastellan_protocol::{codes, RpcError};
use serde_json::{json, Value};
use sqlx::PgPool;

use super::params;

fn refused(msg: impl Into<String>) -> RpcError {
    RpcError::new(codes::OPERATION_FAILED, msg)
}

fn internal(e: impl std::fmt::Display) -> RpcError {
    RpcError::new(codes::INTERNAL_ERROR, e.to_string())
}

/// Every method name the session serves, `task.follow` included — for the
/// unknown-method error to list.
pub(super) const METHODS: [&str; 7] =
    ["task.submit", "task.follow", "task.correct", "task.cancel", "task.status", "inbox.list", "inbox.answer"];

/// Run one request/response method. `task.follow` never reaches here.
pub(super) async fn call(pool: &PgPool, method: &str, p: Value) -> Result<Value, RpcError> {
    match method {
        "task.submit" => submit(pool, params::submit(p)?).await,
        "task.correct" => {
            let (task_id, text) = params::correct(p)?;
            correct(pool, task_id, &text).await
        }
        "task.cancel" => cancel(pool, params::task_id(p)?).await,
        "task.status" => status(pool, params::task_id(p)?).await,
        "inbox.list" => inbox_list(pool, params::inbox_limit(p)?).await,
        "inbox.answer" => inbox_answer(pool, params::answer(p)?).await,
        other => Err(RpcError::new(
            codes::METHOD_NOT_FOUND,
            format!("no such method: {other} (the session serves {})", METHODS.join(", ")),
        )),
    }
}

/// `task.submit` → `{task_id, lane}`. The same payload and audit row as
/// `kastellan-cli ask` (operator trust: the operator's own OS user).
async fn submit(pool: &PgPool, s: params::Submit) -> Result<Value, RpcError> {
    let payload = crate::ask::submission_payload(&s.instruction, s.floor);
    let task_id = submit_and_audit(pool, s.lane, payload).await.map_err(internal)?;
    Ok(json!({"task_id": task_id, "lane": s.lane.as_sql()}))
}

/// `task.correct` → `{seq, audited}`. Only a running task takes one, and
/// only a notebook-mode daemon reads it (see `tasks correct`).
async fn correct(pool: &PgPool, task_id: i64, text: &str) -> Result<Value, RpcError> {
    match correct_and_audit(pool, task_id, text).await.map_err(internal)? {
        CorrectOutcome::Added { seq, audited } => Ok(json!({"seq": seq, "audited": audited})),
        CorrectOutcome::NotRunning => Err(refused(format!("task {task_id} is not running"))),
    }
}

/// `task.cancel` → `{state}` (the task's state after the cancel).
async fn cancel(pool: &PgPool, task_id: i64) -> Result<Value, RpcError> {
    match cancel_and_audit(pool, task_id).await.map_err(internal)? {
        CancelOutcome::Cancelled(t) => Ok(json!({"state": t.state})),
        CancelOutcome::NotCancellable => {
            Err(refused(format!("task {task_id} does not exist or has already ended")))
        }
    }
}

/// `task.status` → `{task_id, state, lane, plan_count, result, notebook, notebook_error}`,
/// where `notebook` is the fold (`notebook::view::fold_view`). A notebook
/// the fold refuses is `notebook: null` with `notebook_error` saying why —
/// the task's state is still worth answering.
async fn status(pool: &PgPool, task_id: i64) -> Result<Value, RpcError> {
    let task = kastellan_db::tasks::get(pool, task_id)
        .await
        .map_err(internal)?
        .ok_or_else(|| refused(format!("no task {task_id}")))?;
    let entries = kastellan_db::notebook::entries(pool, task_id, 0).await.map_err(internal)?;
    let (notebook, notebook_error) = match kastellan_core::scheduler::notebook::view::fold_view(&entries) {
        Ok(v) => (v, Value::Null),
        Err(e) => (Value::Null, Value::String(e)),
    };
    Ok(json!({
        "task_id": task.id,
        "state": task.state,
        "lane": task.lane.as_sql(),
        "plan_count": task.plan_count,
        "result": task.result,
        "notebook": notebook,
        "notebook_error": notebook_error,
    }))
}

/// `inbox.list` → `{asks: [{ask_id, task_id, kind, question, options, deadline_at}]}`,
/// oldest first, as `kastellan-cli inbox list` — but the whole question.
async fn inbox_list(pool: &PgPool, limit: i64) -> Result<Value, RpcError> {
    let asks = kastellan_db::asks::list_pending(pool, limit).await.map_err(internal)?;
    let rfc3339 = |t: time::OffsetDateTime| t.format(&time::format_description::well_known::Rfc3339).ok();
    let asks: Vec<Value> = asks
        .iter()
        .map(|a| {
            json!({
                "ask_id": a.id,
                "task_id": a.task_id,
                "kind": a.kind,
                "question": a.body,
                "options": a.options,
                "deadline_at": rfc3339(a.deadline_at),
            })
        })
        .collect();
    Ok(json!({"asks": asks}))
}

/// `inbox.answer` → `{task_id, audited, audit_error}`; the task returns to
/// the queue. `audit_error` says why the `ask.resolved` row was lost.
async fn inbox_answer(pool: &PgPool, a: params::Answer) -> Result<Value, RpcError> {
    use crate::inbox::ResolveError;
    let r = crate::inbox::resolve_and_audit(pool, a.ask_id, &a.choice, a.note.as_deref())
        .await
        .map_err(|e| match e {
            ResolveError::Refused(m) => refused(m),
            ResolveError::Db(m) => internal(m),
        })?;
    Ok(json!({"task_id": r.task_id, "audited": r.audit_error.is_none(), "audit_error": r.audit_error}))
}
