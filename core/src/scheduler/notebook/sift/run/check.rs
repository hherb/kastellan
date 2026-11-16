//! Step 1 of a sift (see [`super`]): everything that refuses it before
//! anything runs, and the list it goes through.

use kastellan_db::scrapbook::{self, RowKind};
use serde_json::{json, Value};
use sqlx::PgPool;

use super::super::params::{check_read, parse_params, Over, SiftParams};
use super::super::template::items_at;
use super::super::answer::row_class;
use super::super::{may_read, SIFT_METHOD, SIFT_TOOL};
use super::SiftRun;
use crate::cassandra::types::DataClass;
use crate::handoff;
use crate::scheduler::inner_loop::{StepDispatcher, StepOutcome, TaskContext};
use crate::scheduler::notebook::reply::CallSpec;

/// The list a sift goes through: the items, the tool whose output they are
/// (it selects the screen's profile), and their class.
pub(super) struct OverList {
    pub items: Vec<Value>,
    pub tool: String,
    pub class: DataClass,
}

/// Step 1: everything that refuses a sift before anything runs.
pub(super) async fn check<'a>(
    r: &SiftRun<'a>,
    ctx: &TaskContext,
    call: &CallSpec,
    cfg: &super::super::config::SiftConfig,
) -> Result<(SiftParams, &'a dyn super::super::sorter::Sorter, OverList), String> {
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
pub(super) fn check_from(from: usize, len: usize) -> Result<(), String> {
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
            let record = super::super::chain::list_index(&ctx.plans, *n)
                .map(|i| &ctx.plans[i])
                .ok_or_else(|| format!("step {n} has no successful result to sift"))?;
            // A step that ran an invoked skill (#996) sifts its skill's last
            // step, its product.
            let last = record.outcomes().len() - 1;
            let (Some(StepOutcome::Ok(result)), Some(step)) = (record.outcomes().get(last), record.plan.steps.get(last)) else {
                unreachable!("the record was found by its last outcome being Ok, and outcomes follow steps")
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
