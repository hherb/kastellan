//! The answer's table (#922, S5, K8, K10): a scrapbook rendered by code,
//! never retyped by the model. Pure.

use kastellan_db::scrapbook::{Row, RowKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::scheduler::inner_loop::{screen_model_text, SinkBlock, WITHHELD_MARKER};

/// The column that shows where a row came from (its `source`), not a field.
pub(crate) const SOURCE_COLUMN: &str = "source";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OrderBy {
    pub column: String,
    #[serde(default)]
    pub desc: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    pub looked_at: usize,
    pub kept: usize,
    pub withheld: usize,
    pub undecided: usize,
    pub failed_reads: usize,
    /// Items the sift never reached (a cap, the deadline, a sorter failure).
    pub remaining: usize,
}

/// A markdown table of `rows` under `columns`, every row (S5), optionally
/// ordered by one column, then a note with the counts. Every cell is
/// model-written from tool output, so each is screened (K8): a blocked one —
/// and every cell of a withheld row — shows [`WITHHELD_MARKER`], and its
/// block is returned for the audit row.
pub(crate) fn render_table(
    rows: &[Row],
    columns: &[String],
    order_by: Option<&OrderBy>,
    counts: &Counts,
) -> (String, Vec<SinkBlock>) {
    let mut ordered: Vec<&Row> = rows.iter().collect();
    if let Some(o) = order_by {
        ordered.sort_by(|a, b| cell_order(&cell_text(a, &o.column), &cell_text(b, &o.column), o.desc));
    }
    let line = |cells: Vec<String>| format!("| {} |", cells.join(" | "));
    let mut blocks = Vec::new();
    let mut out = vec![
        line(columns.iter().map(|c| escaped(c)).collect()),
        line(columns.iter().map(|_| "---".into()).collect()),
    ];
    for row in ordered {
        out.push(line(columns.iter().map(|c| shown(row, c, &mut blocks)).collect()));
    }
    (format!("{}\n\n{}", out.join("\n"), note(counts)), blocks)
}

/// The order of two cells: every cell that reads as a number before every
/// one that does not — numbers by value, the rest as text — and `desc`
/// reverses each group, never the groups. A total order: mixing the two
/// comparisons pairwise is not one, and `sort_by` may panic on that.
fn cell_order(a: &str, b: &str, desc: bool) -> std::cmp::Ordering {
    let (x, y) = (as_number(a), as_number(b));
    let within = match (x, y) {
        (Some(x), Some(y)) => x.total_cmp(&y),
        _ => a.cmp(b),
    };
    x.is_none().cmp(&y.is_none()).then(if desc { within.reverse() } else { within })
}

/// A cell as shown: screened, then escaped so it cannot end the cell or row.
fn shown(row: &Row, column: &str, blocks: &mut Vec<SinkBlock>) -> String {
    if row.kind == RowKind::Withheld {
        return WITHHELD_MARKER.into();
    }
    let text = cell_text(row, column);
    if let Some(block) = screen_model_text(&text) {
        blocks.push(block);
        return WITHHELD_MARKER.into();
    }
    escaped(&text)
}

/// `text` that cannot end a markdown cell or row.
fn escaped(text: &str) -> String {
    text.replace('|', "\\|").replace(['\n', '\r'], " ")
}

/// A cell's raw text: the field's value, or for [`SOURCE_COLUMN`] the
/// row's source (an object's values, comma-separated).
fn cell_text(row: &Row, column: &str) -> String {
    if column == SOURCE_COLUMN {
        return match &row.source {
            Value::Object(o) => o.values().map(scalar).collect::<Vec<_>>().join(", "),
            other => scalar(other),
        };
    }
    row.fields.get(column).map(scalar).unwrap_or_default()
}

/// A cell read as a number, so `order_by` puts 100 after 30: the cell
/// with any currency or unit around it trimmed off and thousands separators
/// dropped (`$1,200.00` is 1200, `30 EUR` is 30). `None` when what is left is
/// not one number — `3 of 10`, a range, an ISO date — which then sorts as text.
fn as_number(s: &str) -> Option<f64> {
    let core = s.trim_matches(|c: char| !(c.is_ascii_digit() || c == '-' || c == '.'));
    if !core.bytes().any(|b| b.is_ascii_digit()) {
        return None;
    }
    core.replace(',', "").parse().ok()
}

fn scalar(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn note(c: &Counts) -> String {
    let mut parts = vec![format!("{} kept of {} looked at", c.kept, c.looked_at)];
    if c.withheld > 0 {
        parts.push(format!("{} withheld by the safety screen", c.withheld));
    }
    if c.failed_reads > 0 {
        parts.push(format!("{} could not be read", c.failed_reads));
    }
    if c.undecided > 0 {
        parts.push(format!("{} undecided", c.undecided));
    }
    if c.remaining > 0 {
        parts.push(format!("{} not yet looked at", c.remaining));
    }
    format!("{}.", parts.join("; "))
}

#[cfg(test)]
mod tests;
