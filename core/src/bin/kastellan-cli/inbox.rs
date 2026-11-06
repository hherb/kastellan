//! `inbox {list,show,resolve}` — the operator's answer surface for asks the
//! daemon raised (#564 slice 1b).
//!
//! Named `inbox`, not `asks`, because `kastellan-cli ask` already means
//! *submit a task*: two subcommands differing by one letter, one of which
//! approves a plan, is a trap for exactly the operator who is tired enough
//! to be answering an escalation at all.
//!
//! Resolves by row **id**, using `db::asks::resolve` rather than
//! `resolve_with_nonce`. An id has no unforgeability property, which is safe
//! only because this caller is the operator's own local binary; any caller
//! reachable from an untrusted transport must use the nonce form (slice 2).

use std::process::ExitCode;

use kastellan_core::cli_audit::CLI_AUDIT_ACTOR;
use kastellan_core::scheduler::audit::ACTION_ASK_RESOLVED;

use crate::common::{resolve_connect_spec, with_runtime};

pub(crate) fn run_inbox(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: kastellan-cli inbox <list|show|resolve> ...");
        return ExitCode::from(2);
    }
    match args[0].as_str() {
        "list" => with_runtime("inbox", inbox_list(&args[1..])),
        "show" => with_runtime("inbox", inbox_show(&args[1..])),
        "resolve" => with_runtime("inbox", inbox_resolve(&args[1..])),
        other => {
            eprintln!("inbox: unknown subcommand {other}");
            ExitCode::from(2)
        }
    }
}

/// The two answers a `plan_approval` ask offers. Checked here so a typo
/// reads as a usage error (exit 2) rather than as a database refusal —
/// `db::asks::resolve` validates against the ask's own `options` too, and
/// that check is the authoritative one.
pub(crate) const CHOICES: [&str; 2] = ["approve", "deny"];

async fn inbox_list(args: &[String]) -> ExitCode {
    use kastellan_db::pool::connect_runtime_pool;

    let mut limit: i64 = 20;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-n" => {
                limit = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(20);
                i += 2;
            }
            other => {
                eprintln!("inbox list: unknown flag {other}");
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
        Err(e) => { eprintln!("connect: {e}"); return ExitCode::from(1); }
    };

    let asks = match kastellan_db::asks::list_pending(&pool, limit).await {
        Ok(a) => a,
        Err(e) => { eprintln!("inbox list: {e}"); return ExitCode::from(1); }
    };
    if asks.is_empty() {
        println!("no pending asks");
        return ExitCode::SUCCESS;
    }
    println!("{:>6}  {:>7}  {:<20}  QUESTION", "ASK", "TASK", "DEADLINE");
    for a in &asks {
        // The question is the whole point of an inbox — clamped, never
        // omitted. `chars()` not bytes: a multibyte question must not be
        // truncated mid-codepoint.
        let q: String = a.body.chars().take(80).collect();
        let ellipsis = if a.body.chars().count() > 80 { "…" } else { "" };
        println!("{:>6}  {:>7}  {:<20}  {q}{ellipsis}", a.id, a.task_id, a.deadline_at);
    }
    println!("\nanswer with: kastellan-cli inbox resolve <ASK> approve|deny [--note \"...\"]");
    ExitCode::SUCCESS
}

async fn inbox_show(args: &[String]) -> ExitCode {
    use kastellan_db::pool::connect_runtime_pool;

    let Some(ask_id) = args.first().and_then(|s| s.parse::<i64>().ok()) else {
        eprintln!("usage: kastellan-cli inbox show <ask-id>");
        return ExitCode::from(2);
    };
    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("connect: {e}"); return ExitCode::from(1); }
    };
    let ask = match kastellan_db::asks::get(&pool, ask_id).await {
        Ok(Some(a)) => a,
        Ok(None) => { eprintln!("no ask with id {ask_id}"); return ExitCode::from(1); }
        Err(e) => { eprintln!("inbox show: {e}"); return ExitCode::from(1); }
    };
    // Every field `Ask` carries. There is no nonce field on it, deliberately
    // — the plaintext is returned once by `raise` and never stored.
    println!("ask         {}", ask.id);
    println!("task        {}", ask.task_id);
    println!("kind        {}", ask.kind);
    println!("state       {}", ask.state);
    println!("created     {}", ask.created_at);
    println!("deadline    {}", ask.deadline_at);
    println!("options     {}", ask.options);
    println!("plan digest {}", ask.plan_digest.as_deref().unwrap_or("-"));
    println!("resolved at {}", ask.resolved_at.map(|t| t.to_string()).unwrap_or_else(|| "-".into()));
    println!("resolved by {}", ask.resolved_by.as_deref().unwrap_or("-"));
    println!("resolution  {}", ask.resolution.as_ref().map(|r| r.to_string()).unwrap_or_else(|| "-".into()));
    println!("\nquestion:\n{}", ask.body);
    ExitCode::SUCCESS
}

async fn inbox_resolve(args: &[String]) -> ExitCode {
    use kastellan_db::pool::connect_runtime_pool;

    let Some(ask_id) = args.first().and_then(|s| s.parse::<i64>().ok()) else {
        eprintln!("usage: kastellan-cli inbox resolve <ask-id> approve|deny [--note \"<text>\"]");
        return ExitCode::from(2);
    };
    let Some(choice) = args.get(1) else {
        eprintln!("usage: kastellan-cli inbox resolve <ask-id> approve|deny [--note \"<text>\"]");
        return ExitCode::from(2);
    };
    if !CHOICES.contains(&choice.as_str()) {
        eprintln!("inbox resolve: choice must be 'approve' or 'deny', got {choice:?}");
        return ExitCode::from(2);
    }
    let mut note: Option<String> = None;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--note" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--note needs value");
                    return ExitCode::from(2);
                };
                note = Some(v.clone());
                i += 2;
            }
            other => {
                eprintln!("inbox resolve: unknown flag {other}");
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
        Err(e) => { eprintln!("connect: {e}"); return ExitCode::from(1); }
    };

    match resolve_and_audit(&pool, ask_id, choice, note.as_deref()).await {
        Ok(Resolved { task_id, audited }) => {
            println!("ask {ask_id} resolved '{choice}'; task {task_id} returned to the queue");
            if !audited {
                eprintln!("warning: ask.resolved audit row failed; the answer stands");
            }
            ExitCode::SUCCESS
        }
        Err(e) => { eprintln!("{e}"); ExitCode::from(1) }
    }
}

/// An ask answered by [`resolve_and_audit`]: the task it belonged to (now
/// back in the queue), and whether its `ask.resolved` row was written.
pub(crate) struct Resolved {
    pub task_id: i64,
    pub audited: bool,
}

/// Answer ask `ask_id` with `choice` as the operator at this terminal, and
/// write one `actor='cli' action='ask.resolved'` row. Shared by `inbox
/// resolve` and `session`'s `inbox.answer`. `choice` must already be one of
/// [`CHOICES`]; the database checks it against the ask's own options too.
///
/// `Err` is the operator-facing reason nothing was resolved: no such ask,
/// a lost race (already answered, expired, cancelled), or a database error.
/// The audit insert is best-effort — the answer has already landed — and a
/// lost row is reported as `audited: false`.
pub(crate) async fn resolve_and_audit(
    pool: &sqlx::PgPool,
    ask_id: i64,
    choice: &str,
    note: Option<&str>,
) -> Result<Resolved, String> {
    // Read the ask first for its `task_id`: every other `task.*` / `ask.*`
    // audit row is keyed on it, and a row without it cannot be joined to
    // the task the decision was about.
    let task_id = match kastellan_db::asks::get(pool, ask_id).await {
        Ok(Some(a)) => a.task_id,
        Ok(None) => return Err(format!("no ask with id {ask_id}")),
        Err(e) => return Err(format!("inbox resolve: {e}")),
    };

    // Free text is carried for the record and shown to the operator; it is
    // never interpolated into a plan (spec D10). Shaped by `db::asks` so
    // this surface and the channel resolver cannot drift on the key names.
    let resolution = kastellan_db::asks::resolution(choice, note);
    let resolved_by = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "operator".to_string());

    match kastellan_db::asks::resolve(pool, ask_id, &resolved_by, &resolution).await {
        Ok(true) => {}
        // NOT a success. First-responder-wins is a database property;
        // reporting success here would tell the operator their answer
        // stood when someone else's did.
        Ok(false) => {
            return Err(format!(
                "ask {ask_id} was not resolvable — already answered, expired, cancelled, \
                 or past its deadline. Nothing was written."
            ))
        }
        Err(e) => return Err(format!("inbox resolve: {e}")),
    }

    // `via` names the answering SURFACE, and both surfaces write it (#564
    // slice 2). The channel resolver writes `via: "channel"`; without this
    // key here, `payload->>'via'` would be NULL for exactly the CLI half of
    // one `ask.resolved` population, so any observation query splitting on
    // it silently mis-buckets every operator answer given at the terminal.
    let payload = serde_json::json!({
        "ask_id": ask_id,
        "task_id": task_id,
        "choice": choice,
        "resolved_by": resolved_by,
        "free_text": note,
        "via": "cli",
    });
    let audited = match kastellan_db::audit::insert(pool, CLI_AUDIT_ACTOR, ACTION_ASK_RESOLVED, payload).await {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(ask_id, error = %e, "ask.resolved audit insert failed (answer already landed)");
            false
        }
    };
    Ok(Resolved { task_id, audited })
}
