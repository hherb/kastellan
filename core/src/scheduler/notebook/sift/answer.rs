//! An answer's `table` (#922, S5, K10, K11): the scrapbook rows it names,
//! rendered by code after the body, with the counts.

use kastellan_db::scrapbook::{self, Row, RowKind};
use sqlx::PgPool;

use super::params::ScrapRef;
use super::table::{default_order, orders_anything, render_table, Counts, SOURCE_COLUMN};
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
/// shown: a line says so instead, and the rest still render. `strip_own`:
/// `t` was attached by code to a forced answer ([`forced_table`]), so a
/// table in `body` is the model's copy of those rows ([`without_own_tables`]).
pub(crate) async fn with_table(pool: &PgPool, ctx: &TaskContext, body: &str, t: &TableSpec, strip_own: bool) -> (String, DataClass) {
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
    // The prompt says never to copy the rows into `body`; a model that did
    // showed two tables, its own partial one first (DGX task 351). Only on
    // the forced answer: a table the model named may sit beside a body table
    // that is not a copy (totals), and only when the code's table has rows
    // to take its place (review findings).
    let own = (strip_own && !rows.is_empty()).then(|| without_own_tables(body)).flatten();
    if own.is_some() {
        tracing::info!(task_id = ctx.task_id, "answer table: the body's own markdown table taken out");
        refused.push(OWN_TABLE_NOTE.into());
    }
    let mut out = own.unwrap_or_else(|| body.to_string());
    if included {
        out.push_str("\n\n");
        // The model names an order only sometimes (#960), and sometimes one
        // that names no field (#965): the default order (newest first, when
        // dated) — said under the table, as a `limit` then cuts other rows.
        let asked = t.order_by.clone().filter(|o| orders_anything(o, &rows));
        if t.order_by.is_some() && asked.is_none() && !rows.is_empty() {
            tracing::info!(task_id = ctx.task_id, "answer table: order_by names no column; default order used");
            refused.push("(the order named matches no column: the table is in its default order)".into());
        }
        let order = asked.or_else(|| default_order(&columns, &rows));
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

/// The line under a table when the body's own table was taken out.
pub(crate) const OWN_TABLE_NOTE: &str = "(a table written in the answer is left out: its rows are in the table above)";

/// `body` without the markdown tables the model wrote into it, or `None`
/// when it has none. A table is a run of lines starting with `|` whose
/// second line is a delimiter row (`|---|:--:|`). A table inside a fenced
/// code block stays, and the blank lines around a cut table close up to one
/// (#970). Only called when code renders the scrapbook's table, which is
/// the faithful one. Pure.
pub(crate) fn without_own_tables(body: &str) -> Option<String> {
    let lines: Vec<&str> = body.lines().collect();
    let piped = |l: &str| l.trim_start().starts_with('|');
    let delimiter = |l: &str| {
        let t = l.trim();
        t.contains('-') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
    };
    // shortcut: any ``` or ~~~ line opens or closes a fence, whatever its
    // length or character; track the opener if a model nests fences.
    let fence = |l: &str| l.trim_start().starts_with("```") || l.trim_start().starts_with("~~~");
    let (mut kept, mut cut, mut fenced, mut i) = (Vec::<&str>::with_capacity(lines.len()), false, false, 0);
    while i < lines.len() {
        if fence(lines[i]) {
            fenced = !fenced;
        } else if !fenced && piped(lines[i]) && lines.get(i + 1).is_some_and(|l| piped(l) && delimiter(l)) {
            cut = true;
            while i < lines.len() && piped(lines[i]) {
                i += 1;
            }
            // The blank line before the table already separates what follows.
            while kept.last().map_or(true, |l| l.trim().is_empty()) && lines.get(i).is_some_and(|l| l.trim().is_empty()) {
                i += 1;
            }
            continue;
        }
        kept.push(lines[i]);
        i += 1;
    }
    cut.then(|| kept.join("\n").trim_end().to_string())
}

/// #960 item 2 (DGX task 314): after a sift that kept 21 items the model
/// answered in prose ("I would need a further step") and named no table, so
/// the rows never reached the operator. `Err` — the reason shown back with
/// the #945 re-ask — when `reply` answers with no `table` (and no
/// refusal) although this run's newest sift, or the chain it ends, kept
/// something ([`missing_table`]). The loop applies it to a turn's first
/// reply only: the reply re-asked is used either way, so the model can
/// still answer without one. Pure.
///
/// #973 (DGX task 359): an answer whose `table` names no ref that is a
/// scrapbook ref at all (`sb:emails_with_attachments`, with no sift run)
/// shows nothing but a refusal line, so it is shown back the same way.
pub(crate) fn table_left_out(reply: &NotebookReply, plans: &[PlanRecord]) -> Result<(), String> {
    if let Some(named) = table_of_no_scrapbook(reply) {
        let fix = match newest_kept(plans) {
            Some((name, kept)) => format!("sift {name} kept {kept} items; to show them, name `\"scrapbook\": \"{name}\"`"),
            None => "no sift here has rows to show, so write the items in `body` and leave `table` out".into(),
        };
        return Err(format!(
            "the `answer`'s `table` names {}, but a table shows a `sift`'s rows, named `sb:<task>:<sift>`: {fix}",
            named.join(", ")
        ));
    }
    let Some((name, kept)) = missing_table(reply, plans) else { return Ok(()) };
    Err(format!(
        "the `answer` shows no `table`, but sift {name} kept {kept} items. To show them, add \
`\"table\": {{\"scrapbook\": \"{name}\"}}` to the `answer`; to answer without them, send the same answer again"
    ))
}

/// The refs `reply`'s answer table names, when none of them parses as a
/// scrapbook ref. A well-formed ref is left to [`with_table`]. Pure.
fn table_of_no_scrapbook(reply: &NotebookReply) -> Option<&[String]> {
    let refs = reply.answer.as_ref()?.table.as_ref()?.scrapbook.refs();
    // An empty list shows the body alone, with no refusal line: nothing to show back.
    (!refs.is_empty() && refs.iter().all(|r| ScrapRef::parse(r).is_err())).then_some(refs)
}

/// #960 (DGX tasks 335, 342): the forced-answer turn (a planner timeout, the
/// turn cap, the fast lane's ceiling) is never shown back — its budget is
/// spent — and twice it answered with no table after a sift that kept rows.
/// On that turn the table is attached by code instead: the newest sift's
/// scrapbook (and the chain it ends), with the default columns and order.
/// `None` when the answer has a table, is a refusal, or the newest sift kept
/// nothing. Pure.
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
    newest_kept(plans)
}

/// This run's newest sift's scrapbook and its table's row count, when it
/// (or the chain it ends) kept something. Pure.
fn newest_kept(plans: &[PlanRecord]) -> Option<(String, u64)> {
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
        assert_eq!(forced_table(&answer(serde_json::json!({"refused": {"principle": 1, "reason": "harm"}})), &[kept(21)]), None, "a refusal");
    }

    /// #973 (DGX task 359): no sift ran, and the answer's table named
    /// `sb:emails_with_attachments`, so the operator got one refusal line and
    /// no list. A table none of whose refs is a scrapbook ref is shown back
    /// once, naming the newest sift that kept rows when there is one.
    #[test]
    fn an_answer_whose_table_names_no_scrapbook_ref_is_refused() {
        use super::super::chain::tests::{params, sift_record};
        let with_table = |refs: serde_json::Value| {
            serde_json::from_value::<NotebookReply>(serde_json::json!({"answer": {"body": "Here they are:", "table": {"scrapbook": refs}}})).unwrap()
        };
        let made_up = with_table(serde_json::json!("sb:emails_with_attachments"));
        let why = table_left_out(&made_up, &[]).unwrap_err();
        assert!(why.contains("sb:emails_with_attachments") && why.contains("`body`") && !why.contains("kept"), "{why}");
        let kept = sift_record(params(0, "x"), serde_json::json!({"scrapbook": "sb:9:3", "kept": 4}));
        let why = table_left_out(&made_up, std::slice::from_ref(&kept)).unwrap_err();
        assert!(why.contains("sb:9:3") && why.contains("kept 4"), "names the sift that kept rows: {why}");
        assert!(table_left_out(&with_table(serde_json::json!(["bad", "worse"])), &[]).is_err(), "every ref malformed");
        assert_eq!(table_left_out(&with_table(serde_json::json!(["bad", "sb:9:3"])), &[kept]), Ok(()), "one ref shows rows");
        assert_eq!(table_left_out(&with_table(serde_json::json!("sb:9:3")), &[]), Ok(()), "a well-formed ref is with_table's to judge");
        assert_eq!(table_left_out(&with_table(serde_json::json!([])), &[]), Ok(()), "no refs: the body alone");
        // The forced turn is never shown back; code attaches no table to an answer that names one.
        assert_eq!(forced_table(&made_up, &[]), None);
    }

    #[test]
    fn a_body_s_own_markdown_table_is_taken_out() {
        let body = "Six of them:\n\n| date | amount |\n|---|---:|\n| 1 May | 20 |\n\nSee below.";
        assert_eq!(without_own_tables(body), Some("Six of them:\n\nSee below.".into()), "one blank line where the table was (#970)");
        let indented = "a\n  | x |\n  | :-- |\n  | 1 |";
        assert_eq!(without_own_tables(indented), Some("a".into()), "indented, and at the end");
        assert_eq!(without_own_tables("| a |\n|---|\n| 1 |\nAfter."), Some("After.".into()), "prose right after the table stays");
        assert_eq!(without_own_tables("| a |\n|---|\n\nmid\n\n| b |\n|---|\nend"), Some("mid\n\nend".into()), "every table, not only the first");
        // #970: a table inside a fenced code block is the model's example, not a copy of the rows.
        let fenced = "Like this:\n\n```\n| a |\n|---|\n```\n\n| b |\n|---|\n| 1 |";
        assert_eq!(without_own_tables(fenced), Some("Like this:\n\n```\n| a |\n|---|\n```".into()), "only the table outside the fence");
        assert_eq!(without_own_tables("~~~md\n| a |\n|---|\n~~~"), None, "a tilde fence too");
        assert_eq!(without_own_tables("a | b\n| not | a table |\nc"), None, "no delimiter row");
        assert_eq!(without_own_tables("| x |\n| y |"), None, "a second row is not a delimiter");
        assert_eq!(without_own_tables("|---|"), None, "a delimiter alone");
        assert_eq!(without_own_tables("plain prose"), None);
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
