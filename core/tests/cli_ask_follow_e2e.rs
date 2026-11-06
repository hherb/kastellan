//! Subprocess-level pin for #985: `kastellan-cli ask` prints one short
//! stderr line per notebook entry as it lands, and stdout carries only the
//! answer.
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
use std::time::Instant;

use kastellan_db::notebook::{self, EntryKind};
use kastellan_db::tasks::{self, Lane};
use kastellan_tests_common::{cli_binary, current_username, PgCluster};
use serde_json::json;

#[path = "cli_session_e2e/support.rs"]
#[allow(dead_code)] // this file uses the PG bring-up and payloads, not the session driver
mod support;
use support::{bring_up_pg, steps_payload, tick_payload, WAIT};

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

/// The next stderr line starting `ask: ` that is not a `submitted` line
/// (those are read by [`submitted_id`]).
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
    let (mut child, lines) = spawn_ask(&cluster, "find the invoice");
    let task_id = submitted_id(&lines);
    let claimed = tasks::claim_one(&pool, Lane::Fast, 60).await.expect("claim").expect("ask's task");
    assert_eq!(claimed.id, task_id);

    notebook::append(&pool, task_id, EntryKind::Steps, steps_payload()).await.unwrap();
    assert_eq!(next_line(&lines), "ask: plan: 1. find it");
    notebook::append(&pool, task_id, EntryKind::Tick, tick_payload()).await.unwrap();
    assert_eq!(next_line(&lines), "ask: step 1 done: found");
    notebook::append(&pool, task_id, EntryKind::Correction, json!({"text": "the other account"})).await.unwrap();
    assert_eq!(next_line(&lines), "ask: correction: the other account");

    notebook::append(&pool, task_id, EntryKind::Answer, json!({"body": "invoice 42"})).await.unwrap();
    tasks::finalize(&pool, task_id, "completed", Some(json!({"kind": "text", "body": "invoice 42"})), None)
        .await
        .unwrap();

    let deadline = Instant::now() + WAIT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "ask did not exit after the task completed");
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let mut stdout = String::new();
    child.stdout.take().unwrap().read_to_string(&mut stdout).unwrap();
    assert!(status.success(), "ask exits 0 on a completed task: {status}");
    assert_eq!(stdout, "invoice 42\n", "stdout is the answer and nothing else");
    let rest: Vec<String> = lines.iter().collect(); // ends at EOF: ask has exited
    assert!(!rest.iter().any(|l| l.contains("invoice 42")), "the answer is not repeated on stderr: {rest:?}");
}
