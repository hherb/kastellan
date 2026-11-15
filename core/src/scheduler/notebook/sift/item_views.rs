//! What the sorter is shown for one item of a sift (#922): the item's hit,
//! then each of its reads, each rendered through the sorter's own screen —
//! and which of its reads failed, and why (#956).

use std::collections::BTreeMap;
use std::ops::Range;

use serde_json::Value;

use crate::handoff;
use crate::scheduler::inner_loop::{sorter_view, SinkBlock, StepDispatcher, StepOutcome};

/// The code a stashed read is given when it is no longer held: the same as
/// a `handoff.fetch` of a ref the cache has dropped.
pub(crate) const EVICTED_READ_CODE: &str = "HANDOFF_NOT_FOUND";

/// The reads a sift made: each one's outcome and the tool it was made with
/// (the tool selects its screen's profile, and reads may differ).
pub(crate) struct ReadsMade {
    pub outcomes: Vec<StepOutcome>,
    pub tools: Vec<String>,
}

/// One item, ready for the sorter.
pub(crate) struct ItemViews {
    /// The hit's view, then each read's that rendered.
    pub views: Vec<Value>,
    /// What the screen blocked: if any, the item is withheld, never sorted.
    pub blocks: Vec<SinkBlock>,
    /// The error code of each of its reads that failed.
    pub failed: Vec<String>,
}

impl ReadsMade {
    /// Did every read fail? `false` when no read was made. When it is `true`,
    /// sorting each item on its hit alone would read as a real answer (#956).
    pub(crate) fn all_failed(&self) -> bool {
        !self.outcomes.is_empty() && self.outcomes.iter().all(StepOutcome::is_err)
    }

    /// The codes of every failed read, counted.
    pub(crate) fn error_codes(&self) -> BTreeMap<String, usize> {
        let mut codes = BTreeMap::new();
        for o in &self.outcomes {
            if let StepOutcome::Err { code, .. } = o {
                *codes.entry(code.clone()).or_default() += 1;
            }
        }
        codes
    }

    /// `item` (of `item_tool`'s output) and its reads `reads`, each viewed at
    /// `cap`. A read over the dispatch cap came back as its handoff
    /// placeholder (#968): it is read back whole from `dispatcher`, so the
    /// sorter sees it rendered at its cap, as for any long read. One evicted
    /// since (past the task's stash budget) is a failed read: sorting the
    /// placeholder's head as the read would be a silent wrong "no".
    pub(crate) fn item_views(
        &self,
        dispatcher: &dyn StepDispatcher,
        task_id: i64,
        item_tool: &str,
        item: &Value,
        reads: Range<usize>,
        cap: usize,
    ) -> ItemViews {
        let mut out = ItemViews { views: Vec::new(), blocks: Vec::new(), failed: Vec::new() };
        let view = |out: &mut ItemViews, tool: &str, v: &Value| match sorter_view(tool, v, cap) {
            Ok(v) => out.views.push(v),
            Err(b) => out.blocks.push(b),
        };
        view(&mut out, item_tool, item);
        for i in reads {
            match (self.outcomes.get(i), self.tools.get(i)) {
                (Some(StepOutcome::Ok(v)), Some(tool)) => match handoff::placeholder_ref(v) {
                    None => view(&mut out, tool, v),
                    Some(h) => match dispatcher.stashed(task_id, &h) {
                        Some(whole) => view(&mut out, tool, &whole),
                        None => {
                            tracing::warn!(task_id, handoff_ref = h.as_str(), "sift: a stashed read is no longer held; counted as a failed read");
                            out.failed.push(EVICTED_READ_CODE.into());
                        }
                    },
                },
                (Some(StepOutcome::Err { code, .. }), _) => out.failed.push(code.clone()),
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn made(outcomes: Vec<StepOutcome>) -> ReadsMade {
        let tools = vec!["mail".to_string(); outcomes.len()];
        ReadsMade { outcomes, tools }
    }

    fn err(code: &str) -> StepOutcome {
        StepOutcome::Err { code: code.into(), detail: "d".into() }
    }

    #[test]
    fn all_failed_needs_a_read_and_every_one_failed() {
        assert!(!made(vec![]).all_failed(), "no read made is not every read failed");
        assert!(!made(vec![err("A"), StepOutcome::Ok(json!({}))]).all_failed());
        assert!(made(vec![err("A"), err("B")]).all_failed());
    }

    #[test]
    fn error_codes_count_each_failed_read_by_its_code() {
        let m = made(vec![err("B"), StepOutcome::Ok(json!({})), err("A"), err("B")]);
        assert_eq!(m.error_codes(), BTreeMap::from([("A".to_string(), 1), ("B".to_string(), 2)]));
    }
}
