//! Producer-side audit helpers for the operator-managed registry tables:
//! the tool allowlist, relation kinds, and entity kinds.
//!
//! All six helpers share one posture (mirror of each other): compose a
//! DB add/remove with a best-effort `actor='cli'` audit row, and emit
//! the row **only on a real state change** (`Ok(true)` — an actual
//! INSERT/DELETE). An idempotent no-op (`Ok(false)` — the entry already
//! existed / nothing matched) writes no audit row, so "what was true at
//! time T" reconstructions are not confused by intent that never
//! materialised. Audit-insert failures are logged at WARN and swallowed;
//! the underlying DB outcome propagates either way. See the
//! [`crate::cli_audit`] module doc for the shared best-effort rationale.

use sqlx::PgPool;

use crate::cli_audit::CLI_AUDIT_ACTOR;
use crate::scheduler::audit::{
    ACTION_ENTITY_KINDS_ADD, ACTION_ENTITY_KINDS_REMOVE, ACTION_RELATION_KINDS_ADD,
    ACTION_RELATION_KINDS_REMOVE, ACTION_TOOLS_ALLOWLIST_ADD, ACTION_TOOLS_ALLOWLIST_REMOVE,
};

/// Add one allowlist entry and emit one `actor='cli'
/// action='tools.allowlist.add'` audit row on success.
///
/// Returns the DB-layer bool: `Ok(true)` means a row was INSERTed (and
/// an audit row was emitted, best-effort); `Ok(false)` means the entry
/// already existed and **no audit row is written** (the operator's
/// state-change intent did not materialise; logging it would confuse
/// "what was true at time T" reconstructions).
///
/// Audit-insert posture: best-effort. A transient DB failure on the
/// audit row is logged via `tracing::warn!` and swallowed; the
/// underlying `db::tool_allowlists::add` outcome propagates either way.
pub async fn tools_allowlist_add_and_audit(
    pool: &PgPool,
    tool: &str,
    kind: kastellan_db::tool_allowlists::EntryKind,
    argv0: &str,
) -> Result<bool, kastellan_db::tool_allowlists::ToolAllowlistError> {
    let inserted =
        kastellan_db::tool_allowlists::add(pool, tool, kind, argv0, CLI_AUDIT_ACTOR).await?;
    if inserted {
        // `kind` is part of the row's shape (migration 0021), so the audit
        // trail records it too — an `argv0`-only payload can no longer
        // reconstruct "what was true at time T" for this table.
        let payload =
            serde_json::json!({ "tool": tool, "kind": kind.as_str(), "argv0": argv0 });
        if let Err(e) = kastellan_db::audit::insert(
            pool,
            CLI_AUDIT_ACTOR,
            ACTION_TOOLS_ALLOWLIST_ADD,
            payload,
        )
        .await
        {
            tracing::warn!(
                error = %e,
                tool = tool,
                argv0 = argv0,
                "tools_allowlist_add_and_audit: audit insert failed"
            );
        }
    }
    Ok(inserted)
}

/// Remove one allowlist entry and emit one `actor='cli'
/// action='tools.allowlist.remove'` audit row on success.
///
/// Returns `Ok(true)` if a row was deleted (and audit row emitted
/// best-effort); `Ok(false)` if nothing matched (no audit row).
pub async fn tools_allowlist_remove_and_audit(
    pool: &PgPool,
    tool: &str,
    argv0: &str,
) -> Result<bool, kastellan_db::tool_allowlists::ToolAllowlistError> {
    let removed = kastellan_db::tool_allowlists::remove(pool, tool, argv0).await?;
    if removed {
        let payload = serde_json::json!({ "tool": tool, "argv0": argv0 });
        if let Err(e) = kastellan_db::audit::insert(
            pool,
            CLI_AUDIT_ACTOR,
            ACTION_TOOLS_ALLOWLIST_REMOVE,
            payload,
        )
        .await
        {
            tracing::warn!(
                error = %e,
                tool = tool,
                argv0 = argv0,
                "tools_allowlist_remove_and_audit: audit insert failed"
            );
        }
    }
    Ok(removed)
}

/// Add one relation-kind row and emit one `actor='cli'
/// action='relation_kinds.add'` audit row on success.
///
/// Returns the DB-layer bool: `Ok(true)` means a row was INSERTed (and
/// an audit row was emitted, best-effort); `Ok(false)` means the kind
/// was already present and **no audit row is written** (the operator's
/// state-change intent did not materialise; logging it would confuse
/// "what was true at time T" reconstructions). Mirror of the posture
/// in [`tools_allowlist_add_and_audit`].
///
/// Audit-insert posture: best-effort. A transient DB failure on the
/// audit row is logged via `tracing::warn!` and swallowed; the
/// underlying `db::relation_kinds::add` outcome propagates either way.
///
/// **Requires an admin-pool connection** ([`kastellan_db::pool::connect_admin_pool`])
/// — the runtime role does not have INSERT on `relation_kinds`
/// (migration 0017 REVOKE). Passing a runtime-role pool yields
/// `Err(RelationKindError::Db(...))` carrying a Postgres `permission
/// denied` error.
pub async fn relation_kinds_add_and_audit(
    pool: &PgPool,
    kind: &str,
    description: Option<&str>,
) -> Result<bool, kastellan_db::relation_kinds::RelationKindError> {
    let inserted = kastellan_db::relation_kinds::add(pool, kind, description).await?;
    if inserted {
        // `description: null` is the explicit "unset" wire value so a
        // downstream payload reader can distinguish "operator did not
        // pass --description" from "field absent due to schema drift".
        let payload = serde_json::json!({
            "kind": kind,
            "description": description,
        });
        if let Err(e) = kastellan_db::audit::insert(
            pool,
            CLI_AUDIT_ACTOR,
            ACTION_RELATION_KINDS_ADD,
            payload,
        )
        .await
        {
            tracing::warn!(
                error = %e,
                kind = kind,
                "relation_kinds_add_and_audit: audit insert failed"
            );
        }
    }
    Ok(inserted)
}

/// Remove one relation-kind row and emit one `actor='cli'
/// action='relation_kinds.remove'` audit row on success.
///
/// Returns `Ok(true)` if a row was deleted (and audit row emitted
/// best-effort); `Ok(false)` if nothing matched (no audit row). The
/// `'undefined'` sentinel is rejected up front by `db::relation_kinds::remove`
/// with `RelationKindError::RemovalOfUndefinedRejected`; on that path
/// no row is deleted and no audit row is written.
///
/// **Requires an admin-pool connection** — see [`relation_kinds_add_and_audit`].
pub async fn relation_kinds_remove_and_audit(
    pool: &PgPool,
    kind: &str,
) -> Result<bool, kastellan_db::relation_kinds::RelationKindError> {
    let removed = kastellan_db::relation_kinds::remove(pool, kind).await?;
    if removed {
        let payload = serde_json::json!({ "kind": kind });
        if let Err(e) = kastellan_db::audit::insert(
            pool,
            CLI_AUDIT_ACTOR,
            ACTION_RELATION_KINDS_REMOVE,
            payload,
        )
        .await
        {
            tracing::warn!(
                error = %e,
                kind = kind,
                "relation_kinds_remove_and_audit: audit insert failed"
            );
        }
    }
    Ok(removed)
}

/// Add one entity-kind row and emit one `actor='cli'
/// action='entity_kinds.add'` audit row on success.
///
/// Mirror of [`relation_kinds_add_and_audit`]: same idempotency
/// semantics (`Ok(true)` on real INSERT triggers audit; `Ok(false)`
/// writes no row); same payload shape (`{kind, description}` with
/// `description: null` when omitted); same admin-pool requirement
/// (migration 0016 REVOKEs writes from the runtime role).
pub async fn entity_kinds_add_and_audit(
    pool: &PgPool,
    kind: &str,
    description: Option<&str>,
) -> Result<bool, kastellan_db::entity_kinds::EntityKindError> {
    let inserted = kastellan_db::entity_kinds::add(pool, kind, description).await?;
    if inserted {
        let payload = serde_json::json!({
            "kind": kind,
            "description": description,
        });
        if let Err(e) = kastellan_db::audit::insert(
            pool,
            CLI_AUDIT_ACTOR,
            ACTION_ENTITY_KINDS_ADD,
            payload,
        )
        .await
        {
            tracing::warn!(
                error = %e,
                kind = kind,
                "entity_kinds_add_and_audit: audit insert failed"
            );
        }
    }
    Ok(inserted)
}

/// Remove one entity-kind row and emit one `actor='cli'
/// action='entity_kinds.remove'` audit row on success.
///
/// Mirror of [`relation_kinds_remove_and_audit`]. The `'undefined'`
/// sentinel is rejected up front by `db::entity_kinds::remove` with
/// `EntityKindError::RemovalOfUndefinedRejected`; on that path no
/// row is deleted and no audit row is written.
pub async fn entity_kinds_remove_and_audit(
    pool: &PgPool,
    kind: &str,
) -> Result<bool, kastellan_db::entity_kinds::EntityKindError> {
    let removed = kastellan_db::entity_kinds::remove(pool, kind).await?;
    if removed {
        let payload = serde_json::json!({ "kind": kind });
        if let Err(e) = kastellan_db::audit::insert(
            pool,
            CLI_AUDIT_ACTOR,
            ACTION_ENTITY_KINDS_REMOVE,
            payload,
        )
        .await
        {
            tracing::warn!(
                error = %e,
                kind = kind,
                "entity_kinds_remove_and_audit: audit insert failed"
            );
        }
    }
    Ok(removed)
}
