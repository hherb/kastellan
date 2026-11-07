//! Subprocess-level pin for `kastellan-cli session` (#922 slice 2): the real
//! CLI binary, driven over its stdin/stdout, against a per-test Postgres.
//!
//! No daemon runs. The test plays the scheduler itself — it claims the
//! task, appends notebook entries, raises an ask and finalises the task — so
//! what is pinned is the session's side: what it writes to the database for
//! each method, and what it sends back, in which order.
//!
//! 1. Submit/status/cancel, and the errors a client can make (a garbage
//!    line, an unknown method, bad params) — none of which ends the session.
//! 2. A follow sends the backlog, then each new entry as it lands (including
//!    a correction made over the same session while the follow runs), then
//!    `task.finished` with the answer; a second follow from `after_seq`
//!    sends only what came after.
//! 3. A follow of a task that parks on an operator ask says so, and the
//!    inbox methods list and answer the ask.
//! 4. A follow of a cancelled task waits for the planner's `cancelled`
//!    entry before it finishes.
//! 5. The follow cap, an over-cap line, and a dropped session leaving its
//!    tasks alone.
//! 6. A second parking is announced again; a follow whose reads fail says
//!    which `seq` it got to; submit and cancel report a lost audit row and
//!    a suppressed floor (#988).
//!
//! Skips with `[SKIP]` on hosts without Postgres or a supervisor (run with
//! `-- --nocapture` to see it), like `cli_inbox_e2e.rs`, whose PG bring-up
//! and CLI env `support.rs` copies. Once Postgres is up a failure fails.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::io::Write;
use std::time::Duration;

use kastellan_db::notebook::{self, EntryKind};
use kastellan_db::tasks::{self, insert_pending, Lane};
use serde_json::{json, Value};

#[path = "cli_session_e2e/support.rs"]
mod support;
use support::{assert_entry, bring_up_pg, seed_running_task, steps_payload, tick_payload, Session};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_status_cancel_and_the_errors_a_client_can_make() {
    let Some((pool, cluster)) = bring_up_pg("submit").await else { return };
    let mut s = Session::start(&cluster);

    let r = s.ok("task.submit", json!({"instruction": "find the invoice", "lane": "long", "classification_floor": "personal"}));
    let task_id = r["task_id"].as_i64().expect("a task id");
    assert_eq!(r["lane"], "long");
    let task = tasks::get(&pool, task_id).await.unwrap().expect("the row");
    assert_eq!(task.state, "pending");
    assert_eq!(task.lane, Lane::Long);
    assert_eq!(task.payload["instruction"], "find the invoice");
    assert_eq!(task.payload["kind"], "ask", "the same payload as `kastellan-cli ask`");
    assert_eq!(task.payload["classification_floor"], "Personal", "the operator's floor reaches the task");
    assert_eq!(task.payload["classification_floor_source"], "operator");
    let submitted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE actor = 'cli' AND action = 'task.submitted' \
         AND (payload->>'task_id')::bigint = $1",
    )
    .bind(task_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(submitted, 1, "one task.submitted row, actor=cli");

    let st = s.ok("task.status", json!({"task_id": task_id}));
    assert_eq!(st["state"], "pending");
    assert_eq!(st["notebook"]["last_seq"], 0, "no entries yet: {st}");

    // A notebook the fold refuses still answers the task's state.
    let bad = insert_pending(&pool, Lane::Fast, json!({"instruction": "y"})).await.expect("insert");
    notebook::append(&pool, bad, EntryKind::Tick, tick_payload()).await.unwrap();
    let st = s.ok("task.status", json!({"task_id": bad}));
    assert_eq!(st["state"], "pending");
    assert_eq!(st["notebook"], Value::Null, "{st}");
    assert!(st["notebook_error"].as_str().is_some_and(|e| e.contains("no step 1")), "{st}");

    // A garbage line is answered with a null id, and the session goes on.
    s.send_line("this is not json");
    let resp = s.next();
    assert_eq!(resp["error"]["code"], -32700, "{resp}");
    assert_eq!(resp["id"], Value::Null);
    // JSON that is not a request keeps its id, so the client can match it.
    s.send_line(r#"{"id": 77, "method": "task.status", "params": {"task_id": 1}}"#);
    let resp = s.next();
    assert_eq!((resp["error"]["code"].clone(), resp["id"].clone()), (json!(-32600), json!(77)), "{resp}");
    assert_eq!(s.error_code("task.nope", json!({})), -32601);
    assert_eq!(s.error_code("task.submit", json!({"instruction": "  "})), -32602);
    assert_eq!(s.error_code("task.status", json!({"task_id": task_id + 1000})), -32002);
    assert_eq!(s.error_code("task.follow", json!({"task_id": task_id + 1000})), -32002, "a follow of no task is refused");

    assert_eq!(s.ok("task.cancel", json!({"task_id": task_id}))["state"], "cancelled");
    assert_eq!(tasks::observe_state(&pool, task_id).await.unwrap(), "cancelled");
    assert_eq!(s.error_code("task.cancel", json!({"task_id": task_id})), -32002, "a second cancel is refused");
    assert_eq!(s.error_code("task.correct", json!({"task_id": task_id, "text": "x"})), -32002, "an ended task takes no correction");
    s.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_follow_streams_the_backlog_then_live_entries_then_the_finish() {
    let Some((pool, cluster)) = bring_up_pg("follow").await else { return };
    let task_id = seed_running_task(&pool).await;
    notebook::append(&pool, task_id, EntryKind::Steps, steps_payload()).await.unwrap();
    let mut s = Session::start(&cluster);

    assert_eq!(s.ok("task.follow", json!({"task_id": task_id})), json!({"task_id": task_id, "after_seq": 0}));
    assert_entry(&s.next(), task_id, 1, "steps");

    // An entry the planner writes now reaches the client unasked.
    notebook::append(&pool, task_id, EntryKind::Tick, tick_payload()).await.unwrap();
    assert_entry(&s.next(), task_id, 2, "tick");

    // A correction over the same session while the follow runs: its
    // response, then (from the follow) the entry it became.
    let id = s.request("task.correct", json!({"task_id": task_id, "text": "the other account"}));
    let (mut resp, mut entry) = (None, None);
    for _ in 0..2 {
        let m = s.next();
        if m["id"] == id { resp = Some(m) } else { entry = Some(m) }
    }
    assert_eq!(resp.expect("the correction's response")["result"], json!({"seq": 3, "audited": true}));
    let entry = entry.expect("the correction's entry");
    assert_entry(&entry, task_id, 3, "correction");
    assert_eq!(entry["params"]["payload"]["text"], "the other account");

    let st = s.ok("task.status", json!({"task_id": task_id}));
    assert_eq!(st["state"], "running");
    assert_eq!(st["notebook"]["steps"][0]["status"], "done", "{st}");
    assert_eq!(st["notebook"]["findings"][0]["text"], "invoice 42");
    assert_eq!(st["notebook"]["corrections"][0]["text"], "the other account");
    assert_eq!(st["notebook"]["last_seq"], 3);

    notebook::append(&pool, task_id, EntryKind::Answer, json!({"body": "the answer"})).await.unwrap();
    tasks::finalize(&pool, task_id, "completed", Some(json!({"kind": "text", "body": "the answer"})), None)
        .await
        .unwrap();
    assert_entry(&s.next(), task_id, 4, "answer");
    let fin = s.next();
    assert_eq!(fin["method"], "task.finished", "{fin}");
    assert_eq!(fin["params"]["state"], "completed");
    assert_eq!(fin["params"]["answer"], "the answer");

    // A resumed follow sends only what came after its seq, then the finish.
    s.ok("task.follow", json!({"task_id": task_id, "after_seq": 2}));
    assert_entry(&s.next(), task_id, 3, "correction");
    assert_entry(&s.next(), task_id, 4, "answer");
    assert_eq!(s.next()["method"], "task.finished");
    s.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parked_task_is_announced_and_the_inbox_answers_it() {
    let Some((pool, cluster)) = bring_up_pg("inbox").await else { return };
    let task_id = seed_running_task(&pool).await;
    let mut s = Session::start(&cluster);

    // Follow first, then park: nothing NOTIFYs a task parking, so this
    // notification can only come from the follow's poll tick. The entry is
    // the proof the follow has read the task as `running` already: it reads
    // the state before the entries, so raising before the entry arrived could
    // be seen on its first read and never need the tick.
    notebook::append(&pool, task_id, EntryKind::Steps, steps_payload()).await.unwrap();
    s.ok("task.follow", json!({"task_id": task_id}));
    assert_entry(&s.next(), task_id, 1, "steps");
    let raised = kastellan_db::asks::raise(
        &pool,
        task_id,
        "plan_approval",
        "this sends mail to a stranger",
        &json!(["approve", "deny"]),
        Some("digest-a"),
        time::OffsetDateTime::now_utc() + time::Duration::hours(1),
        None,
    )
    .await
    .expect("raise");
    assert_eq!(s.next(), json!({"jsonrpc": "2.0", "method": "task.awaiting_operator", "params": {"task_id": task_id}}));

    let asks = s.ok("inbox.list", Value::Null)["asks"].clone();
    assert_eq!(asks.as_array().map(Vec::len), Some(1), "{asks}");
    assert_eq!(asks[0]["ask_id"], raised.ask_id);
    assert_eq!(asks[0]["task_id"], task_id);
    assert_eq!(asks[0]["question"], "this sends mail to a stranger");

    assert_eq!(s.error_code("inbox.answer", json!({"ask_id": raised.ask_id, "choice": "maybe"})), -32602);
    assert_eq!(
        s.ok("inbox.answer", json!({"ask_id": raised.ask_id, "choice": "approve", "note": "fine"})),
        json!({"task_id": task_id, "audited": true, "audit_error": null})
    );
    assert_eq!(tasks::observe_state(&pool, task_id).await.unwrap(), "pending");
    let resolved: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE actor = 'cli' AND action = 'ask.resolved' \
         AND payload->>'ask_id' = $1::text",
    )
    .bind(raised.ask_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(resolved, 1, "one ask.resolved row, actor=cli");
    assert_eq!(s.error_code("inbox.answer", json!({"ask_id": raised.ask_id, "choice": "deny"})), -32002, "a lost race is refused");
    assert_eq!(s.error_code("inbox.answer", json!({"ask_id": raised.ask_id + 1000, "choice": "deny"})), -32002, "no such ask");

    // The task runs again and parks a second time: announced again (#988).
    // Two entries, the second written only once the first arrived, prove a
    // whole poll has read the task out of `awaiting_operator` before the
    // second ask — so a parked latch that never resets fails here.
    tasks::claim_one(&pool, Lane::Fast, 60).await.expect("claim").expect("the resumed task");
    notebook::append(&pool, task_id, EntryKind::Correction, json!({"text": "a"})).await.unwrap();
    assert_entry(&s.next(), task_id, 2, "correction");
    notebook::append(&pool, task_id, EntryKind::Correction, json!({"text": "b"})).await.unwrap();
    assert_entry(&s.next(), task_id, 3, "correction");
    kastellan_db::asks::raise(
        &pool,
        task_id,
        "plan_approval",
        "and this one too",
        &json!(["approve", "deny"]),
        Some("digest-b"),
        time::OffsetDateTime::now_utc() + time::Duration::hours(1),
        None,
    )
    .await
    .expect("second raise");
    assert_eq!(s.next(), json!({"jsonrpc": "2.0", "method": "task.awaiting_operator", "params": {"task_id": task_id}}));
    s.finish();
}

/// #988: a follow whose reads start failing says so, with the last `seq` it
/// sent — not the `after_seq` it started from — so the client resumes there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_follow_that_loses_its_task_says_where_it_stopped() {
    let Some((pool, cluster)) = bring_up_pg("follow-failed").await else { return };
    let task_id = seed_running_task(&pool).await;
    notebook::append(&pool, task_id, EntryKind::Steps, steps_payload()).await.unwrap();
    notebook::append(&pool, task_id, EntryKind::Tick, tick_payload()).await.unwrap();
    let mut s = Session::start(&cluster);
    s.ok("task.follow", json!({"task_id": task_id}));
    assert_entry(&s.next(), task_id, 1, "steps");
    assert_entry(&s.next(), task_id, 2, "tick");

    // The runtime role can't drop a table; hiding it as admin makes the
    // follow's next read fail the way a lost database does.
    let admin = kastellan_db::pool::connect_admin_pool(&cluster.conn_spec).await.expect("admin pool");
    sqlx::query("ALTER TABLE tasks RENAME TO tasks_hidden").execute(&admin).await.expect("hide tasks");
    let failed = s.next();
    assert_eq!(failed["method"], "task.follow_failed", "{failed}");
    assert_eq!(failed["params"]["task_id"], task_id);
    assert_eq!(failed["params"]["last_seq"], 2, "the last seq sent, not the after_seq (0) it began at: {failed}");
    assert!(failed["params"]["error"].as_str().is_some_and(|e| !e.is_empty()), "{failed}");
    sqlx::query("ALTER TABLE tasks_hidden RENAME TO tasks").execute(&admin).await.expect("restore tasks");
    s.finish();
}

/// #988: `task.submit` and `task.cancel` say when their audit row was lost
/// (the WARN goes to stderr, which a front end never reads), and `task.submit`
/// says when the client's floor suppressed what the keywords would have set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_and_cancel_report_a_lost_audit_row_and_a_suppressed_floor() {
    let Some((pool, cluster)) = bring_up_pg("audited").await else { return };
    let mut s = Session::start(&cluster);
    let clinical = json!({"instruction": "Translate the patient's pathology report.", "classification_floor": "public"});

    let r = s.ok("task.submit", clinical.clone());
    assert_eq!(r["audited"], true, "{r}");
    assert_eq!(r["floor_suppressed"]["inferred_class"], "ClinicalConfidential", "{r}");
    assert!(r["floor_suppressed"]["inferred_signals"].as_array().is_some_and(|a| a.iter().any(|v| v == "patient")), "{r}");
    assert_eq!(s.ok("task.cancel", json!({"task_id": r["task_id"]}))["audited"], true);
    let r = s.ok("task.submit", json!({"instruction": "find the invoice"}));
    assert_eq!(r["floor_suppressed"], Value::Null, "nothing pinned, nothing suppressed: {r}");
    // Out of the queue, so the `claim_one` below takes the task it means to.
    s.ok("task.cancel", json!({"task_id": r["task_id"]}));

    let admin = kastellan_db::pool::connect_admin_pool(&cluster.conn_spec).await.expect("admin pool");
    sqlx::query("ALTER TABLE audit_log RENAME TO audit_log_hidden").execute(&admin).await.expect("hide audit_log");
    let r = s.ok("task.submit", clinical);
    assert_eq!(r["audited"], false, "the task is in, its row is not: {r}");
    let c = s.ok("task.cancel", json!({"task_id": r["task_id"]}));
    assert_eq!((c["state"].clone(), c["audited"].clone()), (json!("cancelled"), json!(false)), "{c}");
    // A running task's cancel writes only its lifecycle row (the scheduler
    // writes the finalize), so this one is lost on its own.
    let r = s.ok("task.submit", json!({"instruction": "find the invoice"}));
    let claimed = tasks::claim_one(&pool, Lane::Fast, 60).await.expect("claim").expect("a task");
    assert_eq!(json!(claimed.id), r["task_id"]);
    assert_eq!(s.ok("task.cancel", json!({"task_id": r["task_id"]}))["audited"], false);
    sqlx::query("ALTER TABLE audit_log_hidden RENAME TO audit_log").execute(&admin).await.expect("restore audit_log");
    s.finish();
}

/// Each of a cancel's rows counts on its own: losing only the producer
/// finalize (a pending task's) or only the `ask.cancelled` row (a parked
/// task's) still reports `audited: false`. A trigger rejects just that row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_that_loses_any_one_of_its_rows_says_so() {
    let Some((pool, cluster)) = bring_up_pg("audited-rows").await else { return };
    let admin = kastellan_db::pool::connect_admin_pool(&cluster.conn_spec).await.expect("admin pool");
    sqlx::query(
        "CREATE FUNCTION reject_one_action() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
         IF NEW.action = TG_ARGV[0] THEN RAISE EXCEPTION 'rejected %', NEW.action; END IF; RETURN NEW; END $$",
    )
    .execute(&admin)
    .await
    .expect("create function");
    let reject = |trigger: &'static str| {
        let admin = admin.clone();
        async move {
            sqlx::query("DROP TRIGGER IF EXISTS reject_one ON audit_log").execute(&admin).await.expect("drop");
            sqlx::query(trigger).execute(&admin).await.expect("create trigger");
        }
    };
    let mut s = Session::start(&cluster);

    reject("CREATE TRIGGER reject_one BEFORE INSERT ON audit_log FOR EACH ROW EXECUTE FUNCTION reject_one_action('task.finalize')").await;
    let r = s.ok("task.submit", json!({"instruction": "find the invoice"}));
    assert_eq!(s.ok("task.cancel", json!({"task_id": r["task_id"]}))["audited"], false, "a pending cancel's finalize");

    reject("CREATE TRIGGER reject_one BEFORE INSERT ON audit_log FOR EACH ROW EXECUTE FUNCTION reject_one_action('ask.cancelled')").await;
    let task_id = seed_running_task(&pool).await;
    kastellan_db::asks::raise(
        &pool,
        task_id,
        "plan_approval",
        "this sends mail to a stranger",
        &json!(["approve", "deny"]),
        Some("digest-a"),
        time::OffsetDateTime::now_utc() + time::Duration::hours(1),
        None,
    )
    .await
    .expect("raise");
    assert_eq!(s.ok("task.cancel", json!({"task_id": task_id}))["audited"], false, "a parked cancel's ask row");
    s.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_follow_waits_for_the_planners_cancelled_entry() {
    let Some((pool, cluster)) = bring_up_pg("cancel").await else { return };
    let task_id = seed_running_task(&pool).await;
    notebook::append(&pool, task_id, EntryKind::Steps, steps_payload()).await.unwrap();
    let mut s = Session::start(&cluster);
    s.ok("task.follow", json!({"task_id": task_id}));
    assert_entry(&s.next(), task_id, 1, "steps");

    // The cancel lands first; the planner writes its entry only when it
    // next looks. Longer than a poll tick, so a follow that finished on the
    // state alone would have said so by now.
    assert_eq!(s.ok("task.cancel", json!({"task_id": task_id}))["state"], "cancelled");
    s.quiet_for(Duration::from_secs(5));
    notebook::append(&pool, task_id, EntryKind::Cancelled, json!({})).await.unwrap();
    assert_entry(&s.next(), task_id, 2, "cancelled");
    let fin = s.next();
    assert_eq!((fin["method"].clone(), fin["params"]["state"].clone()), (json!("task.finished"), json!("cancelled")), "{fin}");

    // Resumed past that entry, a follow does not wait for it again.
    s.ok("task.follow", json!({"task_id": task_id, "after_seq": 2}));
    assert_eq!(s.next()["method"], "task.finished");
    s.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follows_are_capped_and_a_dropped_session_leaves_its_tasks_alone() {
    let Some((pool, cluster)) = bring_up_pg("cap").await else { return };
    let task_id = insert_pending(&pool, Lane::Fast, json!({"instruction": "x"})).await.expect("insert");
    let mut s = Session::start(&cluster);
    // A pending task's follow sends nothing, so each stays live.
    for _ in 0..8 {
        s.ok("task.follow", json!({"task_id": task_id}));
    }
    assert_eq!(s.error_code("task.follow", json!({"task_id": task_id})), -32002, "the ninth is refused");

    // An over-cap line: answered -32600, then the session ends, follows and all.
    let mut big = vec![b'a'; kastellan_protocol::MAX_RECORD_BYTES + 1];
    big.push(b'\n');
    let _ = s.stdin.write_all(&big); // the session may stop reading mid-line
    let resp = s.next();
    assert_eq!((resp["error"]["code"].clone(), resp["id"].clone()), (json!(-32600), Value::Null), "{resp}");
    s.finish();
    assert_eq!(tasks::observe_state(&pool, task_id).await.unwrap(), "pending", "a dropped session cancels nothing");
}
