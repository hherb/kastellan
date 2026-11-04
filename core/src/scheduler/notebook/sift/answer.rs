//! An answer's `table` (#922, S5, K10, K11): the scrapbook rows it names,
//! rendered by code after the body, with the counts.

use kastellan_db::scrapbook::{self, Row, RowKind};
use sqlx::PgPool;

use super::params::ScrapRef;
use super::table::{default_order, render_table, Counts, SOURCE_COLUMN};
use super::{chain, may_read};
use crate::cassandra::types::DataClass;
use crate::scheduler::inner_loop::{notebook_block_audit_payload, PlanRecord, TaskContext};
use crate::scheduler::notebook::reply::{NotebookReply, Refs, TableSpec};

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
        // The model names an order only sometimes (#960): default to newest first.
        let order = t.order_by.clone().or_else(|| default_order(&columns, &rows));
        let (table, blocks) = render_table(&rows, &columns, order.as_ref(), t.limit, &counts);
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

/// #960 item 2 (DGX task 314): after a sift that kept 21 items the model
/// answered in prose ("I would need a further step") and named no table, so
/// the rows never reached the operator. `Err` — the reason shown back with
/// the #945 re-ask — when `reply` answers with no `table` (and no
/// refusal) although this run's newest sift, or the chain it ends, kept
/// something ([`missing_table`]). The loop applies it to a turn's first
/// reply only: the reply re-asked is used either way, so the model can
/// still answer without one. Pure.
pub(crate) fn table_left_out(reply: &NotebookReply, plans: &[PlanRecord]) -> Result<(), String> {
    let Some((name, kept)) = missing_table(reply, plans) else { return Ok(()) };
    Err(format!(
        "the `answer` shows no `table`, but sift {name} kept {kept} items. To show them, add \
`\"table\": {{\"scrapbook\": \"{name}\"}}` to the `answer`; to answer without them, send the same answer again"
    ))
}

/// #960 (DGX tasks 335, 342): the forced-answer turn (a planner timeout, the
/// turn cap, the fast lane's ceiling) is never shown back — its deadline is
/// spent — and twice it answered with no table after a sift that kept rows.
/// On that turn the table is attached by code instead: the newest sift's
/// scrapbook, with the default columns and order. `None` when the answer
/// has a table, is a refusal, or no sift kept anything. Pure.
pub(crate) fn forced_table(reply: &NotebookReply, plans: &[PlanRecord]) -> Option<TableSpec> {
    let (name, _) = missing_table(reply, plans)?;
    Some(TableSpec { scrapbook: Refs::One(name), columns: None, order_by: None, limit: None })
}

/// The newest sift's scrapbook and how many rows its table would show
/// (`distinct`, the chain's when it continues one), when `reply` answers
/// with no `table` and no refusal and that sift kept something. Pure.
fn missing_table(reply: &NotebookReply, plans: &[PlanRecord]) -> Option<(String, u64)> {
    if !reply.answer.as_ref().is_some_and(|a| a.table.is_none() && a.refused.is_none()) {
        return None;
    }
    let s = chain::last_sift(plans)?;
    // The table's rows (`distinct`), as the prompt has the body quote them.
    let count = |v: &serde_json::Value| v["distinct"].as_u64().or_else(|| v["kept"].as_u64());
    let kept = count(&s["chain"]).or_else(|| count(s)).unwrap_or(0);
    let name = s["scrapbook"].as_str().filter(|_| kept > 0)?;
    Some((name.to_string(), kept))
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

    /// #960 item 2: an answer with no table after a sift that kept rows is
    /// refused once, naming the scrapbook; a chain's `kept` counts, the
    /// newest sift decides, and a table, a refusal or nothing kept passes.
    #[test]
    fn an_answer_without_the_table_of_a_sift_that_kept_rows_is_refused() {
        use super::super::chain::tests::{params, sift_record};
        let answer = |extra: serde_json::Value| {
            let mut a = serde_json::json!({"body": "21 payments"});
            a.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            serde_json::from_value::<NotebookReply>(serde_json::json!({"answer": a})).unwrap()
        };
        let plain = answer(serde_json::json!({}));
        let kept = |n: u64| sift_record(params(0, "x"), serde_json::json!({"scrapbook": "sb:9:3", "kept": n}));
        let why = table_left_out(&plain, &[kept(21)]).unwrap_err();
        assert!(why.contains("sb:9:3") && why.contains("kept 21") && why.contains("\"table\""), "{why}");
        assert_eq!(table_left_out(&plain, &[kept(0)]), Ok(()), "nothing kept: no table to show");
        assert_eq!(table_left_out(&plain, &[]), Ok(()), "no sift");
        assert_eq!(table_left_out(&plain, &[kept(21), kept(0)]), Ok(()), "the newest sift decides");
        let link = sift_record(params(2, "x"), serde_json::json!({"scrapbook": "sb:9:4", "kept": 0, "chain": {"kept": 5}}));
        assert!(table_left_out(&plain, &[link]).unwrap_err().contains("kept 5"), "a chain's kept");
        let link = sift_record(params(2, "x"), serde_json::json!({"scrapbook": "sb:9:4", "kept": 4, "distinct": 4, "chain": {"kept": 5, "distinct": 3}}));
        assert!(table_left_out(&plain, &[link]).unwrap_err().contains("kept 3"), "the chain's distinct: the table's rows");
        let own = sift_record(params(0, "x"), serde_json::json!({"scrapbook": "sb:9:3", "kept": 8, "distinct": 6}));
        assert!(table_left_out(&plain, &[own]).unwrap_err().contains("kept 6"), "its own distinct");
        assert_eq!(table_left_out(&answer(serde_json::json!({"table": {"scrapbook": "sb:9:3"}})), &[kept(21)]), Ok(()));
        assert_eq!(table_left_out(&answer(serde_json::json!({"refused": {"principle": 1, "reason": "harm"}})), &[kept(21)]), Ok(()), "a refusal");
        let call: NotebookReply = serde_json::from_value(serde_json::json!({"infeasible": {"reason": "no"}})).unwrap();
        assert_eq!(table_left_out(&call, &[kept(21)]), Ok(()), "not an answer");
        // The forced-answer turn: the same rule, the table attached by code.
        let spec = forced_table(&plain, &[kept(21)]).expect("attached");
        assert_eq!((spec.scrapbook.refs(), spec.columns, spec.order_by, spec.limit), (&["sb:9:3".to_string()][..], None, None, None));
        assert_eq!(forced_table(&plain, &[kept(0)]), None, "nothing kept");
        assert_eq!(forced_table(&answer(serde_json::json!({"table": {"scrapbook": "sb:9:1"}})), &[kept(21)]), None, "its own table");
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
