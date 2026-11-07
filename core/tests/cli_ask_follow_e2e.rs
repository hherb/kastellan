//! Subprocess-level pins for #985: `kastellan-cli ask` prints one short
//! stderr line per notebook entry as it lands, stdout carries only the
//! answer, and every other way out of its follow loop (parked, lost
//! database, Ctrl-C, a cancel from elsewhere) exits as documented.
//!
//! No daemon runs: like `cli_session_e2e.rs` (whose harness this shares),
//! the test plays the scheduler — it claims the task `ask` submitted,
//! appends entries, and finalises it.
//!
//! Skips with `[SKIP]` on hosts without Postgres or a supervisor (run with
//! `-- --nocapture` to see it). Once Postgres is up a failure fails.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use kastellan_db::notebook::{self, EntryKind};
use kastellan_db::tasks::{self, Lane};
use kastellan_tests_common::{cli_binary, current_username, PgCluster};
use serde_json::json;

#[path = "cli_session_e2e/support.rs"]
#[allow(dead_code)] // this file uses the PG bring-up and payloads, not the session driver
mod support;
use support::{bring_up_pg, steps_payload, tick_payload, WAIT};

/// Wait for `ask` to exit; its status and its whole stdout.
fn wait_exit(child: &mut std::process::Child, within: Duration) -> (std::process::ExitStatus, String) {
    let deadline = Instant::now() + within;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "ask did not exit within {within:?}");
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut stdout = String::new();
    child.stdout.take().unwrap().read_to_string(&mut stdout).unwrap();
    (status, stdout)
}

/// Spawn `ask`, claim its task as the scheduler would, and append its plan,
/// read back as the plan line: from there `ask`'s loop has polled once.
async fn ask_with_a_running_task(
    pool: &sqlx::PgPool,
    cluster: &PgCluster,
) -> (std::process::Child, Receiver<String>, i64) {
    let (child, lines) = spawn_ask(cluster, "find the invoice");
    let task_id = submitted_id(&lines);
    let claimed = tasks::claim_one(pool, Lane::Fast, 60).await.expect("claim").expect("ask's task");
    assert_eq!(claimed.id, task_id);
    notebook::append(pool, task_id, EntryKind::Steps, steps_payload()).await.unwrap();
    assert_eq!(next_line(&lines), "ask: plan: 1. find it");
    (child, lines, task_id)
}

async fn complete(pool: &sqlx::PgPool, task_id: i64, body: &str) {
    notebook::append(pool, task_id, EntryKind::Answer, json!({"body": body})).await.unwrap();
    tasks::finalize(pool, task_id, "completed", Some(json!({"kind": "text", "body": body})), None).await.unwrap();
}

/// Hide `tasks` as admin (the runtime role cannot): every read of it fails
/// the way a lost database does. Returns the admin pool to restore it with.
async fn hide_tasks(cluster: &PgCluster) -> sqlx::PgPool {
    let admin = kastellan_db::pool::connect_admin_pool(&cluster.conn_spec).await.expect("admin pool");
    sqlx::query("ALTER TABLE tasks RENAME TO tasks_hidden").execute(&admin).await.expect("hide tasks");
    admin
}

async fn restore_tasks(admin: &sqlx::PgPool) {
    sqlx::query("ALTER TABLE tasks_hidden RENAME TO tasks").execute(admin).await.expect("restore tasks");
}

/// `kastellan-cli ask <instruction>` against `cluster`, with its stderr
/// lines on a channel and its stdout read whole once it exits.
fn spawn_ask(cluster: &PgCluster, instruction: &str) -> (std::process::Child, Receiver<String>) {
    let user = std::env::var("USER").unwrap_or_else(|_| current_username());
    let mut cmd = Command::new(cli_binary());
    cmd.args(["ask", instruction])
        .env_clear()
        .env("KASTELLAN_DATA_DIR", &cluster.data_dir)
        .env("USER", user)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(home) = std::env::var_os("HOME") {
        cmd.env("HOME", home);
    }
    let mut child = cmd.spawn().expect("spawn kastellan-cli ask");
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("[ask stderr] {line}");
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    (child, rx)
}

/// The next stderr line starting `ask: `.
fn next_line(lines: &Receiver<String>) -> String {
    loop {
        let line = lines.recv_timeout(WAIT).unwrap_or_else(|e| panic!("no stderr line from ask within {WAIT:?}: {e}"));
        if line.starts_with("ask: ") {
            return line;
        }
    }
}

/// The task id from `ask: submitted task <id> (lane=…)`.
fn submitted_id(lines: &Receiver<String>) -> i64 {
    let line = next_line(lines);
    let rest = line.strip_prefix("ask: submitted task ").unwrap_or_else(|| panic!("expected the submitted line, got {line:?}"));
    rest.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or_else(|| panic!("no id in {line:?}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_prints_each_entry_to_stderr_and_only_the_answer_to_stdout() {
    let Some((pool, cluster)) = bring_up_pg("ask-follow").await else { return };
    let (mut child, lines, task_id) = ask_with_a_running_task(&pool, &cluster).await;
    notebook::append(&pool, task_id, EntryKind::Tick, tick_payload()).await.unwrap();
    assert_eq!(next_line(&lines), "ask: step 1 done: found");
    notebook::append(&pool, task_id, EntryKind::Correction, json!({"text": "the other account"})).await.unwrap();
    assert_eq!(next_line(&lines), "ask: correction: the other account");
    complete(&pool, task_id, "invoice 42").await;

    let (status, stdout) = wait_exit(&mut child, WAIT);
    assert!(status.success(), "ask exits 0 on a completed task: {status}");
    assert_eq!(stdout, "invoice 42\n", "stdout is the answer and nothing else");
    let rest: Vec<String> = lines.iter().collect(); // ends at EOF: ask has exited
    assert!(!rest.iter().any(|l| l.contains("invoice 42")), "the answer is not repeated on stderr: {rest:?}");
}

/// bottie reads a parked task as exit 0, an empty stdout and this line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parked_task_exits_0_with_the_operator_line() {
    let Some((pool, cluster)) = bring_up_pg("ask-parked").await else { return };
    let (mut child, lines, task_id) = ask_with_a_running_task(&pool, &cluster).await;
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
    assert_eq!(
        next_line(&lines),
        format!(
            "ask: task {task_id} is waiting for an operator decision. Run `kastellan-cli inbox list` to see the \
             question, then `kastellan-cli inbox resolve <id> approve|deny` to answer it. Re-run \
             `kastellan-cli tasks status {task_id}` afterwards for the result."
        )
    );
    let (status, stdout) = wait_exit(&mut child, WAIT);
    assert_eq!(status.code(), Some(0), "a parked task is not a failure");
    assert_eq!(stdout, "", "nothing on stdout: there is no answer yet");
}

/// A database blip does not end `ask`: it resumes after the last entry it
/// printed, and still prints the answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_follow_is_resumed_where_it_stopped() {
    let Some((pool, cluster)) = bring_up_pg("ask-resume").await else { return };
    let (mut child, lines, task_id) = ask_with_a_running_task(&pool, &cluster).await;
    let admin = hide_tasks(&cluster).await;
    let line = next_line(&lines);
    assert!(line.starts_with(&format!("ask: lost track of task {task_id} (")) && line.ends_with("); retrying"), "{line}");
    restore_tasks(&admin).await;

    notebook::append(&pool, task_id, EntryKind::Tick, tick_payload()).await.unwrap();
    let line = loop {
        let line = next_line(&lines);
        if !line.ends_with("; retrying") {
            break line;
        }
    };
    assert_eq!(line, "ask: step 1 done: found", "the plan is not printed twice");
    complete(&pool, task_id, "invoice 42").await;
    let (status, stdout) = wait_exit(&mut child, WAIT);
    assert!(status.success(), "{status}");
    assert_eq!(stdout, "invoice 42\n");
}

/// A database that stays gone ends `ask` with exit 1, saying the task was
/// not cancelled and how to look after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_follow_that_keeps_failing_gives_up_and_says_the_task_may_run_on() {
    let Some((pool, cluster)) = bring_up_pg("ask-give-up").await else { return };
    let (mut child, lines, task_id) = ask_with_a_running_task(&pool, &cluster).await;
    let admin = hide_tasks(&cluster).await;
    let line = loop {
        let line = next_line(&lines);
        if !line.ends_with("; retrying") {
            break line;
        }
    };
    assert!(line.starts_with(&format!("ask: lost track of task {task_id}: ")), "{line}");
    assert!(
        line.ends_with(&format!(
            "; it may still be running: `kastellan-cli tasks status {task_id}` shows it, \
             `kastellan-cli tasks cancel {task_id}` stops it"
        )),
        "{line}"
    );
    let (status, stdout) = wait_exit(&mut child, WAIT);
    assert_eq!(status.code(), Some(1), "{status}");
    assert_eq!(stdout, "");
    restore_tasks(&admin).await;
    assert_eq!(tasks::get(&pool, task_id).await.unwrap().unwrap().state, "running", "giving up cancels nothing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_c_cancels_the_task_and_exits_130() {
    let Some((pool, cluster)) = bring_up_pg("ask-sigint").await else { return };
    let (mut child, lines, task_id) = ask_with_a_running_task(&pool, &cluster).await;
    let killed = Command::new("kill").args(["-INT", &child.id().to_string()]).status().expect("run kill");
    assert!(killed.success());
    assert_eq!(next_line(&lines), format!("ask: cancelled (task id {task_id})"));
    let (status, stdout) = wait_exit(&mut child, WAIT);
    assert_eq!(status.code(), Some(130), "exits, not killed by the signal: {status}");
    assert_eq!(stdout, "");
    assert_eq!(tasks::get(&pool, task_id).await.unwrap().unwrap().state, "cancelled");
}

/// A cancel from elsewhere ends `ask` at once: it prints no `cancelled`
/// entry, so it does not wait (up to 120 s) for the planner to write one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_from_elsewhere_ends_ask_without_waiting_for_the_planner() {
    let Some((pool, cluster)) = bring_up_pg("ask-cancelled").await else { return };
    let (mut child, lines, task_id) = ask_with_a_running_task(&pool, &cluster).await;
    kastellan_core::cli_audit::cancel_and_audit(&pool, task_id).await.expect("cancel");
    assert_eq!(next_line(&lines), "ask: task ended in state 'cancelled'");
    let (status, stdout) = wait_exit(&mut child, WAIT);
    assert_eq!(status.code(), Some(1), "{status}");
    assert_eq!(stdout, "");
}
