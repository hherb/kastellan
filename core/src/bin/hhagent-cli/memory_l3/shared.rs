//! Shared helpers for the trust-changing `memory l3` handlers (`approve`,
//! `pin`) and the `run` refusal-hint path: the latest-registry-snapshot
//! reader, the prologue (id-parse → connect → fetch → layer-guard) that
//! `approve` and `pin` open with, and the registry-snapshot approval
//! decision they both make.

use std::collections::BTreeSet;
use std::process::ExitCode;

use sqlx::PgPool;

use crate::common::resolve_connect_spec;
use hhagent_core::cassandra::types::L3SkillCandidate;
use hhagent_core::memory::l3_approval::{evaluate_approval, ApprovalDecision, RejectReason};
use hhagent_db::memories::{fetch_by_ids, Memory, MemoryLayer};
use hhagent_db::pool::connect_runtime_pool;

/// Fetch the latest `registry.loaded` snapshot's tool-name set, or `None`
/// when the daemon has never recorded one.
pub(super) async fn latest_registry_tools(
    pool: &PgPool,
) -> Result<Option<BTreeSet<String>>, hhagent_db::DbError> {
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

/// Prologue shared by `approve` and `pin`: parse the positional `<id>`,
/// connect, fetch the row, and layer-guard it to a layer-3 skill.
///
/// On any failure prints the diagnostic and returns the [`ExitCode`] to
/// propagate (usage / bad-id → 2, everything else → 1); `cmd` names the
/// subcommand in those messages. On success returns the live pool + the
/// skill row.
pub(super) async fn load_skill_row(
    args: &[String],
    cmd: &str,
) -> Result<(PgPool, Memory), ExitCode> {
    let id_str = match args {
        [s] => s,
        _ => {
            eprintln!("usage: hhagent-cli memory l3 {cmd} <id>");
            return Err(ExitCode::from(2));
        }
    };
    let id: i64 = match id_str.parse() {
        Ok(n) => n,
        Err(e) => {
            eprintln!("memory l3 {cmd}: invalid id '{id_str}': {e}");
            return Err(ExitCode::from(2));
        }
    };

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return Err(ExitCode::from(1)); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return Err(ExitCode::from(1)); }
    };

    let row = match fetch_by_ids(&pool, &[id]).await {
        Ok(mut v) => v.pop(),
        Err(e) => { eprintln!("memory l3 {cmd}: {e}"); return Err(ExitCode::from(1)); }
    };
    let row = match row {
        Some(r) if r.layer == MemoryLayer::Skill => r,
        _ => {
            eprintln!("memory l3 {cmd}: no layer-3 skill with id={id}");
            return Err(ExitCode::from(1));
        }
    };
    Ok((pool, row))
}

/// Re-run the approval gate against the daemon's latest `registry.loaded`
/// snapshot — the defence-in-depth check `approve` and `pin` both perform.
///
/// `Ok(None)` snapshot ⇒ a `NoRegistrySnapshot` rejection (fail-closed). A
/// DB error prints `memory l3 {cmd}: …` and returns [`ExitCode`] 1.
pub(super) async fn decide_against_registry(
    pool: &PgPool,
    template: &L3SkillCandidate,
    cmd: &str,
) -> Result<ApprovalDecision, ExitCode> {
    match latest_registry_tools(pool).await {
        Ok(Some(known)) => Ok(evaluate_approval(template, &known)),
        Ok(None) => Ok(ApprovalDecision::Reject {
            reasons: vec![RejectReason::NoRegistrySnapshot],
        }),
        Err(e) => {
            eprintln!("memory l3 {cmd}: {e}");
            Err(ExitCode::from(1))
        }
    }
}
