//! A sift in the notebook loop (#922, §6.2).
//!
//! 1. Check the call: its parameters, its reads (K3a), that a sorter exists,
//!    that `over` names a list and that `from` falls inside it. A refusal is
//!    a block (`sift_refused: …`): the loop records the step as failed and
//!    the next turn replans.
//! 2. Expand each item's reads (K3b), as many items as fit in
//!    `KASTELLAN_SIFT_MAX_READS` reads — and never more items than that, read
//!    or not — and review and dispatch them as
//!    **one plan** through the shared `inner_loop::execute::review_and_dispatch` (K3) — a Block or an
//!    Escalate there behaves exactly as for any plan. A failed read does not
//!    stop the others ([`StepPolicy::SiftReads`]).
//! 3. Show each item (its hit, then its reads, each through the summary's own
//!    screen) to the sorter. An item the screen blocks is withheld and never
//!    sorted (K4). Stop early at the fast lane's deadline, at an operator's
//!    cancel, or when the sorter fails; the item it failed on is not counted,
//!    so `next_from` starts there. When every read failed, nothing is sorted:
//!    an item decided on its hit alone would read as a real answer (#956).
//! 4. Write the kept and withheld items to the scrapbook (K6), and record the
//!    sift as one step whose result is a summary of counts and a sample (K9):
//!    that summary is all the planner sees. An error that ends the task
//!    mid-sift still writes the rows kept so far when it can, and
//!    `sift.finished` with `stopped: "error"` (#956).

use std::time::Instant;

use kastellan_db::scrapbook::{self, NewRow, Row, RowKind};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use super::params::{read_key, ScrapRef, SiftParams};
use super::sorter::{parse_sort_reply, sorter_message, SortReply, SORTER_ITEM_CAP};
use super::summary::{summary, Reads, Stop};
use super::table::{as_stored, distinct_kept, Counts, SOURCE_TOOL_KEY};
use super::reads::plan_reads;
use super::template::{scrap_parts, unread_why};
use super::item_views::ReadsMade;
use super::{SIFT_METHOD, SIFT_READS_DECISION, SIFT_TOOL};
use crate::cassandra::review::ChainReviewStage;
use crate::scheduler::inner_loop::llm_failure::FAILED_ROW_ERROR_CHARS;
use crate::cassandra::types::{DataClass, Plan, PlannedStep};
use crate::scheduler::agent::FormulationMeta;
use crate::scheduler::audit::SCHEDULER_AUDIT_ACTOR;
use crate::scheduler::inner_loop::execute::{self, ExecState, Executed, StepPolicy};
use crate::scheduler::inner_loop::{
    notebook_block_audit_payload, InnerLoopError, Outcome, PlanRecord, SinkBlock, StepDispatcher, StepOutcome,
    TaskContext,
};
use crate::scheduler::notebook::formulator::NotebookFormulator;
use crate::scheduler::notebook::reply::{CallSpec, NOTEBOOK_STEP_RATIONALE};

mod check;
use check::check;

/// Audit actions (K12), actor `scheduler`.
pub(crate) const ACTION_SIFT_STARTED: &str = "sift.started";
pub(crate) const ACTION_SIFT_FINISHED: &str = "sift.finished";
pub(crate) const ACTION_SIFT_REFUSED: &str = "sift.refused";

/// How a refused sift's block reason begins; the loop reads it to record
/// the step as `SIFT_REFUSED` rather than `BLOCKED`.
pub(crate) const SIFT_REFUSED_BLOCK: &str = "sift_refused: ";

/// Most characters of a refusal's reason in its audit row.
const REFUSED_REASON_MAX_CHARS: usize = 512;

/// The keys of an item that say where it came from, in the order looked for.
pub(crate) const SOURCE_KEYS: [&str; 5] = ["message_id", "id", "url", "path", "handoff_ref"];

/// What a sift needs from the loop: the review chain and the dispatcher for
/// its reads, the formulator for its sorter and the operator's knobs, and
/// the turn's `now` for the sorter.
pub(crate) struct SiftRun<'a> {
    pub pool: &'a PgPool,
    pub review: &'a ChainReviewStage,
    pub dispatcher: &'a dyn StepDispatcher,
    pub nb: &'a dyn NotebookFormulator,
    pub outbox: Option<&'a crate::channel::outbox::ChannelOutbox>,
    pub now: Option<&'a str>,
    /// This run's notebook calls review blocked or the loop rejected, which
    /// left no record in `ctx.plans` (#956).
    pub refused: &'a [super::chain::RefusedCall],
}

/// Run the sift `call` asked for. `Dispatched` means its summary is now the
/// last of `ctx.plans`, like any call's result.
pub(crate) async fn run_sift(
    r: SiftRun<'_>,
    ctx: &mut TaskContext,
    call: &CallSpec,
    meta: &FormulationMeta,
    st: &mut ExecState,
    failed_llm_calls: &mut u32,
) -> Result<Executed, InnerLoopError> {
    let started = Instant::now();
    let cfg = r.nb.sift_config();
    let checked = check(&r, ctx, call, &cfg).await;
    let (p, sorter, over) = match checked {
        Ok(c) => c,
        Err(why) => {
            let reason: String = why.chars().take(REFUSED_REASON_MAX_CHARS).collect();
            audit(r.pool, ACTION_SIFT_REFUSED, json!({"task_id": ctx.task_id, "plan_count": ctx.plan_count, "reason": reason})).await;
            ctx.blocks.push(format!("{SIFT_REFUSED_BLOCK}{why}"));
            return Ok(Executed::Again);
        }
    };
    if r.now.is_none() {
        // No planner timezone: a criterion like "currently due" is judged
        // without today's date (DGX task 307 kept 2017 bills).
        tracing::warn!(task_id = ctx.task_id, "sift: the sorter is not told today's date (no planner timezone configured)");
    }
    // Step n's newest call failed: this sift goes over its older success (#956).
    let over_older = match p.over {
        super::params::Over::Step(n) => super::chain::failed_since(&ctx.plans, r.refused, n)
            .map(|code| format!("step {n}'s newest call failed ({code}): this sift went over its earlier successful result")),
        super::params::Over::Scrapbook(_) => None,
    };
    let items: Vec<Value> = over.items.into_iter().skip(p.from).collect();
    let batch = plan_reads(&p.read, &items, cfg.max_reads);
    let scrap = ScrapRef { task_id: ctx.task_id, sift: ctx.plan_count as i32 };
    // What every early `sift.finished` row starts with.
    let head = json!({"task_id": ctx.task_id, "plan_count": ctx.plan_count, "scrapbook": scrap.to_string(), "over_older": over_older});
    let class = call.classification.max(ctx.classification_floor).max(over.class);
    // A reply that came back past the deadline makes no reads: the sorter
    // would stop at the first item anyway (#956).
    let late = ctx.deadline.is_some_and(|d| Instant::now() >= d);
    let reads_run = !batch.reads.is_empty() && !late;
    audit(r.pool, ACTION_SIFT_STARTED, started_payload(ctx, &scrap, &p, items.len(), batch.reads.len(), !reads_run, meta)).await;

    // The reads: one reviewed plan, dispatched past any failed read.
    let mut made = ReadsMade { outcomes: Vec::new(), tools: Vec::new() };
    if reads_run {
        let plan = reads_plan(call, &p, &batch.reads, class);
        let policy = StepPolicy::SiftReads { cap: cfg.max_reads };
        let executed = execute::review_and_dispatch(
            r.pool, r.review, r.dispatcher, ctx, plan, meta, false, None, st, r.outbox, policy,
        )
        .await;
        match executed {
            Err(e) => return Err(error_stop(r.pool, ctx, head, started, &Counts::default(), 0, e).await),
            Ok(Executed::Dispatched) => {}
            Ok(other) => {
                let mut row = head;
                row["stopped"] = json!(Stop::ReadsNotRun.as_str());
                row["ms"] = json!(started.elapsed().as_millis() as u64);
                audit(r.pool, ACTION_SIFT_FINISHED, row).await;
                return Ok(other);
            }
        }
        let record = ctx.plans.last().expect("review_and_dispatch pushed the reads' record");
        made.outcomes = record.outcomes().to_vec();
        made.tools = record.plan.steps.iter().map(|s| s.tool.clone()).collect();
    }

    // The sorter, one item at a time.
    let (mut counts, mut rows, mut stopped, mut sorter_calls) = (Counts::default(), Vec::new(), None, 0u32);
    let mut unread = 0usize;
    // Why the sorter failed, for `sift.finished` (#956), capped like a `plan.formulate_failed` row's.
    let mut sorter_error: Option<String> = None;
    // Each failed read's code, counted for the items looked at — or, on a
    // `ReadsFailed` stop, for every read made (#956).
    let mut read_errors = std::collections::BTreeMap::<String, usize>::new();
    let mut sort_up_to = items.len();
    // A failed cancel check stops sorting; the rows kept so far are still
    // written before the task fails with it (#956).
    let mut failure: Option<InnerLoopError> = None;
    // An item with no read to make is decided on its hit as ever: only a
    // batch whose every item had a read, and none came back, stops.
    if made.all_failed() && batch.per_item.iter().all(|r| !r.is_empty()) {
        // Not one read came back: no item is looked at, `next_from` stays.
        (stopped, sort_up_to, read_errors) = (Some(Stop::ReadsFailed), 0, made.error_codes());
    }
    for (item, reads) in items.iter().zip(&batch.per_item).take(sort_up_to) {
        if ctx.deadline.is_some_and(|d| Instant::now() >= d) {
            stopped = Some(Stop::Time);
            break;
        }
        // An operator's cancel is seen between items, as between steps.
        match kastellan_db::tasks::observe_state(r.pool, ctx.task_id).await {
            Ok(state) if state == "cancelled" => {
                stopped = Some(Stop::Cancelled);
                break;
            }
            Ok(_) => {}
            Err(e) => {
                failure = Some(e.into());
                break;
            }
        }
        // A read was asked for but none was made for this item (no read in
        // the list applied to it): say so, or the planner takes it as read.
        let unread_item = usize::from(!p.read.is_empty() && reads.is_empty());
        let cap = SORTER_ITEM_CAP / (1 + reads.len());
        let v = made.item_views(r.dispatcher, ctx.task_id, &over.tool, item, reads.clone(), cap);
        // Counted only once the item is (below): the one the sorter fails on is not.
        let mut count_item = |counts: &mut Counts| {
            counts.looked_at += 1;
            counts.failed_reads += usize::from(!v.failed.is_empty());
            unread += unread_item;
            for code in &v.failed {
                *read_errors.entry(code.clone()).or_default() += 1;
            }
        };
        let row = |kind, fields| NewRow { kind, source: source_of(item, &over.tool), fields, classification: class.as_pascal_str().into() };
        if !v.blocks.is_empty() {
            // Screened at the sorter's own cap, which can block where the
            // planner's view of the same result did not: audit it here.
            audit_blocks(r.pool, ctx.task_id, call.step, "sift_item", &v.blocks).await;
            count_item(&mut counts);
            counts.withheld += 1;
            rows.push(row(RowKind::Withheld, json!({})));
            continue;
        }
        sorter_calls += 1;
        let raw = match sorter.sort(ctx, sorter_message(&p.keep_if, &p.fields, r.now, &v.views)).await {
            Ok(raw) => raw,
            // A sorter that cannot be reached will not be reached for the
            // next item either: stop, rather than spend the deadline on it.
            // This item is not counted, so `next_from` is where it is.
            // The call's timeout is cut to the time left, so a failure past
            // the deadline is the deadline's stop (PR #1036 review).
            Err(e) => {
                *failed_llm_calls = failed_llm_calls.saturating_add(1);
                tracing::warn!(task_id = ctx.task_id, error = %e, "sift: the sorter failed; stopping the sift");
                let late = ctx.deadline.is_some_and(|d| Instant::now() >= d);
                stopped = Some(if late { Stop::Time } else { Stop::SorterFailed });
                sorter_error = Some(e.to_string().chars().take(FAILED_ROW_ERROR_CHARS).collect());
                break;
            }
        };
        count_item(&mut counts);
        match parse_sort_reply(&raw, &p.fields) {
            SortReply::Yes(fields) => {
                counts.kept += 1;
                rows.push(row(RowKind::Kept, Value::Object(fields)));
            }
            SortReply::No => {}
            SortReply::Undecided => {
                // The reply's words stay out of the log: they restate the item.
                tracing::warn!(task_id = ctx.task_id, raw_len = raw.len(), "sift: a sorter reply did not parse; undecided");
                counts.undecided += 1;
            }
        }
    }
    if !rows.is_empty() {
        if let Err(e) = scrapbook::append_rows(r.pool, ctx.task_id, scrap.sift, &rows).await {
            // The earlier failure, if any, is the cause the task fails with;
            // this one is still logged, as it may not be the same outage.
            let e = match failure {
                Some(first) => {
                    tracing::warn!(task_id = ctx.task_id, error = %e, "sift: the scrapbook write failed after an earlier error");
                    first
                }
                None => e.into(),
            };
            return Err(error_stop(r.pool, ctx, head, started, &counts, rows.len(), e).await);
        }
    }
    if let Some(e) = failure {
        return Err(error_stop(r.pool, ctx, head, started, &counts, 0, e).await);
    }

    let remaining = items.len() - counts.looked_at;
    // The items looked at are the batch's first `looked_at`.
    let mut per_read = vec![0usize; p.read.len()];
    for range in batch.per_item.iter().take(counts.looked_at).filter(|r| !r.is_empty()) {
        per_read[batch.reads[range.start].0] += 1;
    }
    // Every item looked at went unread: say why (#956).
    let all_unread = !p.read.is_empty() && counts.looked_at > 0 && unread == counts.looked_at;
    let unread_why = all_unread.then(|| unread_why(&p.read, &items[..counts.looked_at], p.from));
    let elements_unread = batch.elements_unread.iter().take(counts.looked_at).sum();
    let reads = Reads { made: made.outcomes.len(), unread, elements_unread, cut: batch.cut, per_read, errors: read_errors, unread_why };
    let (mut summary, blocks) = summary(&scrap, &p, &counts, &reads, &rows, remaining, stopped);
    // Code-written, so it is never screened away with the sample.
    if let Some(prev) = super::chain::continued(&ctx.plans, &call.parameters, p.from) {
        let distinct = chain_distinct(r.pool, ctx, &prev, scrap.sift, &rows).await;
        summary["chain"] = super::chain::totals(&ctx.plans, &prev, counts.looked_at, counts.kept, distinct);
        summary["continues"] = json!(prev);
    }
    if let Some(note) = &over_older {
        summary["over_older"] = json!(note);
    }
    audit_blocks(r.pool, ctx.task_id, call.step, "sift_summary", &blocks).await;
    let finished = json!({
        "task_id": ctx.task_id, "plan_count": ctx.plan_count, "scrapbook": scrap.to_string(),
        "looked_at": counts.looked_at, "kept": counts.kept, "withheld": counts.withheld,
        "undecided": counts.undecided, "failed_reads": counts.failed_reads, "reads": reads.made,
        "unread": reads.unread, "unread_why": reads.unread_why, "read_errors": reads.errors, "reads_cut": reads.cut, "elements_unread": reads.elements_unread, "items_per_read": reads.per_read, "remaining": remaining,
        "stopped": stopped.map(Stop::as_str), "sorter_calls": sorter_calls, "sorter_error": sorter_error,
        "over_older": over_older, "ms": started.elapsed().as_millis() as u64,
    });
    audit(r.pool, ACTION_SIFT_FINISHED, finished).await;
    if stopped == Some(Stop::Cancelled) {
        return Ok(Executed::Finished(Box::new(execute::Finished {
            outcome: Outcome::Cancelled,
            insight: None,
            skill: None,
            python_skill: None,
        })));
    }
    push_summary(r.pool, ctx, call, class, summary).await;
    // A sift with reads armed this in `review_and_dispatch`; one without
    // has gathered something only if it kept a row.
    st.gathered |= counts.kept > 0;
    Ok(Executed::Dispatched)
}

/// The chain's table rows ([`distinct_kept`]): every earlier link's rows —
/// read back, since one booking emailed twice can sit in two links — and
/// this sift's own (`own`, sift `sift`). `None` if a link's name does not
/// parse or its rows cannot be read.
pub(super) async fn chain_distinct(pool: &PgPool, ctx: &TaskContext, prev: &str, sift: i32, own: &[NewRow]) -> Option<usize> {
    let mut all: Vec<Row> = Vec::new();
    for name in super::chain::expand(&ctx.plans, &ctx.conversation, prev) {
        let Ok(r) = ScrapRef::parse(&name) else {
            tracing::warn!(task_id = ctx.task_id, scrapbook = %name, "sift: a chain link's name does not parse; no `distinct`");
            return None;
        };
        match scrapbook::rows(pool, r.task_id, r.sift).await {
            Ok(rows) => all.extend(rows),
            Err(e) => {
                tracing::warn!(task_id = ctx.task_id, scrapbook = %r, error = %e, "sift: a chain link's rows not read; no `distinct`");
                return None;
            }
        }
    }
    all.extend(as_stored(ctx.task_id, sift, own));
    Some(distinct_kept(&all))
}

/// The reads as one plan, each made with the sift's read it names. Each
/// step carries the sift's class, so the answer's ceiling covers what they
/// read (C1, K7).
fn reads_plan(call: &CallSpec, p: &SiftParams, reads: &[(usize, Value)], class: DataClass) -> Plan {
    let steps = reads
        .iter()
        .map(|(t, parameters)| PlannedStep {
            tool: p.read[*t].tool.clone(),
            method: p.read[*t].method.clone(),
            parameters: parameters.clone(),
            returns: "one candidate's text, for the sift's decision".into(),
            done_when: "read".into(),
            classification: class,
        })
        .collect();
    plan(format!("{SIFT_READS_DECISION}{}", p.keep_if), call.step, steps, class)
}

fn plan(decision: String, step: u32, steps: Vec<PlannedStep>, class: DataClass) -> Plan {
    Plan {
        context: String::new(),
        decision,
        rationale: format!("{NOTEBOOK_STEP_RATIONALE}{step}"),
        steps,
        result: None,
        data_ceiling: Some(class),
        refused: None,
        floor_request: None,
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
        python_skill: None,
    }
}

/// Where an item of `tool`'s output came from: its identifying keys
/// ([`SOURCE_KEYS`]); a scrapbook row's own `source` ([`scrap_parts`]); or `{}`. A bare `id`
/// means something only within one tool, so it is written with the tool:
/// otherwise `{"id": 3}` from two tools would be one table row (#1022).
fn source_of(item: &Value, tool: &str) -> Value {
    if let Some((source, _)) = scrap_parts(item) {
        return source.clone();
    }
    let mut picked: Map<String, Value> = SOURCE_KEYS
        .iter()
        .filter_map(|k| item.get(*k).filter(|v| v.is_string() || v.is_number()).map(|v| (k.to_string(), v.clone())))
        .collect();
    if picked.len() == 1 && picked.contains_key("id") {
        picked.insert(SOURCE_TOOL_KEY.into(), Value::String(tool.into()));
    }
    Value::Object(picked)
}

/// A `policy / injection.blocked` row per block a sift's own screen made:
/// no other screen saw that text at that cap. Best-effort.
async fn audit_blocks(pool: &PgPool, task_id: i64, step: u32, part: &str, blocks: &[SinkBlock]) {
    for block in blocks {
        let payload = notebook_block_audit_payload(task_id, step, part, block);
        if let Err(e) = kastellan_db::audit::insert(pool, "policy", "injection.blocked", payload).await {
            tracing::error!(task_id, error = %e, "sift injection.blocked audit insert failed");
        }
    }
}

/// Record the sift as the step's one call, with the summary as its result,
/// so the next turn's `last_result`, the repeat guard, a resume and the
/// conversation's record all see it as they see any call.
async fn push_summary(pool: &PgPool, ctx: &mut TaskContext, call: &CallSpec, class: DataClass, summary: Value) {
    let step = PlannedStep {
        tool: SIFT_TOOL.into(),
        method: SIFT_METHOD.into(),
        parameters: call.parameters.clone(),
        returns: call.returns.clone(),
        done_when: call.done_when.clone(),
        classification: class,
    };
    let record = PlanRecord::new(plan(call.returns.clone(), call.step, vec![step], class), vec![StepOutcome::Ok(summary)]);
    // As for any recorded call (`execute::review_and_dispatch`): a sample
    // the sink screen withheld is otherwise invisible.
    for payload in record.sink_block_audit_payloads(ctx.task_id, ctx.plan_count) {
        if let Err(e) = kastellan_db::audit::insert(pool, "policy", "injection.blocked", payload).await {
            tracing::error!(task_id = ctx.task_id, error = %e, "sift summary injection.blocked audit insert failed");
        }
    }
    ctx.plans.push(record);
}

/// The `sift.started` row; `reads` is the plan's count. When no reads plan
/// runs (`costed`: none planned, or the reply was late) there is no
/// `plan.formulate` row for this turn, so this one carries the turn's model cost.
fn started_payload(
    ctx: &TaskContext,
    scrap: &ScrapRef,
    p: &SiftParams,
    items: usize,
    reads: usize,
    costed: bool,
    meta: &FormulationMeta,
) -> Value {
    let mut v = json!({
        "task_id": ctx.task_id, "plan_count": ctx.plan_count, "scrapbook": scrap.to_string(),
        "criterion_sha256": format!("{:x}", Sha256::digest(p.keep_if.as_bytes())),
        "items": items, "reads": reads,
        "read_methods": p.read.iter().map(|t| read_key(&t.tool, &t.method)).collect::<Vec<_>>(),
        // The planner-written templates, in `read_methods`' order: without
        // them a sift that made no reads cannot be diagnosed (#956).
        "read_parameters": p.read.iter().map(|t| &t.parameters).collect::<Vec<_>>(),
    });
    if costed {
        v["latency_ms"] = json!(meta.latency_ms);
        v["llm_usage"] = serde_json::to_value(&meta.usage).expect("CompletionStats serialises");
    }
    v
}

/// The `sift.finished` row of a sift an error ended (#956): without it a
/// `sift.started` stands alone. `rows_unwritten` counts the kept and
/// withheld rows the scrapbook did not get; `head` is the row's start (the
/// task, the scrapbook, `over_older`). Returns `e`, for the caller to fail with.
async fn error_stop(
    pool: &PgPool,
    ctx: &TaskContext,
    mut head: Value,
    started: Instant,
    counts: &Counts,
    rows_unwritten: usize,
    e: InnerLoopError,
) -> InnerLoopError {
    let error: String = e.to_string().chars().take(FAILED_ROW_ERROR_CHARS).collect();
    tracing::warn!(task_id = ctx.task_id, error = %error, rows_unwritten, "sift: an error ended the sift");
    for (k, v) in [
        ("stopped", json!(Stop::Error.as_str())),
        ("error", json!(error)),
        ("looked_at", json!(counts.looked_at)),
        ("kept", json!(counts.kept)),
        ("rows_unwritten", json!(rows_unwritten)),
        ("ms", json!(started.elapsed().as_millis() as u64)),
    ] {
        head[k] = v;
    }
    audit(pool, ACTION_SIFT_FINISHED, head).await;
    e
}

/// Best-effort, like the other loop rows: losing one must not fail the task.
async fn audit(pool: &PgPool, action: &str, payload: Value) {
    if let Err(e) = kastellan_db::audit::insert(pool, SCHEDULER_AUDIT_ACTOR, action, payload).await {
        tracing::error!(error = %e, action, "sift audit insert failed");
    }
}

#[cfg(test)]
mod tests;
