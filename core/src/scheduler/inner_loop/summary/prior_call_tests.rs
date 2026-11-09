//! The classic plan summary's budget over the calls it shows beside each
//! outcome (#699): [`super::render_plans_summary`] drops old outputs before
//! any call, and a call's `parameters` only when the calls alone overrun it.
//! Classic-only (#993): the step view itself is pinned in `step_view_tests.rs`,
//! and the renders in `call/tests.rs`.

use super::tests::plan_calling;
use super::*;

/// A one-step plan calling `mail.search` with `parameters`, deciding `decision`.
fn search_plan(decision: &str, parameters: serde_json::Value) -> Plan {
    let mut plan = plan_calling("mail", "mail.search");
    plan.decision = decision.into();
    plan.steps[0].parameters = parameters;
    plan
}

/// The budget drops an old output, never the call: what the planner asked for
/// is what stops it asking again. Sixty plans, because outputs are first shrunk
/// evenly (#921) and dropped only once a share would fall below the floor.
#[test]
fn an_elided_output_keeps_its_call() {
    let prose = "lorem ipsum dolor sit amet ".repeat(1_200);
    // More outputs than the budget has floor-sized shares for, so the oldest
    // must be dropped whole rather than shrunk.
    let n = PLANS_SUMMARY_BUDGET / budget::MIN_SHRUNK_OUTPUT_BYTES + 12;
    let plans: Vec<PlanRecord> = (0..n)
        .map(|i| PlanRecord::new(search_plan("d", json!({"query": format!("q{i}")})), vec![StepOutcome::Ok(json!({"text": prose}))]))
        .collect();
    let first = &render_plans_summary(&plans)[0]["step_outcomes"][0];
    assert_eq!(first[ELIDED_KEY], ELIDED_OUTPUT_REASON, "control: the oldest output must be elided: {first}");
    assert_eq!(first[CALL_KEY]["parameters"]["query"], "q0");
}

/// The calls count against the accumulated budget. Without that, forty plans
/// with a full-size call each overran it by the calls' bytes.
#[test]
fn the_summary_budget_counts_the_calls() {
    let prose = "lorem ipsum dolor sit amet ".repeat(1_200);
    let params = json!({"query": "word ".repeat(400)});
    let plans: Vec<PlanRecord> = (0..40)
        .map(|_| PlanRecord::new(search_plan("d", params.clone()), vec![StepOutcome::Ok(json!({"text": prose}))]))
        .collect();
    let summary = render_plans_summary(&plans);
    let bytes: usize = summary.iter().flat_map(|p| p["step_outcomes"].as_array().unwrap()).map(|o| o.to_string().len()).sum();
    let call_bytes: usize = summary
        .iter()
        .flat_map(|p| p["step_outcomes"].as_array().unwrap())
        .map(|o| o[CALL_KEY].to_string().len())
        .sum();
    assert!(call_bytes > 16 * 1024, "control: the calls must be big enough to matter ({call_bytes})");
    assert!(bytes <= PLANS_SUMMARY_BUDGET, "{bytes} over {PLANS_SUMMARY_BUDGET}");

    // Fitting is not enough: the second pass could reach the same total by
    // dropping the calls' parameters while older outputs survived, which is the
    // order this budget exists to avoid. Outputs go first, so every call here
    // must still be whole.
    for (i, step) in summary.iter().flat_map(|p| p["step_outcomes"].as_array().unwrap()).enumerate() {
        let call = &step[CALL_KEY];
        assert!(call.get(ELIDED_KEY).is_none() && call["parameters"].is_object(), "call {i} paid for an output that survived: {call}");
    }
}

/// Found by review: calls were never elided, so enough steps with near-cap
/// parameters overran the budget on their own (64 steps × ~1 KiB per plan, a
/// few plans). Once the outputs are gone, the oldest calls lose their
/// `parameters` too, keeping `tool` and `method`, until the summary fits.
#[test]
fn the_summary_budget_holds_when_the_calls_alone_exceed_it() {
    let params = json!({"query": "word ".repeat(400)});
    let plans: Vec<PlanRecord> = (0..3)
        .map(|_| {
            let mut plan = search_plan("d", params.clone());
            plan.steps = vec![plan.steps[0].clone(); 64];
            PlanRecord::new(plan, vec![StepOutcome::Ok(json!({"results": []})); 64])
        })
        .collect();
    let summary = render_plans_summary(&plans);
    let steps: Vec<&serde_json::Value> = summary.iter().flat_map(|p| p["step_outcomes"].as_array().unwrap()).collect();
    let bytes: usize = steps.iter().map(|o| o.to_string().len()).sum();
    assert!(bytes <= PLANS_SUMMARY_BUDGET, "{bytes} over {PLANS_SUMMARY_BUDGET}");

    let oldest = &steps[0][CALL_KEY];
    assert_eq!(oldest[ELIDED_KEY], ELIDED_REASON, "the oldest call kept its parameters: {oldest}");
    assert_eq!((oldest["tool"].as_str(), oldest["method"].as_str()), (Some("mail"), Some("mail.search")));
    assert!(oldest.get("parameters").is_none(), "{oldest}");
    let newest = &steps[steps.len() - 1][CALL_KEY];
    assert_eq!(newest["parameters"]["query"].as_str().map(|q| q.starts_with("word")), Some(true), "the newest call lost its parameters");
}
