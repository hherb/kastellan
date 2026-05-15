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

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::cassandra::types::Verdict;
use crate::observation::capture::CaptureJson;

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
fn is_delta(baseline: Option<&str>, new: Option<&String>) -> bool {
    let Some(new_kind) = new else { return false; };
    match baseline {
        Some(b) => b != new_kind.as_str(),
        None => new_kind != "approve",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cassandra::types::Severity;

    // ---- VerdictSnapshot::from_verdict ----

    #[test]
    fn verdict_snapshot_approve_has_no_detail() {
        let s = VerdictSnapshot::from_verdict(&Verdict::Approve);
        assert_eq!(s.kind, "approve");
        assert!(s.detail.is_none());
    }

    #[test]
    fn verdict_snapshot_advisory_carries_message_as_detail_string() {
        let s = VerdictSnapshot::from_verdict(&Verdict::Advisory("careful".into()));
        assert_eq!(s.kind, "advisory");
        assert_eq!(s.detail, Some(serde_json::json!("careful")));
    }

    #[test]
    fn verdict_snapshot_escalate_carries_concern_and_severity_object() {
        let s = VerdictSnapshot::from_verdict(&Verdict::Escalate(
            "high latency".into(),
            Severity::High,
        ));
        assert_eq!(s.kind, "escalate");
        assert_eq!(
            s.detail,
            Some(serde_json::json!({"concern": "high latency", "severity": "high"})),
        );
    }

    #[test]
    fn verdict_snapshot_block_carries_reason_as_detail_string() {
        let s = VerdictSnapshot::from_verdict(&Verdict::Block("denied".into()));
        assert_eq!(s.kind, "block");
        assert_eq!(s.detail, Some(serde_json::json!("denied")));
    }

    #[test]
    fn verdict_snapshot_constitutional_block_carries_principle_and_reason() {
        let s = VerdictSnapshot::from_verdict(&Verdict::ConstitutionalBlock {
            principle: 1,
            reason: "physical_harm".into(),
        });
        assert_eq!(s.kind, "constitutional_block");
        assert_eq!(
            s.detail,
            Some(serde_json::json!({"principle": 1, "reason": "physical_harm"})),
        );
    }

    #[test]
    fn verdict_snapshot_round_trips_through_serde_json() {
        let s = VerdictSnapshot::from_verdict(&Verdict::ConstitutionalBlock {
            principle: 2,
            reason: "fraud".into(),
        });
        let j = serde_json::to_value(&s).expect("snapshot must serialise");
        let s2: VerdictSnapshot =
            serde_json::from_value(j).expect("snapshot must round-trip");
        assert_eq!(s, s2);
    }

    // ---- is_delta ----

    #[test]
    fn is_delta_false_when_both_approve() {
        assert!(!is_delta(Some("approve"), Some(&"approve".to_string())));
    }

    #[test]
    fn is_delta_true_when_baseline_approve_new_block() {
        assert!(is_delta(Some("approve"), Some(&"block".to_string())));
    }

    #[test]
    fn is_delta_true_when_baseline_approve_new_constitutional_block() {
        assert!(is_delta(Some("approve"), Some(&"constitutional_block".to_string())));
    }

    #[test]
    fn is_delta_true_when_baseline_missing_new_not_approve() {
        // Baseline absent + new verdict is anything but approve = delta.
        // Operator wants to see "something fired where the capture
        // never observed a verdict."
        assert!(is_delta(None, Some(&"block".to_string())));
    }

    #[test]
    fn is_delta_false_when_baseline_missing_new_approve() {
        // Baseline absent + new approve = not a delta. "Same default
        // posture" — nothing interesting to flag.
        assert!(!is_delta(None, Some(&"approve".to_string())));
    }

    #[test]
    fn is_delta_false_when_new_missing_skipped() {
        // new = None means the plan was skipped (pre-Slice-A capture);
        // no comparison possible. Per spec: skipped plans are never deltas.
        assert!(!is_delta(Some("approve"), None));
        assert!(!is_delta(None, None));
    }
}
