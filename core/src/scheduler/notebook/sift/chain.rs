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

use super::is_sift_plan;
use super::params::{parse_params, Over, SiftParams};
use super::SIFT_TOOL;
use crate::scheduler::inner_loop::{PlanRecord, StepOutcome};
use crate::scheduler::notebook::reply::notebook_step_of;

/// This run's sifts, newest first: each one's index in `plans`, its call's
/// parameters and its summary.
fn sifts(plans: &[PlanRecord]) -> impl Iterator<Item = (usize, &Value, &Value)> {
    plans.iter().enumerate().rev().filter_map(|(i, rec)| match (rec.plan.steps.first(), rec.outcomes().first()) {
        (Some(s), Some(StepOutcome::Ok(v))) if s.tool == SIFT_TOOL => Some((i, &s.parameters, v)),
        _ => None,
    })
}

/// The summaries this run's sifts left, newest first.
pub(crate) fn summaries(plans: &[PlanRecord]) -> impl Iterator<Item = &Value> {
    sifts(plans).map(|(_, _, s)| s)
}

/// The summary this run's sift `name` left, if it ran in this run.
pub(crate) fn summary_of<'a>(plans: &'a [PlanRecord], name: &str) -> Option<&'a Value> {
    sifts(plans).find(|(_, _, s)| s["scrapbook"] == name).map(|(_, _, s)| s)
}

/// The parameters, as parsed, of this run's sift `name`, if it ran in this run.
pub(crate) fn params_of(plans: &[PlanRecord], name: &str) -> Option<SiftParams> {
    sifts(plans).find(|(_, _, s)| s["scrapbook"] == name).and_then(|(_, p, _)| parse_params(p).ok())
}

/// The index in `plans` of the record whose result is notebook step `n`'s
/// list, which a sift `over: {"step": n}` goes through: the newest call made
/// for that step that is not a sift's and whose every step succeeded (a plan
/// stops at its first error, so a last `Ok` means all did).
pub(crate) fn list_index(plans: &[PlanRecord], n: u32) -> Option<usize> {
    plans.iter().rposition(|rec| {
        notebook_step_of(&rec.plan) == Some(n)
            && !is_sift_plan(&rec.plan)
            && matches!(rec.outcomes().last(), Some(StepOutcome::Ok(_)))
    })
}

/// The scrapbook a sift with `params`, starting at `from`, continues: the
/// newest earlier sift of this run whose parameters, as parsed, are the
/// same but for `from` (so `"items": null` and no `items` are one), whose
/// `next_from` is this one's `from`, and that went over the list this one
/// goes over. Anything else — a new criterion, another list, a sift back
/// over items already seen — is a sift of its own.
///
/// "The same list" (#959): `over: {"step": n}` is step n's *newest* result,
/// so if step n succeeded again after the earlier sift ran, this sift goes
/// over that new list and linking it would join rows of two lists in one
/// table. A scrapbook's rows are written once, so it is always the same list.
pub(crate) fn continued(plans: &[PlanRecord], params: &Value, from: usize) -> Option<String> {
    if from == 0 {
        return None;
    }
    let mine = but_from(params)?;
    let same_list = |sift: usize| match &mine.over {
        Over::Step(n) => list_index(plans, *n).is_some_and(|list| list < sift),
        Over::Scrapbook(_) => true,
    };
    let (_, _, prev) = sifts(plans).find(|(i, p, s)| {
        s["next_from"].as_u64() == Some(from as u64) && but_from(p).as_ref() == Some(&mine) && same_list(*i)
    })?;
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
    use crate::scheduler::notebook::reply::NOTEBOOK_STEP_RATIONALE;

    pub(crate) fn sift_record(params: Value, summary: Value) -> PlanRecord {
        record(SIFT_TOOL, String::new(), params, StepOutcome::Ok(summary))
    }

    /// The call that produced notebook step `step`'s list, with `outcome`.
    fn search_record(step: u32, outcome: StepOutcome) -> PlanRecord {
        record("web-search", format!("{NOTEBOOK_STEP_RATIONALE}{step}"), json!({"q": "invoices"}), outcome)
    }

    fn record(tool: &str, rationale: String, params: Value, outcome: StepOutcome) -> PlanRecord {
        let step = PlannedStep {
            tool: tool.into(),
            method: "sift.run".into(),
            parameters: params,
            returns: "kept".into(),
            done_when: "sorted".into(),
            classification: DataClass::Personal,
        };
        let plan = Plan {
            context: String::new(),
            decision: "kept".into(),
            rationale,
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
        PlanRecord::new(plan, vec![outcome])
    }

    pub(crate) fn params(from: usize, keep_if: &str) -> Value {
        json!({"over": {"step": 1}, "keep_if": keep_if, "fields": ["amount"], "from": from})
    }

    /// Step 1's list, then three sifts down it: 0..30, 30..60, 60.. Each
    /// sift is a call for step 1 too, as when the planner sifts its own
    /// step's list, so a sift's record must never count as step 1's list.
    fn chain_of_three() -> Vec<PlanRecord> {
        let sift = |p, s| record(SIFT_TOOL, format!("{NOTEBOOK_STEP_RATIONALE}1"), p, StepOutcome::Ok(s));
        vec![
            search_record(1, StepOutcome::Ok(json!([]))),
            sift(params(0, "an invoice"), json!({"scrapbook": "sb:9:2", "next_from": 30})),
            sift(params(30, "an invoice"), json!({"scrapbook": "sb:9:3", "next_from": 60, "continues": "sb:9:2"})),
            sift(params(60, "an invoice"), json!({"scrapbook": "sb:9:4", "continues": "sb:9:3"})),
        ]
    }

    #[test]
    fn a_sift_from_the_last_ones_next_from_continues_it() {
        let plans = &chain_of_three()[..2];
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
        let plans = &chain_of_three()[..2];
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

    /// #959: step 1 run again between two sifts is another list, so a sift
    /// `from` the first one's `next_from` goes over items it never saw and
    /// continues nothing; a re-run that failed left the list as it was.
    #[test]
    fn a_rerun_of_the_listed_step_between_sifts_ends_the_chain() {
        let mut plans = chain_of_three();
        plans.truncate(2);
        plans.push(search_record(1, StepOutcome::Err { code: "UPSTREAM".into(), detail: "down".into() }));
        assert_eq!(continued(&plans, &params(30, "an invoice"), 30).as_deref(), Some("sb:9:2"), "a failed re-run");
        plans.push(search_record(1, StepOutcome::Ok(json!(["another list"]))));
        assert_eq!(continued(&plans, &params(30, "an invoice"), 30), None, "a new list");
        let over_sb = |from| json!({"over": {"scrapbook": "sb:9:2"}, "keep_if": "x", "fields": ["amount"], "from": from});
        plans.push(sift_record(over_sb(0), json!({"scrapbook": "sb:9:5", "next_from": 30})));
        plans.push(search_record(1, StepOutcome::Ok(json!([]))));
        assert_eq!(continued(&plans, &over_sb(30), 30).as_deref(), Some("sb:9:5"), "a scrapbook's rows never change");
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
        plans[1] = sift_record(params(0, "an invoice"), json!({"scrapbook": "sb:9:2", "next_from": 30, "looked_at": 30, "kept": 4}));
        plans[2] = sift_record(
            params(30, "an invoice"),
            json!({"scrapbook": "sb:9:3", "next_from": 60, "continues": "sb:9:2", "looked_at": 30, "kept": 5}),
        );
        let plans = &plans[..3];
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
