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
pub const SCHEMA_VERSION: u32 = 2;

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
    /// The `cassandra:chain/verdict` row's verdict string, paired with
    /// this plan iteration. `None` means *no* verdict row was found in
    /// the audit stream after this plan — wire-distinct from
    /// `Some("Approve")` (which is a real Approve verdict). Schema-v2
    /// bump (issue #47).
    pub verdict_today: Option<String>,
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
///      [`hhagent_db::audit::PAYLOAD_MAX_BYTES`] (4 KiB) and
///      [`hhagent_db::audit::truncate_payload`] replaced the entire
///      object with the `{_truncated, sha256, len}` envelope — `plan`
///      was nuked along with every other key.
///   3. A genuine writer regression dropped the key.
///
/// Slice B's harness should treat (2) specifically by checking the
/// raw row payload for `_truncated == true` before falling through.
pub fn extract_plans_from_audit_rows(rows: &[CapturedAuditRow]) -> Vec<CapturedPlan> {
    let mut out = Vec::new();
    let mut iter: u32 = 0;
    for (i, row) in rows.iter().enumerate() {
        if row.actor == "agent" && row.action == "plan.formulate" {
            iter = iter.saturating_add(1);
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
mod tests {
    use super::*;

    // ---- slug_model ----

    #[test]
    fn slug_model_lowercases_ascii_input() {
        assert_eq!(slug_model("Qwen3-7B-Instruct"), "qwen3-7b-instruct");
    }

    #[test]
    fn slug_model_normalises_colon_and_underscore_punctuation() {
        assert_eq!(slug_model("gemma4:26b-a4b-it-q8_0"), "gemma4-26b-a4b-it-q8-0");
    }

    #[test]
    fn slug_model_collapses_runs_of_punctuation() {
        // ".:_-" all map to a single '-' between alphanum runs.
        assert_eq!(slug_model("model.with::lots__of---punct"), "model-with-lots-of-punct");
    }

    #[test]
    fn slug_model_trims_leading_and_trailing_hyphens() {
        assert_eq!(slug_model(":foo:"), "foo");
        assert_eq!(slug_model("---foo---"), "foo");
    }

    #[test]
    fn slug_model_returns_empty_string_for_punctuation_only_input() {
        // Pure helper; caller is responsible for upstream validation
        // (e.g. asserting that `llm_model` is non-empty in CaptureJson).
        assert_eq!(slug_model(":::"), "");
    }

    #[test]
    fn slug_model_preserves_alphanumeric_unicode_as_lowercased_ascii_loss() {
        // Non-ASCII alphanumerics are treated as non-alphanumeric for
        // simplicity (filesystem-safe ASCII slug). Operators using
        // non-ASCII model ids will see them mapped to '-' runs.
        assert_eq!(slug_model("Mödel-é"), "m-del");
    }

    // ---- capture_filename ----

    #[test]
    fn capture_filename_shape_pin() {
        let fname = capture_filename("2026-05-13", "gemma4-26b-a4b-it-q8-0");
        assert_eq!(fname, "2026-05-13_gemma4-26b-a4b-it-q8-0.json");
        assert!(!fname.contains('/'));
        assert!(!fname.contains('\\'));
        assert!(fname.ends_with(".json"));
    }

    // ---- parse_fixture_prompt ----

    #[test]
    fn parse_fixture_prompt_happy_path() {
        let md = "# Plain echo control\n\nSay HELLO and nothing else.";
        let (summary, body) = parse_fixture_prompt(md).expect("parse");
        assert_eq!(summary, "Plain echo control");
        assert_eq!(body, "Say HELLO and nothing else.");
    }

    #[test]
    fn parse_fixture_prompt_strips_multiple_blank_lines_after_h1() {
        let md = "# Summary\n\n\n\nBody line.";
        let (summary, body) = parse_fixture_prompt(md).expect("parse");
        assert_eq!(summary, "Summary");
        assert_eq!(body, "Body line.");
    }

    #[test]
    fn parse_fixture_prompt_preserves_internal_blank_lines_in_body() {
        let md = "# Summary\n\nFirst para.\n\nSecond para.";
        let (_, body) = parse_fixture_prompt(md).expect("parse");
        assert_eq!(body, "First para.\n\nSecond para.");
    }

    #[test]
    fn parse_fixture_prompt_preserves_h2_in_body() {
        let md = "# Summary\n\n## Subheading\n\nDetail.";
        let (_, body) = parse_fixture_prompt(md).expect("parse");
        assert_eq!(body, "## Subheading\n\nDetail.");
    }

    #[test]
    fn parse_fixture_prompt_accepts_h1_with_no_space_after_hash() {
        // Edge case: `#FOO` (no space after the hash) is accepted as an
        // H1 with summary `FOO`. Pinned so the fallback branch in
        // parse_fixture_prompt does not silently rot.
        let md = "#FOO\n\nBody.";
        let (summary, body) = parse_fixture_prompt(md).expect("parse");
        assert_eq!(summary, "FOO");
        assert_eq!(body, "Body.");
    }

    #[test]
    fn parse_fixture_prompt_does_not_treat_h2_as_h1() {
        // `## Subheading` is H2, not H1. Without an H1 line, parsing
        // must fail with MissingH1 — the no-space-after-hash branch is
        // gated on `!starts_with("##")`.
        let md = "## Subheading\n\nBody.";
        match parse_fixture_prompt(md) {
            Err(ParseError::MissingH1) => {}
            other => panic!("expected MissingH1, got {other:?}"),
        }
    }

    #[test]
    fn parse_fixture_prompt_rejects_missing_h1() {
        let md = "No leading hash.";
        match parse_fixture_prompt(md) {
            Err(ParseError::MissingH1) => {}
            other => panic!("expected MissingH1, got {other:?}"),
        }
    }

    #[test]
    fn parse_fixture_prompt_rejects_empty_body() {
        let md = "# Just the summary\n\n   \n\n";
        match parse_fixture_prompt(md) {
            Err(ParseError::EmptyBody) => {}
            other => panic!("expected EmptyBody, got {other:?}"),
        }
    }

    // ---- extract_plans_from_audit_rows ----

    fn fake_audit_row(id: i64, actor: &str, action: &str, payload: serde_json::Value)
        -> CapturedAuditRow
    {
        CapturedAuditRow {
            id,
            ts: "2026-05-13T00:00:00Z".into(),
            actor: actor.into(),
            action: action.into(),
            payload,
        }
    }

    fn fake_plan_payload(decision: &str, steps_len: usize, data_ceiling: &str)
        -> serde_json::Value
    {
        let steps: Vec<serde_json::Value> = (0..steps_len)
            .map(|i| serde_json::json!({
                "tool": "shell-exec",
                "method": "shell.exec",
                "parameters": {"argv": ["/usr/bin/echo", format!("s{i}")]},
                "returns": "stdout",
                "done_when": "exit_code == 0",
                "classification": "Public",
            }))
            .collect();
        serde_json::json!({
            "plan": {
                "context": "ctx",
                "decision": decision,
                "rationale": "why",
                "steps": steps,
                "data_ceiling": data_ceiling,
            }
        })
    }

    #[test]
    fn extract_plans_empty_input_returns_empty_vec() {
        let rows: Vec<CapturedAuditRow> = vec![];
        assert!(extract_plans_from_audit_rows(&rows).is_empty());
    }

    #[test]
    fn extract_plans_one_plan_one_verdict() {
        let rows = vec![
            fake_audit_row(1, "agent", "plan.formulate", fake_plan_payload("act", 1, "Public")),
            fake_audit_row(2, "cassandra:chain", "verdict",
                serde_json::json!({"verdict": "Approve"})),
        ];
        let plans = extract_plans_from_audit_rows(&rows);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].iter, 1);
        assert_eq!(plans[0].verdict_today, Some("Approve".to_string()));
        assert_eq!(plans[0].step_count, 1);
        assert_eq!(plans[0].data_ceiling, "Public");
    }

    #[test]
    fn extract_plans_two_plans_two_verdicts_carry_iter_indices() {
        let rows = vec![
            fake_audit_row(1, "agent", "plan.formulate", fake_plan_payload("act", 2, "Personal")),
            fake_audit_row(2, "cassandra:chain", "verdict",
                serde_json::json!({"verdict": "Approve"})),
            fake_audit_row(3, "agent", "plan.formulate",
                fake_plan_payload("task_complete", 0, "Personal")),
            fake_audit_row(4, "cassandra:chain", "verdict",
                serde_json::json!({"verdict": "Approve"})),
        ];
        let plans = extract_plans_from_audit_rows(&rows);
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].iter, 1);
        assert_eq!(plans[0].step_count, 2);
        assert_eq!(plans[1].iter, 2);
        assert_eq!(plans[1].step_count, 0);
        assert_eq!(plans[1].data_ceiling, "Personal");
    }

    #[test]
    fn extract_plans_returns_none_when_verdict_row_missing() {
        // Schema-v2 (issue #47) bumps `verdict_today` from `String` to
        // `Option<String>`. Missing verdict → `None` (was silently
        // defaulted to `"Approve"` in v1, which lost the signal).
        let rows = vec![
            fake_audit_row(1, "agent", "plan.formulate", fake_plan_payload("act", 1, "Public")),
            // No following cassandra:chain/verdict row.
        ];
        let plans = extract_plans_from_audit_rows(&rows);
        assert_eq!(plans.len(), 1);
        assert!(
            plans[0].verdict_today.is_none(),
            "missing verdict row must yield None (schema-v2)"
        );
    }

    #[test]
    fn extract_plans_some_approve_is_distinct_from_none() {
        // The whole point of the schema-v2 bump: `Some("Approve")` and
        // `None` are now distinct values. Same fixture pair as above,
        // with vs without the verdict row.
        let with_verdict = vec![
            fake_audit_row(1, "agent", "plan.formulate", fake_plan_payload("act", 1, "Public")),
            fake_audit_row(2, "cassandra:chain", "verdict",
                serde_json::json!({"verdict": "Approve"})),
        ];
        let without_verdict = vec![
            fake_audit_row(1, "agent", "plan.formulate", fake_plan_payload("act", 1, "Public")),
        ];
        let p_with = extract_plans_from_audit_rows(&with_verdict);
        let p_without = extract_plans_from_audit_rows(&without_verdict);
        assert_eq!(p_with[0].verdict_today, Some("Approve".to_string()));
        assert_eq!(p_without[0].verdict_today, None);
        assert_ne!(p_with[0].verdict_today, p_without[0].verdict_today);
    }

    /// `SCHEMA_VERSION` pin. Bumping requires a deliberate edit here
    /// plus a migration note in the doc-comment.
    #[test]
    fn schema_version_is_two() {
        assert_eq!(SCHEMA_VERSION, 2);
    }

    // ---- write_capture_to_dir ----

    fn sample_capture(fixture_id: &str, model: &str) -> CaptureJson {
        CaptureJson {
            schema_version: SCHEMA_VERSION,
            fixture_id: fixture_id.into(),
            fixture_summary: "summary".into(),
            captured_at: "2026-05-13T10:30:00Z".into(),
            llm_backend: "local".into(),
            llm_model: model.into(),
            llm_base_url: "http://127.0.0.1:11434/v1".into(),
            prompt: "p".into(),
            task_id: 1,
            task_state: "completed".into(),
            plan_iterations: 1,
            plans: vec![],
            audit_rows: vec![],
        }
    }

    #[test]
    fn write_capture_to_dir_creates_parent_and_writes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cap = sample_capture("safe-001-echo-marker", "gemma4:26b-a4b-it-q8_0");
        let path = write_capture_to_dir(tmp.path(), &cap).expect("write");
        assert!(path.exists());
        // Expected filename: <date>_<model_slug>.json under
        // <out_dir>/<fixture_id>/.
        assert_eq!(
            path,
            tmp.path()
                .join("safe-001-echo-marker")
                .join("2026-05-13_gemma4-26b-a4b-it-q8-0.json")
        );
    }

    #[test]
    fn write_capture_to_dir_round_trips_through_json() {
        let tmp = tempfile::tempdir().unwrap();
        let cap = sample_capture("safe-001-echo-marker", "gemma4:26b-a4b-it-q8_0");
        let path = write_capture_to_dir(tmp.path(), &cap).expect("write");
        let bytes = std::fs::read(&path).expect("read back");
        let parsed: CaptureJson = serde_json::from_slice(&bytes).expect("decode");
        assert_eq!(parsed, cap);
    }

    #[test]
    fn write_capture_to_dir_refuses_to_overwrite_existing_baseline() {
        let tmp = tempfile::tempdir().unwrap();
        let cap = sample_capture("safe-001-echo-marker", "gemma4:26b-a4b-it-q8_0");
        let _first = write_capture_to_dir(tmp.path(), &cap).expect("first write");
        let err = write_capture_to_dir(tmp.path(), &cap)
            .expect_err("second write should refuse");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn write_capture_to_dir_rejects_short_captured_at() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cap = sample_capture("safe-001-echo-marker", "gemma4:26b-a4b-it-q8_0");
        cap.captured_at = "2026".into(); // shorter than 10 chars
        let err = write_capture_to_dir(tmp.path(), &cap).expect_err("must reject");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn write_capture_to_dir_rejects_punctuation_only_llm_model() {
        let tmp = tempfile::tempdir().unwrap();
        // Punctuation-only model id slugs to "" — write must refuse
        // rather than producing a filename that begins with "_" .
        let cap = sample_capture("safe-001-echo-marker", ":::");
        let err = write_capture_to_dir(tmp.path(), &cap).expect_err("must reject");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn write_capture_to_dir_rejects_path_traversal_in_fixture_id() {
        let tmp = tempfile::tempdir().unwrap();
        // `..` would escape the captures root; `/` would escape the
        // fixture directory; leading `.` would create a hidden dir;
        // NUL is rejected by the filesystem. All must be rejected up
        // front with InvalidInput.
        for bad in ["../escape", "a/b", "a\\b", ".hidden", "", "with\0nul"] {
            let cap = sample_capture(bad, "gemma4:26b-a4b-it-q8_0");
            let err = write_capture_to_dir(tmp.path(), &cap)
                .expect_err(&format!("must reject fixture_id={bad:?}"));
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::InvalidInput,
                "fixture_id={bad:?} must surface InvalidInput, got {err:?}"
            );
        }
    }
}
