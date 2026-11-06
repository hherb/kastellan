//! The one-line progress note `ask` prints to stderr for each notebook entry
//! as it lands (#985). stdout keeps only the answer, so a script piping
//! `ask` sees exactly what it saw before.
//!
//! The payloads are the planner's own (`core::scheduler::notebook::state`),
//! read here as plain JSON: this is a display, so a field it cannot find is
//! shown as empty rather than refused. Their text is model-written and can
//! quote tool output, so every control character (an ANSI escape, a newline)
//! is blanked before it reaches the terminal, and each text is capped.

use serde_json::Value;

/// The most characters of any one model-written text a note shows.
const TEXT_CAP: usize = 160;

/// The note for one entry of `kind` with `payload`, or `None` for the kinds
/// whose news `ask` prints anyway: the answer (on stdout) and how the task
/// ended (its end line).
pub(crate) fn entry_line(kind: &str, payload: &Value) -> Option<String> {
    let text = |key: &str| clean(payload.get(key).and_then(Value::as_str).unwrap_or(""));
    let step = || payload.get("step").and_then(Value::as_u64).unwrap_or(0);
    let line = match kind {
        "steps" => {
            let steps = payload.get("steps").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
            let listed: Vec<String> = steps
                .iter()
                .map(|s| {
                    let id = s.get("id").and_then(Value::as_u64).unwrap_or(0);
                    let purpose = clean(s.get("purpose").and_then(Value::as_str).unwrap_or(""));
                    format!("{id}. {purpose}")
                })
                .collect();
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

/// `s` on one line, with no control character, capped at [`TEXT_CAP`]
/// characters (an ellipsis marks a cut).
fn clean(s: &str) -> String {
    let mut out: String = s.chars().take(TEXT_CAP).map(|c| if c.is_control() { ' ' } else { c }).collect();
    if s.chars().nth(TEXT_CAP).is_some() {
        out.push('…');
    }
    out
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
