//! Observation-phase capture types and helpers.
//!
//! ## Schema
//!
//! [`SCHEMA_VERSION`] is bumped only on a breaking change to
//! [`CaptureJson`]'s wire shape. Old captures stay readable through
//! their original schema version; we never auto-migrate on disk.
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
/// Old captures stay readable through their original schema version.
pub const SCHEMA_VERSION: u32 = 1;

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
    /// Matches `hhagent_llm_router::Backend::as_tag()` so consumers can
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
    /// Today: always "Approve" (CASSANDRA stub stages). When real rules
    /// land this carries the rule's verdict.
    pub verdict_today: String,
    pub step_count: u32,
    pub data_ceiling: String,
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
pub fn parse_fixture_prompt(_md: &str) -> Result<(String, String), ParseError> {
    unimplemented!()
}

/// Filesystem-safe lower-case slug for an LLM model id.
///
/// "Qwen3-7B-Instruct" → "qwen3-7b-instruct"
/// "gemma4:26b-a4b-it-q8_0" → "gemma4-26b-a4b-it-q8-0"
pub fn slug_model(_model: &str) -> String {
    unimplemented!()
}

/// `format!("{date}_{slug}.json")` — pure, single-line.
pub fn capture_filename(_date_yyyy_mm_dd: &str, _model_slug: &str) -> String {
    unimplemented!()
}

/// Walk an audit-row stream for a single task and extract one
/// [`CapturedPlan`] per `agent/plan.formulate` row. Pairs each plan
/// with the immediately-following `cassandra:chain/verdict` row (if
/// any) to populate `verdict_today`. Missing verdict row defaults to
/// `"Approve"` silently — the original `audit_rows` stream in
/// [`CaptureJson`] still preserves full truth.
pub fn extract_plans_from_audit_rows(_rows: &[CapturedAuditRow]) -> Vec<CapturedPlan> {
    unimplemented!()
}

// ---- IO + async helpers (integration-tested) ----

/// Write `capture` to `<out_dir>/<fixture_id>/<filename>` where
/// `<filename>` is [`capture_filename`] from the capture's
/// `captured_at` (date prefix) and a slug of its `llm_model`. Creates
/// parent dirs as needed. Errors with `io::ErrorKind::AlreadyExists` if
/// the destination file already exists — operators MUST recapture under
/// a different `(date, model_slug)` baseline.
pub fn write_capture_to_dir(_out_dir: &Path, _capture: &CaptureJson)
    -> std::io::Result<PathBuf>
{
    unimplemented!()
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
    _pool: &sqlx::PgPool,
    _task_id: i64,
) -> Result<Vec<CapturedAuditRow>, sqlx::Error> {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- slug_model ----

    #[test]
    fn slug_model_lowercases_ascii_input() {
        assert_eq!(slug_model("Qwen3-7B-Instruct"), "qwen3-7b-instruct");
    }
}
