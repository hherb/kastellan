//! `memory l1 {add,list,remove}` — operator-facing management of
//! layer-1 (in-prompt insight) memories. Add/remove emit one
//! `actor='cli' action='l1.{added,removed}'` audit row per operation.
//! `add` is idempotent (duplicate body_sha256 returns
//! `skipped_duplicate`); `list` prints the in-prompt slice by
//! default, or every L1 row with `--all`.

use std::process::ExitCode;

use crate::common::{resolve_connect_spec, with_runtime};

pub(crate) fn run_memory(args: &[String]) -> ExitCode {
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
    // Per-action dispatch. `with_runtime` is called only from the known
    // arms — an invalid action exits 2 without spawning tokio worker
    // threads (Issue #97).
    match args[0].as_str() {
        "add"    => with_runtime("memory l1", memory_l1_add(&args[1..])),
        "list"   => with_runtime("memory l1", memory_l1_list(&args[1..])),
        "remove" => with_runtime("memory l1", memory_l1_remove(&args[1..])),
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

    // Operator-explicit additions are intentionally NOT auto-linked:
    // the spec routes auto-linking through write paths the agent
    // controls, while operator-added L1 rows go through this CLI.
    // A future `hhagent-cli memory relink` subcommand will fill the
    // gap in batch. Emit a one-line stderr hint on success so the
    // operator isn't surprised by an empty graph lane for these rows.
    let extractor = NoOpEntityExtractor::new();
    match l1_add_and_audit(&pool, &extractor, body).await {
        Ok((L1WriteOutcome::Inserted { memory_id, .. }, _)) => {
            println!("inserted id={memory_id}");
            eprintln!(
                "note: operator-added L1 rows are not auto-linked to entities; \
                 batch backfill via the future 'memory relink' subcommand"
            );
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
