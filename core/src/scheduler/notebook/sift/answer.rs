//! An answer's `table` (#922, S5, K10, K11): the scrapbook rows it names,
//! rendered by code after the body, with the counts.

use kastellan_db::scrapbook::{self, Row, RowKind};
use sqlx::PgPool;

use super::params::ScrapRef;
use super::table::{render_table, Counts, SOURCE_COLUMN};
use super::{may_read, SIFT_TOOL};
use crate::cassandra::types::DataClass;
use crate::scheduler::inner_loop::{StepOutcome, TaskContext};
use crate::scheduler::notebook::reply::TableSpec;

/// `body`, then the table `t` names, and the highest class of its rows. A
/// ref that is malformed, unreadable here (K11) or unreadable from the
/// database is not shown: a line says so instead, and the rest still render.
pub(crate) async fn with_table(pool: &PgPool, ctx: &TaskContext, body: &str, t: &TableSpec) -> (String, DataClass) {
    let mut rows: Vec<Row> = Vec::new();
    let mut counts = Counts::default();
    let mut fields: Option<Vec<String>> = None;
    let mut refused = Vec::new();
    for raw in t.scrapbook.refs() {
        let r = match ScrapRef::parse(raw) {
            Ok(r) if may_read(ctx, r) => r,
            Ok(_) => {
                refused.push(format!("(table {raw} could not be included: it is not from this conversation)"));
                continue;
            }
            Err(e) => {
                refused.push(format!("(table {raw} could not be included: {e})"));
                continue;
            }
        };
        match scrapbook::rows(pool, r.task_id, r.sift).await {
            Ok(got) => {
                let summary = own_summary(ctx, &r);
                add_counts(&mut counts, &got, summary.as_ref());
                if fields.is_none() {
                    fields = summary.as_ref().and_then(summary_fields).or_else(|| first_row_fields(&got));
                }
                rows.extend(got);
            }
            Err(e) => {
                tracing::warn!(task_id = ctx.task_id, scrapbook = %r, error = %e, "answer table: rows not read");
                refused.push(format!("(table {raw} could not be included: it could not be read)"));
            }
        }
    }
    let class = rows.iter().map(row_class).fold(ctx.classification_floor, DataClass::max);
    let columns = t.columns.clone().unwrap_or_else(|| {
        let mut c = fields.unwrap_or_default();
        c.push(SOURCE_COLUMN.into());
        c
    });
    let mut out = body.to_string();
    if !rows.is_empty() || counts.looked_at > 0 {
        out.push_str("\n\n");
        out.push_str(&render_table(&rows, &columns, t.order_by.as_ref(), &counts));
    }
    for line in refused {
        out.push_str("\n\n");
        out.push_str(&line);
    }
    (out, class)
}

/// The summary this task's own sift recorded for `r`, if it ran in this run.
fn own_summary(ctx: &TaskContext, r: &ScrapRef) -> Option<serde_json::Value> {
    let name = r.to_string();
    ctx.plans.iter().rev().find_map(|rec| match (rec.plan.steps.first(), rec.outcomes().first()) {
        (Some(s), Some(StepOutcome::Ok(v))) if s.tool == SIFT_TOOL && v["scrapbook"] == name.as_str() => Some(v.clone()),
        _ => None,
    })
}

fn summary_fields(v: &serde_json::Value) -> Option<Vec<String>> {
    serde_json::from_value(v["fields"].clone()).ok()
}

/// An earlier turn's sift left no summary here: its field names are the
/// first kept row's keys.
fn first_row_fields(rows: &[Row]) -> Option<Vec<String>> {
    let first = rows.iter().find(|r| r.kind == RowKind::Kept)?;
    Some(first.fields.as_object()?.keys().cloned().collect())
}

/// The note's counts: the sift's own when its summary is at hand, else what
/// the rows themselves say.
fn add_counts(c: &mut Counts, rows: &[Row], summary: Option<&serde_json::Value>) {
    let n = |k: &str| summary.and_then(|s| s[k].as_u64()).map(|v| v as usize);
    let kept = rows.iter().filter(|r| r.kind == RowKind::Kept).count();
    let withheld = rows.len() - kept;
    c.kept += kept;
    c.withheld += withheld;
    c.looked_at += n("looked_at").unwrap_or(rows.len());
    c.undecided += n("undecided").unwrap_or(0);
    c.failed_reads += n("failed_reads").unwrap_or(0);
}

/// A row's class; one that does not parse counts as the highest (fail
/// closed: the ceiling must never be too low).
pub(crate) fn row_class(r: &Row) -> DataClass {
    serde_json::from_value(serde_json::Value::String(r.classification.clone())).unwrap_or(DataClass::Secret)
}
