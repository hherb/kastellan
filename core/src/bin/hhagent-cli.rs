//! `hhagent-cli` — operator-facing CLI tool.
//!
//! Subcommands:
//!
//! * `audit tail`  — stream the daemon's `audit-YYYY-MM-DD.jsonl`
//!   files from `~/.local/state/hhagent/`. Works without Postgres
//!   and survives a crashed daemon (the JSONL is the durable replica
//!   of `audit_log` written by the mirror task —
//!   see [`hhagent_core::audit_mirror`]).
//!
//! * `ask "<instruction>" [--fast|--long]` — submit a task to the
//!   scheduler, LISTEN for the completion NOTIFY, then print the
//!   result. Ctrl-C cancels the pending/running task.
//!
//! * `tasks list|status|cancel|fail|tail` — inspect and manage
//!   tasks in the scheduler DB.
//!
//! Usage:
//!
//! ```text
//! hhagent-cli ask "<instruction>" [--fast|--long]
//! hhagent-cli tasks list   [--lane fast|long] [--state <state>] [-n 20]
//! hhagent-cli tasks status <id>
//! hhagent-cli tasks cancel <id>
//! hhagent-cli tasks fail   <id>
//! hhagent-cli tasks tail   <id>
//! hhagent-cli audit tail   [--from-start] [--no-follow] [--state-dir PATH]
//! ```
//!
//! The CLI parser is hand-rolled (no `clap` dep) because the surface
//! is tiny and a parser dep would dominate the binary footprint. If
//! we ever grow to ~5+ subcommands or richer flag parsing, swapping
//! in `clap` is a strictly local change here.

use std::path::PathBuf;
use std::process::ExitCode;

use hhagent_core::audit_mirror;
use hhagent_core::audit_tail::{tail_loop, TailConfig};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("{}", help_text());
        return ExitCode::from(2);
    }
    match args[1].as_str() {
        "audit" => match args.get(2).map(|s| s.as_str()) {
            Some("tail") => run_audit_tail(&args[3..]),
            _ => {
                eprintln!("usage: hhagent-cli audit tail [opts]");
                ExitCode::from(2)
            }
        },
        "ask"   => run_ask(&args[2..]),
        "tasks" => run_tasks(&args[2..]),
        "--help" | "-h" | "help" => {
            println!("{}", help_text());
            ExitCode::from(0)
        }
        other => {
            eprintln!("unknown subcommand: {other}\n\n{}", help_text());
            ExitCode::from(2)
        }
    }
}

fn help_text() -> &'static str {
    "hhagent-cli — operator CLI for hhagent

usage:
    hhagent-cli ask \"<instruction>\" [--fast|--long]
    hhagent-cli tasks list   [--lane fast|long] [--state <state>] [-n 20]
    hhagent-cli tasks status <id>
    hhagent-cli tasks cancel <id>
    hhagent-cli tasks fail   <id>
    hhagent-cli tasks tail   <id>
    hhagent-cli audit tail   [--from-start] [--no-follow] [--state-dir PATH]

flags (audit tail):
    --from-start    Replay every line in every existing audit file
                    before switching to follow mode.
    --no-follow     Exit after replaying existing content (use with
                    --from-start for a 'cat' of the JSONL files).
    --state-dir P   Override the state dir (default: $HHAGENT_STATE_DIR
                    or $HOME/.local/state/hhagent).
"
}

fn run_audit_tail(args: &[String]) -> ExitCode {
    let mut from_start = false;
    let mut follow = true;
    let mut state_dir_arg: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--from-start" => from_start = true,
            "--no-follow" => follow = false,
            "--state-dir" => {
                i += 1;
                match args.get(i) {
                    Some(p) => state_dir_arg = Some(p.clone()),
                    None => {
                        eprintln!("--state-dir requires a path argument");
                        return ExitCode::from(2);
                    }
                }
            }
            other => {
                eprintln!("unknown audit-tail flag: {other}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }

    let state_dir: PathBuf = match state_dir_arg {
        Some(p) => PathBuf::from(p),
        None => match std::env::var_os(audit_mirror::ENV_STATE_DIR) {
            Some(p) => PathBuf::from(p),
            None => match audit_mirror::default_state_dir() {
                Some(p) => p,
                None => {
                    eprintln!(
                        "$HOME unset and no --state-dir given; cannot resolve audit dir"
                    );
                    return ExitCode::from(2);
                }
            },
        },
    };

    // The viewer does file I/O + a 250 ms sleep loop, no
    // `block_in_place` — a current-thread runtime is the right shape
    // (smallest footprint, no extra worker thread). Calling
    // `Builder::new_current_thread()` explicitly so the binary's
    // runtime choice is independent of which `tokio` feature flags
    // happen to be active in the workspace deps.
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to build tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };

    let result = rt.block_on(async {
        let stdout = tokio::io::stdout();
        tail_loop(
            TailConfig {
                state_dir,
                from_start,
                follow,
            },
            stdout,
        )
        .await
    });

    match result {
        Ok(()) => ExitCode::from(0),
        // BrokenPipe is the canonical "downstream `head` / `less`
        // closed early" exit; not an error from the operator's
        // perspective. Match BSD `tail`'s behaviour.
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::from(0),
        Err(e) => {
            eprintln!("hhagent-cli audit tail: {e}");
            ExitCode::from(1)
        }
    }
}

// ---------------------------------------------------------------------------
// Connection helper — shared by every subcommand that needs Postgres.
// ---------------------------------------------------------------------------

/// Build a [`hhagent_db::conn::ConnectSpec`] from `$HHAGENT_DATA_DIR`
/// (if set) or the XDG default. Fails with a human-readable error string
/// when `$HOME` is unset (needed by `ConnectSpec::default_for`).
fn resolve_connect_spec() -> Result<hhagent_db::conn::ConnectSpec, String> {
    let data_dir = match std::env::var_os("HHAGENT_DATA_DIR") {
        Some(p) => std::path::PathBuf::from(p),
        None => hhagent_db::default_data_dir()
            .ok_or_else(|| "$HOME unset; cannot resolve cluster data dir".to_string())?,
    };
    hhagent_db::conn::ConnectSpec::default_for(&data_dir)
        .map_err(|e| format!("resolving Postgres connection: {e}"))
}

// ---------------------------------------------------------------------------
// `ask` subcommand
// ---------------------------------------------------------------------------

fn run_ask(args: &[String]) -> ExitCode {
    let mut lane = hhagent_db::tasks::Lane::Fast;
    let mut instruction: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--long" => { lane = hhagent_db::tasks::Lane::Long; }
            "--fast" => { lane = hhagent_db::tasks::Lane::Fast; }
            other if other.starts_with("--") => {
                eprintln!("ask: unknown flag {other}");
                return ExitCode::from(2);
            }
            other => {
                if instruction.is_some() {
                    eprintln!("ask: only one positional instruction allowed");
                    return ExitCode::from(2);
                }
                instruction = Some(other.to_string());
            }
        }
        i += 1;
    }
    let Some(instruction) = instruction else {
        eprintln!("usage: hhagent-cli ask \"<instruction>\" [--fast|--long]");
        return ExitCode::from(2);
    };

    // Use a multi-thread runtime so `block_in_place` is available if
    // any sqlx internals need it; PgListener::recv does not require it
    // today but the shape is consistent with the rest of the DB code.
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("ask: failed to build tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };
    rt.block_on(ask_async(lane, instruction))
}

async fn ask_async(lane: hhagent_db::tasks::Lane, instruction: String) -> ExitCode {
    use hhagent_core::cli_audit::cancel_and_audit;
    use hhagent_db::pool::connect_runtime_pool;
    use hhagent_db::tasks::{get, insert_pending};
    use sqlx::postgres::PgListener;

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("ask: {e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("ask: db connect failed: {e}"); return ExitCode::from(1); }
    };

    // LISTEN BEFORE INSERT to avoid the race where the NOTIFY arrives
    // before we start listening.
    let mut listener = match PgListener::connect_with(&pool).await {
        Ok(l) => l,
        Err(e) => { eprintln!("ask: listener connect failed: {e}"); return ExitCode::from(1); }
    };
    if let Err(e) = listener.listen("tasks_completed").await {
        eprintln!("ask: listen failed: {e}");
        return ExitCode::from(1);
    }

    let id = match insert_pending(
        &pool,
        lane,
        serde_json::json!({"instruction": instruction, "kind": "ask"}),
    )
    .await
    {
        Ok(i) => i,
        Err(e) => { eprintln!("ask: insert failed: {e}"); return ExitCode::from(1); }
    };

    eprintln!("ask: submitted task {id} (lane={}); waiting for completion…", lane.as_sql());

    // Wait for a terminal-state NOTIFY for our id, OR ctrl-C.
    tokio::pin! {
        let sigint = tokio::signal::ctrl_c();
    }
    loop {
        tokio::select! {
            n = listener.recv() => match n {
                Ok(notif) => {
                    if notif.payload() == id.to_string() { break; }
                }
                Err(e) => { eprintln!("ask: listener.recv: {e}"); return ExitCode::from(1); }
            },
            result = &mut sigint => {
                if result.is_ok() {
                    // Best-effort: even if the audit insert hiccups, the
                    // SIGINT path still exits 130. The helper emits the
                    // producer-side `actor='cli' action='task.cancelled'`
                    // row when the task was in a cancellable state.
                    let _ = cancel_and_audit(&pool, id).await;
                    eprintln!("ask: cancelled (task id {id})");
                    return ExitCode::from(130);  // standard SIGINT exit code
                }
            }
        }
    }

    let task = match get(&pool, id).await {
        Ok(Some(t)) => t,
        Ok(None) => { eprintln!("ask: task {id} disappeared"); return ExitCode::from(1); }
        Err(e) => { eprintln!("ask: get failed: {e}"); return ExitCode::from(1); }
    };

    match (task.state.as_str(), task.result) {
        ("completed", Some(r)) => {
            if r.get("kind").and_then(|v| v.as_str()) == Some("text") {
                if let Some(b) = r.get("body").and_then(|v| v.as_str()) {
                    println!("{b}");
                    return ExitCode::from(0);
                }
            }
            // Unknown kind: dump JSON.
            println!("{}", serde_json::to_string_pretty(&r).unwrap());
            ExitCode::from(0)
        }
        (state, _) => {
            eprintln!("ask: task ended in state '{state}'");
            ExitCode::from(1)
        }
    }
}

// ---------------------------------------------------------------------------
// `tasks` subcommand dispatcher — body added in Tasks 4.2 + 4.3.
// ---------------------------------------------------------------------------

fn run_tasks(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: hhagent-cli tasks <list|status|cancel|fail|tail> ...");
        return ExitCode::from(2);
    }
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("tasks: failed to build tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };
    match args[0].as_str() {
        "list"   => rt.block_on(tasks_list(&args[1..])),
        "status" => rt.block_on(tasks_status(&args[1..])),
        "cancel" => rt.block_on(tasks_cancel(&args[1..])),
        "fail"   => rt.block_on(tasks_fail(&args[1..])),
        "tail"   => tasks_tail(&args[1..]),
        other    => { eprintln!("tasks: unknown subcommand {other}"); ExitCode::from(2) }
    }
}

async fn tasks_list(args: &[String]) -> ExitCode {
    use hhagent_db::pool::connect_runtime_pool;
    use hhagent_db::tasks::{list, Lane};

    let mut lane: Option<Lane> = None;
    let mut state: Option<String> = None;
    let mut limit: i64 = 20;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--lane" => {
                let v = match args.get(i + 1) {
                    Some(v) => v,
                    None => { eprintln!("--lane needs value"); return ExitCode::from(2); }
                };
                lane = match Lane::from_sql(v) {
                    Ok(l) => Some(l),
                    Err(_) => {
                        eprintln!("--lane must be 'fast' or 'long'");
                        return ExitCode::from(2);
                    }
                };
                i += 2;
            }
            "--state" => {
                state = args.get(i + 1).cloned();
                i += 2;
            }
            "-n" => {
                limit = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(20);
                i += 2;
            }
            other => {
                eprintln!("tasks list: unknown flag {other}");
                return ExitCode::from(2);
            }
        }
    }

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let rows = match list(&pool, lane, state.as_deref(), limit).await {
        Ok(r) => r,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    for t in rows {
        let instr = t.payload.get("instruction").and_then(|v| v.as_str()).unwrap_or("");
        // Char-based truncation: byte slicing splits multi-byte UTF-8 codepoints
        // (medical jargon with accented characters or CJK) and panics at runtime.
        let summary: String = instr.chars().take(60).collect();
        println!("{:>6}  {:<10}  {:<5}  {}  {}",
            t.id, t.state, t.lane.as_sql(), t.created_at, summary);
    }
    ExitCode::from(0)
}

async fn tasks_status(args: &[String]) -> ExitCode {
    use hhagent_db::pool::connect_runtime_pool;
    use hhagent_db::tasks::get;

    let id: i64 = match args.first().and_then(|s| s.parse().ok()) {
        Some(i) => i,
        None => { eprintln!("usage: tasks status <id>"); return ExitCode::from(2); }
    };
    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    match get(&pool, id).await {
        Ok(Some(t)) => {
            println!("id:               {}", t.id);
            println!("state:            {}", t.state);
            println!("lane:             {}", t.lane.as_sql());
            println!("plan_count:       {}", t.plan_count);
            println!("created_at:       {}", t.created_at);
            println!("started_at:       {:?}", t.started_at);
            println!("finished_at:      {:?}", t.finished_at);
            println!("lease_expires_at: {:?}", t.lease_expires_at);
            println!("payload:          {}", t.payload);
            if let Some(r) = t.result {
                println!("result:           {}", serde_json::to_string_pretty(&r).unwrap());
            }
            ExitCode::from(0)
        }
        Ok(None) => { eprintln!("task {id} not found"); ExitCode::from(1) }
        Err(e)   => { eprintln!("{e}"); ExitCode::from(1) }
    }
}

async fn tasks_cancel(args: &[String]) -> ExitCode {
    use hhagent_core::cli_audit::{cancel_and_audit, CancelOutcome};
    use hhagent_db::pool::connect_runtime_pool;

    let id: i64 = match args.first().and_then(|s| s.parse().ok()) {
        Some(i) => i,
        None => { eprintln!("usage: tasks cancel <id>"); return ExitCode::from(2); }
    };
    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    // The helper writes the producer-side `actor='cli'
    // action='task.cancelled'` audit row when the UPDATE flips a row.
    // No row is written when the task is already terminal or absent.
    match cancel_and_audit(&pool, id).await {
        Ok(CancelOutcome::Cancelled(_))   => { println!("cancelled task {id}"); ExitCode::from(0) }
        Ok(CancelOutcome::NotCancellable) => { eprintln!("task {id} not in cancellable state"); ExitCode::from(1) }
        Err(e)                            => { eprintln!("{e}"); ExitCode::from(1) }
    }
}

async fn tasks_fail(args: &[String]) -> ExitCode {
    use hhagent_db::pool::connect_runtime_pool;
    use hhagent_db::tasks::mark_failed_running;

    let id: i64 = match args.first().and_then(|s| s.parse().ok()) {
        Some(i) => i,
        None => { eprintln!("usage: tasks fail <id>"); return ExitCode::from(2); }
    };
    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    match mark_failed_running(&pool, id).await {
        Ok(true)  => { println!("marked task {id} as crashed"); ExitCode::from(0) }
        Ok(false) => { eprintln!("task {id} not in 'running' or lease already elapsed"); ExitCode::from(1) }
        Err(e)    => { eprintln!("{e}"); ExitCode::from(1) }
    }
}

/// One-shot scan of the audit JSONL files for rows whose `payload.task_id`
/// equals `<id>`. Prints matching lines verbatim to stdout, in chronological
/// order across files.
///
/// Each line is parsed as JSON so the filter does not false-positive on
/// substring matches like `parent_task_id` or whitespace-padded JSON. Lines
/// that fail to parse are skipped silently — the file might be mid-write
/// from the mirror task.
///
/// Follow mode (live tailing) is a follow-up; use `audit tail` for
/// live tailing of the full stream. This function exits after scanning
/// all existing files so it is suitable for post-mortem inspection of a
/// completed task.
fn tasks_tail(args: &[String]) -> ExitCode {
    let id: i64 = match args.first().and_then(|s| s.parse().ok()) {
        Some(i) => i,
        None => { eprintln!("usage: tasks tail <id>"); return ExitCode::from(2); }
    };

    let state_dir = match std::env::var_os(audit_mirror::ENV_STATE_DIR) {
        Some(p) => std::path::PathBuf::from(p),
        None => match audit_mirror::default_state_dir() {
            Some(p) => p,
            None => {
                eprintln!("$HOME unset; cannot resolve state dir");
                return ExitCode::from(2);
            }
        },
    };

    let entries = match std::fs::read_dir(&state_dir) {
        Ok(it) => it,
        Err(e) => { eprintln!("read_dir({state_dir:?}): {e}"); return ExitCode::from(1); }
    };

    let mut files: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("audit-") && n.ends_with(".jsonl"))
                .unwrap_or(false)
        })
        .collect();
    // Walk files in chronological (lexicographic) order so the oldest
    // events for this task appear before newer ones.
    files.sort();

    use std::io::{BufRead, BufReader};
    for path in files {
        let f = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        for line in BufReader::new(f).lines().map_while(Result::ok) {
            if line_matches_task(&line, id) {
                println!("{line}");
            }
        }
    }

    ExitCode::from(0)
}

/// Returns `true` iff `line` parses as JSON and `payload.task_id == id`.
/// Pure helper so the unit tests below can pin behaviour without disk I/O.
fn line_matches_task(line: &str, id: i64) -> bool {
    let v: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return false,
    };
    v.get("payload")
        .and_then(|p| p.get("task_id"))
        .and_then(|t| t.as_i64())
        == Some(id)
}

#[cfg(test)]
mod tasks_tail_tests {
    use super::line_matches_task;

    #[test]
    fn matches_payload_task_id() {
        let line = r#"{"id":1,"ts":"x","actor":"a","action":"b","payload":{"task_id":42,"x":1}}"#;
        assert!(line_matches_task(line, 42));
        assert!(!line_matches_task(line, 43));
    }

    #[test]
    fn does_not_match_substring_lookalikes() {
        // parent_task_id contains the substring "task_id" — the old
        // string-search filter would false-positive here.
        let line = r#"{"id":1,"ts":"x","actor":"a","action":"b","payload":{"parent_task_id":42}}"#;
        assert!(!line_matches_task(line, 42));
    }

    #[test]
    fn handles_whitespace_padded_json() {
        let line = r#"{ "id" : 1, "payload" : { "task_id" : 42 } }"#;
        assert!(line_matches_task(line, 42));
    }

    #[test]
    fn skips_non_json_lines() {
        assert!(!line_matches_task("not json at all", 42));
        assert!(!line_matches_task("", 42));
    }
}
