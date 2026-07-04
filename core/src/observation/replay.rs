//! Offline replay of captured plans through a candidate
//! `ChainReviewStage`. Pure-functional; no DB, no LLM, no daemon —
//! the harness reads `CaptureJson` files from disk, replays each
//! captured plan through the provided chain, and reports per-fixture
//! verdict deltas against the recorded baseline.
//!
//! Slice B of the rule-iteration harness spec
//! (`docs/superpowers/specs/2026-05-15-rule-iteration-harness-design.md`).
//!
//! ## Public surface
//!
//! - [`VerdictSnapshot`] — JSON-serialisable projection of a `Verdict`.
//! - [`ReplayedPlan`] / [`ReplayResult`] — per-plan / per-capture row.
//! - [`replay_capture`] — async; runs one capture through a chain.
//! - [`load_captures_from_dir`] — I/O; deserialises a captures tree.
//! - [`format_report_table`] — pure; ASCII table for stdout.
//!
//! ## Missing plan body
//!
//! Captures produced before Slice A's audit-payload bump
//! (2026-05-15) carry `plan_json: null`. `replay_capture` emits a
//! [`ReplayedPlan`] with `skipped_reason: Some(...)` and
//! `new_verdict: None` for each such plan; it never silently
//! fabricates a synthetic `Plan` from derived fields, because that
//! would let the operator design rules against fake inputs.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cassandra::review::{ChainReviewStage, ReviewStage, ReviewStageContext};
use crate::cassandra::types::{DataClass, Plan, Verdict};
use crate::observation::capture::{CaptureJson, CapturedAuditRow};

/// JSON-serialisable projection of a [`Verdict`]. Keeps the
/// discriminator kind separate from the detail so the harness can
/// compare verdicts ignoring detail-string churn ("physical harm" vs
/// "weapons" both project to the same `kind = "constitutional_block"`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VerdictSnapshot {
    /// One of `"approve" | "advisory" | "escalate" | "block" |
    /// "constitutional_block"`. Lowercase + underscore matches the
    /// existing `cassandra:chain/verdict` audit-row `verdict_kind`
    /// strings (see `core/src/scheduler/inner_loop.rs`).
    pub kind: String,
    pub detail: Option<serde_json::Value>,
}

impl VerdictSnapshot {
    /// Pure projection of a [`Verdict`] into the wire shape.
    pub fn from_verdict(v: &Verdict) -> Self {
        match v {
            Verdict::Approve => Self {
                kind: "approve".into(),
                detail: None,
            },
            Verdict::Advisory(msg) => Self {
                kind: "advisory".into(),
                detail: Some(serde_json::json!(msg)),
            },
            Verdict::Escalate(concern, severity) => Self {
                kind: "escalate".into(),
                detail: Some(serde_json::json!({
                    "concern": concern,
                    "severity": severity,
                })),
            },
            Verdict::Block(reason) => Self {
                kind: "block".into(),
                detail: Some(serde_json::json!(reason)),
            },
            Verdict::ConstitutionalBlock { principle, reason } => Self {
                kind: "constitutional_block".into(),
                detail: Some(serde_json::json!({
                    "principle": principle,
                    "reason": reason,
                })),
            },
        }
    }
}

/// Result of replaying one plan iteration through the candidate chain.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplayedPlan {
    pub iter: u32,
    /// Verdict recorded in the capture (the `cassandra:chain/verdict`
    /// row's `verdict_kind` string). `None` when the capture has no
    /// verdict row for this iteration.
    pub baseline_verdict: Option<String>,
    /// Verdict from the candidate chain. `None` when the plan body
    /// was missing from the capture (pre-Slice-A) and replay was
    /// skipped.
    pub new_verdict: Option<VerdictSnapshot>,
    /// True iff `new_verdict.kind` differs from `baseline_verdict`.
    /// Detail strings ignored. False whenever `skipped_reason.is_some()`.
    pub is_delta: bool,
    /// Populated iff the plan was skipped. Operator sees which
    /// fixtures need recapture.
    pub skipped_reason: Option<String>,
}

/// Aggregate result for one capture file replayed against a chain.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplayResult {
    pub fixture_id: String,
    pub fixture_summary: String,
    pub captured_at: String,
    pub llm_model: String,
    pub plans_replayed: u32,
    pub plans_skipped_missing_body: u32,
    pub per_plan: Vec<ReplayedPlan>,
}

/// One capture file loaded from disk.
#[derive(Clone, Debug)]
pub struct LoadedCapture {
    pub path: PathBuf,
    pub capture: CaptureJson,
}

/// Pure delta predicate. True iff `baseline` and `new` differ in kind.
/// Detail strings are ignored. `new = None` (skipped) is never a delta.
/// `baseline = None` + `new = Some("approve")` is not a delta (same
/// default posture). `baseline = None` + `new = Some(other)` IS a
/// delta (a rule fired where the capture observed no verdict).
fn is_delta(baseline: Option<&str>, new: Option<&str>) -> bool {
    let Some(new_kind) = new else { return false; };
    match baseline {
        Some(b) => b != new_kind,
        None => new_kind != "approve",
    }
}

/// Pure: format a `[ReplayResult]` slice as an ASCII table for stdout.
/// Column widths are fixed for stable diffs; long fixture ids are
/// truncated to 40 chars. No terminal escapes / colour codes / unicode
/// in the body so the output is grep-friendly and CI-friendly.
pub fn format_report_table(results: &[ReplayResult]) -> String {
    use std::fmt::Write;
    let mut out = String::new();

    // Header.
    writeln!(
        out,
        "{:<40}  {:>4}  {:<11} {:<27} {:<2}",
        "fixture", "iter", "baseline", "new", "d?"
    ).unwrap();
    writeln!(
        out,
        "{}  {}  {} {} {}",
        "-".repeat(40),
        "-".repeat(4),
        "-".repeat(11),
        "-".repeat(27),
        "-".repeat(2),
    ).unwrap();

    let mut total_plans: u32 = 0;
    let mut total_skipped: u32 = 0;
    let mut total_deltas: u32 = 0;

    for r in results {
        for p in &r.per_plan {
            total_plans = total_plans.saturating_add(1);
            if p.skipped_reason.is_some() {
                total_skipped = total_skipped.saturating_add(1);
            }
            if p.is_delta {
                total_deltas = total_deltas.saturating_add(1);
            }

            let fid: String = r.fixture_id.chars().take(40).collect();
            let baseline = p.baseline_verdict.as_deref().unwrap_or("[none]");
            let new_str = match (&p.skipped_reason, &p.new_verdict) {
                (Some(reason), _) => {
                    // Render as "[skipped: <reason truncated to 17 chars>]".
                    let r: String = reason.chars().take(17).collect();
                    format!("[skipped: {r}]")
                }
                (None, Some(snap)) => render_new_verdict(snap),
                (None, None) => "[no replay]".into(),
            };
            let delta_mark = if p.skipped_reason.is_some() {
                "-"
            } else if p.is_delta {
                "*"
            } else {
                "."
            };
            writeln!(
                out,
                "{:<40}  {:>4}  {:<11} {:<27} {:<2}",
                fid, p.iter, baseline, new_str, delta_mark
            ).unwrap();
        }
    }

    let fixture_count = results.len();
    writeln!(out).unwrap();
    writeln!(
        out,
        "{total_plans} plans across {fixture_count} fixtures . {} delta{} . {} skipped",
        total_deltas,
        if total_deltas == 1 { "" } else { "s" },
        total_skipped,
    ).unwrap();

    out
}

/// Pure helper: project a `VerdictSnapshot` into a compact one-line
/// render for the table's "new" column. Constitutional blocks include
/// the principle; escalates include severity; others render as the
/// bare kind.
fn render_new_verdict(snap: &VerdictSnapshot) -> String {
    match snap.kind.as_str() {
        "constitutional_block" => {
            let p = snap.detail.as_ref()
                .and_then(|d| d.get("principle"))
                .and_then(|p| p.as_u64())
                .unwrap_or(0);
            format!("constitutional_block(p={p})")
        }
        "escalate" => {
            let sev = snap.detail.as_ref()
                .and_then(|d| d.get("severity"))
                .and_then(|s| s.as_str())
                .unwrap_or("?");
            format!("escalate({sev})")
        }
        // Bare kinds: approve, advisory, block.
        other => other.to_string(),
    }
}

/// Walk `dir/<fixture_id>/<filename>.json` files and deserialise each
/// into a `CaptureJson`. Returns one entry per file, sorted by
/// `(fixture_id, captured_at, path)` for stable output across runs.
///
/// Errors aggregate at the file level: one malformed file's
/// `serde_json::Error` is logged via `eprintln!` and the file is
/// skipped; the walk continues. The function returns `Err` only when
/// the root directory cannot be opened at all.
pub fn load_captures_from_dir(dir: &Path) -> std::io::Result<Vec<LoadedCapture>> {
    let mut out: Vec<LoadedCapture> = Vec::new();
    for fixture_entry in std::fs::read_dir(dir)? {
        let fixture_entry = match fixture_entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!("replay: skipping unreadable entry in {dir:?}: {e}");
                continue;
            }
        };
        let fixture_path = fixture_entry.path();
        if !fixture_path.is_dir() { continue; }

        let inner = match std::fs::read_dir(&fixture_path) {
            Ok(it) => it,
            Err(e) => {
                eprintln!("replay: skipping unreadable fixture dir {fixture_path:?}: {e}");
                continue;
            }
        };

        for file_entry in inner {
            let Ok(file_entry) = file_entry else { continue; };
            let path = file_entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") { continue; }

            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("replay: read({path:?}) failed: {e}");
                    continue;
                }
            };
            let capture: CaptureJson = match serde_json::from_slice(&bytes) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("replay: parse({path:?}) failed: {e}");
                    continue;
                }
            };
            out.push(LoadedCapture { path, capture });
        }
    }
    // Stable sort: (fixture_id, captured_at, path). Path tie-break
    // makes the walk-order deterministic across filesystems with
    // different inode orderings.
    out.sort_by(|a, b| {
        a.capture.fixture_id.cmp(&b.capture.fixture_id)
            .then_with(|| a.capture.captured_at.cmp(&b.capture.captured_at))
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(out)
}

/// Replay one capture's plan iterations through the candidate chain.
/// Async because `ReviewStage::review` is async; no I/O performed by
/// this function (the chain may be I/O-bearing if a real stage uses
/// async DB queries, but the harness itself is in-process).
///
/// Per-plan behaviour:
/// - `capture.plans[i].plan_json` is JSON null → emit `ReplayedPlan`
///   with `skipped_reason: Some(...)`; never fabricate a synthetic
///   `Plan` from derived fields. The reason distinguishes a
///   `source_truncated` row (payload destroyed at audit-write time,
///   unrecoverable — #62) from a pre-Slice-A row (recoverable by
///   recapture).
/// - `plan_json` deserialises into a `Plan` → call `chain.review` and
///   build a `VerdictSnapshot`.
///
/// `ReviewStageContext` reconstruction:
/// - `task_id`, `instruction`, `plan_count` from the capture.
/// - `classification_floor` from the matching audit-row's
///   `classification_floor` field if present (post-Slice-A); final
///   fallback to `DataClass::Public` (the producer default for a task
///   that doesn't pin a floor).
pub async fn replay_capture(
    capture: &CaptureJson,
    chain: &ChainReviewStage,
) -> ReplayResult {
    let mut per_plan = Vec::with_capacity(capture.plans.len());
    let mut replayed: u32 = 0;
    let mut skipped: u32 = 0;

    // Pull every agent/plan.formulate audit row up front. We look the
    // matching row up *by `plan_count`* per iteration (not by index)
    // so a missing or out-of-order row produces a clean fallback to
    // `DataClass::Public` instead of a silent off-by-one skew. The
    // audit stream is already in id-ascending order by construction.
    let plan_rows: Vec<&CapturedAuditRow> = capture.audit_rows.iter()
        .filter(|r| r.actor == "agent" && r.action == "plan.formulate")
        .collect();

    for cp in capture.plans.iter() {
        if cp.plan_json.is_null() {
            skipped = skipped.saturating_add(1);
            // Schema-v3 (#62): a truncated source row is *not* recoverable by
            // recapture — the audit writer replaced the whole payload with the
            // `truncate_payload` fingerprint. Surface it distinctly so the
            // operator doesn't chase the recapture advice for rows where it
            // can't help.
            let reason = if cp.source_truncated {
                "plan body elided at audit-write time (truncation envelope); \
                 unrecoverable — raise the audit payload budget to capture \
                 plans this large (issue #62)"
            } else {
                "plan body missing; recapture against current daemon \
                 (Slice A's audit-payload v2)"
            };
            per_plan.push(ReplayedPlan {
                iter: cp.iter,
                baseline_verdict: cp.verdict_today.clone(),
                new_verdict: None,
                is_delta: false,
                skipped_reason: Some(reason.into()),
            });
            continue;
        }

        // Decode the plan body. A capture with non-null plan_json
        // that fails to deserialise is operator-facing corruption —
        // surface it as a skip with a distinct reason.
        let plan: Plan = match serde_json::from_value(cp.plan_json.clone()) {
            Ok(p) => p,
            Err(e) => {
                skipped = skipped.saturating_add(1);
                per_plan.push(ReplayedPlan {
                    iter: cp.iter,
                    baseline_verdict: cp.verdict_today.clone(),
                    new_verdict: None,
                    is_delta: false,
                    skipped_reason: Some(format!("plan body decode error: {e}")),
                });
                continue;
            }
        };

        // Classification floor: prefer the audit-row's
        // classification_floor (post-Slice-A) over the plan's
        // data_ceiling (different concept; plan-level inferred
        // ceiling vs task-level producer floor). The row is matched
        // by `plan_count == cp.iter` so a dropped or reordered row
        // doesn't silently misalign with the wrong iteration; fallback
        // on no-match is `DataClass::Public` (producer default).
        let classification_floor = plan_rows.iter()
            .find(|r| {
                r.payload.get("plan_count")
                    .and_then(|v| v.as_u64())
                    == Some(cp.iter as u64)
            })
            .and_then(|r| r.payload.get("classification_floor"))
            .and_then(|v| serde_json::from_value::<DataClass>(v.clone()).ok())
            .unwrap_or(DataClass::Public);

        let ctx = ReviewStageContext {
            task_id: capture.task_id,
            instruction: &capture.prompt,
            classification_floor,
            plan_count: cp.iter,
        };

        let verdict = chain.review(&plan, &ctx).await;
        let snap = VerdictSnapshot::from_verdict(&verdict);

        let delta = is_delta(
            cp.verdict_today.as_deref(),
            Some(snap.kind.as_str()),
        );

        per_plan.push(ReplayedPlan {
            iter: cp.iter,
            baseline_verdict: cp.verdict_today.clone(),
            new_verdict: Some(snap),
            is_delta: delta,
            skipped_reason: None,
        });
        replayed = replayed.saturating_add(1);
    }

    ReplayResult {
        fixture_id: capture.fixture_id.clone(),
        fixture_summary: capture.fixture_summary.clone(),
        captured_at: capture.captured_at.clone(),
        llm_model: capture.llm_model.clone(),
        plans_replayed: replayed,
        plans_skipped_missing_body: skipped,
        per_plan,
    }
}

#[cfg(test)]
mod tests;
