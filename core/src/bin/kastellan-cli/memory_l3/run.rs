//! `memory l3 run <id> [--arg name=value]… [--execute|--yes]` —
//! operator-triggered invocation of an approved crystallised skill. Submits
//! an `l3_run` task to the daemon and waits for completion. Dry-run by
//! default (no `--execute`). See [`memory_l3_run`].

use std::process::ExitCode;

use crate::common::resolve_connect_spec;

/// Parsed argv for `memory l3 run` (after the `run` subcommand token is
/// stripped). `arg_tokens` are the raw `name=value` strings, validated later
/// by [`kastellan_core::memory::l3_invoke::parse_args`]. `param_tokens` are
/// `name=value` runtime params for PYTHON skills (string-valued sugar);
/// `params_json` is the full-JSON channel for richer param types. Both merge
/// into one `params` object sent in the `l3_run` payload.
#[derive(Debug, PartialEq, Eq)]
struct RunArgv {
    id: i64,
    arg_tokens: Vec<String>,
    param_tokens: Vec<String>,
    params_json: Option<String>,
    execute: bool,
}

/// Pure parse of `memory l3 run <id> [--arg name=value]… [--execute|--yes]`.
///
/// Accepts the id positionally (first non-`--` token, anywhere in the argv,
/// so `--execute 5` and `5 --execute` are equivalent), `--arg name=value`
/// (and the GNU `--arg=name=value` form, repeatable), and `--execute` /
/// `--yes` (aliases). Returns the structured form, or a ready-to-print
/// usage error string (the caller emits it to stderr and exits 2). No I/O.
fn parse_run_argv(args: &[String]) -> Result<RunArgv, String> {
    let mut id_str: Option<&String> = None;
    let mut arg_tokens: Vec<String> = Vec::new();
    let mut param_tokens: Vec<String> = Vec::new();
    let mut params_json: Option<String> = None;
    let mut execute = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--execute" | "--yes" => execute = true,
            "--arg" => {
                i += 1;
                match args.get(i) {
                    Some(kv) => arg_tokens.push(kv.clone()),
                    None => return Err("memory l3 run: --arg requires a name=value".to_string()),
                }
            }
            // GNU-style equals form: --arg=name=value
            s if s.starts_with("--arg=") => arg_tokens.push(s["--arg=".len()..].to_string()),
            "--param" => {
                i += 1;
                match args.get(i) {
                    Some(kv) => param_tokens.push(kv.clone()),
                    None => return Err("memory l3 run: --param requires a name=value".to_string()),
                }
            }
            s if s.starts_with("--param=") => param_tokens.push(s["--param=".len()..].to_string()),
            "--params-json" => {
                i += 1;
                match args.get(i) {
                    Some(j) => params_json = Some(j.clone()),
                    None => return Err("memory l3 run: --params-json requires a JSON object".to_string()),
                }
            }
            s if s.starts_with("--params-json=") => {
                params_json = Some(s["--params-json=".len()..].to_string())
            }
            s if id_str.is_none() && !s.starts_with("--") => id_str = Some(&args[i]),
            other => return Err(format!("memory l3 run: unexpected argument '{other}'")),
        }
        i += 1;
    }
    let id_str = id_str.ok_or_else(|| {
        "usage: kastellan-cli memory l3 run <id> [--arg name=value]… [--execute | --yes]".to_string()
    })?;
    let id: i64 = id_str
        .parse()
        .map_err(|e| format!("memory l3 run: invalid id '{id_str}': {e}"))?;
    Ok(RunArgv { id, arg_tokens, param_tokens, params_json, execute })
}

/// Merge `--params-json` (base object) with `--param name=value` overrides into
/// one validated JSON object. Starts from the parsed `--params-json` (or `{}`),
/// then applies each `name=value` token as a STRING value (later wins). Rejects
/// a non-object base, malformed JSON, or a token without `=`. The result is
/// re-validated host-side by `validate_python_params` before dispatch; this
/// function only assembles it.
pub(super) fn build_params(
    params_json: Option<&str>,
    param_tokens: &[String],
) -> Result<serde_json::Value, String> {
    let mut base = match params_json {
        None => serde_json::Map::new(),
        Some(s) => match serde_json::from_str::<serde_json::Value>(s) {
            Ok(serde_json::Value::Object(m)) => m,
            Ok(_) => return Err("--params-json must be a JSON object".to_string()),
            Err(e) => return Err(format!("--params-json is not valid JSON: {e}")),
        },
    };
    for tok in param_tokens {
        let (name, value) = tok
            .split_once('=')
            .ok_or_else(|| format!("--param '{tok}' is not of the form name=value"))?;
        base.insert(name.to_string(), serde_json::Value::String(value.to_string()));
    }
    Ok(serde_json::Value::Object(base))
}

/// Render an [`InvokeReport`] to operator-facing text + an exit code.
///
/// Pure (no I/O) so it is unit-testable. The caller prints the text to stdout
/// when `code == 0`, else to stderr. Exit codes match the pre-reroute CLI:
/// DryRun and all-ok Executed → 0; Refused and any-error Executed → 1.
pub(super) fn render_invoke_report(
    id: i64,
    skill_name: &str,
    report: &kastellan_core::memory::l3_invoke::InvokeReport,
) -> (String, i32) {
    use std::fmt::Write as _;

    use kastellan_core::memory::l3_invoke::InvokeReport;
    use kastellan_core::scheduler::inner_loop::StepOutcome;

    let mut out = String::new();
    match report {
        InvokeReport::Refused { reasons } => {
            let _ = writeln!(out, "REFUSED to run skill '{skill_name}' (#{id}):");
            for r in reasons {
                let _ = writeln!(out, "  - {r}");
            }
            (out, 1)
        }
        InvokeReport::DryRun { steps } => {
            let _ = writeln!(
                out,
                "dry-run: skill '{skill_name}' (#{id}) would dispatch {} step(s):",
                steps.len()
            );
            for (n, s) in steps.iter().enumerate() {
                let _ = writeln!(out, "  [{n}] {}/{} {}", s.tool, s.method, s.parameters);
            }
            let _ = write!(out, "(re-run with --execute to dispatch)");
            (out, 0)
        }
        InvokeReport::Executed { outcomes, steps_total } => {
            let any_err = outcomes.iter().any(|o| o.is_err());
            let _ = writeln!(
                out,
                "executed skill '{skill_name}' (#{id}): {}/{} step(s)",
                outcomes.len(),
                steps_total
            );
            for (n, o) in outcomes.iter().enumerate() {
                match o {
                    StepOutcome::Ok(v) => {
                        let _ = writeln!(out, "  [{n}] ok: {v}");
                    }
                    StepOutcome::Err { code, detail } => {
                        let _ = writeln!(out, "  [{n}] ERR {code}: {detail}");
                    }
                }
            }
            (out, if any_err { 1 } else { 0 })
        }
    }
}

/// `memory l3 run <id> [--arg name=value]… [--execute]`
///
/// Submits an `l3_run` task to the daemon and waits for it to execute the
/// approved skill against the daemon's live tool registry (issue #179, Opt 3 —
/// the in-process registry rebuild and its env-divergence cliff are retired).
/// Dry-run by default (no `--execute`): the daemon validates + returns the
/// concrete steps without dispatching. Requires a running daemon; if a live
/// daemon is merely busy the CLI keeps waiting, but if no daemon is running at
/// all the submit is cancelled (pending-only) and an error is printed — see
/// [`wait_until_claimed_or_no_daemon`].
pub(super) async fn memory_l3_run(args: &[String]) -> ExitCode {
    use std::time::Duration;

    use kastellan_core::cli_audit::submit_and_audit;
    use kastellan_core::memory::l3_invoke::{parse_args, InvokeReport};
    use kastellan_db::pool::connect_runtime_pool;
    use kastellan_db::tasks::{get, Lane};
    use sqlx::postgres::PgListener;

    // --- parse argv ----------------------------------------------------
    let RunArgv { id, arg_tokens, param_tokens, params_json, execute } = match parse_run_argv(args) {
        Ok(v) => v,
        Err(msg) => { eprintln!("{msg}"); return ExitCode::from(2); }
    };
    let args_map = match parse_args(&arg_tokens) {
        Ok(m) => m,
        Err(e) => { eprintln!("memory l3 run: {e}"); return ExitCode::from(2); }
    };
    let params = match build_params(params_json.as_deref(), &param_tokens) {
        Ok(p) => p,
        Err(e) => { eprintln!("memory l3 run: {e}"); return ExitCode::from(2); }
    };

    // --- connect -------------------------------------------------------
    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    // --- LISTEN before submit (avoid the NOTIFY-before-listen race) ----
    let mut listener = match PgListener::connect_with(&pool).await {
        Ok(l) => l,
        Err(e) => { eprintln!("memory l3 run: listener connect failed: {e}"); return ExitCode::from(1); }
    };
    if let Err(e) = listener.listen("tasks_completed").await {
        eprintln!("memory l3 run: listen failed: {e}");
        return ExitCode::from(1);
    }

    // --- submit the l3_run task ----------------------------------------
    let payload = serde_json::json!({
        "kind": "l3_run",
        "memory_id": id,
        "args": args_map,
        "params": params,
        "execute": execute,
    });
    let task_id = match submit_and_audit(&pool, Lane::Long, payload).await {
        Ok(i) => i,
        Err(e) => { eprintln!("memory l3 run: submit failed: {e}"); return ExitCode::from(1); }
    };
    eprintln!("memory l3 run: submitted task {task_id} (lane=long); waiting for the daemon…");

    // `grace`: how long a freshly-submitted task may sit `pending` before we
    // probe for a live daemon (5s comfortably covers claim latency for an idle
    // daemon; a *busy* daemon is handled by the liveness probe, not by raising
    // this). `overall`: the Phase-2 ceiling on the whole execution wait. It is
    // generous (30 min) because a legitimate `--execute` can run a long step
    // list; it exists only so a daemon that claims the task but never NOTIFYs
    // (a hang) cannot block the operator's terminal forever. Both are
    // env-overridable — lower `KASTELLAN_L3_RUN_TIMEOUT_SECS` for snappier
    // dry-runs, raise it for known-slow skills.
    let grace = Duration::from_secs(env_secs("KASTELLAN_L3_RUN_GRACE_SECS", 5));
    let overall = Duration::from_secs(env_secs("KASTELLAN_L3_RUN_TIMEOUT_SECS", 1800));

    // Phase 1: until claimed (daemon present) or grace elapses (no daemon).
    if let Err(code) = wait_until_claimed_or_no_daemon(&pool, &mut listener, task_id, grace).await {
        return code;
    }

    // Fast-path: already terminal? (completed within the grace window) Skip Phase 2.
    let already_done = matches!(
        get(&pool, task_id).await,
        Ok(Some(ref t)) if t.state != "running" && t.state != "pending"
    );
    if !already_done {
        // Phase 2: wait for the terminal NOTIFY for our id, bounded by `overall`.
        let completed = tokio::time::timeout(overall, async {
            loop {
                match listener.recv().await {
                    Ok(n) if n.payload() == task_id.to_string() => return Ok(()),
                    Ok(_) => continue,
                    Err(e) => return Err(format!("listener.recv: {e}")),
                }
            }
        })
        .await;
        match completed {
            Ok(Ok(())) => {}
            Ok(Err(e)) => { eprintln!("memory l3 run: {e}"); return ExitCode::from(1); }
            Err(_) => {
                eprintln!("memory l3 run: timed out after {}s waiting for task {task_id}", overall.as_secs());
                return ExitCode::from(1);
            }
        }
    }

    // --- read + render the result --------------------------------------
    let task = match get(&pool, task_id).await {
        Ok(Some(t)) => t,
        Ok(None) => { eprintln!("memory l3 run: task {task_id} disappeared"); return ExitCode::from(1); }
        Err(e) => { eprintln!("memory l3 run: get failed: {e}"); return ExitCode::from(1); }
    };
    let report: InvokeReport = match task.result {
        Some(r) => match serde_json::from_value(r) {
            Ok(rep) => rep,
            Err(e) => { eprintln!("memory l3 run: unreadable result for task {task_id}: {e}"); return ExitCode::from(1); }
        },
        None => {
            eprintln!("memory l3 run: task {task_id} ended in state '{}' with no result", task.state);
            return ExitCode::from(1);
        }
    };
    // Resolve the skill's display name for the header (best-effort, output-only
    // — a lookup miss never changes the exit path; it just falls back to a
    // placeholder). This is a memory-row read, NOT a tool-registry rebuild, so
    // it re-introduces none of the #179 env coupling.
    let skill_name = resolve_skill_name(&pool, id).await;
    let (text, code) = render_invoke_report(id, &skill_name, &report);
    if code == 0 { println!("{text}"); } else { eprintln!("{text}"); }
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// Parse a u64 seconds env var with a default; non-numeric => default.
fn env_secs(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Best-effort lookup of an L3 skill's display name by id, for operator output
/// only. Reads the stored memory row's name — `metadata.python.name` for a
/// Python skill, else `metadata.template.name` for a templated skill. Any miss
/// (DB error, absent row, no name) falls back to `"<skill>"`. Never affects
/// control flow (a name lookup is a memory-row read, NOT a tool-registry
/// rebuild, so it re-introduces none of the #179 env coupling).
async fn resolve_skill_name(pool: &sqlx::PgPool, id: i64) -> String {
    use kastellan_db::memories::fetch_by_ids;
    fetch_by_ids(pool, &[id])
        .await
        .ok()
        .and_then(|mut rows| rows.pop())
        .and_then(|row| {
            let m = &row.metadata;
            m.get("python")
                .and_then(|p| p.get("name"))
                .or_else(|| m.get("template").and_then(|t| t.get("name")))
                .and_then(|n| n.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "<skill>".to_string())
}

/// Phase-1 wait. Blocks until either the daemon claims the task (it leaves
/// `pending`) or we can soundly conclude no daemon will. Returns:
///
/// - `Ok(())` — proceed to the Phase-2 result wait. The task is claimed
///   (`running`), already terminal, OR a live-but-busy daemon exists. In the
///   busy case the daemon will claim the task once it frees up; the bounded
///   Phase-2 timeout still caps the total wait, so we do not block forever.
/// - `Err(code)` — no live daemon, and the still-`pending` task was cancelled,
///   so a `--execute` directive cannot be silently run later by a daemon that
///   starts after the CLI has given up.
///
/// ## Why a liveness probe, not just "still pending after grace"
///
/// Lanes are drained sequentially within a lane, so an `l3_run` task submitted
/// while the long lane is busy with another long task legitimately sits
/// `pending` for that task's whole duration — far longer than `grace`. Treating
/// "still pending" alone as "no daemon" would cancel a valid submission under
/// load (the original #179 review finding). [`any_live_worker`] distinguishes a
/// *busy* daemon (something is `running` on an unexpired lease → keep waiting)
/// from an *absent* one (nothing running → cancel + error).
///
/// ## Why the cancel is pending-only
///
/// In the no-live-worker branch the daemon could still claim the task in the
/// tiny window between the liveness probe and the cancel. [`cancel_if_pending_and_audit`]
/// only cancels a `pending` row, so if that happens the cancel no-ops, we
/// detect the claim, and we wait for the real result instead of orphaning a
/// live `--execute` we believed we had stopped.
async fn wait_until_claimed_or_no_daemon(
    pool: &sqlx::PgPool,
    listener: &mut sqlx::postgres::PgListener,
    task_id: i64,
    grace: std::time::Duration,
) -> Result<(), std::process::ExitCode> {
    use kastellan_db::tasks::{any_live_worker, get};

    // A NOTIFY may arrive during the grace window if the task completes very
    // fast; we don't rely on it — we authoritatively re-check state below.
    let _ = tokio::time::timeout(grace, async {
        loop {
            match listener.recv().await {
                Ok(n) if n.payload() == task_id.to_string() => return,
                _ => continue,
            }
        }
    })
    .await;

    // Authoritative state re-check.
    let state = match get(pool, task_id).await {
        Ok(Some(t)) => t.state,
        Ok(None) => {
            eprintln!("memory l3 run: task {task_id} disappeared");
            return Err(std::process::ExitCode::from(1));
        }
        Err(e) => {
            eprintln!("memory l3 run: get failed: {e}");
            return Err(std::process::ExitCode::from(1));
        }
    };
    if state != "pending" {
        return Ok(()); // claimed (running) or already terminal — proceed.
    }

    // Still pending after grace. Is a daemon alive but busy, or absent?
    match any_live_worker(pool).await {
        Ok(true) => {
            eprintln!(
                "memory l3 run: task {task_id} still queued (a daemon is busy on another \
                 task); waiting…"
            );
            Ok(())
        }
        Ok(false) => {
            cancel_pending_or_proceed(
                pool,
                task_id,
                &format!(
                    "the daemon does not appear to be running (task {task_id} still pending \
                     after {}s, and no worker is running)",
                    grace.as_secs()
                ),
            )
            .await
        }
        Err(e) => {
            // Liveness probe failed: fail safe — try to cancel (pending-only)
            // rather than wait blindly on a daemon we cannot confirm exists.
            cancel_pending_or_proceed(
                pool,
                task_id,
                &format!("could not verify a running daemon (liveness check failed: {e})"),
            )
            .await
        }
    }
}

/// Cancel the task **only if it is still `pending`**, or proceed if the daemon
/// claimed it in the race window. `reason` is the operator-facing explanation
/// printed when the cancel succeeds. Returns `Err(1)` when the task was
/// cancelled (or the cancel itself errored), `Ok(())` when the daemon had
/// already claimed it (so the caller waits for the real result).
async fn cancel_pending_or_proceed(
    pool: &sqlx::PgPool,
    task_id: i64,
    reason: &str,
) -> Result<(), std::process::ExitCode> {
    use kastellan_core::cli_audit::{cancel_if_pending_and_audit, CancelOutcome};

    match cancel_if_pending_and_audit(pool, task_id).await {
        Ok(CancelOutcome::Cancelled(_)) => {
            eprintln!("memory l3 run: {reason}. Cancelled task {task_id}.");
            Err(std::process::ExitCode::from(1))
        }
        Ok(CancelOutcome::NotCancellable) => {
            // The daemon claimed it between the probe and the cancel.
            eprintln!("memory l3 run: task {task_id} was claimed by the daemon; waiting…");
            Ok(())
        }
        Err(e) => {
            eprintln!("memory l3 run: cancel failed: {e}");
            Err(std::process::ExitCode::from(1))
        }
    }
}

#[cfg(test)]
mod tests;
