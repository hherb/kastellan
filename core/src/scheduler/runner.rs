//! Per-lane runner loop and the public `spawn_scheduler` entry point
//! that the daemon's `main.rs` calls after the pool comes up.
//!
//! ## Module layout
//!
//! The loop orchestration lives here; two cohesive helper families are
//! split into siblings to keep every file under the 500-LOC cap:
//!
//! - [`audit_rows`] — the best-effort `actor='scheduler'` lifecycle,
//!   finalize, and L1/L3/Python-skill crystallisation row writers that
//!   [`drain_lane`] calls after each task finalizes.
//! - [`task_exec`] — [`task_exec::run_one`] (build a `TaskContext` from
//!   the task payload, run it to terminal, purge the handoff cache) plus
//!   the pure `classification_floor_source` payload parser.

use std::sync::Arc;

use sqlx::postgres::PgListener;
use sqlx::PgPool;
use time::OffsetDateTime;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{sleep, Duration};

use crate::channel::outbox::ChannelOutbox;
use crate::entity_extraction::EntityExtractor;
use crate::memory::embedder::Embedder;

use kastellan_db::tasks::{self, Lane, DEFAULT_DEADLINE_FAST_S, DEFAULT_DEADLINE_LONG_S,
    DEFAULT_MAX_PLANS_FAST, DEFAULT_MAX_PLANS_LONG};

use crate::cassandra::review::ChainReviewStage;

use super::agent::PlanFormulator;
use super::audit::{action_task_terminal, ACTION_TASK_RUNNING};
use super::inner_loop::StepDispatcher;

mod audit_rows;
mod task_exec;

use audit_rows::{
    write_finalize_row, write_l1_promoted_row, write_l3_crystallised_row, write_lifecycle_row,
    write_python_skill_crystallised_row,
};
use task_exec::run_one;

/// Heartbeat interval for catch-up SELECT in case a `tasks_inserted`
/// NOTIFY was lost across a listener reconnect.
const HEARTBEAT: Duration = Duration::from_secs(30);

/// How often the expiry sweep looks for overdue operator asks.
///
/// Slice 1a's spec put this at daemon startup only. On a daemon that runs
/// for weeks that is not a deadline: an unanswered ask holds its task in
/// `awaiting_operator` until the next restart, which is the permanent wedge
/// the deadline exists to prevent. The security half needs no sweep —
/// `asks::resolve` and `resolve_with_nonce` both carry
/// `AND deadline_at > now()`, so an expired nonce is dead on time
/// regardless — but the task side does.
const ASK_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

pub struct SchedulerHandle {
    shutdown: watch::Sender<bool>,
    pub fast: JoinHandle<()>,
    pub long: JoinHandle<()>,
    /// The operator-ask expiry sweep (#564 slice 1b). Lane-independent —
    /// `asks::expire_due` is a pool-wide UPDATE — so it is its own task
    /// rather than a call inside `drain_lane`, which is the per-lane claim
    /// hot path.
    pub sweep: JoinHandle<()>,
}

impl SchedulerHandle {
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        let _ = self.fast.await;
        let _ = self.long.await;
        let _ = self.sweep.await;
    }
}

/// Spawn the two lane runners. Returns a handle the daemon's
/// shutdown path uses to flip the watch channel and join.
///
/// `outbox` is where a raised ask is delivered (#564 slice 2, spec D13);
/// `None` on a daemon built or configured without channels. Cloned into
/// both lanes — the `Arc` is what lets a raised ask in either lane reach
/// the one registry the channel bus registers its queues into.
pub fn spawn_scheduler(
    pool: PgPool,
    formulator: Arc<dyn PlanFormulator>,
    review: Arc<ChainReviewStage>,
    dispatcher: Arc<dyn StepDispatcher>,
    entity_extractor: Arc<dyn EntityExtractor>,
    embedder: Arc<dyn Embedder>,
    outbox: Option<Arc<ChannelOutbox>>,
) -> SchedulerHandle {
    let (tx, rx) = watch::channel(false);

    let fast = tokio::spawn(lane_loop(
        pool.clone(), formulator.clone(), review.clone(), dispatcher.clone(),
        entity_extractor.clone(), embedder.clone(), outbox.clone(),
        Lane::Fast, DEFAULT_DEADLINE_FAST_S, DEFAULT_MAX_PLANS_FAST, rx.clone(),
    ));
    let sweep = tokio::spawn(sweep_loop(pool.clone(), rx.clone()));
    let long = tokio::spawn(lane_loop(
        pool, formulator, review, dispatcher,
        entity_extractor, embedder, outbox,
        Lane::Long, DEFAULT_DEADLINE_LONG_S, DEFAULT_MAX_PLANS_LONG, rx,
    ));

    SchedulerHandle { shutdown: tx, fast, long, sweep }
}

/// Expire overdue operator asks on a timer until shutdown.
///
/// A sweep error is logged and the loop continues: the next tick retries in
/// `ASK_SWEEP_INTERVAL`, and killing this task over a transient DB error
/// would silently disable every ask deadline for the life of the process
/// (nothing supervises it and every unit would still report `active` — the
/// same asymmetry the `tasks_resumed` LISTEN comment argues).
async fn sweep_loop(pool: PgPool, mut shutdown: watch::Receiver<bool>) {
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
            }
            _ = sleep(ASK_SWEEP_INTERVAL) => {}
        }
        if *shutdown.borrow() { return; }
        if let Err(e) = super::asks::sweep_expired_and_audit(&pool).await {
            tracing::warn!(error = %e, "operator-ask expiry sweep failed; retrying next tick");
        }
    }
}

// Eleven params: `pool`, the five stage handles (formulator, review,
// dispatcher, entity extractor, embedder), the `outbox`, the shutdown
// watch, and three per-lane tuning values. They are genuinely distinct
// inputs to the loop, so the arg-count heuristic is suppressed rather than
// papered over with a dependency-bundle struct that would only move the
// list to the call site. `outbox` — positionally the 7th — is where a
// raised ask is delivered (#564 slice 2), `None` on a channel-less daemon:
// one more genuinely distinct dependency, not a reason to change the shape
// of the suppression.
#[allow(clippy::too_many_arguments)]
async fn lane_loop(
    pool: PgPool,
    formulator: Arc<dyn PlanFormulator>,
    review: Arc<ChainReviewStage>,
    dispatcher: Arc<dyn StepDispatcher>,
    entity_extractor: Arc<dyn EntityExtractor>,
    embedder: Arc<dyn Embedder>,
    outbox: Option<Arc<ChannelOutbox>>,
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
    // A task resumed from `awaiting_operator` (#564) is an UPDATE, which
    // the INSERT-only `tasks_inserted` trigger cannot see — without this
    // the resumed task waits out a full HEARTBEAT. Its own channel rather
    // than overloading `tasks_inserted`, whose name would then no longer
    // describe what it carries.
    //
    // NOT fatal, unlike the two LISTENs above, and the asymmetry is
    // deliberate: `tasks_inserted` / `tasks_cancelled` are how this lane
    // learns about work and about withdrawal, so losing them is losing the
    // lane's purpose. `tasks_resumed` is a **latency optimisation over the
    // 30 s heartbeat** — `drain_lane` runs unconditionally on the timer, so
    // a resumed task is picked up either way. Killing the whole lane
    // (`return` ends the tokio task, nothing supervises it, and every
    // systemd unit still reports `active`) over a subscription whose
    // absence costs at most one heartbeat inverts the severity.
    if let Err(e) = listener.listen("tasks_resumed").await {
        tracing::error!(
            lane = lane.as_sql(),
            error = %e,
            "LISTEN tasks_resumed failed; resumed tasks will wait up to one HEARTBEAT (30 s) \
             instead of waking immediately",
        );
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
        entity_extractor.clone(), embedder.clone(), outbox.as_deref(),
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
            entity_extractor.clone(), embedder.clone(), outbox.as_deref(),
            lane, deadline_seconds, max_plans, &shutdown,
        ).await;
    }
}

/// Drain every pending task on `lane` until `claim_one` returns `None`.
/// Pulled out of `lane_loop` so the same body runs both in the initial
/// startup pass and on each NOTIFY/heartbeat wake. Honours `shutdown`
/// between every claim.
// Same eleven-input shape as `lane_loop` (it forwards them straight
// through, `outbox` included, as a borrow); see the note there for why the
// arg-count heuristic is suppressed instead of bundled.
#[allow(clippy::too_many_arguments)]
async fn drain_lane(
    pool: &PgPool,
    formulator: Arc<dyn PlanFormulator>,
    review: Arc<ChainReviewStage>,
    dispatcher: Arc<dyn StepDispatcher>,
    entity_extractor: Arc<dyn EntityExtractor>,
    embedder: Arc<dyn Embedder>,
    outbox: Option<&ChannelOutbox>,
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
            // No turn record: an operator skill run is not a channel turn, so
            // there is no conversation for a later turn to read it from (#701).
            if let Err(e) =
                tasks::finalize(pool, claimed.id, "completed", result_payload, None).await
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

        // Per-task DURABLE output dir for opt-in workers (mail): attachments are
        // written straight into `<artifacts_root>/<task_id>/` and survive the
        // task — the worker returns that path, so what it reports is where the
        // file actually is (no harvest/relocate, no silent-loss window; see the
        // final-review I-1 decision). Retention/GC of the artifacts root is an
        // operator concern. Non-fatal on failure — search/text tools still work;
        // only attachment delivery needs it. `claimed.id` is a positive bigserial
        // → plain digits, so the join can't escape the root.
        let task_out_dir = match crate::workspace::default_artifacts_root() {
            Ok(root) => {
                let dir = root.join(claimed.id.to_string());
                match std::fs::create_dir_all(&dir) {
                    Ok(()) => {
                        dispatcher.set_task_out_dir(claimed.id, dir.clone());
                        Some(dir)
                    }
                    Err(e) => {
                        tracing::warn!(
                            task_id = claimed.id, error = %e, dir = ?dir,
                            "task output dir create failed; attachment delivery unavailable"
                        );
                        None
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    task_id = claimed.id, error = %e,
                    "artifacts root unresolved; attachment delivery unavailable"
                );
                None
            }
        };

        let result = run_one(
            pool, formulator.clone(), review.clone(), dispatcher.clone(),
            &claimed, max_plans, outbox,
        ).await;

        // A non-terminal outcome: the task suspended on an operator ask.
        // `db::asks::raise` already moved the row to `awaiting_operator`
        // inside its own transaction, and the `ask.raised` audit row is
        // written where the ask is raised (`scheduler::asks::raise_and_suspend`,
        // called from the inner loop's `Verdict::Escalate` arm). So there is
        // nothing to finalize and no terminal lifecycle row to write here.
        // The L1/L3 hooks below are `Outcome::Completed`-only anyway.
        let Some(final_state) = result.outcome.final_state() else {
            // The per-task out dir is left in place: the task resumes and
            // `create_dir_all` is idempotent, so re-creating it costs
            // nothing and removing it could delete a deliverable a step
            // already wrote before the escalation.
            continue;
        };
        let final_result_payload = result.outcome.result_payload();

        // Capture `finished_at` *before* `tasks::finalize` so the
        // finalize-payload's `total_duration_ms` measures inner-loop
        // wall time only — not inner-loop wall time + the finalize
        // UPDATE's round-trip latency. On a contended DB the latter
        // can be the dominant term and would silently bias the
        // observation-phase latency distribution.
        let finished_at = OffsetDateTime::now_utc();

        if let Err(e) =
            tasks::finalize(
                pool, claimed.id, final_state, final_result_payload,
                // #701: what this turn did, for the next turn in the same
                // conversation to read. `None` for every non-channel task.
                result.turn_record.clone(),
            ).await
        {
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

        // Prune the task output dir iff nothing was delivered: `remove_dir` is
        // non-recursive, so it succeeds only on an empty dir and leaves any
        // real deliverables (and their parent) intact. Keeps the artifacts root
        // from filling with empty per-task dirs for tasks that saved no files.
        if let Some(dir) = task_out_dir {
            let _ = std::fs::remove_dir(&dir);
        }
    }
}
