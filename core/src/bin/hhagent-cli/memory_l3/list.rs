//! `memory l3 list` — operator inspection of crystallised (layer-3) skills,
//! one row per skill with its id, creation time, trust tier, name, and body.

use std::process::ExitCode;

use crate::common::resolve_connect_spec;

pub(super) async fn memory_l3_list(args: &[String]) -> ExitCode {
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
