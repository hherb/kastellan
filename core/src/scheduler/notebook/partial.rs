//! The answer code writes when the forced-answer turn produced none (#1044).
//!
//! DGX task 464: at the time ceiling the forced-answer turn ticked a step and
//! wrote no answer, and its re-ask ticked and called on. The task failed,
//! though four sifts had kept rows and the tick carried a finding. Now, when
//! that turn fails with something found, code writes the answer: the findings
//! ticked so far, marked as not checked, and (attached by the loop as on any
//! forced answer, `sift::answer::forced_table`) the table of every sift that
//! kept rows. It is reviewed like any answer.

use super::reply::{AnswerSpec, NotebookReply};
use super::sift;
use super::state::NotebookState;
use crate::scheduler::inner_loop::PlanRecord;

/// The first line of a code-written answer: it says the answer is partial.
pub(crate) const PARTIAL_HEAD: &str =
    "No answer was written before this task stopped, so this is what it found, not checked against the question.";

/// The answer for a forced-answer turn that failed: [`PARTIAL_HEAD`], then
/// one line per ticked finding. `None` when there is nothing to show (no
/// finding, and no sift kept rows): then the task fails as before. Pure.
pub(crate) fn partial_answer(state: &NotebookState, plans: &[PlanRecord]) -> Option<NotebookReply> {
    let lines: Vec<String> = state.findings.iter().map(|f| format!("- {} ({})", f.text, f.source)).collect();
    let body = if lines.is_empty() { PARTIAL_HEAD.to_string() } else { format!("{PARTIAL_HEAD}\n\n{}", lines.join("\n")) };
    let reply = NotebookReply {
        tick: None,
        steps: None,
        call: None,
        answer: Some(AnswerSpec {
            body,
            data_ceiling: None,
            refused: None,
            table: None,
            l1_insight: None,
            l3_skill: None,
            python_skill: None,
        }),
        infeasible: None,
    };
    // With no findings, the kept rows are the whole answer: no sift, no answer.
    let has_rows = sift::answer::forced_table(&reply, plans).is_some();
    (!lines.is_empty() || has_rows).then_some(reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cassandra::types::DataClass;
    use crate::scheduler::notebook::sift::chain::tests::{params, sift_record};
    use crate::scheduler::notebook::state::Finding;

    fn found(text: &str, source: &str) -> Finding {
        Finding { text: text.into(), source: source.into(), step: 1, classification: DataClass::Personal }
    }

    fn body(r: &NotebookReply) -> &str {
        &r.answer.as_ref().expect("an answer").body
    }

    #[test]
    fn a_failed_forced_turn_answers_with_what_was_found() {
        let state = NotebookState { findings: vec![found("Invoice 202607-01: $19,431.18", "msg 37294")], ..Default::default() };
        let r = partial_answer(&state, &[]).expect("a finding is something to show");
        assert_eq!(body(&r), format!("{PARTIAL_HEAD}\n\n- Invoice 202607-01: $19,431.18 (msg 37294)"));
        assert!(r.call.is_none() && r.tick.is_none() && r.steps.is_none() && r.infeasible.is_none());
        assert!(r.answer.as_ref().unwrap().table.is_none(), "the loop attaches the kept rows, as on any forced answer");

        // Task 464's shape: kept rows and no finding still make an answer.
        let kept = sift_record(params(0, "x"), serde_json::json!({"scrapbook": "sb:464:2", "kept": 7}));
        let r = partial_answer(&NotebookState::default(), &[kept]).expect("kept rows are something to show");
        assert_eq!(body(&r), PARTIAL_HEAD);
        let nothing = sift_record(params(0, "x"), serde_json::json!({"scrapbook": "sb:464:3", "kept": 0}));
        assert_eq!(partial_answer(&NotebookState::default(), &[nothing]), None, "nothing found: the task fails as before");
    }
}
