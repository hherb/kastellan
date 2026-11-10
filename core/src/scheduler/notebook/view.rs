//! The operator's JSON view of a task's notebook (#922 slice 2).
//!
//! `kastellan-cli session`'s `task.status` returns what a notebook says
//! *now*: each step with its status and note, the model's summary, the
//! findings, the corrections. That is exactly the pure [`fold`] the
//! planner loop uses, so this module renders the fold rather than
//! re-deriving it — the operator and the planner can never disagree about
//! which step is done.
//!
//! The fold's types stay `pub(crate)`; only this one function and its JSON
//! shape are public, so the CLI binary depends on a shape, not on the
//! loop's internals.
//!
//! Trust: this is an **operator surface** (design C2). The notes, findings
//! and summary are model-written from tool output and are shown as they
//! are stored, the same way `kastellan-cli tasks status` shows a result.

use kastellan_db::notebook::Entry;
use serde_json::{json, Value};

use super::state::{fold, StepStatus};

/// Fold `entries` (one task's, in `seq` order) and render the result:
///
/// ```json
/// {"steps": [{"id": 1, "purpose": "...", "tool": "...", "done_when": "...",
///             "status": "pending|done|failed", "note": "..." | null}],
///  "summary": "..." | null,
///  "findings": [{"text": "...", "source": "...", "step": 1, "classification": "Public"}],
///  "corrections": [{"seq": 3, "text": "..."}],
///  "terminal": false,
///  "last_seq": 4}
/// ```
///
/// An empty notebook (a task still on its first turn)
/// renders as no steps and `last_seq: 0`. A notebook the fold refuses
/// returns the fold's own error text, so the caller can say which entry is
/// wrong instead of showing a half-read notebook.
pub fn fold_view(entries: &[Entry]) -> Result<Value, String> {
    let state = fold(entries).map_err(|e| e.to_string())?;
    let steps: Vec<Value> = state
        .steps
        .iter()
        .map(|s| {
            json!({
                "id": s.spec.id,
                "purpose": s.spec.purpose,
                "tool": s.spec.tool,
                "done_when": s.spec.done_when,
                "status": match s.status {
                    StepStatus::Pending => "pending",
                    StepStatus::Done => "done",
                    StepStatus::Failed => "failed",
                },
                "note": s.note,
            })
        })
        .collect();
    let findings: Vec<Value> = state
        .findings
        .iter()
        .map(|f| json!({"text": f.text, "source": f.source, "step": f.step, "classification": f.classification}))
        .collect();
    let corrections: Vec<Value> =
        state.corrections.iter().map(|c| json!({"seq": c.seq, "text": c.text})).collect();
    Ok(json!({
        "steps": steps,
        "summary": state.summary,
        "findings": findings,
        "corrections": corrections,
        "terminal": state.terminal,
        "last_seq": state.last_seq,
    }))
}

#[cfg(test)]
mod tests {
    use super::fold_view;
    use kastellan_db::notebook::{Entry, EntryKind};
    use serde_json::json;

    fn entry(seq: i64, kind: EntryKind, payload: serde_json::Value) -> Entry {
        Entry { task_id: 1, seq, kind, payload, created_at: time::OffsetDateTime::UNIX_EPOCH }
    }

    fn step(id: u32) -> serde_json::Value {
        json!({"id": id, "purpose": format!("p{id}"), "tool": "mail", "done_when": "found"})
    }

    #[test]
    fn an_empty_notebook_is_no_steps_at_seq_zero() {
        let v = fold_view(&[]).unwrap();
        assert_eq!(v["steps"], json!([]));
        assert_eq!(v["last_seq"], 0);
        assert_eq!(v["terminal"], false);
        assert_eq!(v["summary"], serde_json::Value::Null);
    }

    #[test]
    fn each_step_shows_its_status_note_and_the_findings_their_step() {
        let entries = [
            entry(1, EntryKind::Steps, json!({"steps": [step(1), step(2), step(3)]})),
            entry(2, EntryKind::Tick, json!({
                "step": 1, "note": "found it", "summary": "one done",
                "findings": [{"text": "invoice 42", "source": "msg 7"}],
                "classification": "Personal",
            })),
            entry(3, EntryKind::FailedStep, json!({
                "step": 2, "code": "-32002", "detail": "boom", "classification": "Public",
            })),
            entry(4, EntryKind::Correction, json!({"text": "other account"})),
        ];
        let v = fold_view(&entries).unwrap();
        let status: Vec<&str> = v["steps"].as_array().unwrap().iter().map(|s| s["status"].as_str().unwrap()).collect();
        assert_eq!(status, ["done", "failed", "pending"]);
        assert_eq!(v["steps"][0]["note"], "found it");
        assert_eq!(v["steps"][2]["note"], serde_json::Value::Null);
        assert_eq!(v["steps"][2]["purpose"], "p3");
        assert_eq!(v["summary"], "one done");
        assert_eq!(v["findings"], json!([{"text": "invoice 42", "source": "msg 7", "step": 1, "classification": "Personal"}]));
        assert_eq!(v["corrections"], json!([{"seq": 4, "text": "other account"}]));
        assert_eq!(v["last_seq"], 4);
        assert_eq!(v["terminal"], false);
    }

    #[test]
    fn an_answered_notebook_is_terminal() {
        let entries = [
            entry(1, EntryKind::Steps, json!({"steps": [step(1)]})),
            entry(2, EntryKind::Answer, json!({"body": "done"})),
        ];
        assert_eq!(fold_view(&entries).unwrap()["terminal"], true);
    }

    #[test]
    fn a_notebook_the_fold_refuses_names_the_entry() {
        // A tick for a step that was never listed.
        let entries = [entry(1, EntryKind::Tick, json!({
            "step": 9, "note": "n", "summary": "s", "findings": [], "classification": "Public",
        }))];
        let err = fold_view(&entries).unwrap_err();
        assert!(err.contains("entry 1") && err.contains("no step 9"), "{err}");
    }
}
