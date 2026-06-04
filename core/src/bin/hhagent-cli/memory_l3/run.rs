//! `memory l3 run <id> [--arg name=value]… [--execute|--yes]` —
//! operator-triggered invocation of an approved crystallised skill. Dry-run
//! by default (substitute + live-registry re-validate + print the steps that
//! WOULD dispatch); `--execute` runs them through the sandbox, stopping at
//! the first error. See [`memory_l3_run`] for the operator-environment
//! prerequisite (issue #179).

use std::process::ExitCode;

use crate::common::resolve_connect_spec;

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
/// When this bites, the refusal now prints a `hint:` line (via
/// `diagnose_registry_divergence`) distinguishing an unset-env cliff from a
/// genuinely unknown tool. The structural fix — moving execution into the
/// daemon so there is a single registry (issue #179, Opt 3) — is folded into
/// the autonomous-invocation slice (ROADMAP line 165), which builds that path.
pub(super) async fn memory_l3_run(args: &[String]) -> ExitCode {
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

            // Issue #179: when a refusal is (partly) about a tool missing from
            // this CLI's in-process registry rebuild, explain *why* the local
            // view differs from the daemon's. The snapshot read is best-effort
            // — a diagnostic-only DB error must never change the exit path.
            let needed: BTreeSet<String> =
                template.steps.iter().map(|s| s.tool.clone()).collect();
            let snapshot = super::shared::latest_registry_tools(&pool).await.ok().flatten();
            let hints = hhagent_core::memory::l3_invoke::diagnose_registry_divergence(
                &needed, &live_tools, snapshot.as_ref(),
            );
            for h in &hints {
                eprintln!("  hint: {h}");
            }

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
