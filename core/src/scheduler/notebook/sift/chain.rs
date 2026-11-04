//! A chain of sifts (#922): one sift looks at a limited number of items, so
//! a long list takes several, each `from` the last one's `next_from`, and
//! each writes its own scrapbook. A sift that goes on where an earlier one
//! stopped **continues** it: its summary says `continues: <that ref>`, and
//! an answer's table in the same task that names the last one shows the
//! whole chain (a later turn's table sees only the sift it names). DGX
//! tasks 302 and 307 tabled only the last of four and three sifts (9 of
//! about 86 rows, and 16 of 52) although the prompt asked for every name.
//! Pure.

use serde_json::{json, Value};

use super::params::{parse_params, SiftParams};
use super::SIFT_TOOL;
use crate::scheduler::inner_loop::{PlanRecord, StepOutcome};

/// This run's sifts, newest first: each call's parameters and its summary.
fn sifts(plans: &[PlanRecord]) -> impl Iterator<Item = (&Value, &Value)> {
    plans.iter().rev().filter_map(|rec| match (rec.plan.steps.first(), rec.outcomes().first()) {
        (Some(s), Some(StepOutcome::Ok(v))) if s.tool == SIFT_TOOL => Some((&s.parameters, v)),
        _ => None,
    })
}

/// The summary this run's newest sift left, if one ran in this run.
pub(crate) fn last_sift(plans: &[PlanRecord]) -> Option<&Value> {
    sifts(plans).next().map(|(_, s)| s)
}

/// The summary this run's sift `name` left, if it ran in this run.
pub(crate) fn summary_of<'a>(plans: &'a [PlanRecord], name: &str) -> Option<&'a Value> {
    sifts(plans).find(|(_, s)| s["scrapbook"] == name).map(|(_, s)| s)
}

/// The scrapbook a sift with `params`, starting at `from`, continues: the
/// newest earlier sift of this run whose parameters, as parsed, are the
/// same but for `from` (so `"items": null` and no `items` are one) and
/// whose `next_from` is this one's `from`. Anything else — a new criterion,
/// another list, a sift back over items already seen — is a sift of its own.
pub(crate) fn continued(plans: &[PlanRecord], params: &Value, from: usize) -> Option<String> {
    if from == 0 {
        return None;
    }
    let mine = but_from(params)?;
    let (_, prev) = sifts(plans)
        .find(|(p, s)| s["next_from"].as_u64() == Some(from as u64) && but_from(p).as_ref() == Some(&mine))?;
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

/// The totals of the chain that ends with a sift continuing `prev`, which
/// looked at `looked_at` items and kept `kept`: every link's counts plus
/// its own, so the planner can quote the whole chain. DGX tasks 308 and 309
/// quoted the last link (25 and 15) above a table of the chain (59 and 39).
/// `distinct` is the chain's table rows, counted by the caller over every
/// link's rows (a duplicate can sit in two links); `None` when they could not
/// all be read, and then it is left out.
pub(crate) fn totals(plans: &[PlanRecord], prev: &str, looked_at: usize, kept: usize, distinct: Option<usize>) -> Value {
    let links: Vec<&Value> = expand(plans, prev).iter().filter_map(|n| summary_of(plans, n)).collect();
    let sum = |k: &str| links.iter().filter_map(|s| s[k].as_u64()).sum::<u64>();
    let mut t = json!({
        "sifts": links.len() + 1,
        "looked_at": sum("looked_at") + looked_at as u64,
        "kept": sum("kept") + kept as u64,
    });
    if let Some(d) = distinct {
        t["distinct"] = json!(d);
    }
    t
}

/// `params` parsed, with `from` cleared (a copied-back `continues` is
/// ignored by the parse, [`super::params`]); `None` if they do not parse.
fn but_from(params: &Value) -> Option<SiftParams> {
    parse_params(params).ok().map(|p| SiftParams { from: 0, ..p })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::cassandra::types::{DataClass, Plan, PlannedStep};

    pub(crate) fn sift_record(params: Value, summary: Value) -> PlanRecord {
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

    pub(crate) fn params(from: usize, keep_if: &str) -> Value {
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
        let mut spelled = params(30, "an invoice");
        spelled["items"] = Value::Null;
        assert_eq!(continued(plans, &spelled, 30).as_deref(), Some("sb:9:2"), "review finding: `items: null` is no `items`");
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

    /// DGX tasks 308/309: the answer quoted the last link's `kept` (25, 15)
    /// above a table of the whole chain (59, 39). The chain's totals are
    /// every link's counts plus this sift's own.
    #[test]
    fn totals_add_every_link_and_this_sift() {
        let mut plans = chain_of_three();
        plans[0] = sift_record(params(0, "an invoice"), json!({"scrapbook": "sb:9:2", "next_from": 30, "looked_at": 30, "kept": 4}));
        plans[1] = sift_record(
            params(30, "an invoice"),
            json!({"scrapbook": "sb:9:3", "next_from": 60, "continues": "sb:9:2", "looked_at": 30, "kept": 5}),
        );
        let plans = &plans[..2];
        assert_eq!(totals(plans, "sb:9:3", 12, 3, Some(10)), json!({"sifts": 3, "looked_at": 72, "kept": 12, "distinct": 10}));
        assert_eq!(totals(plans, "sb:9:2", 30, 5, None), json!({"sifts": 2, "looked_at": 60, "kept": 9}), "a two-link chain, rows unread");
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
