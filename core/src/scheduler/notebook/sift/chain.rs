//! A chain of sifts (#922): one sift looks at a limited number of items, so
//! a long list takes several, each `from` the last one's `next_from`, and
//! each writes its own scrapbook. A sift that goes on where an earlier one
//! stopped **continues** it: its summary says `continues: <that ref>`, and
//! an answer's table that names the last one shows the whole chain. DGX
//! tasks 302 and 307 tabled only the last of four and three sifts (9 of
//! about 86 rows, and 16 of 52) although the prompt asked for every name.
//! Pure.

use serde_json::Value;

use super::SIFT_TOOL;
use crate::scheduler::inner_loop::{PlanRecord, StepOutcome};

/// This run's sifts, newest first: each call's parameters and its summary.
fn sifts(plans: &[PlanRecord]) -> impl Iterator<Item = (&Value, &Value)> {
    plans.iter().rev().filter_map(|rec| match (rec.plan.steps.first(), rec.outcomes().first()) {
        (Some(s), Some(StepOutcome::Ok(v))) if s.tool == SIFT_TOOL => Some((&s.parameters, v)),
        _ => None,
    })
}

/// The summary this run's sift `name` left, if it ran in this run.
pub(crate) fn summary_of<'a>(plans: &'a [PlanRecord], name: &str) -> Option<&'a Value> {
    sifts(plans).find(|(_, s)| s["scrapbook"] == name).map(|(_, s)| s)
}

/// The scrapbook a sift with `params`, starting at `from`, continues: the
/// newest earlier sift of this run whose parameters are the same but for
/// `from` and whose `next_from` is this one's `from`. Anything else — a
/// new criterion, another list, a sift back over items already seen — is a
/// sift of its own.
pub(crate) fn continued(plans: &[PlanRecord], params: &Value, from: usize) -> Option<String> {
    if from == 0 {
        return None;
    }
    let (_, prev) = sifts(plans)
        .find(|(p, s)| s["next_from"].as_u64() == Some(from as u64) && but_from(p) == but_from(params))?;
    prev["scrapbook"].as_str().map(String::from)
}

/// `name` and the sifts it continues, oldest first.
pub(crate) fn expand(plans: &[PlanRecord], name: &str) -> Vec<String> {
    let mut chain = vec![name.to_string()];
    while let Some(prev) = summary_of(plans, chain.last().expect("never empty")).and_then(|s| s["continues"].as_str()) {
        // Code writes the links, each to an earlier sift, so they cannot
        // loop; never spin if one somehow did.
        if chain.iter().any(|c| c == prev) {
            break;
        }
        chain.push(prev.to_string());
    }
    chain.reverse();
    chain
}

/// `params` without `from`, and without a `continues` the model copied
/// back from a summary (ignored, [`super::params`]).
fn but_from(params: &Value) -> Value {
    let mut p = params.clone();
    if let Some(o) = p.as_object_mut() {
        o.remove("from");
        o.remove("continues");
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cassandra::types::{DataClass, Plan, PlannedStep};
    use serde_json::json;

    fn sift_record(params: Value, summary: Value) -> PlanRecord {
        let step = PlannedStep {
            tool: SIFT_TOOL.into(),
            method: "sift.run".into(),
            parameters: params,
            returns: "kept".into(),
            done_when: "sorted".into(),
            classification: DataClass::Personal,
        };
        let plan = Plan {
            context: String::new(),
            decision: "kept".into(),
            rationale: String::new(),
            steps: vec![step],
            result: None,
            data_ceiling: None,
            refused: None,
            floor_request: None,
            l1_insight: None,
            l3_skill: None,
            invoke_skill: None,
            python_skill: None,
        };
        PlanRecord::new(plan, vec![StepOutcome::Ok(summary)])
    }

    fn params(from: usize, keep_if: &str) -> Value {
        json!({"over": {"step": 1}, "keep_if": keep_if, "fields": ["amount"], "from": from})
    }

    /// Three sifts down one list: 0..30, 30..60, 60..
    fn chain_of_three() -> Vec<PlanRecord> {
        vec![
            sift_record(params(0, "an invoice"), json!({"scrapbook": "sb:9:2", "next_from": 30})),
            sift_record(params(30, "an invoice"), json!({"scrapbook": "sb:9:3", "next_from": 60, "continues": "sb:9:2"})),
            sift_record(params(60, "an invoice"), json!({"scrapbook": "sb:9:4", "continues": "sb:9:3"})),
        ]
    }

    #[test]
    fn a_sift_from_the_last_ones_next_from_continues_it() {
        let plans = &chain_of_three()[..1];
        assert_eq!(continued(plans, &params(30, "an invoice"), 30).as_deref(), Some("sb:9:2"));
        let mut copied = params(30, "an invoice");
        copied["continues"] = json!("sb:9:1");
        assert_eq!(continued(plans, &copied, 30).as_deref(), Some("sb:9:2"), "a copied `continues` is ignored");
    }

    #[test]
    fn a_new_criterion_another_start_or_the_first_sift_continues_nothing() {
        let plans = &chain_of_three()[..1];
        assert_eq!(continued(plans, &params(30, "a receipt"), 30), None, "another criterion");
        assert_eq!(continued(plans, &params(20, "an invoice"), 20), None, "not where the last one stopped");
        assert_eq!(continued(plans, &params(0, "an invoice"), 0), None, "from the start");
        let other_list = json!({"over": {"step": 3}, "keep_if": "an invoice", "fields": ["amount"], "from": 30});
        assert_eq!(continued(plans, &other_list, 30), None, "another list");
    }

    /// Review finding: a re-run from 30 (C) must not hide A from a later D:
    /// the newest same-parameter sift is not always the one continued.
    #[test]
    fn a_rerun_does_not_hide_the_sift_continued() {
        let mut plans = chain_of_three();
        plans.pop();
        plans.push(sift_record(params(30, "an invoice"), json!({"scrapbook": "sb:9:4", "next_from": 60, "continues": "sb:9:2"})));
        assert_eq!(continued(&plans, &params(30, "an invoice"), 30).as_deref(), Some("sb:9:2"), "C continues A, not B");
        assert_eq!(continued(&plans, &params(60, "an invoice"), 60).as_deref(), Some("sb:9:4"), "D, the newest at 60");
    }

    #[test]
    fn the_last_name_expands_to_the_whole_chain_oldest_first() {
        let plans = chain_of_three();
        assert_eq!(expand(&plans, "sb:9:4"), ["sb:9:2", "sb:9:3", "sb:9:4"]);
        assert_eq!(expand(&plans, "sb:9:2"), ["sb:9:2"], "the first link continues nothing");
        assert_eq!(expand(&plans, "sb:7:1"), ["sb:7:1"], "a ref this run did not write stands alone");
    }

    #[test]
    fn a_link_loop_ends() {
        let plans = vec![
            sift_record(params(0, "x"), json!({"scrapbook": "sb:9:2", "continues": "sb:9:3"})),
            sift_record(params(0, "x"), json!({"scrapbook": "sb:9:3", "continues": "sb:9:2"})),
        ];
        assert_eq!(expand(&plans, "sb:9:3"), ["sb:9:2", "sb:9:3"]);
    }
}
