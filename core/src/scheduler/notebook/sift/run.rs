//! A sift in the notebook loop (#922, §6.2).
//!
//! 1. Check the call: its parameters, its reads (K3a), that a sorter exists,
//!    that `over` names a list and that `from` falls inside it. A refusal is
//!    a block (`sift_refused: …`): the loop records the step as failed and
//!    the next turn replans.
//! 2. Expand each item's reads (K3b), as many items as fit in
//!    `KASTELLAN_SIFT_MAX_READS` reads — and never more items than that, read
//!    or not — and review and dispatch them as
//!    **one plan** through the classic loop's own code (K3) — a Block or an
//!    Escalate there behaves exactly as for any plan. A failed read does not
//!    stop the others ([`StepPolicy::SiftReads`]).
//! 3. Show each item (its hit, then its reads, each through the summary's own
//!    screen) to the sorter. An item the screen blocks is withheld and never
//!    sorted (K4). Stop early at the fast lane's deadline, at an operator's
//!    cancel, or when the sorter fails; the item it failed on is not counted,
//!    so `next_from` starts there.
//! 4. Write the kept and withheld items to the scrapbook (K6), and record the
//!    sift as one step whose result is a summary of counts and a sample (K9):
//!    that summary is all the planner sees.

use std::time::Instant;

use kastellan_db::scrapbook::{self, NewRow, RowKind};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use super::params::{check_read, parse_params, read_key, Over, ScrapRef, SiftParams};
use super::sorter::{parse_sort_reply, sorter_message, SortReply, SORTER_ITEM_CAP};
use super::summary::{summary, Reads, Stop};
use super::table::{distinct_kept, Counts};
use super::reads::plan_reads;
use super::template::items_at;
use super::answer::row_class;
use super::{is_sift_plan, may_read, SIFT_METHOD, SIFT_READS_DECISION, SIFT_TOOL};
use crate::cassandra::review::ChainReviewStage;
use crate::handoff;
use crate::cassandra::types::{DataClass, Plan, PlannedStep};
use crate::scheduler::agent::FormulationMeta;
use crate::scheduler::audit::SCHEDULER_AUDIT_ACTOR;
use crate::scheduler::inner_loop::execute::{self, ExecState, Executed, StepPolicy};
use crate::scheduler::inner_loop::{
    notebook_block_audit_payload, sorter_view, InnerLoopError, Outcome, PlanRecord, SinkBlock, StepDispatcher,
    StepOutcome, TaskContext,
};
use crate::scheduler::notebook::formulator::NotebookFormulator;
use crate::scheduler::notebook::reply::{notebook_step_of, CallSpec, NOTEBOOK_STEP_RATIONALE};

/// Audit actions (K12), actor `scheduler`.
pub(crate) const ACTION_SIFT_STARTED: &str = "sift.started";
pub(crate) const ACTION_SIFT_FINISHED: &str = "sift.finished";
pub(crate) const ACTION_SIFT_REFUSED: &str = "sift.refused";

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
            ctx.blocks.push(format!("sift_refused: {why}"));
            return Ok(Executed::Again);
        }
    };
    if r.now.is_none() {
        // No planner timezone: a criterion like "currently due" is judged
        // without today's date (DGX task 307 kept 2017 bills).
        tracing::warn!(task_id = ctx.task_id, "sift: the sorter is not told today's date (no planner timezone configured)");
    }
    let items: Vec<Value> = over.items.into_iter().skip(p.from).collect();
    let batch = plan_reads(&p.read, &items, cfg.max_reads);
    let scrap = ScrapRef { task_id: ctx.task_id, sift: ctx.plan_count as i32 };
    let class = call.classification.max(ctx.classification_floor).max(over.class);
    audit(r.pool, ACTION_SIFT_STARTED, started_payload(ctx, &scrap, &p, items.len(), batch.reads.len(), meta)).await;

    // The reads: one reviewed plan, dispatched past any failed read.
    let (mut outcomes, mut read_tools) = (Vec::new(), Vec::new());
    if !batch.reads.is_empty() {
        let plan = reads_plan(call, &p, &batch.reads, class);
        let policy = StepPolicy::SiftReads { cap: cfg.max_reads };
        match execute::review_and_dispatch(
            r.pool, r.review, r.dispatcher, ctx, plan, meta, false, None, st, r.outbox, policy,
        )
        .await?
        {
            Executed::Dispatched => {}
            other => {
                let stopped = Stop::ReadsNotRun.as_str();
                audit(r.pool, ACTION_SIFT_FINISHED, json!({"task_id": ctx.task_id, "plan_count": ctx.plan_count,
                    "scrapbook": scrap.to_string(), "stopped": stopped, "ms": started.elapsed().as_millis() as u64})).await;
                return Ok(other);
            }
        }
        let record = ctx.plans.last().expect("review_and_dispatch pushed the reads' record");
        outcomes = record.outcomes().to_vec();
        // Each read's tool selects its screen's profile: reads may differ.
        read_tools = record.plan.steps.iter().map(|s| s.tool.clone()).collect();
    }

    // The sorter, one item at a time.
    let (mut counts, mut rows, mut stopped, mut sorter_calls) = (Counts::default(), Vec::new(), None, 0u32);
    let mut unread = 0usize;
    let task_id = ctx.task_id;
    for (item, reads) in items.iter().zip(&batch.per_item) {
        if ctx.deadline.is_some_and(|d| Instant::now() >= d) {
            stopped = Some(Stop::Time);
            break;
        }
        // An operator's cancel is seen between items, as between steps.
        if kastellan_db::tasks::observe_state(r.pool, ctx.task_id).await? == "cancelled" {
            stopped = Some(Stop::Cancelled);
            break;
        }
        // A read was asked for but none was made for this item (no read in
        // the list applied to it): say so, or the planner takes it as read.
        let unread_item = usize::from(!p.read.is_empty() && reads.is_empty());
        let cap = SORTER_ITEM_CAP / (1 + reads.len());
        let (mut views, mut blocks, mut failed_reads) = (Vec::new(), Vec::new(), 0usize);
        let mut view = |tool: &str, v: &Value| match sorter_view(tool, v, cap) {
            Ok(v) => views.push(v),
            Err(b) => blocks.push(b),
        };
        view(&over.tool, item);
        for i in reads.clone() {
            match (outcomes.get(i), read_tools.get(i)) {
                // A read over the dispatch cap came back as its handoff
                // placeholder (#968): read it back whole, so the sorter sees
                // it rendered at its cap, as for any long read.
                (Some(StepOutcome::Ok(v)), Some(tool)) => match handoff::placeholder_ref(v) {
                    None => view(tool, v),
                    Some(h) => match r.dispatcher.stashed(task_id, &h) {
                        Some(whole) => view(tool, &whole),
                        // Evicted (past the task's stash budget): sorting the
                        // placeholder's head as the read would be a silent wrong "no".
                        None => {
                            tracing::warn!(task_id, handoff_ref = h.as_str(), "sift: a stashed read is no longer held; counted as a failed read");
                            failed_reads += 1;
                        }
                    },
                },
                (Some(StepOutcome::Err { .. }), _) => failed_reads += 1,
                _ => {}
            }
        }
        let row = |kind, fields| NewRow { kind, source: source_of(item), fields, classification: class.as_pascal_str().into() };
        if !blocks.is_empty() {
            // Screened at the sorter's own cap, which can block where the
            // planner's view of the same result did not: audit it here.
            audit_blocks(r.pool, ctx.task_id, call.step, "sift_item", &blocks).await;
            counts.looked_at += 1;
            counts.failed_reads += failed_reads;
            unread += unread_item;
            counts.withheld += 1;
            rows.push(row(RowKind::Withheld, json!({})));
            continue;
        }
        sorter_calls += 1;
        let raw = match sorter.sort(ctx, sorter_message(&p.keep_if, &p.fields, r.now, &views)).await {
            Ok(raw) => raw,
            // A sorter that cannot be reached will not be reached for the
            // next item either: stop, rather than spend the deadline on it.
            // This item is not counted, so `next_from` is where it is.
            Err(e) => {
                *failed_llm_calls = failed_llm_calls.saturating_add(1);
                tracing::warn!(task_id = ctx.task_id, error = %e, "sift: the sorter failed; stopping the sift");
                stopped = Some(Stop::SorterFailed);
                break;
            }
        };
        counts.looked_at += 1;
        counts.failed_reads += failed_reads;
        unread += unread_item;
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
        scrapbook::append_rows(r.pool, ctx.task_id, scrap.sift, &rows).await?;
    }

    let remaining = items.len() - counts.looked_at;
    // The items looked at are the batch's first `looked_at`.
    let mut per_read = vec![0usize; p.read.len()];
    for range in batch.per_item.iter().take(counts.looked_at).filter(|r| !r.is_empty()) {
        per_read[batch.reads[range.start].0] += 1;
    }
    let reads = Reads { made: batch.reads.len(), unread, cut: batch.cut, per_read };
    let (mut summary, blocks) = summary(&scrap, &p, &counts, &reads, &rows, remaining, stopped);
    // Code-written, so it is never screened away with the sample.
    if let Some(prev) = super::chain::continued(&ctx.plans, &call.parameters, p.from) {
        let distinct = chain_distinct(r.pool, ctx, &prev, &rows).await;
        summary["chain"] = super::chain::totals(&ctx.plans, &prev, counts.looked_at, counts.kept, distinct);
        summary["continues"] = json!(prev);
    }
    audit_blocks(r.pool, ctx.task_id, call.step, "sift_summary", &blocks).await;
    let finished = json!({
        "task_id": ctx.task_id, "plan_count": ctx.plan_count, "scrapbook": scrap.to_string(),
        "looked_at": counts.looked_at, "kept": counts.kept, "withheld": counts.withheld,
        "undecided": counts.undecided, "failed_reads": counts.failed_reads, "reads": reads.made,
        "unread": reads.unread, "reads_cut": reads.cut, "items_per_read": reads.per_read, "remaining": remaining,
        "stopped": stopped.map(Stop::as_str), "sorter_calls": sorter_calls,
        "ms": started.elapsed().as_millis() as u64,
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

/// The chain's table rows ([`distinct_kept`]): every earlier link's kept rows
/// — read back, since one booking emailed twice can sit in two links — and
/// this sift's own. `None` if a link's rows cannot be read.
async fn chain_distinct(pool: &PgPool, ctx: &TaskContext, prev: &str, own: &[NewRow]) -> Option<usize> {
    let mut fields: Vec<Value> = Vec::new();
    for name in super::chain::expand(&ctx.plans, prev) {
        let Ok(r) = ScrapRef::parse(&name) else {
            tracing::warn!(task_id = ctx.task_id, scrapbook = %name, "sift: a chain link's name does not parse; no `distinct`");
            return None;
        };
        match scrapbook::rows(pool, r.task_id, r.sift).await {
            Ok(rows) => fields.extend(rows.into_iter().filter(|row| row.kind == RowKind::Kept).map(|row| row.fields)),
            Err(e) => {
                tracing::warn!(task_id = ctx.task_id, scrapbook = %r, error = %e, "sift: a chain link's rows not read; no `distinct`");
                return None;
            }
        }
    }
    let own = own.iter().filter(|row| row.kind == RowKind::Kept).map(|row| &row.fields);
    Some(distinct_kept(fields.iter().chain(own)))
}

/// The list a sift goes through: the items, the tool whose output they are
/// (it selects the screen's profile), and their class.
struct OverList {
    items: Vec<Value>,
    tool: String,
    class: DataClass,
}

/// Step 1: everything that refuses a sift before anything runs.
async fn check<'a>(
    r: &SiftRun<'a>,
    ctx: &TaskContext,
    call: &CallSpec,
    cfg: &super::config::SiftConfig,
) -> Result<(SiftParams, &'a dyn super::sorter::Sorter, OverList), String> {
    if call.method != SIFT_METHOD {
        return Err(format!("`{}` is not a method of `{SIFT_TOOL}`: its one method is `{SIFT_METHOD}`", call.method));
    }
    let sorter = r.nb.sorter().ok_or("this planner has no sorter, so it cannot sift")?;
    let p = parse_params(&call.parameters)?;
    for t in &p.read {
        check_read(t, cfg)?;
    }
    let over = resolve_over(r.pool, r.dispatcher, ctx, &p).await?;
    check_from(p.from, over.items.len())?;
    Ok((p, sorter, over))
}

/// A `from` at or past the end of the list would take no items and report
/// an empty sift as a success (DGX task 322: a scrapbook of 27 rows sifted
/// `from` the search's `next_from`, 64). Refuse it, naming the length; an
/// empty list from 0 is a real empty sift and runs.
fn check_from(from: usize, len: usize) -> Result<(), String> {
    if from > 0 && from >= len {
        return Err(format!(
            "`from` {from} is at or past the end of this list ({len} items): `next_from` counts items of the list it came from"
        ));
    }
    Ok(())
}

/// The list `p.over` names. A step's list is the raw result of its last
/// successful call that was not itself a sift (so a second sift over the
/// same step, `from` further on, still finds the search) — read back whole
/// from the handoff cache when it was stashed. A scrapbook's is
/// its kept rows, each `{fields, source}`, if this task may read it (K11);
/// their class is the highest of theirs.
async fn resolve_over(
    pool: &PgPool,
    dispatcher: &dyn StepDispatcher,
    ctx: &TaskContext,
    p: &SiftParams,
) -> Result<OverList, String> {
    match &p.over {
        Over::Step(n) => {
            let record = ctx
                .plans
                .iter()
                .rev()
                .filter(|rec| notebook_step_of(&rec.plan) == Some(*n) && !is_sift_plan(&rec.plan))
                .find(|rec| matches!(rec.outcomes().first(), Some(StepOutcome::Ok(_))))
                .ok_or_else(|| format!("step {n} has no successful result to sift"))?;
            let (Some(StepOutcome::Ok(result)), Some(step)) = (record.outcomes().first(), record.plan.steps.first()) else {
                unreachable!("the record was found by its first outcome being Ok, and outcomes follow steps")
            };
            // A result too big for the planner was stashed whole (DGX task
            // 337, a 68 KB search): the sift reads it back. The stash was not
            // screened past the dispatch screen's window, so each item is
            // screened on its own before the sorter sees it (K4); rows and read
            // parameters taken from items are screened where they are shown
            // (table cells) or reviewed (the reads plan). The list carries the
            // step's class, as the result itself would (K7).
            let stashed = match handoff::placeholder_ref(result) {
                Some(h) => Some(dispatcher.stashed(ctx.task_id, &h).ok_or_else(|| {
                    format!(
                        "step {n}'s result was too big to show and is no longer held: list the same call in `steps` \
under a new id, call it, and sift `over` that id"
                    )
                })?),
                None => None,
            };
            let result = stashed.as_ref().unwrap_or(result);
            Ok(OverList { items: items_at(result, p.items.as_deref())?, tool: step.tool.clone(), class: step.classification })
        }
        Over::Scrapbook(r) => {
            if !may_read(ctx, *r) {
                return Err(format!("scrapbook {r} is not from this conversation"));
            }
            let rows = scrapbook::rows(pool, r.task_id, r.sift).await.map_err(|e| {
                tracing::warn!(task_id = ctx.task_id, scrapbook = %r, error = %e, "sift: scrapbook rows not read");
                format!("scrapbook {r} could not be read")
            })?;
            let kept: Vec<&scrapbook::Row> = rows.iter().filter(|row| row.kind == RowKind::Kept).collect();
            let class = kept.iter().map(|row| row_class(row)).fold(ctx.classification_floor, DataClass::max);
            let items = kept.iter().map(|row| json!({"fields": row.fields, "source": row.source})).collect();
            Ok(OverList { items, tool: SIFT_TOOL.into(), class })
        }
    }
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

/// Where an item came from: its identifying keys ([`SOURCE_KEYS`]); a
/// scrapbook row's own `source`; or `{}`.
fn source_of(item: &Value) -> Value {
    if let (Some(source), Some(_)) = (item.get("source"), item.get("fields")) {
        return source.clone();
    }
    let picked: Map<String, Value> = SOURCE_KEYS
        .iter()
        .filter_map(|k| item.get(*k).filter(|v| v.is_string() || v.is_number()).map(|v| (k.to_string(), v.clone())))
        .collect();
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

/// The `sift.started` row. With no reads there is no `plan.formulate` row
/// for this turn, so this one carries the turn's model cost.
fn started_payload(ctx: &TaskContext, scrap: &ScrapRef, p: &SiftParams, items: usize, reads: usize, meta: &FormulationMeta) -> Value {
    let mut v = json!({
        "task_id": ctx.task_id, "plan_count": ctx.plan_count, "scrapbook": scrap.to_string(),
        "criterion_sha256": format!("{:x}", Sha256::digest(p.keep_if.as_bytes())),
        "items": items, "reads": reads,
        "read_methods": p.read.iter().map(|t| read_key(&t.tool, &t.method)).collect::<Vec<_>>(),
    });
    if reads == 0 {
        v["latency_ms"] = json!(meta.latency_ms);
        v["llm_usage"] = serde_json::to_value(&meta.usage).expect("CompletionStats serialises");
    }
    v
}

/// Best-effort, like the other loop rows: losing one must not fail the task.
async fn audit(pool: &PgPool, action: &str, payload: Value) {
    if let Err(e) = kastellan_db::audit::insert(pool, SCHEDULER_AUDIT_ACTOR, action, payload).await {
        tracing::error!(error = %e, action, "sift audit insert failed");
    }
}

#[cfg(test)]
mod tests;
