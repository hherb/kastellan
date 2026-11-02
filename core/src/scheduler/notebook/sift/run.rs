//! A sift in the notebook loop (#922, §6.2).
//!
//! 1. Check the call: its parameters, its read (K3a), that a sorter exists,
//!    and that `over` names a list. A refusal is a block (`sift_refused: …`):
//!    the loop records the step as failed and the next turn replans.
//! 2. Expand each item's reads (K3b), as many items as fit in
//!    `KASTELLAN_SIFT_MAX_READS` reads, and review and dispatch them as
//!    **one plan** through the classic loop's own code (K3) — a Block or an
//!    Escalate there behaves exactly as for any plan. A failed read does not
//!    stop the others ([`StepPolicy::SiftReads`]).
//! 3. Show each item (its hit, then its reads, each through the summary's own
//!    screen) to the sorter. An item the screen blocks is withheld and never
//!    sorted (K4). Stop early at the fast lane's deadline.
//! 4. Write the kept and withheld items to the scrapbook (K6), and record the
//!    sift as one step whose result is a summary of counts and a sample (K9):
//!    that summary is all the planner sees.

use std::time::Instant;

use kastellan_db::scrapbook::{self, NewRow, RowKind};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use super::params::{check_read, parse_params, Over, ReadTemplate, ScrapRef, SiftParams};
use super::sorter::{parse_sort_reply, sorter_message, SortReply, SORTER_ITEM_CAP};
use super::table::Counts;
use super::template::{expand_reads, items_at};
use super::answer::row_class;
use super::{is_sift_plan, may_read, SIFT_METHOD, SIFT_READS_DECISION, SIFT_TOOL};
use crate::cassandra::review::ChainReviewStage;
use crate::cassandra::types::{DataClass, Plan, PlannedStep};
use crate::scheduler::agent::FormulationMeta;
use crate::scheduler::audit::SCHEDULER_AUDIT_ACTOR;
use crate::scheduler::inner_loop::execute::{self, ExecState, Executed, StepPolicy};
use crate::scheduler::inner_loop::{
    sorter_view, InnerLoopError, PlanRecord, StepDispatcher, StepOutcome, TaskContext,
};
use crate::scheduler::notebook::formulator::NotebookFormulator;
use crate::scheduler::notebook::reply::{notebook_step_of, CallSpec, NOTEBOOK_STEP_RATIONALE};

/// Audit actions (K12), actor `scheduler`.
pub(crate) const ACTION_SIFT_STARTED: &str = "sift.started";
pub(crate) const ACTION_SIFT_FINISHED: &str = "sift.finished";

/// How many kept rows the planner is shown, so it can judge whether the
/// criterion worked (K9).
pub(crate) const SAMPLE_ROWS: usize = 3;

/// The keys of an item that say where it came from, in the order looked for.
pub(crate) const SOURCE_KEYS: [&str; 5] = ["message_id", "id", "url", "path", "handoff_ref"];

/// What a sift needs from the loop: the review chain and the dispatcher for
/// its reads, and the formulator for its sorter and the operator's knobs.
pub(crate) struct SiftRun<'a> {
    pub pool: &'a PgPool,
    pub review: &'a ChainReviewStage,
    pub dispatcher: &'a dyn StepDispatcher,
    pub nb: &'a dyn NotebookFormulator,
    pub outbox: Option<&'a crate::channel::outbox::ChannelOutbox>,
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
            ctx.blocks.push(format!("sift_refused: {why}"));
            return Ok(Executed::Again);
        }
    };
    let items: Vec<Value> = over.items.into_iter().skip(p.from).collect();
    let batch = plan_reads(p.read.as_ref(), &items, cfg.max_reads);
    let scrap = ScrapRef { task_id: ctx.task_id, sift: ctx.plan_count as i32 };
    let class = call.classification.max(ctx.classification_floor).max(over.class);
    audit(r.pool, ACTION_SIFT_STARTED, started_payload(ctx, &scrap, &p, items.len(), batch.reads.len(), meta)).await;

    // The reads: one reviewed plan, dispatched past any failed read.
    let (mut outcomes, mut read_tool) = (Vec::new(), String::new());
    if let (Some(t), false) = (&p.read, batch.reads.is_empty()) {
        let plan = reads_plan(call, &p, t, &batch.reads, class);
        let policy = StepPolicy::SiftReads { cap: cfg.max_reads };
        match execute::review_and_dispatch(
            r.pool, r.review, r.dispatcher, ctx, plan, meta, false, None, st, r.outbox, policy,
        )
        .await?
        {
            Executed::Dispatched => {}
            other => return Ok(other),
        }
        let record = ctx.plans.last().expect("review_and_dispatch pushed the reads' record");
        outcomes = record.outcomes().to_vec();
        read_tool = record.plan.steps.first().map(|s| s.tool.clone()).unwrap_or_default();
    }

    // The sorter, one item at a time.
    let (mut counts, mut rows, mut stopped, mut sorter_calls) = (Counts::default(), Vec::new(), None, 0u32);
    for (item, reads) in items.iter().zip(&batch.per_item) {
        if ctx.deadline.is_some_and(|d| Instant::now() >= d) {
            stopped = Some("time");
            break;
        }
        counts.looked_at += 1;
        let cap = SORTER_ITEM_CAP / (1 + reads.len());
        let mut views = Vec::new();
        let mut blocked = sorter_view(&over.tool, item, cap).map(|v| views.push(v)).is_err();
        for outcome in outcomes.get(reads.clone()).unwrap_or_default() {
            match outcome {
                StepOutcome::Ok(v) => blocked |= sorter_view(&read_tool, v, cap).map(|v| views.push(v)).is_err(),
                StepOutcome::Err { .. } => counts.failed_reads += 1,
            }
        }
        let row = |kind, fields| NewRow { kind, source: source_of(item), fields, classification: class.as_pascal_str().into() };
        if blocked {
            counts.withheld += 1;
            rows.push(row(RowKind::Withheld, json!({})));
            continue;
        }
        sorter_calls += 1;
        match sorter.sort(ctx, sorter_message(&p.keep_if, &p.fields, &views)).await {
            Ok(raw) => match parse_sort_reply(&raw, &p.fields) {
                SortReply::Yes(fields) => {
                    counts.kept += 1;
                    rows.push(row(RowKind::Kept, Value::Object(fields)));
                }
                SortReply::No => {}
                SortReply::Undecided => counts.undecided += 1,
            },
            // A sorter that cannot be reached will not be reached for the
            // next item either: stop, rather than spend the deadline on it.
            Err(e) => {
                *failed_llm_calls = failed_llm_calls.saturating_add(1);
                counts.undecided += 1;
                tracing::warn!(task_id = ctx.task_id, error = %e, "sift: the sorter failed; stopping the sift");
                stopped = Some("sorter_failed");
                break;
            }
        }
    }
    if !rows.is_empty() {
        scrapbook::append_rows(r.pool, ctx.task_id, scrap.sift, &rows).await?;
    }

    let remaining = items.len() - counts.looked_at;
    let summary = summary(&scrap, &p, &counts, &rows, remaining, stopped);
    let finished = json!({
        "task_id": ctx.task_id, "plan_count": ctx.plan_count, "scrapbook": scrap.to_string(),
        "looked_at": counts.looked_at, "kept": counts.kept, "withheld": counts.withheld,
        "undecided": counts.undecided, "failed_reads": counts.failed_reads, "remaining": remaining,
        "stopped": stopped, "sorter_calls": sorter_calls, "ms": started.elapsed().as_millis() as u64,
    });
    audit(r.pool, ACTION_SIFT_FINISHED, finished).await;
    push_summary(r.pool, ctx, call, class, summary).await;
    st.gathered |= counts.kept > 0;
    Ok(Executed::Dispatched)
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
    let sorter = r.nb.sorter().ok_or("this planner has no sorter, so it cannot sift")?;
    let p = parse_params(&call.parameters)?;
    if let Some(t) = &p.read {
        check_read(t, cfg)?;
    }
    let over = resolve_over(r.pool, ctx, &p).await?;
    Ok((p, sorter, over))
}

/// The list `p.over` names. A step's list is the raw result of its last
/// successful call that was not itself a sift (so a second sift over the
/// same step, `from` further on, still finds the search). A scrapbook's is
/// its kept rows, each `{fields, source}`, if this task may read it (K11);
/// their class is the highest of theirs.
async fn resolve_over(pool: &PgPool, ctx: &TaskContext, p: &SiftParams) -> Result<OverList, String> {
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

/// The reads to make, and which of them belong to each item taken.
struct Batch {
    reads: Vec<Value>,
    /// Item `i`'s reads are `reads[per_item[i]]`. One entry per item taken.
    per_item: Vec<std::ops::Range<usize>>,
}

/// Take items in list order while their reads fit in `cap`; an item's reads
/// stay together. A first item with more reads than `cap` alone keeps its
/// first `cap`, so a sift always makes progress. Pure.
fn plan_reads(read: Option<&ReadTemplate>, items: &[Value], cap: usize) -> Batch {
    let mut batch = Batch { reads: Vec::new(), per_item: Vec::new() };
    for item in items {
        let mut reads = read.map(|t| expand_reads(t, item)).unwrap_or_default();
        if batch.reads.len() + reads.len() > cap {
            if !batch.per_item.is_empty() {
                break;
            }
            reads.truncate(cap);
        }
        let start = batch.reads.len();
        batch.reads.extend(reads);
        batch.per_item.push(start..batch.reads.len());
    }
    batch
}

/// The reads as one plan. Each step carries the sift's class, so the
/// answer's ceiling covers what they read (C1, K7).
fn reads_plan(call: &CallSpec, p: &SiftParams, t: &ReadTemplate, reads: &[Value], class: DataClass) -> Plan {
    let steps = reads
        .iter()
        .map(|parameters| PlannedStep {
            tool: t.tool.clone(),
            method: t.method.clone(),
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

/// What the planner is shown (K9): the ref, the criterion, the counts, and a
/// sample of what was kept. Screened when it is recorded, like any result.
fn summary(
    scrap: &ScrapRef,
    p: &SiftParams,
    c: &Counts,
    rows: &[NewRow],
    remaining: usize,
    stopped: Option<&str>,
) -> Value {
    let sample: Vec<Value> = rows
        .iter()
        .filter(|r| r.kind == RowKind::Kept)
        .take(SAMPLE_ROWS)
        .map(|r| json!({"fields": r.fields, "source": r.source}))
        .collect();
    let mut s = json!({
        "scrapbook": scrap.to_string(), "criterion": p.keep_if, "fields": p.fields,
        "looked_at": c.looked_at, "kept": c.kept, "withheld": c.withheld, "undecided": c.undecided,
        "failed_reads": c.failed_reads, "remaining": remaining, "sample": sample,
    });
    if remaining > 0 {
        s["next_from"] = json!(p.from + c.looked_at);
    }
    if let Some(why) = stopped {
        s["stopped"] = json!(why);
    }
    s
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
        "read_tool": p.read.as_ref().map(|t| &t.tool), "read_method": p.read.as_ref().map(|t| &t.method),
    });
    if reads == 0 {
        v["latency_ms"] = json!(meta.latency_ms);
        v["llm_usage"] = serde_json::to_value(&meta.usage).unwrap_or(Value::Null);
    }
    v
}

/// Best-effort, like the other loop rows: losing one must not fail the task.
async fn audit(pool: &PgPool, action: &str, payload: Value) {
    if let Err(e) = kastellan_db::audit::insert(pool, SCHEDULER_AUDIT_ACTOR, action, payload).await {
        tracing::error!(error = %e, action, "sift audit insert failed");
    }
}
