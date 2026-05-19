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
//! * `ask "<instruction>" [--fast|--long] [--classification-floor <DataClass>]` — submit a task to the
//!   scheduler, LISTEN for the completion NOTIFY, then print the
//!   result. Ctrl-C cancels the pending/running task.
//!
//! * `tasks list|status|cancel|fail|tail` — inspect and manage
//!   tasks in the scheduler DB.
//!
//! * `tools allowlist add|remove|list` — manage the per-tool argv0
//!   allowlist stored in `tool_allowlists`. Add/remove emit one
//!   `actor='cli' action='tools.allowlist.{add,remove}'` audit row
//!   on a real state change; idempotent no-ops and validation errors
//!   write no audit row.
//!
//! * `memory l1 add|list|remove` — operator-facing management of
//!   layer-1 (in-prompt insight) memories. Add/remove emit one
//!   `actor='cli' action='l1.{added,removed}'` audit row per
//!   operation. `add` is idempotent (duplicate body_sha256 returns
//!   `skipped_duplicate`); `list` prints the in-prompt slice by
//!   default, or every L1 row with `--all`.
//!
//! Usage:
//!
//! ```text
//! hhagent-cli ask "<instruction>" [--fast|--long] [--classification-floor <DataClass>]
//! hhagent-cli tasks list   [--lane fast|long] [--state <state>] [-n 20]
//! hhagent-cli tasks status <id>
//! hhagent-cli tasks cancel <id>
//! hhagent-cli tasks fail   <id>
//! hhagent-cli tasks tail   <id>
//! hhagent-cli tools allowlist add    <tool> <argv0>
//! hhagent-cli tools allowlist remove <tool> <argv0>
//! hhagent-cli tools allowlist list   [--tool <name>]
//! hhagent-cli memory l1 add    <body>
//! hhagent-cli memory l1 list   [--all]
//! hhagent-cli memory l1 remove <id>
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
        "ask"         => run_ask(&args[2..]),
        "tasks"       => run_tasks(&args[2..]),
        "tools"       => run_tools(&args[2..]),
        "memory"      => run_memory(&args[2..]),
        "observation" => run_observation(&args[2..]),
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
    hhagent-cli ask \"<instruction>\" [--fast|--long] [--classification-floor <DataClass>]
    hhagent-cli tasks list   [--lane fast|long] [--state <state>] [-n 20]
    hhagent-cli tasks status <id>
    hhagent-cli tasks cancel <id>
    hhagent-cli tasks fail   <id>
    hhagent-cli tasks tail   <id>
    hhagent-cli tools allowlist add    <tool> <argv0>
    hhagent-cli tools allowlist remove <tool> <argv0>
    hhagent-cli tools allowlist list   [--tool <name>]
    hhagent-cli memory l1 add    <body>
    hhagent-cli memory l1 list   [--all]
    hhagent-cli memory l1 remove <id>
    hhagent-cli observation replay     [--captures-dir PATH] [--model SLUG]
    hhagent-cli audit tail   [--from-start] [--no-follow] [--state-dir PATH]

flags (ask):
    --fast | --long             Lane selection (default: --fast).
    --classification-floor V    Set the task-level data classification
                                floor. Valid values: Public (default),
                                Personal, ClinicalConfidential, Secret.
                                Pin a non-Public floor when the task
                                involves sensitive data so the Stage 0
                                reviewer can catch classification leaks
                                in the agent's plans.

flags (audit tail):
    --from-start    Replay every line in every existing audit file
                    before switching to follow mode.
    --no-follow     Exit after replaying existing content (use with
                    --from-start for a 'cat' of the JSONL files).
    --state-dir P   Override the state dir (default: $HHAGENT_STATE_DIR
                    or $HOME/.local/state/hhagent).

flags (observation replay):
    --captures-dir P  Override the captures directory (default:
                      tests/observation/captures relative to
                      CARGO_MANIFEST_DIR for cargo-run, or cwd for
                      installed binaries).
    --model SLUG      Filter to captures whose filename contains the
                      slug (e.g. gemma4-26b-a4b-it-q8-0). Without it,
                      every <fixture_id>/*.json is replayed.
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

/// Parse a `--classification-floor` CLI value into a `DataClass`.
///
/// Case-insensitive; accepts canonical `PascalCase`, lowercase,
/// `UPPERCASE`, hyphen-separated, snake_case, and space-separated
/// forms (`clinical_confidential`, `clinical-confidential`,
/// `clinical confidential` all map to
/// `DataClass::ClinicalConfidential`).
///
/// Returns `Err(message)` on unknown values or empty input; the
/// message lists every valid value so the operator can correct in
/// one step.
pub(crate) fn parse_classification_floor(
    raw: &str,
) -> Result<hhagent_core::cassandra::DataClass, String> {
    use hhagent_core::cassandra::DataClass;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(
            "--classification-floor: empty value; valid values: Public, Personal, ClinicalConfidential, Secret"
                .to_string(),
        );
    }
    // Normalise: drop all `_`, `-`, and ASCII whitespace; lowercase.
    let normalised: String = trimmed
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && *c != '_' && *c != '-')
        .flat_map(|c| c.to_lowercase())
        .collect();
    match normalised.as_str() {
        "public" => Ok(DataClass::Public),
        "personal" => Ok(DataClass::Personal),
        "clinicalconfidential" => Ok(DataClass::ClinicalConfidential),
        "secret" => Ok(DataClass::Secret),
        _ => Err(format!(
            "--classification-floor: unknown value {raw:?}; valid values: Public, Personal, ClinicalConfidential, Secret"
        )),
    }
}

// ---------------------------------------------------------------------------
// `ask` subcommand
// ---------------------------------------------------------------------------

fn run_ask(args: &[String]) -> ExitCode {
    let mut lane = hhagent_db::tasks::Lane::Fast;
    let mut floor: Option<hhagent_core::cassandra::DataClass> = None;
    let mut instruction: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--long" => { lane = hhagent_db::tasks::Lane::Long; }
            "--fast" => { lane = hhagent_db::tasks::Lane::Fast; }
            "--classification-floor" => {
                i += 1;
                let Some(val) = args.get(i) else {
                    eprintln!("--classification-floor requires a value");
                    return ExitCode::from(2);
                };
                match parse_classification_floor(val) {
                    Ok(f) => floor = Some(f),
                    Err(msg) => {
                        eprintln!("{msg}");
                        return ExitCode::from(2);
                    }
                }
            }
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
        eprintln!("usage: hhagent-cli ask \"<instruction>\" [--fast|--long] [--classification-floor <DataClass>]");
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
    rt.block_on(ask_async(lane, instruction, floor))
}

/// Pure builder for the producer-side classification-floor decision.
///
/// Resolves `(floor, source, signals)` for an `ask` submission given:
///   - `instruction`: the user prompt (input to `infer_floor`).
///   - `operator_flag`: `Some(class)` iff `--classification-floor` was passed.
///   - `warn_on_suppress`: callback fired when the operator-explicit value
///     is LOWER than what inference would have produced (so the suppression
///     is operator-visible via `tracing::warn!` in the production caller).
///
/// Trust posture (producer-trusted, mirroring spec §2):
///   - Operator-explicit ALWAYS wins. The operator is committing to the
///     floor; inference results are visible via the warn callback but
///     never override.
///   - No operator flag + Public-with-no-signals → `Default` (no
///     elevation, no presentation noise).
///   - No operator flag + matched signals → `CliInferred` (carries the
///     matched tags for audit-row provenance).
///
/// Extracted as a pure helper so the wire shape is unit-testable
/// without spinning up a Postgres pool or the tokio runtime.
fn resolve_floor_for_submission(
    instruction: &str,
    operator_flag: Option<hhagent_core::cassandra::DataClass>,
    warn_on_suppress: &mut dyn FnMut(hhagent_core::cassandra::DataClass, &[&'static str]),
) -> (
    hhagent_core::cassandra::DataClass,
    hhagent_core::scheduler::inner_loop::ClassificationFloorSource,
    Vec<&'static str>,
) {
    use hhagent_core::cassandra::DataClass;
    use hhagent_core::classification_inference::infer_floor;
    use hhagent_core::scheduler::inner_loop::ClassificationFloorSource as Src;

    if let Some(op) = operator_flag {
        // Operator wins. Optionally warn if inference would have elevated.
        let inferred = infer_floor(instruction);
        if inferred.class.rank() > op.rank() {
            warn_on_suppress(inferred.class, &inferred.signals);
        }
        return (op, Src::Operator, vec![]);
    }
    let inferred = infer_floor(instruction);
    if inferred.class == DataClass::Public && inferred.signals.is_empty() {
        return (DataClass::Public, Src::Default, vec![]);
    }
    (inferred.class, Src::CliInferred, inferred.signals)
}

async fn ask_async(
    lane: hhagent_db::tasks::Lane,
    instruction: String,
    floor: Option<hhagent_core::cassandra::DataClass>,
) -> ExitCode {
    use hhagent_core::cli_audit::{cancel_and_audit, submit_and_audit};
    use hhagent_db::pool::connect_runtime_pool;
    use hhagent_db::tasks::get;
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

    let mut payload = serde_json::json!({"instruction": instruction, "kind": "ask"});

    // Resolve floor + source + signals via the pure helper. The closure
    // captures `floor` so we can render its PascalCase string for the
    // warn line when inference would have elevated above the operator's
    // pinned value.
    let mut suppressed: Option<(hhagent_core::cassandra::DataClass, Vec<&'static str>)> = None;
    let (resolved_floor, resolved_source, resolved_signals) =
        resolve_floor_for_submission(&instruction, floor, &mut |c, s| {
            suppressed = Some((c, s.to_vec()));
        });
    if let Some((inferred_class, inferred_sigs)) = &suppressed {
        tracing::warn!(
            inferred_class = inferred_class.as_pascal_str(),
            inferred_signals = ?inferred_sigs,
            operator_floor = floor.map(|f| f.as_pascal_str()).unwrap_or(""),
            "--classification-floor explicitly suppressed an elevation the keyword classifier would have made"
        );
    }
    if let serde_json::Value::Object(ref mut m) = payload {
        // Floor as PascalCase string (matches `scheduler::runner`'s reader).
        m.insert(
            "classification_floor".into(),
            serde_json::to_value(resolved_floor).expect("DataClass serialises"),
        );
        // Source: always written, snake_case (matches the reader's
        // `from_str::<ClassificationFloorSource>` expectation).
        m.insert(
            "classification_floor_source".into(),
            serde_json::json!(resolved_source.as_snake_str()),
        );
        // Signals: only when present (omitted for Operator / Default /
        // empty CliInferred — though the helper never emits empty
        // CliInferred).
        if !resolved_signals.is_empty() {
            m.insert(
                "classification_floor_signals".into(),
                serde_json::json!(resolved_signals),
            );
        }
    }
    let id = match submit_and_audit(&pool, lane, payload).await {
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
                    // Best-effort: even if the UPDATE or audit insert
                    // hiccups, the SIGINT path still exits 130 — the
                    // user has signalled they want out. On success the
                    // helper emits the producer-side
                    // `actor='cli' action='task.cancelled'` row.
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

// ============================================================
// `tools allowlist {add,remove,list}` subcommand tree
// ============================================================

fn run_tools(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: hhagent-cli tools allowlist <add|remove|list> ...");
        return ExitCode::from(2);
    }
    match args[0].as_str() {
        "allowlist" => run_tools_allowlist(&args[1..]),
        other => {
            eprintln!("tools: unknown subcommand {other}");
            ExitCode::from(2)
        }
    }
}

fn run_tools_allowlist(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: hhagent-cli tools allowlist <add|remove|list> ...");
        return ExitCode::from(2);
    }
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("tools allowlist: failed to build tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };
    match args[0].as_str() {
        "add"    => rt.block_on(tools_allowlist_add(&args[1..])),
        "remove" => rt.block_on(tools_allowlist_remove(&args[1..])),
        "list"   => rt.block_on(tools_allowlist_list(&args[1..])),
        other    => {
            eprintln!("tools allowlist: unknown subcommand {other}");
            ExitCode::from(2)
        }
    }
}

async fn tools_allowlist_add(args: &[String]) -> ExitCode {
    use hhagent_core::cli_audit::tools_allowlist_add_and_audit;
    use hhagent_db::pool::connect_runtime_pool;

    let (tool, argv0) = match args {
        [t, a] => (t.clone(), a.clone()),
        _ => {
            eprintln!("usage: hhagent-cli tools allowlist add <tool> <argv0>");
            return ExitCode::from(2);
        }
    };

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    match tools_allowlist_add_and_audit(&pool, &tool, &argv0).await {
        Ok(true)  => { println!("added {tool} {argv0}"); ExitCode::from(0) }
        Ok(false) => { println!("already present"); ExitCode::from(0) }
        Err(e @ (hhagent_db::tool_allowlists::ToolAllowlistError::InvalidArgv0
            | hhagent_db::tool_allowlists::ToolAllowlistError::InvalidToolName
            | hhagent_db::tool_allowlists::ToolAllowlistError::Argv0HasNul
            | hhagent_db::tool_allowlists::ToolAllowlistError::Argv0HasDotDot)) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
        Err(e) => { eprintln!("{e}"); ExitCode::from(1) }
    }
}

async fn tools_allowlist_remove(args: &[String]) -> ExitCode {
    use hhagent_core::cli_audit::tools_allowlist_remove_and_audit;
    use hhagent_db::pool::connect_runtime_pool;

    let (tool, argv0) = match args {
        [t, a] => (t.clone(), a.clone()),
        _ => {
            eprintln!("usage: hhagent-cli tools allowlist remove <tool> <argv0>");
            return ExitCode::from(2);
        }
    };
    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    match tools_allowlist_remove_and_audit(&pool, &tool, &argv0).await {
        Ok(true)  => { println!("removed {tool} {argv0}"); ExitCode::from(0) }
        Ok(false) => { println!("not present"); ExitCode::from(0) }
        Err(e @ (hhagent_db::tool_allowlists::ToolAllowlistError::InvalidArgv0
            | hhagent_db::tool_allowlists::ToolAllowlistError::InvalidToolName
            | hhagent_db::tool_allowlists::ToolAllowlistError::Argv0HasNul
            | hhagent_db::tool_allowlists::ToolAllowlistError::Argv0HasDotDot)) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
        Err(e) => { eprintln!("{e}"); ExitCode::from(1) }
    }
}

async fn tools_allowlist_list(args: &[String]) -> ExitCode {
    use hhagent_db::pool::connect_runtime_pool;
    use hhagent_db::tool_allowlists::{list_all, list_for_tool_full};

    let mut tool_filter: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--tool" => {
                tool_filter = args.get(i + 1).cloned();
                if tool_filter.is_none() {
                    eprintln!("--tool requires a name argument");
                    return ExitCode::from(2);
                }
                i += 2;
            }
            other => {
                eprintln!("tools allowlist list: unknown flag {other}");
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

    // --tool pushes the WHERE down to the PK-indexed query
    // (`list_for_tool_full`); the no-filter path stays a single
    // server-side scan via `list_all`.
    let entries = match &tool_filter {
        Some(t) => match list_for_tool_full(&pool, t).await {
            Ok(v) => v,
            Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
        },
        None => match list_all(&pool).await {
            Ok(v) => v,
            Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
        },
    };
    println!("{:<16}  {:<48}  {:<24}  {}",
        "TOOL", "ARGV0", "CREATED_AT", "CREATED_BY");
    for e in entries {
        println!("{:<16}  {:<48}  {:<24}  {}",
            e.tool, e.argv0, e.created_at, e.created_by);
    }
    ExitCode::from(0)
}

// ============================================================
// `memory l1 {add,list,remove}` subcommand tree
// ============================================================

fn run_memory(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: hhagent-cli memory l1 <add|list|remove> ...");
        return ExitCode::from(2);
    }
    match args[0].as_str() {
        "l1"  => run_memory_l1(&args[1..]),
        other => {
            eprintln!("memory: unknown subgroup '{other}'; expected: l1");
            ExitCode::from(2)
        }
    }
}

fn run_memory_l1(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: hhagent-cli memory l1 <add|list|remove> ...");
        return ExitCode::from(2);
    }
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("memory l1: failed to build tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };
    match args[0].as_str() {
        "add"    => rt.block_on(memory_l1_add(&args[1..])),
        "list"   => rt.block_on(memory_l1_list(&args[1..])),
        "remove" => rt.block_on(memory_l1_remove(&args[1..])),
        other    => {
            eprintln!("memory l1: unknown action '{other}'; expected: add | list | remove");
            ExitCode::from(2)
        }
    }
}

async fn memory_l1_add(args: &[String]) -> ExitCode {
    use hhagent_core::cli_audit::l1_add_and_audit;
    use hhagent_core::entity_extraction::NoOpEntityExtractor;
    use hhagent_core::memory::l1_promote::L1WriteOutcome;
    use hhagent_db::pool::connect_runtime_pool;

    let body = match args {
        [b] => b,
        _ => {
            eprintln!("usage: hhagent-cli memory l1 add <body>");
            return ExitCode::from(2);
        }
    };

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    let extractor = NoOpEntityExtractor::new();
    match l1_add_and_audit(&pool, &extractor, body).await {
        Ok((L1WriteOutcome::Inserted { memory_id, .. }, _)) => {
            println!("inserted id={memory_id}");
            ExitCode::from(0)
        }
        Ok((L1WriteOutcome::SkippedDuplicate { memory_id }, _)) => {
            println!("skipped_duplicate id={memory_id} (body_sha256 already at layer 1)");
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("memory l1 add: {e}");
            ExitCode::from(1)
        }
    }
}

async fn memory_l1_list(args: &[String]) -> ExitCode {
    use hhagent_core::memory::l1_promote::list_l1;
    use hhagent_db::pool::connect_runtime_pool;

    let mut all = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--all" => {
                all = true;
                i += 1;
            }
            other => {
                eprintln!("memory l1 list: unknown flag '{other}'");
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

    let rows = match list_l1(&pool, all).await {
        Ok(r) => r,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    println!("{:<8}  {:<32}  BODY",
        "ID", "CREATED_AT");
    for r in rows {
        println!("{:<8}  {:<32}  {}",
            r.id, r.created_at, r.body);
    }
    ExitCode::from(0)
}

async fn memory_l1_remove(args: &[String]) -> ExitCode {
    use hhagent_core::cli_audit::l1_remove_and_audit;
    use hhagent_db::pool::connect_runtime_pool;

    let id_str = match args {
        [s] => s,
        _ => {
            eprintln!("usage: hhagent-cli memory l1 remove <id>");
            return ExitCode::from(2);
        }
    };
    let id: i64 = match id_str.parse() {
        Ok(n) => n,
        Err(e) => {
            eprintln!("memory l1 remove: invalid id '{id_str}': {e}");
            return ExitCode::from(2);
        }
    };

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    match l1_remove_and_audit(&pool, id).await {
        Ok((true, _))  => { println!("removed id={id}"); ExitCode::from(0) }
        Ok((false, _)) => {
            println!("no row at layer 1 with id={id} (already gone or wrong layer)");
            ExitCode::from(0)
        }
        Err(e) => { eprintln!("memory l1 remove: {e}"); ExitCode::from(1) }
    }
}

// ============================================================
// `observation replay` subcommand
// ============================================================

fn run_observation(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: hhagent-cli observation replay [opts]");
        return ExitCode::from(2);
    }
    match args[0].as_str() {
        "replay" => run_observation_replay(&args[1..]),
        other => {
            eprintln!("observation: unknown subcommand {other}");
            ExitCode::from(2)
        }
    }
}

fn run_observation_replay(args: &[String]) -> ExitCode {
    let mut captures_dir: Option<PathBuf> = None;
    let mut model_filter: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--captures-dir" => {
                i += 1;
                match args.get(i) {
                    Some(p) => captures_dir = Some(PathBuf::from(p)),
                    None => {
                        eprintln!("--captures-dir requires a PATH argument");
                        return ExitCode::from(2);
                    }
                }
            }
            "--model" => {
                i += 1;
                match args.get(i) {
                    Some(s) => model_filter = Some(s.clone()),
                    None => {
                        eprintln!("--model requires a SLUG argument");
                        return ExitCode::from(2);
                    }
                }
            }
            other => {
                eprintln!("observation replay: unknown flag {other}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }

    let dir = captures_dir.unwrap_or_else(default_captures_dir);

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("observation replay: failed to build tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };

    rt.block_on(observation_replay_async(&dir, model_filter.as_deref()))
}

/// Default captures dir. For `cargo run` invocations
/// `CARGO_MANIFEST_DIR` points at `core/`; the workspace root is one
/// level up. For installed binaries neither env var is set; fall back
/// to CWD-relative `tests/observation/captures`. Operator can always
/// override via `--captures-dir`.
///
/// Invariant: this binary lives in the `core/` crate. If it ever
/// moves, the `pop()`-to-workspace-root assumption breaks and the
/// default path resolves to the wrong place. The `debug_assert`
/// below catches the relocation during local dev (release builds
/// will silently produce the wrong default — `--captures-dir`
/// remains the escape hatch).
fn default_captures_dir() -> PathBuf {
    if let Some(manifest) = std::env::var_os("CARGO_MANIFEST_DIR") {
        let mut p = PathBuf::from(manifest);
        debug_assert_eq!(
            p.file_name().and_then(|s| s.to_str()),
            Some("core"),
            "default_captures_dir assumes hhagent-cli lives in core/ \
             (CARGO_MANIFEST_DIR = {p:?})"
        );
        p.pop(); // strip `/core` to reach workspace root
        p.push("tests/observation/captures");
        return p;
    }
    PathBuf::from("tests/observation/captures")
}

async fn observation_replay_async(
    dir: &std::path::Path,
    model_filter: Option<&str>,
) -> ExitCode {
    use std::sync::Arc;
    use hhagent_core::cassandra::review::{
        ChainReviewStage, ConstitutionalGuard, DeterministicPolicy,
    };
    use hhagent_core::observation::replay::{
        format_report_table, load_captures_from_dir, replay_capture, ReplayResult,
    };

    let loaded = match load_captures_from_dir(dir) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("observation replay: cannot open {dir:?}: {e}");
            return ExitCode::from(1);
        }
    };

    if loaded.is_empty() {
        println!("(no captures found in {})", dir.display());
        return ExitCode::from(0);
    }

    // Production chain composition. Operator iterates by editing the
    // ConstitutionalGuard / DeterministicPolicy bodies in
    // core/src/cassandra/review.rs and re-running this subcommand.
    let chain = ChainReviewStage::new(vec![
        Arc::new(ConstitutionalGuard),
        Arc::new(DeterministicPolicy),
    ]);

    let mut results: Vec<ReplayResult> = Vec::new();
    let mut filtered_out: u32 = 0;
    for entry in loaded {
        if let Some(filter) = model_filter {
            let fname = entry.path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !fname.contains(filter) {
                filtered_out = filtered_out.saturating_add(1);
                continue;
            }
        }
        let r = replay_capture(&entry.capture, &chain).await;
        results.push(r);
    }

    if results.is_empty() {
        eprintln!(
            "observation replay: no captures matched filter (--model {} filtered out {})",
            model_filter.unwrap_or("<none>"),
            filtered_out,
        );
        return ExitCode::from(0);
    }

    print!("{}", format_report_table(&results));
    ExitCode::from(0)
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

#[cfg(test)]
mod parse_classification_floor_tests {
    use super::parse_classification_floor;
    use hhagent_core::cassandra::DataClass;

    #[test]
    fn accepts_canonical_pascal_case() {
        assert_eq!(parse_classification_floor("Public").unwrap(), DataClass::Public);
        assert_eq!(parse_classification_floor("Personal").unwrap(), DataClass::Personal);
        assert_eq!(parse_classification_floor("ClinicalConfidential").unwrap(), DataClass::ClinicalConfidential);
        assert_eq!(parse_classification_floor("Secret").unwrap(), DataClass::Secret);
    }

    #[test]
    fn accepts_lowercase() {
        assert_eq!(parse_classification_floor("public").unwrap(), DataClass::Public);
        assert_eq!(parse_classification_floor("clinical_confidential").unwrap(), DataClass::ClinicalConfidential);
    }

    #[test]
    fn accepts_uppercase() {
        assert_eq!(parse_classification_floor("PUBLIC").unwrap(), DataClass::Public);
        assert_eq!(parse_classification_floor("CLINICAL_CONFIDENTIAL").unwrap(), DataClass::ClinicalConfidential);
    }

    #[test]
    fn accepts_mixed_case_and_separator_variants() {
        // Hyphen-separated common in CLIs; spaces unusual but cheap to allow.
        assert_eq!(parse_classification_floor("clinical-confidential").unwrap(), DataClass::ClinicalConfidential);
        assert_eq!(parse_classification_floor("Clinical Confidential").unwrap(), DataClass::ClinicalConfidential);
    }

    #[test]
    fn rejects_unknown_value_with_helpful_message() {
        let err = parse_classification_floor("topsecret").unwrap_err();
        assert!(err.contains("topsecret"), "expected input echoed; got: {err}");
        assert!(err.contains("valid values"), "expected 'valid values' phrase; got: {err}");
        assert!(err.contains("Public"), "expected list of valid values; got: {err}");
        assert!(err.contains("ClinicalConfidential"), "expected list of valid values; got: {err}");
    }

    #[test]
    fn rejects_empty_string() {
        let err = parse_classification_floor("").unwrap_err();
        assert!(err.contains("empty"), "got: {err}");
    }

    #[test]
    fn trims_surrounding_whitespace() {
        assert_eq!(parse_classification_floor("  Public  ").unwrap(), DataClass::Public);
    }
}

/// Tests for the producer-side floor resolution helper.
#[cfg(test)]
mod resolve_floor_for_submission_tests {
    use super::resolve_floor_for_submission;
    use hhagent_core::cassandra::DataClass;
    use hhagent_core::scheduler::inner_loop::ClassificationFloorSource as Src;

    #[test]
    fn no_operator_flag_no_signals_returns_default() {
        let mut suppressed = false;
        let (cls, src, sigs) = resolve_floor_for_submission(
            "How do I write a quicksort in Rust?",
            None,
            &mut |_, _| { suppressed = true; },
        );
        assert_eq!(cls, DataClass::Public);
        assert_eq!(src, Src::Default);
        assert!(sigs.is_empty());
        assert!(!suppressed, "no warn should fire for non-clinical prompt with no operator flag");
    }

    #[test]
    fn no_operator_flag_clinical_signals_returns_cli_inferred() {
        let mut suppressed = false;
        let (cls, src, sigs) = resolve_floor_for_submission(
            "Translate the patient's pathology report.",
            None,
            &mut |_, _| { suppressed = true; },
        );
        assert_eq!(cls, DataClass::ClinicalConfidential);
        assert_eq!(src, Src::CliInferred);
        assert!(sigs.contains(&"patient"));
        assert!(sigs.contains(&"pathology"));
        assert!(!suppressed, "no warn — operator flag was absent, not suppressing");
    }

    #[test]
    fn operator_flag_wins_and_warns_when_inference_would_elevate() {
        let mut suppressed_class: Option<DataClass> = None;
        let mut suppressed_signals: Vec<&'static str> = vec![];
        let (cls, src, sigs) = resolve_floor_for_submission(
            "Translate the patient's pathology report.",
            Some(DataClass::Public),  // operator pinned LOWER than inference
            &mut |c, s| {
                suppressed_class = Some(c);
                suppressed_signals = s.to_vec();
            },
        );
        assert_eq!(cls, DataClass::Public, "operator wins");
        assert_eq!(src, Src::Operator);
        assert!(sigs.is_empty(), "Operator source carries no signals");
        assert_eq!(suppressed_class, Some(DataClass::ClinicalConfidential),
            "warn should fire because inference would have elevated to Clinical");
        assert!(suppressed_signals.contains(&"patient"));
    }

    #[test]
    fn operator_flag_wins_and_no_warn_when_inference_does_not_elevate() {
        let mut suppressed = false;
        let (cls, src, sigs) = resolve_floor_for_submission(
            "How do I write a quicksort in Rust?",
            Some(DataClass::ClinicalConfidential),
            &mut |_, _| { suppressed = true; },
        );
        assert_eq!(cls, DataClass::ClinicalConfidential);
        assert_eq!(src, Src::Operator);
        assert!(sigs.is_empty());
        assert!(!suppressed, "inference inferred Public (not elevating); no warn");
    }

    #[test]
    fn operator_flag_equal_to_inference_does_not_warn() {
        // Inference would return Clinical; operator pinned Clinical.
        // The suppression condition is strict inequality (inferred > op);
        // matching values are not a suppression.
        let mut suppressed = false;
        let (cls, src, sigs) = resolve_floor_for_submission(
            "Translate the patient's pathology report.",
            Some(DataClass::ClinicalConfidential),
            &mut |_, _| { suppressed = true; },
        );
        assert_eq!(cls, DataClass::ClinicalConfidential);
        assert_eq!(src, Src::Operator);
        assert!(sigs.is_empty());
        assert!(!suppressed, "matching operator value is not a suppression");
    }
}
