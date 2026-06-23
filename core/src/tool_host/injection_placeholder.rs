//! The placeholder substituted for a tool result that fails the output
//! injection screen.
//!
//! When [`crate::tool_host::dispatch`] blocks a worker result, the raw output
//! must not reach the planner — but the planner still needs an *intelligible*
//! signal that content was withheld, otherwise it sees a silent gap and may
//! re-run the step. The planner-summary render surfaces step output via
//! `injection_guard::extract_scannable_text`, which emits only **string leaf
//! values** — so the structured `injection_blocked`/`score`/`reason_codes`
//! fields are invisible to it. The [`WITHHELD_NOTE`] string is the leaf the
//! planner actually sees (issue #340; mirrors the `fetch_screen` withheld-note
//! so both screening chokepoints signal the same way).

use serde_json::Value;

/// Human-readable signal the planner sees when a tool result is withheld for
/// failing the injection screen. A **string leaf** so the planner-summary
/// render's `extract_scannable_text` surfaces it (the structured fields are
/// stripped by that render, see module docs — #340).
pub const WITHHELD_NOTE: &str = "[tool output withheld: failed injection screen]";

/// Build the placeholder `Value` substituted for an injection-blocked tool
/// result.
///
/// - `note` ([`WITHHELD_NOTE`]) is the planner-facing signal.
/// - `injection_blocked`/`score`/`reason_codes` are kept for audit-shape parity
///   with the `fetch_screen` placeholder and for any structured consumer.
pub fn injection_blocked_placeholder(score: f32, reason_codes: &[&str]) -> Value {
    serde_json::json!({
        "injection_blocked": true,
        "note":              WITHHELD_NOTE,
        "score":             score,
        "reason_codes":      reason_codes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_carries_human_readable_note() {
        let v = injection_blocked_placeholder(0.9, &["instruction_override"]);
        // The note is a string leaf — the only field the planner-summary render
        // surfaces (#340). It must clearly signal *withheld*, not look like data.
        let note = v["note"].as_str().expect("note is a string");
        assert_eq!(note, WITHHELD_NOTE);
        assert!(note.contains("withheld"), "note must signal content was withheld");
    }

    #[test]
    fn placeholder_keeps_structured_fields_for_audit_parity() {
        let v = injection_blocked_placeholder(0.75, &["secret_exfiltration", "instruction_override"]);
        assert_eq!(v["injection_blocked"], true);
        assert_eq!(v["score"].as_f64().expect("score is a number"), 0.75);
        let codes = v["reason_codes"].as_array().expect("reason_codes is an array");
        assert!(codes.iter().any(|c| c == "secret_exfiltration"));
        assert!(codes.iter().any(|c| c == "instruction_override"));
    }

    #[test]
    fn placeholder_does_not_leak_raw_output() {
        // The placeholder never carries the blocked body — only the constant
        // note and structured metadata. (The raw value is dropped at the call
        // site; this pins that the builder itself adds nothing else.)
        let v = injection_blocked_placeholder(1.0, &[]);
        let obj = v.as_object().expect("placeholder is an object");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["injection_blocked", "note", "reason_codes", "score"]);
    }
}
