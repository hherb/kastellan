//! The repeated-call guard (#922).
//!
//! A notebook turn sees only its step's last result, so a model that does
//! not think can lose track and make the same call again and again: DGX task
//! 291 (thinking off on routine turns) alternated between two
//! `mail.get_message` calls for 20 turns. A call identical to one this task
//! already made **successfully** returns the same result, so it is not
//! dispatched; the next turn is told why and replans, thinking. A call that
//! failed may be made again: the failure may be transient, and it gave no
//! facts to continue from.
//!
//! "Identical" is the same tool, method and parameters with every scalar
//! read as text, because the model writes a message id as `"38996"` one turn
//! and `38996` the next. A new page (`offset`) is a different call.

use serde_json::Value;

use crate::cassandra::types::PlannedStep;
use crate::scheduler::inner_loop::{PlanRecord, StepOutcome};

/// The block reason shown to the model for a repeated call.
pub(crate) fn repeated_call_reason(method: &str) -> String {
    format!(
        "repeated call: `{method}` with these exact parameters was already made in this task, so it was not \
         run again — its facts belong in your findings; continue from them, or make a different call"
    )
}

/// Was `step` already dispatched in this task, and did it succeed? Pure.
pub(crate) fn already_made(plans: &[PlanRecord], step: &PlannedStep) -> bool {
    let key = call_key(step);
    plans
        .iter()
        .flat_map(|r| r.plan.steps.iter().zip(r.outcomes()))
        .any(|(s, o)| matches!(o, StepOutcome::Ok(_)) && call_key(s) == key)
}

fn call_key(step: &PlannedStep) -> (String, String, Value) {
    (step.tool.clone(), step.method.clone(), textual(&step.parameters))
}

/// `v` with every number and boolean turned into its text, so `38996` and
/// `"38996"` compare equal. Object keys already compare by value.
fn textual(v: &Value) -> Value {
    match v {
        Value::Number(n) => Value::String(n.to_string()),
        Value::Bool(b) => Value::String(b.to_string()),
        Value::Array(a) => Value::Array(a.iter().map(textual).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), textual(v))).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cassandra::types::{DataClass, Plan};
    use serde_json::json;

    fn step(method: &str, params: Value) -> PlannedStep {
        PlannedStep {
            tool: "mail".into(),
            method: method.into(),
            parameters: params,
            returns: "r".into(),
            done_when: "d".into(),
            classification: DataClass::Personal,
        }
    }

    fn record(s: PlannedStep, ran: bool) -> PlanRecord {
        record_with(s, if ran { vec![StepOutcome::Ok(json!({}))] } else { vec![] })
    }

    fn record_with(s: PlannedStep, outcomes: Vec<StepOutcome>) -> PlanRecord {
        let plan = Plan {
            context: String::new(),
            decision: "d".into(),
            rationale: String::new(),
            steps: vec![s],
            result: None,
            data_ceiling: Some(DataClass::Personal),
            refused: None,
            floor_request: None,
            l1_insight: None,
            l3_skill: None,
            invoke_skill: None,
            python_skill: None,
        };
        PlanRecord::new(plan, outcomes)
    }

    #[test]
    fn the_same_call_is_caught_whatever_the_ids_spelling() {
        let plans = vec![record(step("mail.get_message", json!({"message_id": "38996"})), true)];
        assert!(already_made(&plans, &step("mail.get_message", json!({"message_id": 38996}))));
        assert!(already_made(&plans, &step("mail.get_message", json!({"message_id": "38996"}))));
    }

    #[test]
    fn a_different_page_message_or_method_is_a_new_call() {
        let first = json!({"message_id": 38996, "filename": "a.pdf"});
        let plans = vec![record(step("mail.get_attachment_text", first.clone()), true)];
        let mut next_page = first.clone();
        next_page["offset"] = json!(8000);
        assert!(!already_made(&plans, &step("mail.get_attachment_text", next_page)));
        assert!(!already_made(&plans, &step("mail.get_attachment_text", json!({"message_id": 38983, "filename": "a.pdf"}))));
        assert!(!already_made(&plans, &step("mail.get_message", first)));
    }

    #[test]
    fn a_call_that_never_ran_does_not_count() {
        let plans = vec![record(step("mail.get_message", json!({"message_id": 1})), false)];
        assert!(!already_made(&plans, &step("mail.get_message", json!({"message_id": 1}))));
    }

    #[test]
    fn a_call_that_failed_may_be_retried() {
        let failed = StepOutcome::Err { code: "TIMEOUT".into(), detail: "worker timed out".into() };
        let plans = vec![record_with(step("mail.get_message", json!({"message_id": 1})), vec![failed])];
        assert!(!already_made(&plans, &step("mail.get_message", json!({"message_id": 1}))));
    }
}
