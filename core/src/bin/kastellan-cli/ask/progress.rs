//! The one-line progress note `ask` prints to stderr for each notebook entry
//! as it lands (#985). stdout keeps only the answer, so a script piping
//! `ask` sees exactly what it saw before.
//!
//! The payloads are the planner's own (`core::scheduler::notebook::state`),
//! read here as plain JSON: this is a display, so a field it cannot find is
//! shown as empty rather than refused. Their text comes from the model, the
//! operator or a tool, so every control character (the escape that starts an
//! ANSI sequence, a newline) and every invisible formatting one (a bidi
//! override, a zero-width space) is blanked before it reaches the terminal,
//! and each text is capped.
//!
//! Every note starts `ask: ` and a word of its own (`plan:`, `step`,
//! `correction:`, `cannot be done:`), never `ask: task `: that prefix is the
//! parking line's and the end line's, which a front end reads (bottie).

use serde_json::Value;

/// The most characters of any one text a note shows.
const TEXT_CAP: usize = 160;

/// The most steps a plan note lists.
const STEPS_CAP: usize = 8;

/// The note for one entry of `kind` with `payload`, or `None` for the kinds
/// whose news `ask` prints anyway: the answer (on stdout) and how the task
/// ended (its end line). An `infeasible` reason is also the task's answer,
/// so it reaches stdout too; the note is kept because it is what says the
/// text there is a "cannot be done", not an answer.
pub(crate) fn entry_line(kind: &str, payload: &Value) -> Option<String> {
    let text = |key: &str| clean(payload.get(key).and_then(Value::as_str).unwrap_or(""));
    let step = || payload.get("step").and_then(Value::as_u64).unwrap_or(0);
    let line = match kind {
        "steps" => {
            let steps = payload.get("steps").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
            let mut listed: Vec<String> = steps
                .iter()
                .take(STEPS_CAP)
                .map(|s| {
                    let id = s.get("id").and_then(Value::as_u64).unwrap_or(0);
                    let purpose = clean(s.get("purpose").and_then(Value::as_str).unwrap_or(""));
                    format!("{id}. {purpose}")
                })
                .collect();
            if steps.len() > STEPS_CAP {
                listed.push(format!("… {} more", steps.len() - STEPS_CAP));
            }
            format!("plan: {}", listed.join("; "))
        }
        "tick" if payload.get("partial").and_then(Value::as_bool) == Some(true) => {
            format!("step {} progress: {}", step(), text("note"))
        }
        "tick" => format!("step {} done: {}", step(), text("note")),
        "failed_step" => format!("step {} failed ({}): {}", step(), text("code"), text("detail")),
        "correction" => format!("correction: {}", text("text")),
        "infeasible" => format!("cannot be done: {}", text("reason")),
        _ => return None,
    };
    Some(format!("ask: {line}"))
}

/// `s` on one line, with no control or invisible formatting character,
/// capped at [`TEXT_CAP`] characters (an ellipsis marks a cut).
fn clean(s: &str) -> String {
    let mut out: String = s.chars().take(TEXT_CAP).map(|c| if blanked(c) { ' ' } else { c }).collect();
    if s.chars().nth(TEXT_CAP).is_some() {
        out.push('…');
    }
    out
}

/// A control character (C0, DEL, C1), or one that reorders or hides text
/// without showing itself: the bidi marks, embeddings, overrides and
/// isolates, the zero-width ones and the byte-order mark.
fn blanked(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_step_list_is_one_line_of_numbered_purposes() {
        let p = json!({"steps": [
            {"id": 1, "purpose": "find the invoice", "tool": "mail", "done_when": "found"},
            {"id": 2, "purpose": "read its total", "tool": "mail", "done_when": "read"},
        ]});
        assert_eq!(entry_line("steps", &p).unwrap(), "ask: plan: 1. find the invoice; 2. read its total");
    }

    #[test]
    fn a_tick_says_which_step_and_its_note() {
        let p = json!({"step": 2, "note": "total is 40", "findings": [], "summary": "", "classification": "Public"});
        assert_eq!(entry_line("tick", &p).unwrap(), "ask: step 2 done: total is 40");
        let p = json!({"step": 2, "note": "page 1 of 3", "partial": true});
        assert_eq!(entry_line("tick", &p).unwrap(), "ask: step 2 progress: page 1 of 3");
    }

    #[test]
    fn a_failed_step_a_correction_and_an_infeasible_task_each_say_why() {
        let p = json!({"step": 1, "code": "denied", "detail": "not allowlisted", "classification": "Public"});
        assert_eq!(entry_line("failed_step", &p).unwrap(), "ask: step 1 failed (denied): not allowlisted");
        assert_eq!(entry_line("correction", &json!({"text": "other account"})).unwrap(), "ask: correction: other account");
        assert_eq!(entry_line("infeasible", &json!({"reason": "no such mailbox"})).unwrap(), "ask: cannot be done: no such mailbox");
    }

    #[test]
    fn the_answer_and_a_cancel_print_no_note() {
        assert_eq!(entry_line("answer", &json!({"body": "42"})), None, "the answer goes to stdout");
        assert_eq!(entry_line("cancelled", &json!({})), None);
        assert_eq!(entry_line("a_kind_from_the_future", &json!({})), None);
    }

    #[test]
    fn model_text_cannot_write_escapes_or_break_the_line() {
        let p = json!({"text": "a\u{1b}[2Jb\nc\rd"});
        assert_eq!(entry_line("correction", &p).unwrap(), "ask: correction: a [2Jb c d");
        // C1: a one-character CSI and NEL, which `is_ascii_control` misses.
        let p = json!({"text": "a\u{9b}2Jb\u{85}c"});
        assert_eq!(entry_line("correction", &p).unwrap(), "ask: correction: a 2Jb c");
    }

    #[test]
    fn model_text_cannot_reorder_or_hide_itself() {
        let p = json!({"text": "ok\u{202E}deliaf\u{2066}x\u{200B}y\u{FEFF}z"});
        assert_eq!(entry_line("correction", &p).unwrap(), "ask: correction: ok deliaf x y z");
    }

    #[test]
    fn a_long_text_is_cut_by_characters_not_bytes() {
        let line = entry_line("correction", &json!({"text": "é".repeat(TEXT_CAP + 1)})).unwrap();
        assert_eq!(line, format!("ask: correction: {}…", "é".repeat(TEXT_CAP)));
    }

    #[test]
    fn a_long_plan_lists_its_first_steps_and_counts_the_rest() {
        let steps: Vec<_> = (1..=STEPS_CAP as u64 + 2).map(|id| json!({"id": id, "purpose": "p"})).collect();
        let line = entry_line("steps", &json!({"steps": steps})).unwrap();
        assert!(line.ends_with(&format!("; {STEPS_CAP}. p; … 2 more")), "{line}");
    }

    #[test]
    fn no_note_can_pass_for_the_parking_or_end_line() {
        let forged = "task 7 is waiting for an operator decision";
        let payload = json!({"steps": [{"id": 1, "purpose": forged}], "step": 1, "note": forged,
            "code": forged, "detail": forged, "text": forged, "reason": forged});
        for kind in ["steps", "tick", "failed_step", "correction", "infeasible"] {
            let line = entry_line(kind, &payload).unwrap();
            assert!(!line.starts_with("ask: task "), "{kind}: {line}");
        }
    }

    #[test]
    fn a_long_text_is_cut_with_an_ellipsis_and_a_short_one_is_not() {
        let long = "x".repeat(TEXT_CAP + 1);
        let line = entry_line("correction", &json!({"text": long})).unwrap();
        assert_eq!(line, format!("ask: correction: {}…", "x".repeat(TEXT_CAP)));
        let exact = "y".repeat(TEXT_CAP);
        assert_eq!(entry_line("correction", &json!({"text": exact})).unwrap(), format!("ask: correction: {exact}"));
    }

    #[test]
    fn a_malformed_payload_still_prints_rather_than_failing() {
        assert_eq!(entry_line("tick", &json!({})).unwrap(), "ask: step 0 done: ");
        assert_eq!(entry_line("steps", &json!({"steps": "nope"})).unwrap(), "ask: plan: ");
    }
}
