//! `memory l3 {list,remove}` — operator-facing inspection + pruning of
//! layer-3 (crystallised skill) memories. Skills are agent-crystallised,
//! never operator-authored, so there is no `add`. `remove` emits one
//! `actor='cli' action='l3.removed'` audit row.

use std::process::ExitCode;

use crate::common::{resolve_connect_spec, with_runtime};

pub(crate) fn run_memory_l3(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: hhagent-cli memory l3 <list|remove> ...");
        return ExitCode::from(2);
    }
    match args[0].as_str() {
        "list"   => with_runtime("memory l3", memory_l3_list(&args[1..])),
        "remove" => with_runtime("memory l3", memory_l3_remove(&args[1..])),
        other    => {
            eprintln!("memory l3: unknown action '{other}'; expected: list | remove");
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
        let trust = r.metadata.get("trust").and_then(|v| v.as_str()).unwrap_or("?");
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
