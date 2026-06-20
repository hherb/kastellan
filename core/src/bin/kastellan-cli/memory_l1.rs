//! `memory l1 {add,list,remove,reembed}` — operator-facing management of
//! layer-1 (in-prompt insight) memories. Add/remove emit one
//! `actor='cli' action='l1.{added,removed}'` audit row per operation.
//! `add` is idempotent (duplicate body_sha256 returns
//! `skipped_duplicate`); `list` prints the in-prompt slice by
//! default, or every L1 row with `--all`; `reembed` is the embedding
//! **backfill** — it (re)embeds every `layer = 1` row whose `embedding
//! IS NULL` (pre-#324 rows + operator-added rows) through the real
//! `RouterEmbedder`, so they re-enter the semantic recall lane. Each
//! embed is already audited (`action='embed'`) by the router; `reembed`
//! changes no rows' existence, so it emits no separate `l1.*` audit row.

use std::process::ExitCode;

use crate::common::{resolve_connect_spec, with_runtime};

pub(crate) fn run_memory(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: kastellan-cli memory <l1|l3> ...");
        return ExitCode::from(2);
    }
    match args[0].as_str() {
        "l1"  => run_memory_l1(&args[1..]),
        "l3"  => crate::memory_l3::run_memory_l3(&args[1..]),
        other => {
            eprintln!("memory: unknown subgroup '{other}'; expected: l1 | l3");
            ExitCode::from(2)
        }
    }
}

fn run_memory_l1(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: kastellan-cli memory l1 <add|list|remove|reembed> ...");
        return ExitCode::from(2);
    }
    // Per-action dispatch. `with_runtime` is called only from the known
    // arms — an invalid action exits 2 without spawning tokio worker
    // threads (Issue #97).
    match args[0].as_str() {
        "add"     => with_runtime("memory l1", memory_l1_add(&args[1..])),
        "list"    => with_runtime("memory l1", memory_l1_list(&args[1..])),
        "remove"  => with_runtime("memory l1", memory_l1_remove(&args[1..])),
        "reembed" => with_runtime("memory l1", memory_l1_reembed(&args[1..])),
        other     => {
            eprintln!("memory l1: unknown action '{other}'; expected: add | list | remove | reembed");
            ExitCode::from(2)
        }
    }
}

async fn memory_l1_add(args: &[String]) -> ExitCode {
    use kastellan_core::cli_audit::l1_add_and_audit;
    use kastellan_core::entity_extraction::NoOpEntityExtractor;
    use kastellan_core::memory::l1_promote::L1WriteOutcome;
    use kastellan_db::pool::connect_runtime_pool;

    let body = match args {
        [b] => b,
        _ => {
            eprintln!("usage: kastellan-cli memory l1 add <body>");
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
    // A future `kastellan-cli memory relink` subcommand will fill the
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
    use kastellan_core::memory::l1_promote::list_l1;
    use kastellan_db::pool::connect_runtime_pool;

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
    use kastellan_core::cli_audit::l1_remove_and_audit;
    use kastellan_db::pool::connect_runtime_pool;

    let id_str = match args {
        [s] => s,
        _ => {
            eprintln!("usage: kastellan-cli memory l1 remove <id>");
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

/// `memory l1 reembed` — backfill embeddings for NULL-embedding L1 rows.
///
/// Unlike `add` (which injects a `NoOpEmbedder`), this builds the **real**
/// `RouterEmbedder` from the host's `KASTELLAN_LLM_*` env — the same config
/// the daemon's forward write path uses — so a backfilled vector is identical
/// to what an on-insert embed would have produced. Idempotent and safe to
/// re-run (see [`reembed_l1_null`]); prints a one-line `scanned=/embedded=/
/// skipped=` summary. Takes no arguments.
async fn memory_l1_reembed(args: &[String]) -> ExitCode {
    use std::sync::Arc;
    use kastellan_core::memory::{format_reembed_report, reembed_l1_null, RouterEmbedder};
    use kastellan_db::pool::connect_runtime_pool;

    if !args.is_empty() {
        eprintln!("usage: kastellan-cli memory l1 reembed");
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

    // Build the Router-backed embedder. `from_env` reads the host's
    // KASTELLAN_LLM_* config (chat/embed endpoints, models); on a daemon host
    // the operator runs this with the same env the daemon uses.
    let router_cfg = match kastellan_llm_router::RouterConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("memory l1 reembed: RouterConfig::from_env: {e}");
            return ExitCode::from(1);
        }
    };
    let router = match kastellan_llm_router::Router::new(router_cfg) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!("memory l1 reembed: Router::new: {e}");
            return ExitCode::from(1);
        }
    };
    let embedder = RouterEmbedder::new(pool.clone(), router);

    match reembed_l1_null(&pool, &embedder).await {
        Ok(report) => {
            println!("{}", format_reembed_report(&report));
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("memory l1 reembed: {e}");
            ExitCode::from(1)
        }
    }
}
