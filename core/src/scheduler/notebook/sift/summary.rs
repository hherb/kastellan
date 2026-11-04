//! The summary a sift leaves for the planner (#922, K9), and why a sift
//! stopped early. Pure.

use kastellan_db::scrapbook::{NewRow, RowKind};
use serde_json::{json, Value};

use super::params::{ScrapRef, SiftParams};
use super::table::{distinct_kept, Counts};
use super::SIFT_TOOL;
use crate::scheduler::inner_loop::{screen_model_text, sorter_view, SinkBlock, WITHHELD_MARKER};

/// How many kept rows the planner is shown, so it can judge whether the
/// criterion worked (K9).
pub(crate) const SAMPLE_ROWS: usize = 3;

/// Why a sift stopped before the end of its list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stop {
    /// The fast lane's deadline passed.
    Time,
    /// The operator cancelled the task.
    Cancelled,
    /// The sorter could not be reached.
    SorterFailed,
    /// The reads plan was blocked, escalated or ended the task: nothing was
    /// sorted (the audit row only; the loop records the plan's own outcome).
    ReadsNotRun,
}

impl Stop {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Stop::Time => "time",
            Stop::Cancelled => "cancelled",
            Stop::SorterFailed => "sorter_failed",
            Stop::ReadsNotRun => "reads_not_run",
        }
    }
}

/// How many reads the sift made, how many items it looked at for which no
/// read was made (a placeholder named no field of the item, or a `*` list
/// held nothing readable), and how many of the first item's reads were cut
/// to fit the cap. `per_read[i]`: the items looked at whose reads came from
/// the sift's read `i` — a read before the one applied fell through, so a
/// misspelled placeholder shows as a read used for no item (review finding).
pub(crate) struct Reads {
    pub made: usize,
    pub unread: usize,
    pub cut: usize,
    pub per_read: Vec<usize>,
}

/// What the planner is shown (K9): the ref, the criterion, the counts — with
/// `distinct`, the kept rows a table shows ([`distinct_kept`]) — and a
/// sample of what was kept. The sample's values are model-written from tool
/// output, so each is screened (shown as [`WITHHELD_MARKER`] if blocked), and
/// if the whole summary would still fail the record's screen, the
/// model-written parts go (`sample_withheld`): the ref and the counts must
/// always get through, or the planner can neither show the sift nor run it
/// again. Every block is returned for its audit row.
pub(crate) fn summary(
    scrap: &ScrapRef,
    p: &SiftParams,
    c: &Counts,
    reads: &Reads,
    rows: &[NewRow],
    remaining: usize,
    stopped: Option<Stop>,
) -> (Value, Vec<SinkBlock>) {
    let mut blocks = Vec::new();
    let sample: Vec<Value> = rows
        .iter()
        .filter(|r| r.kind == RowKind::Kept)
        .take(SAMPLE_ROWS)
        .map(|r| json!({"fields": screened(&r.fields, &mut blocks), "source": screened(&r.source, &mut blocks)}))
        .collect();
    let mut s = json!({
        "scrapbook": scrap.to_string(), "criterion": p.keep_if, "fields": p.fields,
        "looked_at": c.looked_at, "kept": c.kept, "withheld": c.withheld, "undecided": c.undecided,
        "failed_reads": c.failed_reads, "reads": reads.made, "unread": reads.unread,
        "distinct": distinct_kept(rows.iter().filter(|r| r.kind == RowKind::Kept).map(|r| &r.fields)),
        "remaining": remaining, "sample": sample,
    });
    if remaining > 0 {
        s["next_from"] = json!(p.from + c.looked_at);
    }
    if reads.per_read.len() > 1 {
        s["items_per_read"] = json!(reads.per_read);
    }
    if reads.cut > 0 {
        s["reads_cut"] = json!(reads.cut);
    }
    if let Some(why) = stopped {
        s["stopped"] = json!(why.as_str());
    }
    if let Err(block) = sorter_view(SIFT_TOOL, &s, SUMMARY_SCREEN_CAP) {
        blocks.push(block);
        if let Some(o) = s.as_object_mut() {
            for k in ["sample", "criterion", "fields"] {
                o.remove(k);
            }
        }
        s["sample_withheld"] = json!(true);
    }
    (s, blocks)
}

/// The cap the summary is screened at before it is recorded: above its size
/// (`keep_if` and the field names are bounded by [`parse_params`]).
const SUMMARY_SCREEN_CAP: usize = 64 * 1024;

/// `v` with every string in it shown as [`WITHHELD_MARKER`] when the
/// planner-text screen blocks it, the block kept in `blocks`.
fn screened(v: &Value, blocks: &mut Vec<SinkBlock>) -> Value {
    match v {
        Value::String(t) => match screen_model_text(t) {
            Some(b) => {
                blocks.push(b);
                json!(WITHHELD_MARKER)
            }
            None => v.clone(),
        },
        Value::Array(a) => Value::Array(a.iter().map(|x| screened(x, blocks)).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), screened(x, blocks))).collect()),
        other => other.clone(),
    }
}
