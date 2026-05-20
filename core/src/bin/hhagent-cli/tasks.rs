//! `tasks {list,status,cancel,fail,tail}` — inspect and manage tasks
//! in the scheduler DB.

use std::process::ExitCode;

use hhagent_core::audit_mirror;

use crate::common::{resolve_connect_spec, with_runtime};

pub(crate) fn run_tasks(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: hhagent-cli tasks <list|status|cancel|fail|tail> ...");
        return ExitCode::from(2);
    }
    // Per-action dispatch. `with_runtime` is called only from the known
    // async arms — an invalid action exits 2 without spawning tokio
    // worker threads (Issue #97).
    match args[0].as_str() {
        "list"   => with_runtime("tasks", tasks_list(&args[1..])),
        "status" => with_runtime("tasks", tasks_status(&args[1..])),
        "cancel" => with_runtime("tasks", tasks_cancel(&args[1..])),
        "fail"   => with_runtime("tasks", tasks_fail(&args[1..])),
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
