//! Writer for `MemoryLayer::Skill` (L3) rows. One caller:
//!
//! **Agent-raised** — via `Plan.l3_skill` consumed by
//! `crate::scheduler::runner::drain_lane` on `Outcome::Completed`
//! when the task executed >= 1 tool step (the grounding gate).
//!
//! The agent emits a parameterised tool-call template on a terminal
//! plan; this module validates it (structural + `{{placeholder}}`
//! closed-world integrity + reserved-tag + caps), dedups on a
//! canonical SHA-256 over the template, and inserts at `layer = 3`
//! marked `trust: "untrusted"` via
//! [`hhagent_db::memories::insert_memory_at_layer`].
//!
//! **Crystallised skills are non-executable in this slice.** There is
//! no invocation path; `trust: "untrusted"` is a forward-compatible
//! placeholder for the future Skill trust enum. Unlike `l1_promote`,
//! there is NO entity auto-link and NO operator write path.
//!
//! See `docs/superpowers/specs/2026-05-31-l3-skill-crystallisation-design.md`.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::cassandra::types::L3SkillCandidate;
use hhagent_db::memories::{insert_memory_at_layer, load_layer, Memory, MemoryLayer};
use hhagent_db::DbError;

/// Max bytes for the skill `name` (a stable identifier).
pub const L3_MAX_NAME_BYTES: usize = 64;
/// Max bytes for the skill `description` (becomes the memory `body`).
pub const L3_MAX_DESC_BYTES: usize = 512;
/// Max bytes for a single parameter's `description`.
pub const L3_MAX_PARAM_DESC_BYTES: usize = 256;
/// Max declared parameters.
pub const L3_MAX_PARAMS: usize = 16;
/// Max template steps (lower bound is 1 — the grounding floor).
pub const L3_MAX_STEPS: usize = 32;
/// Max bytes for a `tool` or `method` identifier.
pub const L3_MAX_IDENT_BYTES: usize = 64;
/// Max bytes for the canonical-serialised template (under the audit 4 KiB cap).
pub const L3_MAX_TEMPLATE_BYTES: usize = 4096;

/// Reserved substrings that would close/open the future `<skills>`
/// render block. Defensive against prompt-injection (threat-model §6),
/// symmetric with `l1_promote`'s `<l1_insights>` defence.
const RESERVED_TAG_OPEN: &str = "<skills>";
const RESERVED_TAG_CLOSE: &str = "</skills>";

/// Provenance for an L3 row write. The audit-row `source` field is
/// never producer-supplied; only `runner::drain_lane` constructs this.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum L3Source {
    /// Agent-raised write from `runner::drain_lane` after
    /// `Outcome::Completed`. The originating `task_id` rides in the
    /// audit-row payload for cross-restart trace stitching.
    AgentRaised { task_id: i64 },
}

/// Error kinds the L3 writer can produce.
#[derive(Debug, thiserror::Error)]
pub enum L3Error {
    #[error("L3 skill validation failed: {0}")]
    Validation(String),
    #[error("L3 db error: {0}")]
    Db(#[from] hhagent_db::DbError),
}

/// Outcome of a single `crystallise_l3` call.
#[derive(Clone, Debug)]
pub enum L3WriteOutcome {
    /// New L3 row inserted at the carried `memory_id`.
    Inserted { memory_id: i64 },
    /// A row with the same `body_sha256` already exists at `layer = 3`.
    SkippedDuplicate { memory_id: i64 },
}

impl L3WriteOutcome {
    pub fn memory_id(&self) -> i64 {
        match self {
            L3WriteOutcome::Inserted { memory_id }
            | L3WriteOutcome::SkippedDuplicate { memory_id } => *memory_id,
        }
    }
}

/// `true` iff `s` is a strict snake_case identifier: starts with
/// `[a-z]`, then `[a-z0-9_]*`. Used for skill name + parameter names.
fn is_snake_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// `true` iff `s` is a tool/method identifier: starts with `[a-z0-9]`,
/// then `[a-z0-9_.-]*`. Looser than snake_case because tool names carry
/// hyphens (`shell-exec`) and methods carry dots (`shell.exec`).
fn is_tool_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.' || c == '-')
}

/// Scan a single string for `{{name}}` placeholders, inserting each
/// referenced name into `out`. Rejects an unterminated `{{` and a
/// placeholder whose body is not a snake_case identifier.
fn scan_placeholders(s: &str, out: &mut BTreeSet<String>) -> Result<(), L3Error> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let start = i + 2;
            let mut j = start;
            while j + 1 < bytes.len() && !(bytes[j] == b'}' && bytes[j + 1] == b'}') {
                j += 1;
            }
            if !(j + 1 < bytes.len() && bytes[j] == b'}' && bytes[j + 1] == b'}') {
                return Err(L3Error::Validation("unterminated placeholder '{{'".into()));
            }
            let ident = &s[start..j];
            if !is_snake_ident(ident) {
                return Err(L3Error::Validation(format!(
                    "malformed placeholder name '{ident}'"
                )));
            }
            out.insert(ident.to_string());
            i = j + 2;
        } else {
            i += 1;
        }
    }
    Ok(())
}

/// Recursively collect every `{{name}}` placeholder from a step's
/// `parameters` JSON value (placeholders only live in string leaves).
fn collect_placeholders(v: &serde_json::Value, out: &mut BTreeSet<String>) -> Result<(), L3Error> {
    match v {
        serde_json::Value::String(s) => scan_placeholders(s, out),
        serde_json::Value::Array(a) => {
            for e in a {
                collect_placeholders(e, out)?;
            }
            Ok(())
        }
        serde_json::Value::Object(m) => {
            for e in m.values() {
                collect_placeholders(e, out)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Validate an L3 skill candidate. On success returns a normalised
/// candidate (trimmed string fields) so the writer never stores
/// leading/trailing whitespace. On failure returns
/// [`L3Error::Validation`] with a human-readable reason.
pub fn validate_l3_skill(c: &L3SkillCandidate) -> Result<L3SkillCandidate, L3Error> {
    // --- name ---
    let name = c.name.trim();
    if name.is_empty() {
        return Err(L3Error::Validation("name is empty after trim".into()));
    }
    if name.len() > L3_MAX_NAME_BYTES {
        return Err(L3Error::Validation(format!(
            "name exceeds {L3_MAX_NAME_BYTES} bytes ({})",
            name.len()
        )));
    }
    if !is_snake_ident(name) {
        return Err(L3Error::Validation(format!(
            "name '{name}' is not snake_case ([a-z][a-z0-9_]*)"
        )));
    }

    // --- description ---
    if c.description.contains('\n') || c.description.contains('\r') {
        return Err(L3Error::Validation("description contains newline".into()));
    }
    let description = c.description.trim();
    if description.is_empty() {
        return Err(L3Error::Validation("description is empty after trim".into()));
    }
    if description.bytes().any(|b| b < 0x20) {
        return Err(L3Error::Validation("description contains control character".into()));
    }
    if description.contains(RESERVED_TAG_OPEN) || description.contains(RESERVED_TAG_CLOSE) {
        return Err(L3Error::Validation("description contains reserved tag substring".into()));
    }
    if description.len() > L3_MAX_DESC_BYTES {
        return Err(L3Error::Validation(format!(
            "description exceeds {L3_MAX_DESC_BYTES} bytes ({})",
            description.len()
        )));
    }

    // --- parameters ---
    if c.parameters.len() > L3_MAX_PARAMS {
        return Err(L3Error::Validation(format!(
            "too many parameters ({} > {L3_MAX_PARAMS})",
            c.parameters.len()
        )));
    }
    let mut declared: BTreeSet<String> = BTreeSet::new();
    let mut norm_params: Vec<crate::cassandra::types::L3Param> =
        Vec::with_capacity(c.parameters.len());
    for p in &c.parameters {
        let pn = p.name.trim();
        if !is_snake_ident(pn) {
            return Err(L3Error::Validation(format!(
                "parameter name '{pn}' is not snake_case"
            )));
        }
        if !declared.insert(pn.to_string()) {
            return Err(L3Error::Validation(format!("duplicate parameter '{pn}'")));
        }
        let pd = p.description.trim();
        if pd.is_empty() {
            return Err(L3Error::Validation(format!(
                "parameter '{pn}' has empty description"
            )));
        }
        // Param descriptions are surfaced verbatim into the `<skills>`
        // prompt block (`l3_surface::render_skill_entry`), exactly like the
        // skill description. They must therefore carry the same anti-breakout
        // guards: no newline/control chars (would corrupt the bullet layout)
        // and no reserved `<skills>`/`</skills>` tag substring (would let an
        // agent-authored param description escape the block into model-trusted
        // framing). Mirrors the `description` guards above and the L1 `body`
        // guard in `l1_promote`.
        if pd.contains('\n') || pd.contains('\r') {
            return Err(L3Error::Validation(format!(
                "parameter '{pn}' description contains newline"
            )));
        }
        if pd.bytes().any(|b| b < 0x20) {
            return Err(L3Error::Validation(format!(
                "parameter '{pn}' description contains control character"
            )));
        }
        if pd.contains(RESERVED_TAG_OPEN) || pd.contains(RESERVED_TAG_CLOSE) {
            return Err(L3Error::Validation(format!(
                "parameter '{pn}' description contains reserved tag substring"
            )));
        }
        if pd.len() > L3_MAX_PARAM_DESC_BYTES {
            return Err(L3Error::Validation(format!(
                "parameter '{pn}' description exceeds {L3_MAX_PARAM_DESC_BYTES} bytes"
            )));
        }
        norm_params.push(crate::cassandra::types::L3Param {
            name: pn.to_string(),
            description: pd.to_string(),
        });
    }

    // --- steps ---
    if c.steps.is_empty() {
        return Err(L3Error::Validation("skill must have at least one step".into()));
    }
    if c.steps.len() > L3_MAX_STEPS {
        return Err(L3Error::Validation(format!(
            "too many steps ({} > {L3_MAX_STEPS})",
            c.steps.len()
        )));
    }
    let mut referenced: BTreeSet<String> = BTreeSet::new();
    let mut norm_steps: Vec<crate::cassandra::types::L3TemplateStep> =
        Vec::with_capacity(c.steps.len());
    for s in &c.steps {
        let tool = s.tool.trim();
        let method = s.method.trim();
        if tool.len() > L3_MAX_IDENT_BYTES || !is_tool_ident(tool) {
            return Err(L3Error::Validation(format!("step tool '{tool}' is invalid")));
        }
        if method.len() > L3_MAX_IDENT_BYTES || !is_tool_ident(method) {
            return Err(L3Error::Validation(format!("step method '{method}' is invalid")));
        }
        if !s.parameters.is_object() {
            return Err(L3Error::Validation(
                "step parameters must be a JSON object".into(),
            ));
        }
        collect_placeholders(&s.parameters, &mut referenced)?;
        norm_steps.push(crate::cassandra::types::L3TemplateStep {
            tool: tool.to_string(),
            method: method.to_string(),
            parameters: s.parameters.clone(),
        });
    }

    // --- closed-world placeholder invariant ---
    for r in &referenced {
        if !declared.contains(r) {
            return Err(L3Error::Validation(format!("undeclared placeholder '{r}'")));
        }
    }
    for d in &declared {
        if !referenced.contains(d) {
            return Err(L3Error::Validation(format!("unused parameter '{d}'")));
        }
    }

    let normalised = L3SkillCandidate {
        name: name.to_string(),
        description: description.to_string(),
        parameters: norm_params,
        steps: norm_steps,
    };

    // --- total size cap (canonical form) ---
    let canonical = canonical_json(&normalised);
    if canonical.len() > L3_MAX_TEMPLATE_BYTES {
        return Err(L3Error::Validation(format!(
            "template exceeds {L3_MAX_TEMPLATE_BYTES} bytes ({})",
            canonical.len()
        )));
    }

    Ok(normalised)
}

/// Recursively sort all object keys so two candidates that differ only
/// in JSON key order serialise identically. Load-bearing for dedup:
/// a non-canonical serialiser would under-dedup.
fn sort_value_keys(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(m) => {
            let mut entries: Vec<(String, serde_json::Value)> = m.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut sorted = serde_json::Map::new();
            for (k, val) in entries {
                sorted.insert(k, sort_value_keys(val));
            }
            serde_json::Value::Object(sorted)
        }
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.into_iter().map(sort_value_keys).collect())
        }
        other => other,
    }
}

/// Deterministic JSON string for a candidate (object keys sorted at
/// every depth; array order — i.e. step order — preserved).
pub fn canonical_json(c: &L3SkillCandidate) -> String {
    let v = serde_json::to_value(c).expect("L3SkillCandidate serialises");
    serde_json::to_string(&sort_value_keys(v)).expect("canonical serialise")
}

/// SHA-256 over the canonical template, lowercase 64-char hex.
pub fn compute_template_sha256(c: &L3SkillCandidate) -> String {
    let mut h = Sha256::new();
    h.update(canonical_json(c).as_bytes());
    format!("{:x}", h.finalize())
}

/// Build the `metadata` JSONB for a new L3 row. Schema:
/// `{source, task_id, trust, body_sha256, created_at, template}`.
/// `template` is the full normalised candidate (name/parameters/steps;
/// the `description` is duplicated there + as the memory `body`).
///
/// **`body_sha256` naming note:** unlike L1 (where it hashes the body),
/// this is the *canonical-template* digest from
/// [`compute_template_sha256`] — `sha256(canonical_json(candidate))`,
/// NOT `sha256(body)`. The L1 key name is reused so the dedup
/// EXISTS-check (`metadata->>'body_sha256'`) is layer-agnostic.
///
/// **Coupling note:** the literal `"agent_raised"` MUST match
/// `L3Source`'s serde `rename_all = "snake_case"` output. Cross-pinned
/// by `build_l3_metadata_serde_agrees_with_l3_source`.
///
/// Called by [`crystallise_l3`].
pub(crate) fn build_l3_metadata(
    source: &L3Source,
    candidate: &L3SkillCandidate,
    body_sha256: &str,
    created_at_rfc3339: &str,
) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    match source {
        L3Source::AgentRaised { task_id } => {
            obj.insert(
                "source".into(),
                serde_json::Value::String("agent_raised".into()),
            );
            obj.insert(
                "task_id".into(),
                serde_json::Value::Number(serde_json::Number::from(*task_id)),
            );
        }
    }
    obj.insert(
        "trust".into(),
        serde_json::Value::String("untrusted".into()),
    );
    obj.insert(
        "body_sha256".into(),
        serde_json::Value::String(body_sha256.into()),
    );
    obj.insert(
        "created_at".into(),
        serde_json::Value::String(created_at_rfc3339.into()),
    );
    obj.insert(
        "template".into(),
        serde_json::to_value(candidate).expect("candidate serialises"),
    );
    serde_json::Value::Object(obj)
}

/// Crystallise a single L3 skill. Validates, computes the canonical
/// SHA-256, EXISTS-checks against `layer = 3` rows by
/// `metadata->>'body_sha256'`, inserts on miss with `body = description`
/// and `trust: "untrusted"`. Idempotent on the template SHA.
///
/// **No entity auto-link** (unlike `promote_l1`): a skill's description
/// is not an entity-bearing insight, and recall surfacing is out of
/// scope this slice.
pub async fn crystallise_l3(
    pool: &PgPool,
    candidate: &L3SkillCandidate,
    source: L3Source,
) -> Result<L3WriteOutcome, L3Error> {
    let normalised = validate_l3_skill(candidate)?;
    let body_sha256 = compute_template_sha256(&normalised);

    let existing: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM memories \
         WHERE layer = $1 AND metadata->>'body_sha256' = $2 \
         LIMIT 1",
    )
    .bind(MemoryLayer::Skill.as_db())
    .bind(&body_sha256)
    .fetch_optional(pool)
    .await
    .map_err(|e| {
        L3Error::Db(hhagent_db::DbError::Query(format!(
            "crystallise_l3 EXISTS-check body_sha256={body_sha256}: {e}"
        )))
    })?;

    if let Some(existing_id) = existing {
        return Ok(L3WriteOutcome::SkippedDuplicate { memory_id: existing_id });
    }

    let created_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("rfc3339 format");
    let metadata = build_l3_metadata(&source, &normalised, &body_sha256, &created_at);

    let new_id = insert_memory_at_layer(
        pool,
        &normalised.description, // the body is the human description
        &metadata,
        None, // no embedding for L3 v1
        MemoryLayer::Skill,
    )
    .await?;

    Ok(L3WriteOutcome::Inserted { memory_id: new_id })
}

/// Operator-facing list view: every row at `layer = 3`, newest-first.
pub async fn list_l3(pool: &PgPool) -> Result<Vec<Memory>, DbError> {
    load_layer(pool, MemoryLayer::Skill, usize::MAX).await
}

/// Operator-facing remove, layer-guarded via
/// `hhagent_db::memories::delete_memory_at_layer` (cannot delete an
/// L0/L1/L2 row even on a typoed id). Returns `true` iff a row was deleted.
pub async fn remove_l3(pool: &PgPool, id: i64) -> Result<bool, DbError> {
    hhagent_db::memories::delete_memory_at_layer(pool, id, MemoryLayer::Skill).await
}

#[cfg(test)]
mod tests;
