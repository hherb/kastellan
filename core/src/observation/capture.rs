//! Observation-phase capture types and helpers.
//!
//! ## Schema
//!
//! [`SCHEMA_VERSION`] is bumped only on a breaking change to
//! [`CaptureJson`]'s wire shape, and the value is written into every
//! capture file so a future reader can branch on it. Today the crate
//! only *writes* captures — there is no version-aware reader, so
//! backward compatibility with older versions is the responsibility of
//! whatever consumer reads them next (e.g. an observation-phase
//! analyser). We never auto-migrate on disk.
//!
//! ## Helper purity
//!
//! Every helper below `// ---- Pure helpers ----` performs no I/O and
//! has no global state. They are unit-tested under `mod tests` with
//! deterministic fixtures.
//!
//! [`write_capture_to_dir`] and [`fetch_audit_rows_for_task`] are the
//! two non-pure surfaces:
//!
//! - `write_capture_to_dir` touches the filesystem and refuses to
//!   overwrite existing baseline captures (operators must use a new
//!   `(date, model_slug)`).
//! - `fetch_audit_rows_for_task` issues one SQL SELECT and is pinned by
//!   an integration test under `core/tests/observation_fetch_audit_e2e.rs`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Bumped only on a breaking change to [`CaptureJson`]'s wire shape.
/// The value is written into every capture file so a future reader can
/// branch on it (no such reader exists in-tree today — see the
/// module-level docstring).
///
/// History:
/// * v1 — initial wire shape (PR #46).
/// * v2 — [`CapturedPlan::verdict_today`] changed from `String` to
///   `Option<String>` so a missing `cassandra:chain/verdict` row is
///   distinguishable from a real `Approve` verdict. Issue #47.
/// * v3 — [`CapturedPlan::source_truncated`] added so a plan distilled
///   from a [`kastellan_db::audit::truncate_payload`] envelope is
///   distinguishable from a pre-Slice-A capture or a genuine zero-step
///   plan (both of which also carry `plan_json: null`). Issue #62.
pub const SCHEMA_VERSION: u32 = 3;

/// Top-level on-disk envelope for one captured fixture run.
///
/// One file per `(date, model_slug)` baseline: see
/// [`capture_filename`]. Recapture writes a new file under the same
/// fixture directory; [`write_capture_to_dir`] refuses to overwrite an
/// existing one.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CaptureJson {
    pub schema_version: u32,
    pub fixture_id: String,
    pub fixture_summary: String,
    /// RFC 3339 string (UTC).
    pub captured_at: String,
    /// Matches `kastellan_llm_router::Backend::as_tag()` so consumers can
    /// fold producer-side audit rows in directly.
    pub llm_backend: String,
    /// Verbatim from `RouterConfig::local_model` at capture time.
    pub llm_model: String,
    pub llm_base_url: String,
    /// Prompt body (after the H1 summary line).
    pub prompt: String,
    pub task_id: i64,
    /// `tasks.state` at terminal.
    pub task_state: String,
    pub plan_iterations: u32,
    pub plans: Vec<CapturedPlan>,
    /// Every `audit_log` row whose payload references this `task_id`,
    /// sorted by id ascending. Includes the verdict rows the helpers
    /// also derive `CapturedPlan` entries from — it's both inputs to
    /// downstream analysis and an audit-record of the capture itself.
    pub audit_rows: Vec<CapturedAuditRow>,
}

/// One planner iteration distilled from the audit-row stream.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CapturedPlan {
    pub iter: u32,
    /// Full `Plan` JSON as the planner produced it (decoded from the
    /// `agent/plan.formulate` row's payload).
    pub plan_json: serde_json::Value,
    /// The `cassandra:chain/verdict` row's verdict string, paired with
    /// this plan iteration. `None` means *no* verdict row was found in
    /// the audit stream after this plan — wire-distinct from
    /// `Some("Approve")` (which is a real Approve verdict). Schema-v2
    /// bump (issue #47).
    pub verdict_today: Option<String>,
    pub step_count: u32,
    pub data_ceiling: String,
    /// `true` iff the source `agent/plan.formulate` row's payload was a
    /// [`kastellan_db::audit::truncate_payload`] envelope
    /// (`{_truncated: true, sha256, len}`) — meaning every payload key,
    /// including `plan`, was elided at write time. When set, the
    /// `plan_json: null` / `step_count: 0` fields on this struct are
    /// *artefacts of truncation*, not a real zero-step plan. This makes
    /// the row wire-distinct from a pre-Slice-A capture (also
    /// `plan_json: null`, but `source_truncated: false`). Schema-v3
    /// (issue #62).
    ///
    /// `#[serde(default)]` so a v2 capture that predates this field
    /// still deserialises (the field reads back as `false`).
    #[serde(default)]
    pub source_truncated: bool,
}

/// Trimmed projection of `db::audit::AuditRow` suitable for JSON
/// serialisation in capture files.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CapturedAuditRow {
    pub id: i64,
    /// RFC 3339 string.
    pub ts: String,
    pub actor: String,
    pub action: String,
    pub payload: serde_json::Value,
}

/// Errors from [`parse_fixture_prompt`].
#[derive(Debug, Error)]
pub enum ParseError {
    #[error("prompt.md is missing the first '# ...' H1 summary line")]
    MissingH1,
    #[error("prompt.md has H1 summary but empty body")]
    EmptyBody,
}

// ---- Pure helpers (unit-tested below) ----

/// Parse a fixture's `prompt.md` into `(summary, body)`.
///
/// - First H1 (`# ...`) line is the summary; trimmed.
/// - Subsequent blank lines after the H1 are stripped.
/// - Everything else is the body, trimmed.
/// - Missing H1 → [`ParseError::MissingH1`].
/// - Empty body after the H1 → [`ParseError::EmptyBody`].
pub fn parse_fixture_prompt(md: &str) -> Result<(String, String), ParseError> {
    // Find the first '# ' line; everything before it is discarded.
    let mut lines = md.lines();
    let summary_line = loop {
        match lines.next() {
            None => return Err(ParseError::MissingH1),
            Some(l) => {
                let trimmed = l.trim_start();
                if let Some(rest) = trimmed.strip_prefix("# ") {
                    break rest.trim().to_string();
                }
                if trimmed.starts_with('#') && !trimmed.starts_with("##") {
                    // "# alone" with no space → treat the rest of the line
                    // (after '#') as the summary; still satisfies the H1
                    // contract. This is an edge case for malformed inputs.
                    break trimmed[1..].trim().to_string();
                }
            }
        }
    };

    // Remainder is the body. Skip leading blank lines.
    let body: String = lines.collect::<Vec<_>>().join("\n");
    let body = body.trim_start_matches(['\n', ' ', '\t', '\r']);
    let body = body.trim();
    if body.is_empty() {
        return Err(ParseError::EmptyBody);
    }
    Ok((summary_line, body.to_string()))
}

/// Filesystem-safe lower-case slug for an LLM model id.
///
/// "Qwen3-7B-Instruct" → "qwen3-7b-instruct"
/// "gemma4:26b-a4b-it-q8_0" → "gemma4-26b-a4b-it-q8-0"
pub fn slug_model(model: &str) -> String {
    let mut out = String::with_capacity(model.len());
    let mut prev_was_hyphen = true; // drop leading hyphens by treating start as one
    for ch in model.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            out.push(lower);
            prev_was_hyphen = false;
        } else if !prev_was_hyphen {
            out.push('-');
            prev_was_hyphen = true;
        }
        // else: skip — collapses runs of punctuation
    }
    // Drop a trailing '-' from the punctuation-end case.
    if out.ends_with('-') {
        out.pop();
    }
    out
}

/// `format!("{date}_{slug}.json")` — pure, single-line.
pub fn capture_filename(date_yyyy_mm_dd: &str, model_slug: &str) -> String {
    format!("{date_yyyy_mm_dd}_{model_slug}.json")
}

/// Walk an audit-row stream for a single task and extract one
/// [`CapturedPlan`] per `agent/plan.formulate` row.
///
/// Pairing semantics: each plan is paired with the **first**
/// `cassandra:chain/verdict` row that follows it in the audit stream.
/// This is safe because the scheduler writes rows in strict
/// `[plan, verdict, plan, verdict, ...]` order (see
/// `core::scheduler::inner_loop`); the "first downstream verdict" and
/// "immediately-following verdict" always coincide for valid input.
///
/// Missing verdict row → `verdict_today: None`. Schema-v2 (issue #47)
/// makes this distinct from `Some("Approve")` so downstream analysis
/// can separate "agent ran, reviewer said Approve" from "agent ran,
/// reviewer never weighed in". The original `audit_rows` stream in
/// [`CaptureJson`] still preserves full truth either way.
///
/// Note on `plan_json: Value::Null`. Slice A (2026-05-15) added the
/// `plan` payload key, but a `null` extracted here is *ambiguous* and
/// can mean any of three things, in increasing severity:
///   1. Pre-Slice-A capture (operator must recapture).
///   2. The producer wrote a payload that exceeded
///      [`kastellan_db::audit::PAYLOAD_MAX_BYTES`] (4 KiB) and
///      [`kastellan_db::audit::truncate_payload`] replaced the entire
///      object with the `{_truncated, sha256, len}` envelope — `plan`
///      was nuked along with every other key.
///   3. A genuine writer regression dropped the key.
///
/// Case (2) is now surfaced explicitly: when the source row's payload
/// is a truncation envelope (`_truncated == true`), the resulting
/// [`CapturedPlan::source_truncated`] is set. A consumer can then
/// separate a truncated row (`source_truncated: true`) from a
/// pre-Slice-A / genuinely-empty plan (`source_truncated: false`)
/// rather than mis-classifying every `plan_json: null` as a zero-step
/// plan (issue #62).
pub fn extract_plans_from_audit_rows(rows: &[CapturedAuditRow]) -> Vec<CapturedPlan> {
    let mut out = Vec::new();
    let mut iter: u32 = 0;
    for (i, row) in rows.iter().enumerate() {
        if row.actor == "agent" && row.action == "plan.formulate" {
            iter = iter.saturating_add(1);
            // A truncated row's payload is the `{_truncated, sha256, len}`
            // envelope from `truncate_payload` — every real key, `plan`
            // included, was elided. Detect it so the null `plan_json`
            // below is attributable to truncation rather than a
            // pre-Slice-A capture or a real zero-step plan.
            let source_truncated = row
                .payload
                .get("_truncated")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let plan_json = row.payload.get("plan").cloned().unwrap_or(serde_json::Value::Null);
            let step_count = plan_json
                .get("steps")
                .and_then(|s| s.as_array())
                .map(|a| a.len() as u32)
                .unwrap_or(0);
            let data_ceiling = plan_json
                .get("data_ceiling")
                .and_then(|d| d.as_str())
                .unwrap_or("Public")
                .to_string();
            // Look ahead for the next cassandra:chain/verdict row.
            // `None` means no verdict row followed this plan; the
            // schema-v2 (issue #47) `Option<String>` shape makes that
            // distinguishable from a real `Some("Approve")` verdict.
            let verdict_today: Option<String> = rows[i + 1..]
                .iter()
                .find(|r| r.actor == "cassandra:chain" && r.action == "verdict")
                .and_then(|r| r.payload.get("verdict").and_then(|v| v.as_str()).map(String::from));
            out.push(CapturedPlan {
                iter,
                plan_json,
                verdict_today,
                step_count,
                data_ceiling,
                source_truncated,
            });
        }
    }
    out
}

// ---- IO + async helpers (integration-tested) ----

/// Write `capture` to `<out_dir>/<fixture_id>/<filename>` where
/// `<filename>` is [`capture_filename`] from the capture's
/// `captured_at` (date prefix) and a slug of its `llm_model`. Creates
/// parent dirs as needed. Errors with `io::ErrorKind::AlreadyExists` if
/// the destination file already exists — operators MUST recapture under
/// a different `(date, model_slug)` baseline.
///
/// `fixture_id` must be a single path segment: rejects empty, anything
/// containing `/` or `\`, leading `.`, or NUL. This prevents a
/// hand-edited or replayed `CaptureJson` from escaping `out_dir`.
///
/// Filesystem-collision is closed atomically via `create_new(true)`:
/// the kernel returns `AlreadyExists` directly when the destination
/// exists, eliminating the TOCTOU window a check-then-write would
/// leave open.
pub fn write_capture_to_dir(out_dir: &Path, capture: &CaptureJson)
    -> std::io::Result<PathBuf>
{
    // Reject fixture_ids that aren't single path segments. The on-disk
    // layout is `<out_dir>/<fixture_id>/<filename>`; a `..` or `/` in
    // the id would let a forged capture escape the captures root.
    let fid = capture.fixture_id.as_str();
    let bad_segment = fid.is_empty()
        || fid.starts_with('.')
        || fid.contains('/')
        || fid.contains('\\')
        || fid.contains('\0');
    if bad_segment {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "fixture_id must be a single path segment with no '/', '\\\\', \
                 leading '.', or NUL; got {fid:?}"
            ),
        ));
    }

    // Derive the destination filename. `captured_at` is RFC 3339;
    // take the first 10 chars (`YYYY-MM-DD`) as the date prefix.
    let date_prefix = capture.captured_at.get(..10).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "captured_at must start with YYYY-MM-DD (RFC 3339 calendar date prefix)",
        )
    })?;
    let slug = slug_model(&capture.llm_model);
    if slug.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "llm_model slugged to empty string",
        ));
    }
    let fname = capture_filename(date_prefix, &slug);

    let fixture_dir = out_dir.join(fid);
    std::fs::create_dir_all(&fixture_dir)?;
    let dest = fixture_dir.join(fname);

    let bytes = serde_json::to_vec_pretty(capture).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    })?;
    // Atomic check-and-create: the kernel returns AlreadyExists if the
    // destination exists, closing the TOCTOU window. Operators MUST
    // recapture under a new (date, model_slug) baseline.
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&dest)?;
    f.write_all(&bytes)?;
    f.sync_all()?;
    Ok(dest)
}

/// Fetch every `audit_log` row whose payload references this `task_id`,
/// sorted by id ascending. Used by the orchestrator integration test
/// after each fixture's CLI subprocess completes.
///
/// The SQL predicate: `payload @> jsonb_build_object('task_id', $1)`.
/// That catches every spec §7 lifecycle row (`task.running`,
/// `task.<state>`, `task.finalize`), every CLI producer row
/// (`task.submitted`, `task.cancelled`), every short-circuit row
/// (`step.unknown_tool`, `step.spawn_failed`), every per-tool dispatch
/// row that carries `task_id` in its `req`, and the per-plan rows.
pub async fn fetch_audit_rows_for_task(
    pool: &sqlx::PgPool,
    task_id: i64,
) -> Result<Vec<CapturedAuditRow>, sqlx::Error> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT id, ts, actor, action, payload \
         FROM audit_log \
         WHERE payload @> jsonb_build_object('task_id', $1::bigint) \
         ORDER BY id ASC",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let ts: time::OffsetDateTime = r.try_get("ts")?;
        // RFC 3339 formatting of a valid OffsetDateTime cannot fail;
        // `to_string()` would emit time's Debug-ish shape, NOT RFC 3339,
        // and silently violate the CapturedAuditRow.ts contract.
        let ts_rfc3339 = ts
            .format(&time::format_description::well_known::Rfc3339)
            .expect("RFC 3339 format cannot fail for a valid OffsetDateTime");
        out.push(CapturedAuditRow {
            id: r.try_get("id")?,
            ts: ts_rfc3339,
            actor: r.try_get("actor")?,
            action: r.try_get("action")?,
            payload: r.try_get("payload")?,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
