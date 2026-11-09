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
//!
//! A call whose result was handoff-stashed and is no longer held (purged at
//! a suspend, or evicted) is not a repeat: its result is gone, so making it
//! again is the only way back to it (#964).

use serde_json::Value;

use crate::cassandra::types::{InvokeDirective, PlannedStep};
use crate::handoff::{self, HandoffRef};
use crate::scheduler::inner_loop::{PlanRecord, StepOutcome};

/// The block reason shown to the model for a repeated call.
pub(crate) fn repeated_call_reason(method: &str) -> String {
    format!(
        "repeated call: `{method}` with these exact parameters was already made in this task, so it was not \
         run again — its facts belong in your findings; continue from them, or make a different call"
    )
}

/// Was `step` already dispatched in this task, and did it succeed with a
/// result still to hand? `held` says whether a stash is still held. Pure.
pub(crate) fn already_made(plans: &[PlanRecord], step: &PlannedStep, held: impl Fn(&HandoffRef) -> bool) -> bool {
    let key = call_key(step);
    // A sift's reads reached the planner only as the sift's counts, so they
    // do not count: reading one of those items is a first read.
    plans
        .iter()
        .filter(|r| !r.plan.decision.starts_with(crate::scheduler::notebook::sift::SIFT_READS_DECISION))
        .flat_map(|r| r.plan.steps.iter().zip(r.outcomes()))
        .any(|(s, o)| match o {
            StepOutcome::Ok(v) => call_key(s) == key && handoff::placeholder_ref(v).map_or(true, |h| held(&h)),
            _ => false,
        })
}

/// Did this task already run skill `d` with the same args, every step
/// succeeding with a result still to hand (#996)? An invoke plan has no step
/// of its own to compare until `execute` expands it, so it is compared by
/// its directive; one that failed partway may be run again. Pure.
pub(crate) fn already_invoked(plans: &[PlanRecord], d: &InvokeDirective, held: impl Fn(&HandoffRef) -> bool) -> bool {
    let same = |p: &InvokeDirective| p.name == d.name && p.args == d.args && textual(&p.params) == textual(&d.params);
    plans.iter().filter(|r| r.plan.invoke_skill.as_ref().is_some_and(same)).any(|r| {
        !r.outcomes().is_empty()
            && r.outcomes().len() == r.plan.steps.len()
            && r.outcomes().iter().all(|o| matches!(o, StepOutcome::Ok(v) if handoff::placeholder_ref(v).map_or(true, |h| held(&h))))
    })
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

    fn made(plans: &[PlanRecord], step: &PlannedStep) -> bool {
        already_made(plans, step, |_| true)
    }

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

    fn invoked(name: &str, q: &str, outcomes: Vec<StepOutcome>) -> PlanRecord {
        let mut plan = record(step("mail.a", json!({"q": q})), false).plan;
        plan.steps.push(step("mail.b", json!({"id": q})));
        plan.invoke_skill = Some(directive(name, q));
        PlanRecord::new(plan, outcomes)
    }

    fn directive(name: &str, q: &str) -> InvokeDirective {
        InvokeDirective { name: name.into(), args: [("q".to_string(), q.to_string())].into(), params: Value::Null }
    }

    /// The same skill with the same args is a repeat once every step ran,
    /// while its results are held; other args, another skill, a run that
    /// failed partway, or a purged stash are not.
    #[test]
    fn a_skill_run_whole_is_a_repeat() {
        let ok = || StepOutcome::Ok(json!({}));
        let plans = vec![invoked("s", "x", vec![ok(), ok()])];
        assert!(already_invoked(&plans, &directive("s", "x"), |_| true));
        assert!(!already_invoked(&plans, &directive("s", "y"), |_| true));
        assert!(!already_invoked(&plans, &directive("t", "x"), |_| true));
        let failed = StepOutcome::Err { code: "E".into(), detail: "d".into() };
        assert!(!already_invoked(&[invoked("s", "x", vec![ok(), failed])], &directive("s", "x"), |_| true));
        let cache = crate::handoff::HandoffCache::new();
        let placeholder = cache.stash_if_oversized(1, &json!({"results": []}), 0).expect("cap 0").placeholder;
        let stashed = vec![invoked("s", "x", vec![ok(), StepOutcome::Ok(placeholder)])];
        assert!(already_invoked(&stashed, &directive("s", "x"), |_| true));
        assert!(!already_invoked(&stashed, &directive("s", "x"), |_| false));
    }

    #[test]
    fn the_same_call_is_caught_whatever_the_ids_spelling() {
        let plans = vec![record(step("mail.get_message", json!({"message_id": "38996"})), true)];
        assert!(made(&plans, &step("mail.get_message", json!({"message_id": 38996}))));
        assert!(made(&plans, &step("mail.get_message", json!({"message_id": "38996"}))));
    }

    #[test]
    fn a_different_page_message_or_method_is_a_new_call() {
        let first = json!({"message_id": 38996, "filename": "a.pdf"});
        let plans = vec![record(step("mail.get_attachment_text", first.clone()), true)];
        let mut next_page = first.clone();
        next_page["offset"] = json!(8000);
        assert!(!made(&plans, &step("mail.get_attachment_text", next_page)));
        assert!(!made(&plans, &step("mail.get_attachment_text", json!({"message_id": 38983, "filename": "a.pdf"}))));
        assert!(!made(&plans, &step("mail.get_message", first)));
    }

    #[test]
    fn a_call_that_never_ran_does_not_count() {
        let plans = vec![record(step("mail.get_message", json!({"message_id": 1})), false)];
        assert!(!made(&plans, &step("mail.get_message", json!({"message_id": 1}))));
    }

    #[test]
    fn a_call_that_failed_may_be_retried() {
        let failed = StepOutcome::Err { code: "TIMEOUT".into(), detail: "worker timed out".into() };
        let plans = vec![record_with(step("mail.get_message", json!({"message_id": 1})), vec![failed])];
        assert!(!made(&plans, &step("mail.get_message", json!({"message_id": 1}))));
    }

    /// A sift's reads were never shown to the planner (it saw only the
    /// sift's counts), so reading one of those items again is not a repeat.
    #[test]
    fn a_read_a_sift_made_is_not_a_repeat() {
        let read = step("mail.get_message", json!({"message_id": 1}));
        let mut r = record(read.clone(), true);
        r.plan.decision = format!("{}an invoice", crate::scheduler::notebook::sift::SIFT_READS_DECISION);
        assert!(!made(&[r], &read));
    }

    /// #964: a stashed result the planner saw only as its placeholder is a
    /// repeat while the stash is held (a sift can read it), and not once it
    /// is gone (purged at a suspend): then the call is the way back to it.
    #[test]
    fn a_stashed_call_is_a_repeat_only_while_its_stash_is_held() {
        let cache = crate::handoff::HandoffCache::new();
        let search = step("mail.search", json!({"query": "invoice"}));
        let placeholder = cache.stash_if_oversized(1, &json!({"results": []}), 0).expect("cap 0").placeholder;
        let plans = vec![record_with(search.clone(), vec![StepOutcome::Ok(placeholder)])];
        assert!(already_made(&plans, &search, |_| true), "held");
        assert!(!already_made(&plans, &search, |_| false), "gone");
    }
}
