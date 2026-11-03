//! An answer's `table` (#922, S5, K10, K11): the scrapbook rows it names,
//! rendered by code after the body, with the counts.

use kastellan_db::scrapbook::{self, Row, RowKind};
use sqlx::PgPool;

use super::params::ScrapRef;
use super::table::{render_table, Counts, SOURCE_COLUMN};
use super::{chain, may_read};
use crate::cassandra::types::DataClass;
use crate::scheduler::inner_loop::{notebook_block_audit_payload, PlanRecord, TaskContext};
use crate::scheduler::notebook::reply::TableSpec;

/// The most scrapbooks one table names: as many as a turn records. Each
/// brings its whole chain, which this does not cap.
pub(crate) const TABLE_REFS_MAX: usize = crate::scheduler::conversation::record::SCRAPBOOKS_PER_TURN;

/// `body`, then the table `t` names, and the highest class of its rows. A
/// ref names its whole chain ([`table_refs`]). A ref that is malformed,
/// unreadable here (K11), unreadable from the database or empty is not
/// shown: a line says so instead, and the rest still render.
pub(crate) async fn with_table(pool: &PgPool, ctx: &TaskContext, body: &str, t: &TableSpec) -> (String, DataClass) {
    let mut rows: Vec<Row> = Vec::new();
    let mut counts = Counts::default();
    let mut fields: Option<Vec<String>> = None;
    let mut included = false;
    let (named, mut refused) = table_refs(&ctx.plans, t.scrapbook.refs());
    let mut links = Vec::new();
    for raw in &named {
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
            Ok(got) if got.is_empty() && own_summary(ctx, &r).is_none() => {
                refused.push(format!("(table {raw} could not be included: it has no rows)"));
            }
            Ok(got) => {
                included = true;
                let summary = own_summary(ctx, &r);
                add_counts(&mut counts, &got, summary.as_ref());
                links.push(Link::of(raw, summary.as_ref()));
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
    counts.remaining = remaining(&links);
    let class = rows.iter().map(row_class).fold(ctx.classification_floor, DataClass::max);
    let columns = t.columns.clone().filter(|c| !c.is_empty()).unwrap_or_else(|| {
        let mut c = fields.unwrap_or_default();
        c.push(SOURCE_COLUMN.into());
        c
    });
    let mut out = body.to_string();
    if included {
        out.push_str("\n\n");
        let (table, blocks) = render_table(&rows, &columns, t.order_by.as_ref(), t.limit, &counts);
        out.push_str(&table);
        // As for any screened model text: the withheld cell is otherwise
        // invisible. An answer has no notebook step: `step` 0.
        for block in blocks {
            let payload = notebook_block_audit_payload(ctx.task_id, 0, "answer_table_cell", &block);
            if let Err(e) = kastellan_db::audit::insert(pool, "policy", "injection.blocked", payload).await {
                tracing::error!(task_id = ctx.task_id, error = %e, "answer table injection.blocked audit insert failed");
            }
        }
    }
    for line in refused {
        out.push_str("\n\n");
        out.push_str(&line);
    }
    (out, class)
}

/// The scrapbooks a table named `named` joins, each once, and a line for
/// each named past [`TABLE_REFS_MAX`]. The cap is on the refs named, before
/// each is expanded into its chain ([`chain::expand`], oldest first): the
/// last sift of a chain of nine shows every link, not the first eight
/// without the one named (review finding). A ref is written canonically
/// (`sb:9:04` is `sb:9:4`), so its chain is found; a malformed one is kept
/// as written for its refusal. Pure.
fn table_refs(plans: &[PlanRecord], named: &[String]) -> (Vec<String>, Vec<String>) {
    let canonical = |raw: &String| ScrapRef::parse(raw).map_or_else(|_| raw.clone(), |r| r.to_string());
    let mut distinct: Vec<String> = Vec::new();
    for r in named.iter().map(canonical) {
        if !distinct.contains(&r) {
            distinct.push(r);
        }
    }
    let past = distinct.split_off(distinct.len().min(TABLE_REFS_MAX));
    let refused = past.iter().map(|r| format!("(table {r} could not be included: a table names at most {TABLE_REFS_MAX})")).collect();
    let mut refs: Vec<String> = Vec::new();
    for r in distinct.iter().flat_map(|r| chain::expand(plans, r)) {
        if !refs.contains(&r) {
            refs.push(r);
        }
    }
    (refs, refused)
}

/// The summary this task's own sift recorded for `r`, if it ran in this run.
fn own_summary(ctx: &TaskContext, r: &ScrapRef) -> Option<serde_json::Value> {
    chain::summary_of(&ctx.plans, &r.to_string()).cloned()
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
/// the rows themselves say — kept plus withheld, so an earlier turn's
/// "looked at" is a lower bound. `remaining` is the chain's ([`remaining`]).
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

/// An included scrapbook: its name, its `remaining`, and the one it
/// continues.
struct Link {
    name: String,
    remaining: usize,
    continues: Option<String>,
}

impl Link {
    fn of(name: &str, summary: Option<&serde_json::Value>) -> Self {
        let get = |k: &str| summary.map(|s| s[k].clone()).unwrap_or_default();
        Link {
            name: name.into(),
            remaining: get("remaining").as_u64().unwrap_or(0) as usize,
            continues: get("continues").as_str().map(String::from),
        }
    }
}

/// The items the table's scrapbooks did not look at: each link's
/// `remaining`, except a link another included one continues — that one
/// went on through them. A continuer left out (the table's cap, an
/// unreadable link) leaves its link's `remaining` counted. Pure.
fn remaining(links: &[Link]) -> usize {
    let continued: Vec<&str> = links.iter().filter_map(|l| l.continues.as_deref()).collect();
    links.iter().filter(|l| !continued.contains(&l.name.as_str())).map(|l| l.remaining).sum()
}

/// A row's class; one that does not parse counts as the highest (fail
/// closed: the ceiling must never be too low).
pub(crate) fn row_class(r: &Row) -> DataClass {
    serde_json::from_value(serde_json::Value::String(r.classification.clone())).unwrap_or_else(|_| {
        tracing::warn!(task_id = r.task_id, sift = r.sift, row = r.row, "scrapbook row class does not parse; counted as Secret");
        DataClass::Secret
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review finding: only an included continuer takes over a link's
    /// `remaining`; a chain cut short (the cap, an unreadable link) says
    /// what was not looked at.
    #[test]
    fn remaining_counts_only_the_links_nothing_included_continues() {
        let link = |name: &str, remaining, continues: Option<&str>| Link {
            name: name.into(),
            remaining,
            continues: continues.map(String::from),
        };
        let whole = [link("a", 70, None), link("b", 40, Some("a")), link("c", 0, Some("b"))];
        assert_eq!(remaining(&whole), 0);
        assert_eq!(remaining(&whole[..2]), 40, "c left out: b's remaining stands");
        assert_eq!(remaining(&[link("a", 70, None), link("x", 5, None)]), 75, "two separate sifts");
        assert_eq!(remaining(&[]), 0);
    }

    /// Review finding: the cap is on the refs named, so the last of a chain
    /// of nine shows all nine; a ref past it is the one refused, and a ref
    /// written `sb:9:04` finds its chain.
    #[test]
    fn the_cap_is_on_refs_named_and_a_chain_brings_every_link() {
        use super::super::chain::tests::{params, sift_record};
        let plans: Vec<PlanRecord> = (1..=9)
            .map(|n| {
                let continues = (n > 1).then(|| format!("sb:9:{}", n - 1));
                sift_record(params(30 * (n - 1), "x"), serde_json::json!({"scrapbook": format!("sb:9:{n}"), "continues": continues}))
            })
            .collect();
        let all: Vec<String> = (1..=9).map(|n| format!("sb:9:{n}")).collect();
        assert_eq!(table_refs(&plans, &["sb:9:9".into()]), (all.clone(), vec![]));
        assert_eq!(table_refs(&plans, &["sb:9:09".into(), "sb:9:9".into()]).0, all, "canonical, and once");
        let named: Vec<String> = (1..=9).map(|n| format!("sb:7:{n}")).collect();
        let (refs, refused) = table_refs(&plans, &named);
        assert_eq!(refs, named[..TABLE_REFS_MAX]);
        assert_eq!(refused, ["(table sb:7:9 could not be included: a table names at most 8)"]);
        assert_eq!(table_refs(&plans, &["bad".into()]).0, ["bad"], "kept as written for its refusal");
    }

    #[test]
    fn a_row_class_that_does_not_parse_counts_as_secret() {
        let row = |class: &str| Row {
            task_id: 1,
            sift: 1,
            row: 1,
            kind: RowKind::Kept,
            source: serde_json::json!({}),
            fields: serde_json::json!({}),
            classification: class.into(),
        };
        assert_eq!(row_class(&row("Personal")), DataClass::Personal);
        assert_eq!(row_class(&row("personal-ish")), DataClass::Secret);
        assert_eq!(row_class(&row("")), DataClass::Secret);
    }
}
