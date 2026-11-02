//! The answer's table (#922, S5, K8, K10): a scrapbook rendered by code,
//! never retyped by the model. Pure.

use kastellan_db::scrapbook::{Row, RowKind};
use serde::Deserialize;
use serde_json::Value;

use crate::scheduler::inner_loop::{screen_model_text, WITHHELD_MARKER};

/// The column that shows where a row came from (its `source`), not a field.
pub(crate) const SOURCE_COLUMN: &str = "source";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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
}

/// A markdown table of `rows` under `columns`, every row (S5), ordered by
/// one column as text (stable), then a note with the counts. Every cell is
/// model-written from tool output, so each is screened (K8): a blocked one —
/// and every cell of a withheld row — shows [`WITHHELD_MARKER`].
pub(crate) fn render_table(rows: &[Row], columns: &[String], order_by: Option<&OrderBy>, counts: &Counts) -> String {
    let mut ordered: Vec<&Row> = rows.iter().collect();
    if let Some(o) = order_by {
        ordered.sort_by(|a, b| {
            let (a, b) = (cell_text(a, &o.column), cell_text(b, &o.column));
            if o.desc { b.cmp(&a) } else { a.cmp(&b) }
        });
    }
    let line = |cells: Vec<String>| format!("| {} |", cells.join(" | "));
    let mut out = vec![line(columns.to_vec()), line(columns.iter().map(|_| "---".into()).collect())];
    for row in ordered {
        out.push(line(columns.iter().map(|c| shown(row, c)).collect()));
    }
    format!("{}\n\n{}", out.join("\n"), note(counts))
}

/// A cell as shown: screened, then escaped so it cannot end the cell or row.
fn shown(row: &Row, column: &str) -> String {
    if row.kind == RowKind::Withheld {
        return WITHHELD_MARKER.into();
    }
    let text = cell_text(row, column);
    if screen_model_text(&text).is_some() {
        return WITHHELD_MARKER.into();
    }
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
    format!("{}.", parts.join("; "))
}

#[cfg(test)]
mod tests;
