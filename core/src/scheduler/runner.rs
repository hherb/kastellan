//! Per-lane runner loop and the public `spawn_scheduler` entry point
//! that the daemon's `main.rs` calls after the pool comes up.

use std::sync::Arc;

use sqlx::postgres::PgListener;
use sqlx::PgPool;
use time::OffsetDateTime;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{sleep, Duration};

use crate::entity_extraction::EntityExtractor;
use crate::memory::embedder::Embedder;

use kastellan_db::tasks::{self, Lane, Task, DEFAULT_DEADLINE_FAST_S, DEFAULT_DEADLINE_LONG_S,
    DEFAULT_MAX_PLANS_FAST, DEFAULT_MAX_PLANS_LONG};

use crate::cassandra::review::ChainReviewStage;
use crate::cassandra::types::DataClass;

use super::agent::PlanFormulator;
use super::audit::{
    action_task_terminal, build_finalize_payload, build_lifecycle_payload, build_l1_write_payload,
    build_l3_write_payload, compute_duration_ms, TaskFinalizeStats, ACTION_L1_PROMOTED,
    ACTION_L3_CRYSTALLISED, ACTION_TASK_FINALIZE, ACTION_TASK_RUNNING, SCHEDULER_AUDIT_ACTOR,
};
use super::inner_loop::{
    run_to_terminal, ClassificationFloorSource, InnerLoopResult, Outcome, StepDispatcher,
    TaskContext,
};

/// Heartbeat interval for catch-up SELECT in case a `tasks_inserted`
/// NOTIFY was lost across a listener reconnect.
const HEARTBEAT: Duration = Duration::from_secs(30);

pub struct SchedulerHandle {
    shutdown: watch::Sender<bool>,
    pub fast: JoinHandle<()>,
    pub long: JoinHandle<()>,
}

impl SchedulerHandle {
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        let _ = self.fast.await;
        let _ = self.long.await;
    }
}

/// Spawn the two lane runners. Returns a handle the daemon's
/// shutdown path uses to flip the watch channel and join.
pub fn spawn_scheduler(
    pool: PgPool,
    formulator: Arc<dyn PlanFormulator>,
    review: Arc<ChainReviewStage>,
    dispatcher: Arc<dyn StepDispatcher>,
    entity_extractor: Arc<dyn EntityExtractor>,
    embedder: Arc<dyn Embedder>,
) -> SchedulerHandle {
    let (tx, rx) = watch::channel(false);

    let fast = tokio::spawn(lane_loop(
        pool.clone(), formulator.clone(), review.clone(), dispatcher.clone(),
        entity_extractor.clone(), embedder.clone(),
        Lane::Fast, DEFAULT_DEADLINE_FAST_S, DEFAULT_MAX_PLANS_FAST, rx.clone(),
    ));
    let long = tokio::spawn(lane_loop(
        pool, formulator, review, dispatcher,
        entity_extractor, embedder,
        Lane::Long, DEFAULT_DEADLINE_LONG_S, DEFAULT_MAX_PLANS_LONG, rx,
    ));

    SchedulerHandle { shutdown: tx, fast, long }
}

// Five of the nine params are the shared scheduler dependencies
// (pool + the four stage handles); the rest are the per-lane tuning
// constants. They are genuinely distinct inputs to the loop, so the
// arg-count heuristic is suppressed rather than papered over with a
// dependency-bundle struct that would only move the list to the call site.
#[allow(clippy::too_many_arguments)]
async fn lane_loop(
    pool: PgPool,
    formulator: Arc<dyn PlanFormulator>,
    review: Arc<ChainReviewStage>,
    dispatcher: Arc<dyn StepDispatcher>,
    entity_extractor: Arc<dyn EntityExtractor>,
    embedder: Arc<dyn Embedder>,
    lane: Lane,
    deadline_seconds: i64,
    max_plans: u32,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut listener = match PgListener::connect_with(&pool).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("scheduler[{}]: PgListener connect failed: {e}", lane.as_sql());
            return;
        }
    };
    if let Err(e) = listener.listen("tasks_inserted").await {
        eprintln!("scheduler[{}]: LISTEN tasks_inserted failed: {e}", lane.as_sql());
        return;
    }
    if let Err(e) = listener.listen("tasks_cancelled").await {
        eprintln!("scheduler[{}]: LISTEN tasks_cancelled failed: {e}", lane.as_sql());
        return;
    }

    // Initial drain: a task inserted *before* the LISTEN above does
    // not produce a NOTIFY visible to this listener (PG does not queue
    // notifications for late subscribers). Without this, a daemon
    // restart with pending tasks would wait one full HEARTBEAT before
    // picking them up — and tests that race the scheduler against
    // pre-inserted rows would time out. Doing the drain *after* LISTEN
    // is what keeps the drain race-free with newly-arriving tasks.
    drain_lane(
        &pool, formulator.clone(), review.clone(), dispatcher.clone(),
        entity_extractor.clone(), embedder.clone(),
        lane, deadline_seconds, max_plans, &shutdown,
    ).await;
    if *shutdown.borrow() { return; }

    loop {
        // Wait for a wake-up: shutdown, NOTIFY, or heartbeat.
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
            }
            _ = listener.recv() => { /* fall through to drain */ }
            _ = sleep(HEARTBEAT) => { /* fall through */ }
        }

        drain_lane(
            &pool, formulator.clone(), review.clone(), dispatcher.clone(),
            entity_extractor.clone(), embedder.clone(),
            lane, deadline_seconds, max_plans, &shutdown,
        ).await;
    }
}

/// Drain every pending task on `lane` until `claim_one` returns `None`.
/// Pulled out of `lane_loop` so the same body runs both in the initial
/// startup pass and on each NOTIFY/heartbeat wake. Honours `shutdown`
/// between every claim.
// Same nine-input shape as `lane_loop` (it forwards them straight
// through); see the note there for why the arg-count heuristic is
// suppressed instead of bundled.
#[allow(clippy::too_many_arguments)]
async fn drain_lane(
    pool: &PgPool,
    formulator: Arc<dyn PlanFormulator>,
    review: Arc<ChainReviewStage>,
    dispatcher: Arc<dyn StepDispatcher>,
    entity_extractor: Arc<dyn EntityExtractor>,
    embedder: Arc<dyn Embedder>,
    lane: Lane,
    deadline_seconds: i64,
    max_plans: u32,
    shutdown: &watch::Receiver<bool>,
) {
    loop {
        if *shutdown.borrow() { return; }
        let claimed = match tasks::claim_one(pool, lane, deadline_seconds).await {
            Ok(Some(t)) => t,
            Ok(None) => return,  // nothing pending; caller goes back to listener
            Err(e) => {
                eprintln!("scheduler[{}]: claim_one error: {e}", lane.as_sql());
                return;
            }
        };

        // Spec §7 lifecycle row: pending → running.
        // Best-effort: a DB error logging the transition must not
        // prevent us from running the task (the canonical state lives
        // in `tasks.state`; the audit row is for observation phase).
        //
        // `claimed.plan_count` is the value carried by the row from
        // the DB at claim time. For a freshly-inserted pending task
        // this is 0; for a task resumed after crash recovery (future
        // work; `sweep_crashed` does not yet re-enqueue), it would be
        // the count from before the crash — i.e. "plans run so far"
        // rather than "plans run this session".
        write_lifecycle_row(
            pool,
            ACTION_TASK_RUNNING,
            claimed.id,
            claimed.lane,
            claimed.plan_count,
        ).await;

        // Operator-submitted L3 skill run (issue #179): execute in-daemon
        // against the live registry, then finalize. A refusal still finalizes
        // `completed` — it is a valid outcome the CLI renders, not a crash. The
        // task *state* deliberately does not encode refused-vs-ran: the
        // task ran to a well-defined conclusion, and the refused/dry-run/executed
        // distinction lives in `tasks.result` as the serialized
        // `InvokeReport` variant (`Refused` / `DryRun` / `Executed`). The
        // separate trust trail is the `l3.invoke_rejected` audit row
        // `invoke_l3` writes for every refusal (a security event worth its own
        // row), so a refused run is fully observable without overloading the
        // task lifecycle state.
        // Skips `run_one` and the agent-task post-run hooks below (the
        // finalize-summary row, L1/L3 crystallisation) — an operator skill run
        // is not an agent task; its audit trail is the running/terminal
        // lifecycle rows here plus the `l3.invoked` / `l3.invoke_outcome` /
        // `l3.invoke_rejected` rows that `invoke_l3` writes.
        if crate::scheduler::l3_run::is_l3_run_payload(&claimed.payload) {
            let report = crate::scheduler::l3_run::run_l3_run_task(
                pool,
                dispatcher.as_ref(),
                &claimed.payload,
            )
            .await;
            // Serialization of a plain-serde InvokeReport is effectively
            // infallible; warn (rather than silently NULL the result) so a
            // future non-serializable variant can't fail invisibly.
            let result_payload = match serde_json::to_value(&report) {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(
                        task_id = claimed.id, error = %e,
                        "l3_run: could not serialize InvokeReport (result_payload will be NULL)"
                    );
                    None
                }
            };
            if let Err(e) =
                tasks::finalize(pool, claimed.id, "completed", result_payload).await
            {
                tracing::warn!(
                    lane = lane.as_sql(), task_id = claimed.id, error = %e,
                    "l3_run finalize UPDATE failed"
                );
            }
            write_lifecycle_row(
                pool,
                &action_task_terminal("completed"),
                claimed.id,
                claimed.lane,
                0,  // l3_run runs no agent plans; 0 is the correct terminal count
            )
            .await;
            continue;
        }

        let result = run_one(
            pool, formulator.clone(), review.clone(), dispatcher.clone(),
            &claimed, max_plans,
        ).await;

        let final_state = result.outcome.final_state();
        let final_result_payload = result.outcome.result_payload();

        // Capture `finished_at` *before* `tasks::finalize` so the
        // finalize-payload's `total_duration_ms` measures inner-loop
        // wall time only — not inner-loop wall time + the finalize
        // UPDATE's round-trip latency. On a contended DB the latter
        // can be the dominant term and would silently bias the
        // observation-phase latency distribution.
        let finished_at = OffsetDateTime::now_utc();

        if let Err(e) = tasks::finalize(pool, claimed.id, final_state, final_result_payload).await {
            tracing::warn!(
                lane = lane.as_sql(), task_id = claimed.id, error = %e,
                "tasks::finalize UPDATE failed (audit lifecycle row still emitted)"
            );
        }

        // Spec §7 lifecycle row: running → <terminal state>. Fires
        // even if the `finalize` UPDATE was a no-op (e.g. the task
        // was already cancelled out from under us by a CLI cancel
        // racing the inner loop) — the scheduler still *observed*
        // this transition, and that's what the actor='scheduler' row
        // records. See [`super::audit`] module docs for the wider
        // audit-vs-DB-state divergence note. Best-effort, same as the
        // `running` row above.
        write_lifecycle_row(
            pool,
            &action_task_terminal(final_state),
            claimed.id,
            claimed.lane,
            i32::try_from(result.plan_count).unwrap_or(i32::MAX),
        ).await;

        // Spec §7 finalize summary row. Best-effort: the lifecycle
        // row above carries the headline state; this row carries the
        // counters for per-task latency analysis.
        write_finalize_row(pool, &claimed, final_state, &result, finished_at).await;

        // Agent-raised L1 promotion. Best-effort: a degraded write
        // never aborts task finalize. The terminal plan's `l1_insight`
        // is captured by the inner loop into `result.terminal_l1_insight`
        // only when Outcome::Completed; all other outcomes leave the
        // field None, so this branch is a no-op for them.
        if let Some(insight) = result.terminal_l1_insight.as_deref() {
            write_l1_promoted_row(pool, &*entity_extractor, &*embedder, claimed.id, insight).await;
        }

        // Agent-raised L3 skill crystallisation. Best-effort, same
        // posture as the L1 hook. terminal_l3_skill is Some only on
        // Outcome::Completed + dispatch_count >= 1 (the grounding gate);
        // all other outcomes leave it None, so this is a no-op for them.
        if let Some(skill) = result.terminal_l3_skill.as_ref() {
            write_l3_crystallised_row(pool, claimed.id, skill).await;
        }

        // Agent-raised Python-skill crystallisation. Same best-effort posture
        // as the L1/L3 hooks; Some only on Outcome::Completed + dispatch_count>=1.
        if let Some(skill) = result.terminal_python_skill.as_ref() {
            write_python_skill_crystallised_row(pool, claimed.id, skill).await;
        }
    }
}

/// Insert a `scheduler/task.<...>` lifecycle row. Errors are logged
/// at WARN and swallowed — the canonical lifecycle state lives in the
/// `tasks` table and the row is an observation-phase aid, not a
/// correctness signal.
async fn write_lifecycle_row(
    pool: &PgPool,
    action: &str,
    task_id: i64,
    lane: Lane,
    plan_count: i32,
) {
    let payload = build_lifecycle_payload(task_id, lane, plan_count);
    if let Err(e) =
        kastellan_db::audit::insert(pool, SCHEDULER_AUDIT_ACTOR, action, payload).await
    {
        tracing::warn!(
            task_id, action, error = %e,
            "audit insert for scheduler lifecycle row failed (best-effort)"
        );
    }
}

/// Insert the per-task `scheduler/task.finalize` summary row. Best-
/// effort, same posture as [`write_lifecycle_row`].
async fn write_finalize_row(
    pool: &PgPool,
    claimed: &Task,
    final_state: &str,
    result: &InnerLoopResult,
    finished_at: OffsetDateTime,
) {
    let stats = TaskFinalizeStats {
        // `result.plan_count` is the inner loop's u32 counter; the DB
        // column is i32. The cap on plans is small (single digits in
        // practice), so the saturation is operationally dead code, but
        // a silent `as i32` truncation would be a subtle bug if a
        // future change ever lifted the cap.
        plan_count: i32::try_from(result.plan_count).unwrap_or(i32::MAX),
        total_llm_calls: result.plan_count,
        total_dispatch_calls: result.dispatch_count,
        total_duration_ms: compute_duration_ms(claimed.started_at, finished_at),
        started_at: claimed.started_at,
        finished_at,
    };
    let payload = build_finalize_payload(claimed.id, claimed.lane, final_state, &stats);
    if let Err(e) = kastellan_db::audit::insert(
        pool, SCHEDULER_AUDIT_ACTOR, ACTION_TASK_FINALIZE, payload,
    ).await {
        tracing::warn!(
            task_id = claimed.id, state = final_state, error = %e,
            "audit insert for scheduler task.finalize row failed (best-effort)"
        );
    }
}

/// Best-effort agent-raised L1 promotion writer. Called by
/// [`drain_lane`] after the `task.finalize` audit row is written.
///
/// Posture: errors are logged at WARN and swallowed. The task is
/// already finalized in the canonical `tasks` table; the L1 row +
/// audit row are observability aids, not correctness signals.
/// Validation errors from `promote_l1` are also swallowed (with
/// distinct WARN diagnostics so the operator can see which path failed).
async fn write_l1_promoted_row(
    pool: &PgPool,
    extractor: &dyn EntityExtractor,
    embedder: &dyn Embedder,
    task_id: i64,
    insight: &str,
) {
    use crate::memory::l1_promote::{promote_l1, L1Error, L1Source};

    let source = L1Source::AgentRaised { task_id };
    let outcome = match promote_l1(pool, extractor, embedder, insight, source.clone()).await {
        Ok(o) => o,
        Err(L1Error::Validation(msg)) => {
            tracing::warn!(
                task_id, error = %msg,
                "agent-raised L1 promotion rejected on validation (skipping audit row)"
            );
            return;
        }
        Err(L1Error::Db(e)) => {
            tracing::warn!(
                task_id, error = %e,
                "agent-raised L1 promotion DB error (skipping audit row)"
            );
            return;
        }
    };

    let body_sha256 = crate::memory::l1_promote::compute_body_sha256(insight.trim());
    let payload = build_l1_write_payload(&outcome, &source, &body_sha256);

    if let Err(e) = kastellan_db::audit::insert(
        pool, SCHEDULER_AUDIT_ACTOR, ACTION_L1_PROMOTED, payload,
    ).await {
        tracing::warn!(
            task_id, error = %e,
            "audit insert for scheduler l1.promoted row failed (best-effort)"
        );
    }
}

/// Crystallise the agent-raised L3 skill + emit one `actor='scheduler'
/// action='l3.crystallised'` audit row. Best-effort: errors (validation
/// or DB) are logged at WARN and swallowed — the task is already
/// finalized; the L3 row + audit row are observability aids, not
/// correctness signals.
async fn write_l3_crystallised_row(
    pool: &PgPool,
    task_id: i64,
    skill: &crate::cassandra::types::L3SkillCandidate,
) {
    use crate::memory::l3_crystallise::{
        compute_template_sha256, crystallise_l3, validate_l3_skill, L3Error, L3Source,
    };

    let source = L3Source::AgentRaised { task_id };
    let outcome = match crystallise_l3(pool, skill, source.clone()).await {
        Ok(o) => o,
        Err(L3Error::Validation(msg)) => {
            tracing::warn!(
                task_id, error = %msg,
                "agent-raised L3 crystallisation rejected on validation (skipping audit row)"
            );
            return;
        }
        Err(L3Error::Db(e)) => {
            tracing::warn!(
                task_id, error = %e,
                "agent-raised L3 crystallisation DB error (skipping audit row)"
            );
            return;
        }
    };

    // Recompute over the SAME normalised candidate the writer stored, so
    // the audited body_sha256 + skill_name match the stored row exactly.
    // crystallise_l3 already validated successfully above, so this
    // re-validation cannot fail; the Err arm is defensive/unreachable.
    let normalised = match validate_l3_skill(skill) {
        Ok(n) => n,
        Err(_) => return,
    };
    let body_sha256 = compute_template_sha256(&normalised);
    let payload = build_l3_write_payload(&outcome, &source, &normalised.name, &body_sha256);

    if let Err(e) = kastellan_db::audit::insert(
        pool, SCHEDULER_AUDIT_ACTOR, ACTION_L3_CRYSTALLISED, payload,
    ).await {
        tracing::warn!(
            task_id, error = %e,
            "audit insert for scheduler l3.crystallised row failed (best-effort)"
        );
    }
}

/// Crystallise the agent-raised Python skill + emit one `actor='scheduler'
/// action='l3.crystallised'` audit row carrying `kind: "python"`. Best-effort
/// (validation/DB errors logged at WARN and swallowed), mirroring
/// [`write_l3_crystallised_row`].
async fn write_python_skill_crystallised_row(
    pool: &PgPool,
    task_id: i64,
    skill: &crate::cassandra::types::PythonSkillCandidate,
) {
    use crate::memory::l3_crystallise::L3Source;
    use crate::memory::l3py_crystallise::{
        compute_python_sha256, crystallise_python_skill, validate_python_skill, PyError,
        PyWriteOutcome,
    };

    let source = L3Source::AgentRaised { task_id };
    let outcome = match crystallise_python_skill(pool, skill, source.clone()).await {
        Ok(o) => o,
        Err(PyError::Validation(msg)) => {
            tracing::warn!(
                task_id,
                error = %msg,
                "agent-raised python skill rejected on validation (skipping audit row)"
            );
            return;
        }
        Err(PyError::Db(e)) => {
            tracing::warn!(
                task_id,
                error = %e,
                "agent-raised python skill DB error (skipping audit row)"
            );
            return;
        }
    };

    // Recompute over the SAME normalised candidate the writer stored, so
    // the audited body_sha256 + skill_name match the stored row exactly.
    // crystallise_python_skill already validated successfully above, so
    // this re-validation cannot fail; the Err arm is defensive/unreachable.
    let normalised = match validate_python_skill(skill) {
        Ok(n) => n,
        Err(_) => return,
    };
    let body_sha256 = compute_python_sha256(&normalised);

    // Reuse the L3 crystallise payload shape; PyWriteOutcome maps 1:1 to
    // the L3WriteOutcome arms the builder expects. Add `kind: "python"` so
    // the audit tail can distinguish Python skills from templated ones.
    let l3_outcome = match outcome {
        PyWriteOutcome::Inserted { memory_id } => {
            crate::memory::l3_crystallise::L3WriteOutcome::Inserted { memory_id }
        }
        PyWriteOutcome::SkippedDuplicate { memory_id } => {
            crate::memory::l3_crystallise::L3WriteOutcome::SkippedDuplicate { memory_id }
        }
    };
    let mut payload =
        build_l3_write_payload(&l3_outcome, &source, &normalised.name, &body_sha256);
    if let serde_json::Value::Object(ref mut m) = payload {
        m.insert("kind".into(), serde_json::Value::String("python".into()));
    }

    if let Err(e) = kastellan_db::audit::insert(
        pool,
        SCHEDULER_AUDIT_ACTOR,
        ACTION_L3_CRYSTALLISED,
        payload,
    )
    .await
    {
        tracing::warn!(
            task_id,
            error = %e,
            "audit insert for scheduler python l3.crystallised row failed (best-effort)"
        );
    }
}

async fn run_one(
    pool: &PgPool,
    formulator: Arc<dyn PlanFormulator>,
    review: Arc<ChainReviewStage>,
    dispatcher: Arc<dyn StepDispatcher>,
    task: &Task,
    max_plans: u32,
) -> InnerLoopResult {
    let instruction = task.payload.get("instruction")
        .and_then(|v| v.as_str()).unwrap_or("").to_string();

    // SECURITY: an unrecognised classification_floor is a hard error —
    // silently defaulting to Public would downgrade clinically-classified
    // data into the lowest review band. Field absence is the producer
    // opting out (treated as no floor / Public); a present-but-bad value
    // is a producer bug that must surface.
    let classification_floor = match task.payload.get("classification_floor") {
        None => DataClass::Public,
        Some(v) => {
            let Some(s) = v.as_str() else {
                return failed_result(format!(
                    "classification_floor in payload is not a string: {v:?}"
                ));
            };
            match serde_json::from_str::<DataClass>(&format!("\"{}\"", s)) {
                Ok(dc) => dc,
                Err(_) => return failed_result(format!(
                    "unknown classification_floor: {s:?} (expected one of \
                     Public, Personal, ClinicalConfidential, Secret)"
                )),
            }
        }
    };
    // Provenance: source defaults to "default" when absent. Validation
    // lives in the pure helper `parse_classification_floor_source_from_payload`
    // so it can be unit-tested without seeding a task in Postgres.
    let classification_floor_source = match parse_classification_floor_source_from_payload(
        task.payload.get("classification_floor_source"),
    ) {
        Ok(src) => src,
        Err(detail) => return failed_result(detail),
    };
    // Signals: empty array iff absent or not an array. Each entry must
    // be a string; non-string entries are skipped silently (better than
    // failing the task on a non-load-bearing presentation field).
    let classification_floor_signals: Vec<String> = task.payload
        .get("classification_floor_signals")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect())
        .unwrap_or_default();

    // Bound the override by u32::MAX explicitly: an `as u32` cast would
    // silently roll over a producer-supplied 2^33 to a small number,
    // which then *under*shoots the lane default. Falling back to the
    // lane default on any out-of-range value keeps behaviour predictable.
    let max_plans_override = task.payload.get("max_plans")
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(max_plans);

    let ctx = TaskContext {
        task_id: task.id,
        lane: task.lane,
        instruction,
        classification_floor,
        classification_floor_source,
        classification_floor_signals,
        plans: vec![],
        advisories: vec![],
        blocks: vec![],
        plan_count: 0,
        max_plans: max_plans_override,
    };

    let task_id = ctx.task_id;
    let dispatcher_for_purge = std::sync::Arc::clone(&dispatcher);
    let result = match run_to_terminal(pool, formulator, review, dispatcher, ctx).await {
        Ok(r) => r,
        Err(e) => failed_result(format!("inner_loop: {e}")),
    };
    dispatcher_for_purge.purge_task(task_id);
    result
}

/// Build an `InnerLoopResult` representing a `Failed` outcome with
/// zero counters. Used at the pre-loop validation points in
/// [`run_one`] (bad payload shape, classification override) where the
/// inner loop never runs — counters are 0 in those branches.
fn failed_result(detail: String) -> InnerLoopResult {
    InnerLoopResult {
        outcome: Outcome::Failed(detail),
        plan_count: 0,
        dispatch_count: 0,
        terminal_l1_insight: None,
        terminal_l3_skill: None,
        terminal_python_skill: None,
    }
}

// Production `StepDispatcher`: see [`super::tool_dispatch::ToolHostStepDispatcher`]
// (moved out of this file 2026-05-11 when the placeholder was replaced
// with the real `tool_host::dispatch` wiring — Task 3.2.bis).

/// Parse the producer-supplied `classification_floor_source` payload
/// field at task-entry time.
///
/// Semantics:
/// - **Absent (`None`)** → `Ok(Default)`. The producer opted out of
///   provenance; the floor was not set by inference or operator flag.
/// - **`"operator"`** → `Ok(Operator)`. Operator pinned the floor via
///   `kastellan-cli ask --classification-floor X`.
/// - **`"cli_inferred"`** → `Ok(CliInferred)`. The CLI's
///   `classification_inference` keyword classifier elevated above Public.
/// - **`"default"`** → `Ok(Default)`. Explicit "no provenance" — same
///   semantic as absent.
/// - **`"agent_raised"`** → `Err`. Reserved for the inner loop's
///   [`super::inner_loop::apply_floor_raise`]; any producer that writes
///   it directly is forging audit-trail provenance ([issue #71]).
///   The producer cannot raise the floor — only the agent can, via
///   `Plan.floor_request`, and the inner loop is the only legitimate
///   writer of `AgentRaised`. Fail-closed at entry so the audit-log
///   contract cannot be silently misattributed.
/// - **Non-string JSON value** → `Err`. Payload shape error.
/// - **Unknown string** → `Err`. Producer-bug surface; surfaces the
///   bad value so a misspelt token is easy to spot in the failure
///   message.
///
/// All `Err` variants carry a human-readable diagnostic suitable for
/// passing straight into [`failed_result`].
///
/// Pure function: no I/O, no side effects. Renaming any branch of
/// [`ClassificationFloorSource`] is an audit-trail contract break; the
/// reject here matches on the parsed variant (not the wire string) so a
/// rename of `AgentRaised` + its serde tag propagates automatically.
///
/// [issue #71]: https://github.com/hherb/kastellan/issues/71
fn parse_classification_floor_source_from_payload(
    value: Option<&serde_json::Value>,
) -> Result<ClassificationFloorSource, String> {
    let Some(v) = value else {
        return Ok(ClassificationFloorSource::Default);
    };
    let Some(s) = v.as_str() else {
        return Err(format!(
            "classification_floor_source in payload is not a string: {v:?}"
        ));
    };
    // Parse first, then reject the `AgentRaised` variant on a structural
    // match. Binding the reject to the enum variant (rather than a
    // string literal) means a future rename of `AgentRaised` + its
    // serde tag + `as_snake_str` continues to be rejected here without
    // a parallel edit. The dedicated diagnostic is preserved so an
    // operator grepping the daemon journal for "reserved" still finds
    // this site.
    match serde_json::from_value::<ClassificationFloorSource>(v.clone()) {
        Ok(ClassificationFloorSource::AgentRaised) => Err(format!(
            "classification_floor_source = {:?} is reserved for the inner \
             loop's apply_floor_raise — producers must not supply it. \
             Use operator / cli_inferred / default at submission time.",
            ClassificationFloorSource::AgentRaised.as_snake_str(),
        )),
        Ok(src) => Ok(src),
        Err(_) => Err(format!(
            "unknown classification_floor_source: {s:?} (expected one of \
             operator, cli_inferred, default)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn absent_payload_field_parses_as_default() {
        // The producer opted out of provenance — no operator flag, no
        // CLI inference matched. `Default` is the documented absent-case
        // sentinel.
        let got = parse_classification_floor_source_from_payload(None).unwrap();
        assert_eq!(got, ClassificationFloorSource::Default);
    }

    #[test]
    fn operator_string_parses_as_operator() {
        let v = json!("operator");
        let got = parse_classification_floor_source_from_payload(Some(&v)).unwrap();
        assert_eq!(got, ClassificationFloorSource::Operator);
    }

    #[test]
    fn cli_inferred_string_parses_as_cli_inferred() {
        let v = json!("cli_inferred");
        let got = parse_classification_floor_source_from_payload(Some(&v)).unwrap();
        assert_eq!(got, ClassificationFloorSource::CliInferred);
    }

    #[test]
    fn default_string_parses_as_default() {
        let v = json!("default");
        let got = parse_classification_floor_source_from_payload(Some(&v)).unwrap();
        assert_eq!(got, ClassificationFloorSource::Default);
    }

    #[test]
    fn agent_raised_string_is_rejected_as_reserved() {
        // Issue #71: producers must not be able to forge `agent_raised`
        // provenance. The inner loop's `apply_floor_raise` is the only
        // legitimate writer. The error string must mention "reserved"
        // so an operator searching the daemon journal can find this
        // site without reading the code.
        let v = json!("agent_raised");
        let err = parse_classification_floor_source_from_payload(Some(&v)).unwrap_err();
        assert!(
            err.contains("agent_raised"),
            "error must echo the rejected value: {err}",
        );
        assert!(
            err.contains("reserved") || err.contains("apply_floor_raise"),
            "error must mention why the value is rejected: {err}",
        );
    }

    #[test]
    fn non_string_payload_value_is_rejected_as_shape_error() {
        let v = json!(42);
        let err = parse_classification_floor_source_from_payload(Some(&v)).unwrap_err();
        assert!(
            err.contains("not a string"),
            "shape error must surface as 'not a string': {err}",
        );
    }

    #[test]
    fn unknown_string_value_is_rejected_with_value_echoed() {
        let v = json!("garbage");
        let err = parse_classification_floor_source_from_payload(Some(&v)).unwrap_err();
        assert!(
            err.contains("garbage"),
            "error must echo the bad value: {err}",
        );
        assert!(
            err.contains("unknown") || err.contains("expected one of"),
            "error must name the contract: {err}",
        );
    }

    #[test]
    fn agent_raised_reject_binds_to_enum_variant_not_string_literal() {
        // Defense-in-depth pin: the reject inside
        // `parse_classification_floor_source_from_payload` matches on
        // the parsed `ClassificationFloorSource::AgentRaised` variant.
        // Feeding it the canonical wire form via `as_snake_str()`
        // exercises the same path a forging producer would. If a future
        // refactor rewires the reject to a hard-coded string literal,
        // and someone separately renames `AgentRaised` + its serde tag
        // (which `as_snake_str_matches_serde_wire_form` in `inner_loop`
        // forces to stay in lockstep), the literal would silently go
        // out of date — this test would still catch it because the
        // input is derived from the enum, not a constant.
        let wire = ClassificationFloorSource::AgentRaised.as_snake_str();
        let v = json!(wire);
        let err = parse_classification_floor_source_from_payload(Some(&v)).unwrap_err();
        assert!(
            err.contains(wire),
            "error must echo the rejected wire form {wire:?}: {err}",
        );
        assert!(
            err.contains("reserved") || err.contains("apply_floor_raise"),
            "error must name the contract: {err}",
        );
    }

    #[test]
    fn write_l1_promoted_row_signature_compile_pin() {
        // Compile-only: the function exists with the widened signature
        // (pool, extractor, embedder, task_id, insight). Full DB-backed coverage is in
        // core/tests/scheduler_lanes_e2e.rs.
        fn _signature_pin<'a>(
            pool: &'a sqlx::PgPool,
            extractor: &'a crate::entity_extraction::NoOpEntityExtractor,
            embedder: &'a crate::memory::embedder::NoOpEmbedder,
            task_id: i64,
            insight: &'a str,
        ) -> impl std::future::Future<Output = ()> + 'a {
            super::write_l1_promoted_row(pool, extractor, embedder, task_id, insight)
        }
        let _ = _signature_pin;
    }

    #[test]
    fn rejected_agent_raised_diagnostic_does_not_list_it_in_the_expected_set() {
        // Defense-in-depth pin: the "unknown value" message lists the
        // producer-legal set (operator / cli_inferred / default).
        // A future refactor that drops the explicit `agent_raised`
        // reject and falls back to the generic parser would silently
        // re-allow producer-supplied `agent_raised` — pin the contract
        // here. Asserts the dedicated reject message does NOT contain
        // the substring "expected one of" (it lives in a different code
        // path), and that the generic "unknown" message does NOT list
        // `agent_raised`.
        let agent_raised_err = parse_classification_floor_source_from_payload(
            Some(&json!("agent_raised")),
        )
        .unwrap_err();
        assert!(
            !agent_raised_err.contains("expected one of"),
            "agent_raised reject must use the dedicated message, not the generic \
             'expected one of': {agent_raised_err}",
        );
        let unknown_err =
            parse_classification_floor_source_from_payload(Some(&json!("nope"))).unwrap_err();
        assert!(
            !unknown_err.contains("agent_raised"),
            "the 'unknown' diagnostic must not advertise agent_raised as a \
             producer-legal value: {unknown_err}",
        );
    }
}
