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
//!
//! Skips with `[SKIP]` on hosts without Postgres or a supervisor (run with
//! `-- --nocapture` to see it), like `cli_inbox_e2e.rs`, whose PG bring-up
//! and CLI env this copies. Once Postgres is up a failure fails.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use kastellan_db::notebook::{self, EntryKind};
use kastellan_db::pool::connect_runtime_pool;
use kastellan_db::probe::run as probe_run;
use kastellan_db::tasks::{self, insert_pending, Lane};
use kastellan_tests_common::{
    bring_up_pg_cluster, cli_binary, current_username, pg_bin_dir_or_skip, skip_if_no_supervisor,
    unique_suffix, PgCluster,
};
use serde_json::{json, Value};

/// Long enough for a follow's 3 s poll tick plus a slow CI host.
const WAIT: Duration = Duration::from_secs(30);

async fn bring_up_pg(label: &str) -> Option<(sqlx::PgPool, PgCluster)> {
    if skip_if_no_supervisor() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ss-d",
        "ss-l",
        &format!("kastellan-postgres-cli-session-e2e-{label}-{suffix}"),
    );
    probe_run(&cluster.conn_spec, "core", "startup", json!({"test": format!("cli_session_e2e-{label}")}))
        .await
        .expect("probe run");
    let pool = connect_runtime_pool(&cluster.conn_spec).await.expect("runtime pool");
    Some((pool, cluster))
}

/// A task in `running`, as a claimed one is.
async fn seed_running_task(pool: &sqlx::PgPool) -> i64 {
    let id = insert_pending(pool, Lane::Fast, json!({"instruction": "x", "max_plans": 3})).await.expect("insert");
    tasks::claim_one(pool, Lane::Fast, 60).await.expect("claim").expect("a task");
    id
}

/// The real `kastellan-cli session`, with a line reader on its stdout.
struct Session {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<Value>,
    next_id: i64,
}

impl Session {
    fn start(cluster: &PgCluster) -> Session {
        let user = std::env::var("USER").unwrap_or_else(|_| current_username());
        let mut cmd = Command::new(cli_binary());
        cmd.arg("session")
            .env_clear()
            .env("KASTELLAN_DATA_DIR", &cluster.data_dir)
            .env("USER", user)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(home) = std::env::var_os("HOME") {
            cmd.env("HOME", home);
        }
        let mut child = cmd.spawn().expect("spawn kastellan-cli session");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let v: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON: {line:?}: {e}"));
                if tx.send(v).is_err() {
                    return;
                }
            }
        });
        Session { child, stdin, lines, next_id: 0 }
    }

    fn send_line(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").expect("write to session");
        self.stdin.flush().unwrap();
    }

    /// Send a request; return its id.
    fn request(&mut self, method: &str, params: Value) -> i64 {
        self.next_id += 1;
        let req = json!({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params});
        self.send_line(&req.to_string());
        self.next_id
    }

    /// The next message, whatever it is.
    fn next(&self) -> Value {
        match self.lines.recv_timeout(WAIT) {
            Ok(v) => v,
            Err(RecvTimeoutError::Timeout) => panic!("no message from the session within {WAIT:?}"),
            Err(RecvTimeoutError::Disconnected) => panic!("the session closed its stdout"),
        }
    }

    /// Assert nothing arrives for `d`.
    fn quiet_for(&self, d: Duration) {
        match self.lines.recv_timeout(d) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(v) => panic!("expected nothing for {d:?}, got {v}"),
            Err(RecvTimeoutError::Disconnected) => panic!("the session closed its stdout"),
        }
    }

    /// Send a request and return its response, which must come next.
    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.request(method, params);
        let resp = self.next();
        assert_eq!(resp["id"], id, "expected the response to {method} next, got {resp}");
        resp
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let resp = self.call(method, params);
        assert!(resp.get("error").is_none(), "{method} failed: {resp}");
        resp["result"].clone()
    }

    fn error_code(&mut self, method: &str, params: Value) -> i64 {
        let resp = self.call(method, params);
        resp["error"]["code"].as_i64().unwrap_or_else(|| panic!("{method} should have failed: {resp}"))
    }

    /// Close stdin and expect a clean exit.
    fn finish(mut self) {
        drop(self.stdin);
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "session must exit 0 at EOF: {status}");
                return;
            }
            assert!(Instant::now() < deadline, "session did not exit at EOF");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn steps_payload() -> Value {
    json!({"steps": [{"id": 1, "purpose": "find it", "tool": "mail", "done_when": "found"}]})
}

fn tick_payload() -> Value {
    json!({"step": 1, "note": "found", "findings": [{"text": "invoice 42", "source": "msg 7"}],
           "summary": "one invoice", "classification": "Public"})
}

/// Assert `msg` is a `notebook.entry` for `seq` of `kind`.
fn assert_entry(msg: &Value, task_id: i64, seq: i64, kind: &str) {
    assert_eq!(msg["method"], "notebook.entry", "expected an entry, got {msg}");
    assert_eq!(msg["params"]["task_id"], task_id, "{msg}");
    assert_eq!(msg["params"]["seq"], seq, "{msg}");
    assert_eq!(msg["params"]["kind"], kind, "{msg}");
}

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
