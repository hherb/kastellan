//! The answer's table (#922, S5, K8, K10): a scrapbook rendered by code,
//! never retyped by the model. Pure.

use kastellan_db::scrapbook::{NewRow, Row, RowKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::scheduler::inner_loop::{screen_model_text, SinkBlock, WITHHELD_MARKER};

/// The column that shows where a row came from (its `source`), not a field.
pub(crate) const SOURCE_COLUMN: &str = "source";

/// The key a bare-`id` source carries its tool under (#1022).
pub(crate) const SOURCE_TOOL_KEY: &str = "tool";

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

/// A markdown table of `rows` under `columns`, optionally ordered by one
/// column, then a note with the counts. Rows alike in every field are shown
/// once, their sources joined (one booking emailed three times, DGX task
/// 301); then `limit` keeps the first rows. Every row is
/// shown unless a limit says otherwise, and the note says so (S5). Every
/// cell is model-written from tool output, so each shown one is screened
/// (K8): a blocked one — and every cell of a withheld row — shows
/// [`WITHHELD_MARKER`], and its block is returned for the audit row.
pub(crate) fn render_table(
    rows: &[Row],
    columns: &[String],
    order_by: Option<&OrderBy>,
    limit: Option<usize>,
    counts: &Counts,
) -> (String, Vec<SinkBlock>) {
    let mut ordered: Vec<&Row> = rows.iter().collect();
    if let Some(o) = order_by {
        ordered.sort_by(|a, b| cell_order(&cell_text(a, &o.column), &cell_text(b, &o.column), o.desc));
    }
    let groups = alike(&ordered);
    let shown_rows = limit.filter(|n| *n > 0).map_or(groups.len(), |n| n.min(groups.len()));
    // Items merged, not rows: one message kept by three sifts is no duplicate (issue 1026).
    let merged = groups[..shown_rows].iter().map(|g| items(g.iter().copied()) - 1).sum();
    let line = |cells: Vec<String>| format!("| {} |", cells.join(" | "));
    let mut blocks = Vec::new();
    let mut out = vec![
        line(columns.iter().map(|c| escaped(c)).collect()),
        line(columns.iter().map(|_| "---".into()).collect()),
    ];
    for group in &groups[..shown_rows] {
        out.push(line(columns.iter().map(|c| shown(group, c, &mut blocks)).collect()));
    }
    let cut = (shown_rows < groups.len()).then_some((shown_rows, groups.len()));
    // Named as the header spells it: the model may type `Date` for `date`
    // (#970). An exact header wins, as it does in `cell_text`'s sort.
    let by = order_by.map(|o| {
        let exact = columns.iter().find(|c| **c == o.column);
        (exact.or_else(|| columns.iter().find(|c| c.eq_ignore_ascii_case(&o.column))).unwrap_or(&o.column).as_str(), o.desc)
    });
    (format!("{}\n\n{}", out.join("\n"), note(counts, merged, cut, by)), blocks)
}

/// `rows` in groups, in order of first appearance: a kept row joins the
/// first earlier group whose fields — every one, shown or not — are the
/// same as its own and not all empty. Only the shown columns would merge
/// twelve monthly invoices shown by vendor and amount (review finding). It
/// also joins a group holding a row of the same source from another sift
/// (#1018: a forced table joins every sift, and one message was a row per
/// sift that kept it) — unless the group already holds a row of its own
/// sift, so two rows of one sift are never merged on their source, even
/// through a third. An empty source (`{}`: an item with no identifying key)
/// names no item and joins nothing. A withheld row is always a group of its
/// own: its fields are not shown, so nothing says it is the same as another.
/// The summary's `distinct` ([`distinct_kept`]) counts these groups.
fn alike<'a>(rows: &[&'a Row]) -> Vec<Vec<&'a Row>> {
    let mut groups: Vec<Vec<&Row>> = Vec::new();
    for &r in rows {
        let same_item = |g: &[&Row]| {
            sourced(r)
                && g.iter().any(|x| x.source == r.source)
                && g.iter().all(|x| (x.task_id, x.sift) != (r.task_id, r.sift))
        };
        let same = (r.kind == RowKind::Kept)
            .then(|| {
                groups.iter_mut().find(|g| {
                    g[0].kind == RowKind::Kept && ((mergeable(&r.fields) && g[0].fields == r.fields) || same_item(g))
                })
            })
            .flatten();
        match same {
            Some(g) => g.push(r),
            None => groups.push(vec![r]),
        }
    }
    groups
}

/// Fields with nothing in them say nothing about sameness: such a row is
/// never merged with another.
fn mergeable(fields: &Value) -> bool {
    fields.as_object().is_some_and(|o| o.values().any(|v| !scalar(v).is_empty()))
}

/// Does `r` name the item it came from? An empty source (`{}`) does not.
fn sourced(r: &Row) -> bool {
    r.source.as_object().is_some_and(|o| !o.is_empty())
}

/// How many rows a table of `rows`' kept ones shows before any `limit`:
/// [`alike`]'s groups. The summary gives it as `distinct` (#960: the body
/// quoted `kept`, 45, above a table of 30 rows), a chain's over every link
/// (#1022: one message in two overlapping pages is one row).
pub(crate) fn distinct_kept(rows: &[Row]) -> usize {
    let kept: Vec<&Row> = rows.iter().filter(|r| r.kind == RowKind::Kept).collect();
    alike(&kept).len()
}

/// How many items `rows` are (issue 1026): rows of one source from
/// different sifts are one item, as [`alike`] joins them; within one sift
/// each row is its own (a list can hold an item twice), so a source counts
/// its most rows in any one sift. A row with no source is one item.
pub(crate) fn items<'a>(rows: impl IntoIterator<Item = &'a Row>) -> usize {
    use std::collections::HashMap;
    let (mut per_sift, mut alone) = (HashMap::<(String, i64, i32), usize>::new(), 0);
    for r in rows {
        if sourced(r) {
            *per_sift.entry((r.source.to_string(), r.task_id, r.sift)).or_default() += 1;
        } else {
            alone += 1;
        }
    }
    let mut most = HashMap::<String, usize>::new();
    for ((source, ..), n) in per_sift {
        let m = most.entry(source).or_default();
        *m = (*m).max(n);
    }
    alone + most.values().sum::<usize>()
}

/// `rows` as stored for sift `sift` of `task_id` (numbered from 1), so a
/// sift's rows in hand count by the rule its stored ones do.
pub(crate) fn as_stored(task_id: i64, sift: i32, rows: &[NewRow]) -> Vec<Row> {
    rows.iter()
        .enumerate()
        .map(|(i, r)| Row {
            task_id,
            sift,
            row: i as i32 + 1,
            kind: r.kind,
            source: r.source.clone(),
            fields: r.fields.clone(),
            classification: r.classification.clone(),
        })
        .collect()
}

/// The order of a table that names none (#960): newest first by its date
/// column — one named `date`, else the first whose last word is `date`
/// (`due_date`, `due date`, `dueDate`; never `update` or `mandate`), in any
/// case. Text order is date order only for `YYYY-MM-DD`, which the sorter is
/// asked to write but may not: a cell of `rows` in that column written any
/// other way (`03/11/2026`, or `2025`, which sorts as a number) keeps the
/// order found (review finding), as does a table with no such column.
pub(crate) fn default_order(columns: &[String], rows: &[Row]) -> Option<OrderBy> {
    let column = columns.iter().find(|c| c.eq_ignore_ascii_case("date")).or_else(|| columns.iter().find(|c| ends_in_date_word(c)))?;
    let iso = |c: &str| {
        c.get(..10).is_some_and(|d| d.bytes().enumerate().all(|(i, b)| if i == 4 || i == 7 { b == b'-' } else { b.is_ascii_digit() }))
    };
    let all_iso = rows.iter().map(|r| cell_text(r, column)).filter(|c| !c.is_empty()).all(|c| iso(&c));
    all_iso.then(|| OrderBy { column: column.clone(), desc: true })
}

/// Is `date` the last word of `name`: after a non-letter, or a camel-case
/// `Date`?
fn ends_in_date_word(name: &str) -> bool {
    let Some((before, word)) = name.len().checked_sub(4).and_then(|i| name.get(..i).zip(name.get(i..))) else { return false };
    let Some(last) = before.chars().last() else { return false };
    word.eq_ignore_ascii_case("date") && (!last.is_ascii_alphabetic() || (word.starts_with('D') && last.is_ascii_lowercase()))
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

/// A group's cell as shown: screened, then escaped so it cannot end the
/// cell or row. Its `source` lists every distinct one, each screened alone
/// so one blocked source does not hide the rest; any other cell is the first
/// row's that is not empty (rows of one source from several sifts, #1018).
fn shown(group: &[&Row], column: &str, blocks: &mut Vec<SinkBlock>) -> String {
    if group[0].kind == RowKind::Withheld {
        return WITHHELD_MARKER.into();
    }
    if column == SOURCE_COLUMN {
        let mut sources: Vec<String> = Vec::new();
        for s in group.iter().map(|r| cell_text(r, column)) {
            if !sources.contains(&s) {
                sources.push(s);
            }
        }
        return sources.iter().map(|s| screened(s, blocks)).collect::<Vec<_>>().join(", ");
    }
    let first = group.iter().map(|r| cell_text(r, column)).find(|c| !c.is_empty());
    screened(&first.unwrap_or_default(), blocks)
}

fn screened(text: &str, blocks: &mut Vec<SinkBlock>) -> String {
    if let Some(block) = screen_model_text(text) {
        blocks.push(block);
        return WITHHELD_MARKER.into();
    }
    escaped(text)
}

/// `text` that cannot end a markdown cell or row.
fn escaped(text: &str) -> String {
    text.replace('|', "\\|").replace(['\n', '\r'], " ")
}

/// A cell's raw text: the field's value, or for [`SOURCE_COLUMN`] the
/// row's source (an object's values, comma-separated, but for the
/// [`SOURCE_TOOL_KEY`] tag, which keeps items apart rather than names one). A column with no
/// field of its exact name takes one that differs only in case: `Date` for
/// `date` left every cell empty, so the table's order did nothing (#965).
fn cell_text(row: &Row, column: &str) -> String {
    if column == SOURCE_COLUMN {
        return match &row.source {
            Value::Object(o) => o.iter().filter(|(k, _)| *k != SOURCE_TOOL_KEY).map(|(_, v)| scalar(v)).collect::<Vec<_>>().join(", "),
            other => scalar(other),
        };
    }
    let any_case = || row.fields.as_object()?.iter().find(|(k, _)| k.eq_ignore_ascii_case(column)).map(|(_, v)| v);
    row.fields.get(column).or_else(any_case).map(scalar).unwrap_or_default()
}

/// Does `o` order anything: has any of `rows` a cell in its column? One
/// that names no field (a typo) would leave the rows as found, and the
/// default order unused (#965). Pure.
pub(crate) fn orders_anything(o: &OrderBy, rows: &[Row]) -> bool {
    rows.iter().any(|r| !cell_text(r, &o.column).is_empty())
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

/// The counts, then what the table did to the rows: `merged` duplicates
/// shown once, and `cut` = (shown, of) when a limit left rows out — with
/// the `order` (column, descending) "the first" follows, which may be the
/// default one nobody named (#965 item 2).
fn note(c: &Counts, merged: usize, cut: Option<(usize, usize)>, order: Option<(&str, bool)>) -> String {
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
    if merged > 0 {
        parts.push(format!("{merged} duplicate{} shown once", if merged == 1 { "" } else { "s" }));
    }
    if let Some((shown, of)) = cut {
        let by = order.map_or(String::new(), |(column, desc)| format!(", by `{column}` {}", if desc { "descending" } else { "ascending" }));
        parts.push(format!("showing the first {shown} of {of} rows{by}"));
    }
    format!("{}.", parts.join("; "))
}

#[cfg(test)]
mod tests;
