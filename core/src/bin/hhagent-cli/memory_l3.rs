//! `memory l3 {list,approve,revoke,remove,run}` — operator-facing
//! inspection, trust management, pruning, and execution of layer-3
//! (crystallised skill) memories. Skills are agent-crystallised, never
//! operator-authored, so there is no `add`. `approve`/`revoke`/`remove`/`run`
//! each emit their own `actor='cli'` audit row(s); `run` (operator-triggered
//! invocation) is dry-run by default and only dispatches under `--execute`.

use std::process::ExitCode;

use crate::common::{resolve_connect_spec, with_runtime};

pub(crate) fn run_memory_l3(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: hhagent-cli memory l3 <list|approve|revoke|remove|run> ...");
        return ExitCode::from(2);
    }
    match args[0].as_str() {
        "list"    => with_runtime("memory l3", memory_l3_list(&args[1..])),
        "approve" => with_runtime("memory l3", memory_l3_approve(&args[1..])),
        "revoke"  => with_runtime("memory l3", memory_l3_revoke(&args[1..])),
        "remove"  => with_runtime("memory l3", memory_l3_remove(&args[1..])),
        "run"     => with_runtime("memory l3", memory_l3_run(&args[1..])),
        other     => {
            eprintln!("memory l3: unknown action '{other}'; expected: list | approve | revoke | remove | run");
            ExitCode::from(2)
        }
    }
}

async fn memory_l3_list(args: &[String]) -> ExitCode {
    use hhagent_core::memory::l3_crystallise::list_l3;
    use hhagent_db::pool::connect_runtime_pool;

    if !args.is_empty() {
        eprintln!("memory l3 list: takes no arguments");
        return ExitCode::from(2);
    }

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    let rows = match list_l3(&pool).await {
        Ok(r) => r,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    println!("{:<8}  {:<24}  {:<10}  NAME / DESCRIPTION", "ID", "CREATED_AT", "TRUST");
    for r in rows {
        let trust = hhagent_core::memory::l3_approval::SkillTrust::from_metadata_str(
            r.metadata.get("trust").and_then(|v| v.as_str()).unwrap_or(""),
        )
        .as_str();
        let name = r.metadata
            .get("template").and_then(|t| t.get("name")).and_then(|v| v.as_str())
            .unwrap_or("?");
        println!("{:<8}  {:<24}  {:<10}  {} — {}", r.id, r.created_at, trust, name, r.body);
    }
    ExitCode::from(0)
}

async fn memory_l3_remove(args: &[String]) -> ExitCode {
    use hhagent_core::cli_audit::l3_remove_and_audit;
    use hhagent_db::pool::connect_runtime_pool;

    let id_str = match args {
        [s] => s,
        _ => {
            eprintln!("usage: hhagent-cli memory l3 remove <id>");
            return ExitCode::from(2);
        }
    };
    let id: i64 = match id_str.parse() {
        Ok(n) => n,
        Err(e) => {
            eprintln!("memory l3 remove: invalid id '{id_str}': {e}");
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

    match l3_remove_and_audit(&pool, id).await {
        Ok((true, _))  => { println!("removed id={id}"); ExitCode::from(0) }
        Ok((false, _)) => {
            println!("no row at layer 3 with id={id} (already gone or wrong layer)");
            ExitCode::from(0)
        }
        Err(e) => { eprintln!("memory l3 remove: {e}"); ExitCode::from(1) }
    }
}

/// Fetch the latest `registry.loaded` snapshot's tool-name set, or `None`
/// when the daemon has never recorded one.
async fn latest_registry_tools(
    pool: &sqlx::PgPool,
) -> Result<Option<std::collections::BTreeSet<String>>, hhagent_db::DbError> {
    use hhagent_core::memory::l3_approval::extract_tool_names;
    use hhagent_core::scheduler::audit::ACTION_REGISTRY_LOADED;

    let payload: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT payload FROM audit_log \
         WHERE actor = 'core' AND action = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(ACTION_REGISTRY_LOADED)
    .fetch_optional(pool)
    .await
    .map_err(|e| hhagent_db::DbError::Query(format!("latest_registry_tools: {e}")))?;

    Ok(payload.map(|p| extract_tool_names(&p)))
}

async fn memory_l3_approve(args: &[String]) -> ExitCode {
    use std::collections::BTreeSet;

    use hhagent_core::cassandra::types::L3SkillCandidate;
    use hhagent_core::cli_audit::{l3_approve_and_audit, l3_approve_rejected_audit};
    use hhagent_core::memory::l3_approval::{evaluate_approval, ApprovalDecision, RejectReason};
    use hhagent_db::memories::{fetch_by_ids, MemoryLayer};
    use hhagent_db::pool::connect_runtime_pool;

    let id_str = match args {
        [s] => s,
        _ => {
            eprintln!("usage: hhagent-cli memory l3 approve <id>");
            return ExitCode::from(2);
        }
    };
    let id: i64 = match id_str.parse() {
        Ok(n) => n,
        Err(e) => {
            eprintln!("memory l3 approve: invalid id '{id_str}': {e}");
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

    // --- fetch + layer-guard the row -------------------------------------
    let row = match fetch_by_ids(&pool, &[id]).await {
        Ok(mut v) => v.pop(),
        Err(e) => { eprintln!("memory l3 approve: {e}"); return ExitCode::from(1); }
    };
    let row = match row {
        Some(r) if r.layer == MemoryLayer::Skill => r,
        _ => {
            eprintln!("memory l3 approve: no layer-3 skill with id={id}");
            return ExitCode::from(1);
        }
    };
    let body_sha256 = row.metadata.get("body_sha256").and_then(|v| v.as_str());

    // --- parse the stored template ---------------------------------------
    let template: L3SkillCandidate = match row
        .metadata
        .get("template")
        .cloned()
        .and_then(|t| serde_json::from_value(t).ok())
    {
        Some(t) => t,
        None => {
            let reasons = vec!["stored L3 row has no parseable 'template'".to_string()];
            let _ = l3_approve_rejected_audit(&pool, id, None, body_sha256, &reasons).await;
            eprintln!("memory l3 approve: id={id} has no parseable template; not approved");
            return ExitCode::from(1);
        }
    };
    let skill_name = template.name.clone();

    // --- registry snapshot → decision ------------------------------------
    let decision = match latest_registry_tools(&pool).await {
        Ok(Some(known)) => evaluate_approval(&template, &known),
        Ok(None) => ApprovalDecision::Reject { reasons: vec![RejectReason::NoRegistrySnapshot] },
        Err(e) => { eprintln!("memory l3 approve: {e}"); return ExitCode::from(1); }
    };

    match decision {
        ApprovalDecision::Approve => {
            let tools: Vec<String> = {
                let mut s = BTreeSet::new();
                for st in &template.steps { s.insert(st.tool.clone()); }
                s.into_iter().collect()
            };
            let sha = body_sha256.unwrap_or("");
            if let Err(e) = l3_approve_and_audit(&pool, id, &skill_name, sha, &tools).await {
                eprintln!("memory l3 approve: {e}");
                return ExitCode::from(1);
            }
            println!("approved skill '{skill_name}' (#{id}) → trust=user_approved");
            ExitCode::from(0)
        }
        ApprovalDecision::Reject { reasons } => {
            let rendered: Vec<String> = reasons.iter().map(|r| r.to_string()).collect();
            let _ = l3_approve_rejected_audit(&pool, id, Some(&skill_name), body_sha256, &rendered).await;
            eprintln!("approval REJECTED for skill '{skill_name}' (#{id}):");
            for r in &rendered { eprintln!("  - {r}"); }
            ExitCode::from(1)
        }
    }
}

async fn memory_l3_revoke(args: &[String]) -> ExitCode {
    use hhagent_core::cli_audit::l3_revoke_and_audit;
    use hhagent_db::pool::connect_runtime_pool;

    let id_str = match args {
        [s] => s,
        _ => {
            eprintln!("usage: hhagent-cli memory l3 revoke <id>");
            return ExitCode::from(2);
        }
    };
    let id: i64 = match id_str.parse() {
        Ok(n) => n,
        Err(e) => {
            eprintln!("memory l3 revoke: invalid id '{id_str}': {e}");
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

    match l3_revoke_and_audit(&pool, id).await {
        Ok((true, _))  => { println!("revoked id={id} → trust=untrusted"); ExitCode::from(0) }
        Ok((false, _)) => {
            println!("no row at layer 3 with id={id} (already gone or wrong layer)");
            ExitCode::from(0)
        }
        Err(e) => { eprintln!("memory l3 revoke: {e}"); ExitCode::from(1) }
    }
}

/// Parsed argv for `memory l3 run` (after the `run` subcommand token is
/// stripped). `arg_tokens` are the raw `name=value` strings, validated later
/// by [`hhagent_core::memory::l3_invoke::parse_args`].
#[derive(Debug, PartialEq, Eq)]
struct RunArgv {
    id: i64,
    arg_tokens: Vec<String>,
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
            s if id_str.is_none() && !s.starts_with("--") => id_str = Some(&args[i]),
            other => return Err(format!("memory l3 run: unexpected argument '{other}'")),
        }
        i += 1;
    }
    let id_str = id_str.ok_or_else(|| {
        "usage: hhagent-cli memory l3 run <id> [--arg name=value]… [--execute | --yes]".to_string()
    })?;
    let id: i64 = id_str
        .parse()
        .map_err(|e| format!("memory l3 run: invalid id '{id_str}': {e}"))?;
    Ok(RunArgv { id, arg_tokens, execute })
}

/// `memory l3 run <id> [--arg name=value]… [--execute]`
///
/// Default (no `--execute`): DRY-RUN — substitute + live-registry
/// re-validate, then print the concrete steps that WOULD dispatch. Spawns
/// nothing; a successful dry-run writes no audit row, but a *refused* run
/// is always audited (`l3.invoke_rejected`) even in dry-run mode — a
/// refused run attempt is a security event worth a trail (see `invoke_l3`).
/// `--execute` runs the steps through the sandbox, stopping at the first error.
///
/// ## Operator-environment prerequisite (fail-safe)
///
/// The live re-validation rebuilds the tool registry **in-process from this
/// CLI's environment** (`HHAGENT_SHELL_EXEC_BIN`, the gliner-relex env, …) —
/// not from the daemon's recorded `registry.loaded` snapshot. So an operator
/// running `run` in a shell that lacks the daemon's env vars sees an empty /
/// reduced registry, and an otherwise-validly-approved skill is **refused**
/// ("tool … not in registry"). This is fail-safe (refuse, and `--execute`'s
/// own sandbox still contains anything that does run), but it means the
/// operator must invoke `run` with the same tool-registry env the daemon uses.
/// Parity with the snapshot used by `approve` is tracked in issue #179 (the
/// daemon-snapshot-vs-live tradeoff is a deliberate design question).
async fn memory_l3_run(args: &[String]) -> ExitCode {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use hhagent_core::cassandra::types::L3SkillCandidate;
    use hhagent_core::memory::l3_approval::SkillTrust;
    use hhagent_core::memory::l3_invoke::{invoke_l3, parse_args, InvokeReport};
    use hhagent_core::scheduler::inner_loop::{StepDispatcher, StepOutcome};
    use hhagent_core::scheduler::tool_dispatch::ToolHostStepDispatcher;
    use hhagent_db::memories::{fetch_by_ids, MemoryLayer};
    use hhagent_db::pool::connect_runtime_pool;

    // --- parse argv: <id> then --arg k=v … and --execute ---------------
    let RunArgv { id, arg_tokens, execute } = match parse_run_argv(args) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };
    let args_map = match parse_args(&arg_tokens) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("memory l3 run: {e}");
            return ExitCode::from(2);
        }
    };

    // --- connect ------------------------------------------------------
    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    // --- load + layer-guard the row ----------------------------------
    let row = match fetch_by_ids(&pool, &[id]).await {
        Ok(mut v) => v.pop(),
        Err(e) => { eprintln!("memory l3 run: {e}"); return ExitCode::from(1); }
    };
    let row = match row {
        Some(r) if r.layer == MemoryLayer::Skill => r,
        _ => {
            eprintln!("memory l3 run: no layer-3 skill with id={id}");
            return ExitCode::from(1);
        }
    };
    let template: L3SkillCandidate = match row
        .metadata.get("template").cloned().and_then(|t| serde_json::from_value(t).ok())
    {
        Some(t) => t,
        None => {
            eprintln!("memory l3 run: id={id} has no parseable template");
            return ExitCode::from(1);
        }
    };
    let trust = SkillTrust::from_metadata_str(
        row.metadata.get("trust").and_then(|v| v.as_str()).unwrap_or(""),
    );
    let body_sha256 = row.metadata.get("body_sha256").and_then(|v| v.as_str()).unwrap_or("");

    // --- rebuild the live registry in-process (no registry.loaded write) ---
    let gliner = hhagent_core::registry_build::build_gliner_relex_entry();
    let (registry, _records) =
        match hhagent_core::registry_build::build_tool_registry(&pool, gliner).await {
            Ok(x) => x,
            Err(e) => { eprintln!("memory l3 run: building registry: {e}"); return ExitCode::from(1); }
        };
    let live_tools: BTreeSet<String> =
        registry.entries().map(|(name, _)| name.to_string()).collect();

    // --- build the dispatcher (same machinery as the daemon) — only under
    // --execute. A dry-run returns before any dispatch, so the heavyweight
    // sandbox / lifecycle / vault / tool-host construction stays off the
    // dry-run path entirely: a dry-run provably cannot spawn a worker. The
    // no-op stand-in keeps invoke_l3's single signature (it still audits a
    // dry-run *refusal*).
    let dispatcher: Arc<dyn StepDispatcher> = if execute {
        let sandboxes = Arc::new(hhagent_sandbox::SandboxBackends::default_for_current_os());
        let lifecycle: Arc<dyn hhagent_core::worker_lifecycle::WorkerLifecycleManager> =
            Arc::new(hhagent_core::worker_lifecycle::CompositeLifecycle::new(Arc::clone(&sandboxes)));
        let vault = Arc::new(hhagent_core::secrets::Vault::new());
        Arc::new(ToolHostStepDispatcher::new(
            pool.clone(),
            vault,
            lifecycle,
            Arc::new(registry),
        ))
    } else {
        Arc::new(DryRunNeverDispatches)
    };

    // --- invoke -------------------------------------------------------
    let report = invoke_l3(
        &pool, id, dispatcher.as_ref(), &template, trust, body_sha256, &args_map, &live_tools, execute,
    )
    .await;

    match report {
        InvokeReport::Refused { reasons } => {
            eprintln!("REFUSED to run skill '{}' (#{id}):", template.name);
            for r in &reasons { eprintln!("  - {r}"); }
            ExitCode::from(1)
        }
        InvokeReport::DryRun { steps } => {
            println!("dry-run: skill '{}' (#{id}) would dispatch {} step(s):", template.name, steps.len());
            for (n, s) in steps.iter().enumerate() {
                println!("  [{n}] {}/{} {}", s.tool, s.method, s.parameters);
            }
            println!("(re-run with --execute to dispatch)");
            ExitCode::from(0)
        }
        InvokeReport::Executed { outcomes, steps_total } => {
            let any_err = outcomes.iter().any(|o| o.is_err());
            println!("executed skill '{}' (#{id}): {}/{} step(s)", template.name, outcomes.len(), steps_total);
            for (n, o) in outcomes.iter().enumerate() {
                match o {
                    StepOutcome::Ok(v) =>
                        println!("  [{n}] ok: {v}"),
                    // Step errors are diagnostics → stderr (consistent with how
                    // Refused reasons are printed).
                    StepOutcome::Err { code, detail } =>
                        eprintln!("  [{n}] ERR {code}: {detail}"),
                }
            }
            if any_err { ExitCode::from(1) } else { ExitCode::from(0) }
        }
    }
}

/// No-op [`StepDispatcher`] used only on the dry-run path of `memory l3 run`.
///
/// `invoke_l3` returns `DryRun` before reaching `run_steps`, so this is never
/// actually invoked; it exists only so the heavyweight sandbox / lifecycle /
/// vault / tool-host construction can stay behind `--execute`. `dispatch_step`
/// returns an error (rather than panicking) as a defensive backstop should a
/// future refactor ever route a dry-run through it.
struct DryRunNeverDispatches;

#[async_trait::async_trait]
impl hhagent_core::scheduler::inner_loop::StepDispatcher for DryRunNeverDispatches {
    async fn dispatch_step(
        &self,
        _step: &hhagent_core::cassandra::types::PlannedStep,
    ) -> hhagent_core::scheduler::inner_loop::StepOutcome {
        hhagent_core::scheduler::inner_loop::StepOutcome::Err {
            code: "DRY_RUN_NO_DISPATCH".to_string(),
            detail: "internal: dry-run dispatcher must never dispatch".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_run_argv, RunArgv};

    fn v(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_id_args_and_execute() {
        let got = parse_run_argv(&v(&["5", "--arg", "a=b", "--execute"])).unwrap();
        assert_eq!(got, RunArgv { id: 5, arg_tokens: v(&["a=b"]), execute: true });
    }

    #[test]
    fn accepts_gnu_equals_arg_form_and_repeats() {
        let got = parse_run_argv(&v(&["7", "--arg=k=v", "--arg", "x=y"])).unwrap();
        assert_eq!(got.id, 7);
        assert_eq!(got.arg_tokens, v(&["k=v", "x=y"]));
        assert!(!got.execute, "no --execute/--yes ⇒ dry-run");
    }

    #[test]
    fn yes_is_an_alias_for_execute() {
        let got = parse_run_argv(&v(&["3", "--yes"])).unwrap();
        assert!(got.execute);
    }

    #[test]
    fn id_may_follow_flags() {
        let got = parse_run_argv(&v(&["--execute", "9"])).unwrap();
        assert_eq!(got, RunArgv { id: 9, arg_tokens: vec![], execute: true });
    }

    #[test]
    fn missing_id_is_a_usage_error() {
        let err = parse_run_argv(&v(&["--execute"])).unwrap_err();
        assert!(err.contains("usage"), "got: {err}");
    }

    #[test]
    fn empty_argv_is_a_usage_error() {
        let err = parse_run_argv(&[]).unwrap_err();
        assert!(err.contains("usage"), "got: {err}");
    }

    #[test]
    fn dangling_arg_flag_is_rejected() {
        let err = parse_run_argv(&v(&["1", "--arg"])).unwrap_err();
        assert!(err.contains("--arg requires"), "got: {err}");
    }

    #[test]
    fn non_numeric_id_is_rejected() {
        let err = parse_run_argv(&v(&["abc"])).unwrap_err();
        assert!(err.contains("invalid id"), "got: {err}");
    }

    #[test]
    fn second_positional_is_rejected() {
        // A stray second bare token (e.g. a typo'd second id) must not be
        // silently swallowed.
        let err = parse_run_argv(&v(&["1", "2"])).unwrap_err();
        assert!(err.contains("unexpected argument '2'"), "got: {err}");
    }

    #[test]
    fn unknown_flag_is_rejected() {
        let err = parse_run_argv(&v(&["1", "--bogus"])).unwrap_err();
        assert!(err.contains("unexpected argument '--bogus'"), "got: {err}");
    }
}
