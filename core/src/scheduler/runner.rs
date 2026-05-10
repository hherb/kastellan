//! Per-lane runner loop and the public `spawn_scheduler` entry point
//! that the daemon's `main.rs` calls after the pool comes up.

use std::path::PathBuf;
use std::sync::Arc;

use sqlx::postgres::PgListener;
use sqlx::PgPool;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{sleep, Duration};

use hhagent_db::tasks::{self, Lane, Task, DEFAULT_DEADLINE_FAST_S, DEFAULT_DEADLINE_LONG_S,
    DEFAULT_MAX_PLANS_FAST, DEFAULT_MAX_PLANS_LONG};

use crate::cassandra::review::ChainReviewStage;
use crate::cassandra::types::DataClass;

use super::agent::PlanFormulator;
use super::inner_loop::{run_to_terminal, StepDispatcher, TaskContext};

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
    _workspace_root: PathBuf,
) -> SchedulerHandle {
    let (tx, rx) = watch::channel(false);

    let fast = tokio::spawn(lane_loop(
        pool.clone(), formulator.clone(), review.clone(), dispatcher.clone(),
        Lane::Fast, DEFAULT_DEADLINE_FAST_S, DEFAULT_MAX_PLANS_FAST, rx.clone(),
    ));
    let long = tokio::spawn(lane_loop(
        pool, formulator, review, dispatcher,
        Lane::Long, DEFAULT_DEADLINE_LONG_S, DEFAULT_MAX_PLANS_LONG, rx,
    ));

    SchedulerHandle { shutdown: tx, fast, long }
}

async fn lane_loop(
    pool: PgPool,
    formulator: Arc<dyn PlanFormulator>,
    review: Arc<ChainReviewStage>,
    dispatcher: Arc<dyn StepDispatcher>,
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

    loop {
        // Wait for a wake-up: shutdown, NOTIFY, or heartbeat.
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
            }
            _ = listener.recv() => { /* fall through to drain */ }
            _ = sleep(HEARTBEAT) => { /* fall through */ }
        }

        // Drain pending tasks on this lane.
        loop {
            if *shutdown.borrow() { return; }
            let claimed = match tasks::claim_one(&pool, lane, deadline_seconds).await {
                Ok(Some(t)) => t,
                Ok(None) => break,  // nothing pending; back to listener
                Err(e) => {
                    eprintln!("scheduler[{}]: claim_one error: {e}", lane.as_sql());
                    break;
                }
            };

            let outcome = run_one(
                &pool, formulator.clone(), review.clone(), dispatcher.clone(),
                &claimed, max_plans,
            ).await;

            let final_state = outcome.final_state();
            let result = outcome.result_payload();
            if let Err(e) = tasks::finalize(&pool, claimed.id, final_state, result).await {
                eprintln!("scheduler[{}]: finalize task {} failed: {e}",
                          lane.as_sql(), claimed.id);
            }
        }
    }
}

async fn run_one(
    pool: &PgPool,
    formulator: Arc<dyn PlanFormulator>,
    review: Arc<ChainReviewStage>,
    dispatcher: Arc<dyn StepDispatcher>,
    task: &Task,
    max_plans: u32,
) -> super::inner_loop::Outcome {
    use super::inner_loop::Outcome;

    let instruction = task.payload.get("instruction")
        .and_then(|v| v.as_str()).unwrap_or("").to_string();
    let classification_floor = task.payload.get("classification_floor")
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str(&format!("\"{}\"", s)).ok())
        .unwrap_or(DataClass::Public);
    let max_plans_override = task.payload.get("max_plans")
        .and_then(|v| v.as_u64()).map(|n| n as u32).unwrap_or(max_plans);

    let ctx = TaskContext {
        task_id: task.id,
        lane: task.lane,
        instruction,
        classification_floor,
        plans: vec![],
        advisories: vec![],
        blocks: vec![],
        plan_count: 0,
        max_plans: max_plans_override,
    };

    match run_to_terminal(pool, formulator, review, dispatcher, ctx).await {
        Ok(o) => o,
        Err(e) => Outcome::Failed(format!("inner_loop: {e}")),
    }
}
