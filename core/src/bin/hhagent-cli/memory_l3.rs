//! `memory l3 {list,remove}` — operator-facing inspection + pruning of
//! layer-3 (crystallised skill) memories. Skills are agent-crystallised,
//! never operator-authored, so there is no `add`. `remove` emits one
//! `actor='cli' action='l3.removed'` audit row.

use std::process::ExitCode;

use crate::common::{resolve_connect_spec, with_runtime};

pub(crate) fn run_memory_l3(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: hhagent-cli memory l3 <list|approve|revoke|remove> ...");
        return ExitCode::from(2);
    }
    match args[0].as_str() {
        "list"    => with_runtime("memory l3", memory_l3_list(&args[1..])),
        "approve" => with_runtime("memory l3", memory_l3_approve(&args[1..])),
        "revoke"  => with_runtime("memory l3", memory_l3_revoke(&args[1..])),
        "remove"  => with_runtime("memory l3", memory_l3_remove(&args[1..])),
        other     => {
            eprintln!("memory l3: unknown action '{other}'; expected: list | approve | revoke | remove");
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
