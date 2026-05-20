//! `tools allowlist {add,remove,list}` — manage the per-tool argv0
//! allowlist stored in `tool_allowlists`. Add/remove emit one
//! `actor='cli' action='tools.allowlist.{add,remove}'` audit row
//! on a real state change; idempotent no-ops and validation errors
//! write no audit row.

use std::process::ExitCode;

use crate::common::{multi_thread_runtime, resolve_connect_spec};

pub(crate) fn run_tools(args: &[String]) -> ExitCode {
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
    let rt = match multi_thread_runtime("tools allowlist") {
        Ok(rt) => rt,
        Err(code) => return code,
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
