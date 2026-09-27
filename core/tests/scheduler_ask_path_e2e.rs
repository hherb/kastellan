//! End-to-end: escalate -> suspend -> resolve -> resume, through the real
//! lane runner (#564 slice 1b).
//!
//! Driven through `spawn_scheduler` rather than `run_to_terminal`, because
//! `drain_lane`'s non-finalize branch is half of what is under test and only
//! the lane runner reaches it.
//!
//! Skips silently with `[SKIP]` on hosts without Postgres or a reachable
//! supervisor; run with `-- --nocapture` to see whether it ran.
//!
//! Issue #15 will eventually hoist the bring-up helpers into a shared
//! fixture; until then we copy and adapt the recipe from
//! `core/tests/scheduler_asks_e2e.rs`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kastellan_core::cassandra::review::{ChainReviewStage, ReviewStage, ReviewStageContext};
use kastellan_core::cassandra::types::{DataClass, Plan, PlannedStep, Severity, Verdict};
use kastellan_core::channel::ask_message::parse_ask_command;
use kastellan_core::channel::ingest::build_channel_task_payload;
use kastellan_core::channel::outbox::ChannelOutbox;
use kastellan_core::channel::{ChannelId, ConversationId, IncomingMessage, PeerId};
use kastellan_core::memory::embedder::NoOpEmbedder;
use kastellan_core::scheduler::agent::{AgentError, FormulationMeta, PlanFormulator};
use kastellan_core::scheduler::asks::delivery::{REASON_NO_CHANNEL, REASON_NO_ORIGIN};
use kastellan_core::scheduler::audit::{
    ACTION_ASK_APPROVAL_APPLIED, ACTION_ASK_UNDELIVERED, ACTION_TASK_FINALIZE,
};
use kastellan_core::scheduler::inner_loop::{StepDispatcher, StepOutcome, TaskContext};
use kastellan_core::scheduler::{spawn_scheduler, SchedulerHandle};
use kastellan_db::tasks::{self, insert_pending, Lane};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix, PgCluster,
};

/// Async helper: bring up a PG cluster (via the shared
/// [`kastellan_tests_common::bring_up_pg_cluster`]), run migrations,
/// return pool + cluster handle. The `PgCluster` carries the cleanup
/// guards internally and drops them in the right order at end of scope.
/// Returns `None` when PG or supervisor is unavailable (skip).
async fn bring_up_pg(label: &str) -> Option<(sqlx::PgPool, PgCluster)> {
    if skip_if_no_supervisor() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let suffix = format!("{}-{}", label, unique_suffix());
    let service_name = format!("kastellan-sched-test-pg-askpath-{suffix}");
    let cluster = tokio::task::block_in_place(|| {
        bring_up_pg_cluster(&bin_dir, "apd", "apl", &service_name)
    });

    kastellan_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "scheduler-ask-path"}),
    )
    .await
    .ok()?;

    let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .ok()?;

    Some((pool, cluster))
}

// ---------------------------------------------------------------------------
// Stubs
// ---------------------------------------------------------------------------

/// Escalates the next `remaining` plans it sees, then approves.
///
/// Two of the tests set `remaining` to `u32::MAX` — i.e. "escalate
/// forever". That is load-bearing, not laziness: with a reviewer that
/// stops escalating after the first plan, the resumed run would simply be
/// approved by the *reviewer*, and the test would pass whether or not the
/// `Escalate` arm ever consulted the operator's decision.
struct EscalatingReview {
    remaining: Mutex<u32>,
}

#[async_trait]
impl ReviewStage for EscalatingReview {
    fn name(&self) -> &str {
        "escalating"
    }

    async fn review(&self, _plan: &Plan, _ctx: &ReviewStageContext<'_>) -> Verdict {
        let mut n = self.remaining.lock().unwrap();
        if *n > 0 {
            *n -= 1;
            Verdict::Escalate("needs a human".to_string(), Severity::High)
        } else {
            Verdict::Approve
        }
    }
}

/// Plays a fixed list of verdicts, one per review, then approves forever.
///
/// [`EscalatingReview`] can only escalate the FIRST n plans, which cannot
/// stage the case D11 is about: a run that COMPLETES a plan (executing its
/// steps) and escalates on a later one. That is the only shape in which a
/// suspension has any history to carry.
struct ScriptedReview {
    verdicts: Mutex<std::collections::VecDeque<Verdict>>,
}

#[async_trait]
impl ReviewStage for ScriptedReview {
    fn name(&self) -> &str {
        "scripted"
    }

    async fn review(&self, _plan: &Plan, _ctx: &ReviewStageContext<'_>) -> Verdict {
        self.verdicts.lock().unwrap().pop_front().unwrap_or(Verdict::Approve)
    }
}

/// Records `ctx.plans.len()` on every call, and picks its plan from that
/// same number.
///
/// The recorded sequence is the assertion D11 actually earns: a resumed run
/// must formulate against the history it already had, not against an empty
/// one. Keying the plan off `ctx.plans.len()` (rather than a call counter)
/// also means a run that restored its history walks FORWARD from where it
/// stopped instead of re-deriving the plans it already ran.
struct HistoryRecordingFormulator {
    /// One entry per `formulate_plan` call: the number of completed plans
    /// the context carried at that moment.
    seen: Arc<Mutex<Vec<usize>>>,
}

#[async_trait]
impl PlanFormulator for HistoryRecordingFormulator {
    async fn formulate_plan(
        &self,
        ctx: &TaskContext,
    ) -> Result<(Plan, FormulationMeta), AgentError> {
        let n = ctx.plans.len();
        self.seen.lock().unwrap().push(n);
        let plan = match n {
            0 => one_step_plan_with_param(1),
            1 => one_step_plan_with_param(2),
            _ => task_complete_plan("ok"),
        };
        Ok((plan, test_meta()))
    }
}

/// Records the `parameters` of every step it is asked to dispatch, so a
/// test can see whether a step ran twice.
struct RecordingDispatcher {
    dispatched: Arc<Mutex<Vec<serde_json::Value>>>,
}

#[async_trait]
impl StepDispatcher for RecordingDispatcher {
    async fn dispatch_step(&self, _task_id: i64, step: &PlannedStep) -> StepOutcome {
        self.dispatched.lock().unwrap().push(step.parameters.clone());
        StepOutcome::Ok(serde_json::json!("done"))
    }
}

/// Returns `StepOutcome::Ok` for every step without doing anything. None
/// of these tests reaches step execution on purpose; this exists so the
/// scheduler can be built.
struct NoopDispatcher;

#[async_trait]
impl StepDispatcher for NoopDispatcher {
    async fn dispatch_step(&self, _task_id: i64, _step: &PlannedStep) -> StepOutcome {
        StepOutcome::Ok(serde_json::json!("done"))
    }
}

/// One formulator covering all three variants. `vary` makes each call
/// return a plan differing in a field the digest INCLUDES; `calls` is the
/// counter the denial test reads.
struct TestFormulator {
    vary: bool,
    calls: Arc<Mutex<u32>>,
}

#[async_trait]
impl PlanFormulator for TestFormulator {
    async fn formulate_plan(
        &self,
        _ctx: &TaskContext,
    ) -> Result<(Plan, FormulationMeta), AgentError> {
        let n = {
            let mut c = self.calls.lock().unwrap();
            *c += 1;
            *c
        };
        // `parameters` is digested; `context` is not. Varying the former is
        // what makes the replan a genuinely different plan.
        let plan = if self.vary {
            one_step_plan_with_param(n)
        } else {
            task_complete_plan("ok")
        };
        Ok((plan, test_meta()))
    }
}

/// A formulator whose plan depends on the plan's INDEX WITHIN THE RUN —
/// `ctx.plans.len()` — rather than on a call counter.
///
/// That distinction is the whole test. Keying on the index makes the
/// digests STABLE: the index-0 plan is always plan A and the index-1 plan is
/// always plan B, so a run that resumes at index 1 re-derives exactly the
/// plan it was suspended on — the one whose steps never ran — and its
/// digest matches the approval. A call counter (what
/// `TestFormulator { vary: true }` uses) instead produces a brand-new digest
/// every time, which is the separate "a different replan re-escalates" case.
///
/// Since D11 a resumed run restores the plans it already completed, so it
/// starts at the index it stopped at rather than back at 0.
///
/// Index >= 2 returns a terminal plan so the task can actually finish once
/// both escalations are covered.
struct PerRunIndexFormulator;

#[async_trait]
impl PlanFormulator for PerRunIndexFormulator {
    async fn formulate_plan(
        &self,
        ctx: &TaskContext,
    ) -> Result<(Plan, FormulationMeta), AgentError> {
        let plan = match ctx.plans.len() {
            0 => one_step_plan_with_param(1),
            1 => one_step_plan_with_param(2),
            _ => task_complete_plan("ok"),
        };
        Ok((plan, test_meta()))
    }
}

/// The `FormulationMeta` fixture, copied from `ScriptedFormulator` in
/// `core/tests/scheduler_lanes_e2e.rs`. Nothing under test reads it; it
/// only has to be well-formed enough for the `plan.formulate` audit row.
fn test_meta() -> FormulationMeta {
    FormulationMeta {
        prompt_name: "agent_planner".into(),
        prompt_sha256: "test".into(),
        llm_model: "test-model".into(),
        llm_backend: "local".into(),
        latency_ms: 1,
        retry_count: 0,
        assembled_prompt_sha256: "test-assembled-sha".into(),
        l0_count: 0,
        l1_count: 0,
        skill_count: 0,
        recalled_memory_ids: Vec::new(),
        recall_count: 0,
        recall_query_sha256:
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
        graph_seed_entity_ids: Vec::new(),
        graph_seed_count: 0,
        graph_seed_source: kastellan_core::entity_extraction::SeedSource::None,
        usage: Default::default(),
    }
}

// ---------------------------------------------------------------------------
// Plan factories (mirroring core/tests/scheduler_lanes_e2e.rs)
// ---------------------------------------------------------------------------

fn task_complete_plan(body: &str) -> Plan {
    Plan {
        context: "c".into(),
        decision: "task_complete".into(),
        rationale: "done".into(),
        steps: vec![],
        result: Some(serde_json::json!({"kind": "text", "body": body})),
        data_ceiling: Some(DataClass::Public),
        refused: None,
        floor_request: None,
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
        python_skill: None,
    }
}

/// A non-terminal one-step plan whose `parameters` carry `n`. `parameters`
/// is one of the fields `plan_digest` INCLUDES, so two calls with different
/// `n` produce two different digests — which is what makes the "a different
/// replan re-escalates" test test anything.
fn one_step_plan_with_param(n: u32) -> Plan {
    Plan {
        context: "c".into(),
        decision: "act".into(),
        rationale: "r".into(),
        steps: vec![PlannedStep {
            tool: "sleep".into(),
            method: "doit".into(),
            parameters: serde_json::json!({"n": n}),
            returns: "x".into(),
            done_when: "x".into(),
            classification: DataClass::Public,
        }],
        result: None,
        data_ceiling: Some(DataClass::Public),
        refused: None,
        floor_request: None,
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
        python_skill: None,
    }
}

// ---------------------------------------------------------------------------
// Scheduler spawn helpers
// ---------------------------------------------------------------------------

/// The shared body. `formulator` is the only thing the three variants
/// differ in; everything else is a no-op stub.
fn spawn_with(
    pool: &sqlx::PgPool,
    formulator: Arc<dyn PlanFormulator>,
    review: Arc<ChainReviewStage>,
) -> SchedulerHandle {
    spawn_scheduler(
        pool.clone(),
        formulator,
        review,
        Arc::new(NoopDispatcher),
        Arc::new(kastellan_core::entity_extraction::NoOpEntityExtractor::new()),
        Arc::new(NoOpEmbedder::new()),
        None,
    )
}

/// Returns the same terminal plan on every call.
fn spawn_test_scheduler(pool: &sqlx::PgPool, review: Arc<ChainReviewStage>) -> SchedulerHandle {
    spawn_with(
        pool,
        Arc::new(TestFormulator { vary: false, calls: Arc::new(Mutex::new(0)) }),
        review,
    )
}

/// As above, plus the shared counter of `formulate_plan` calls.
fn spawn_test_scheduler_counting(
    pool: &sqlx::PgPool,
    review: Arc<ChainReviewStage>,
) -> (SchedulerHandle, Arc<Mutex<u32>>) {
    let calls = Arc::new(Mutex::new(0));
    let h = spawn_with(
        pool,
        Arc::new(TestFormulator { vary: false, calls: Arc::clone(&calls) }),
        review,
    );
    (h, calls)
}

/// Returns a DIFFERENT plan each call.
fn spawn_test_scheduler_varying(
    pool: &sqlx::PgPool,
    review: Arc<ChainReviewStage>,
) -> SchedulerHandle {
    spawn_with(
        pool,
        Arc::new(TestFormulator { vary: true, calls: Arc::new(Mutex::new(0)) }),
        review,
    )
}

/// Returns plan A, then plan B, then a terminal plan — keyed on the plan
/// index within the current run, so a resume replays the same digests.
fn spawn_test_scheduler_per_run_index(
    pool: &sqlx::PgPool,
    review: Arc<ChainReviewStage>,
) -> SchedulerHandle {
    spawn_with(pool, Arc::new(PerRunIndexFormulator), review)
}

/// Spawn with a history-recording formulator and a step-recording
/// dispatcher, returning both records.
#[allow(clippy::type_complexity)]
fn spawn_test_scheduler_recording_history(
    pool: &sqlx::PgPool,
    review: Arc<ChainReviewStage>,
) -> (SchedulerHandle, Arc<Mutex<Vec<usize>>>, Arc<Mutex<Vec<serde_json::Value>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let dispatched = Arc::new(Mutex::new(Vec::new()));
    let handle = spawn_scheduler(
        pool.clone(),
        Arc::new(HistoryRecordingFormulator { seen: Arc::clone(&seen) }),
        review,
        Arc::new(RecordingDispatcher { dispatched: Arc::clone(&dispatched) }),
        Arc::new(kastellan_core::entity_extraction::NoOpEntityExtractor::new()),
        Arc::new(NoOpEmbedder::new()),
        None,
    );
    (handle, seen, dispatched)
}

/// A reviewer chain of one [`ScriptedReview`] playing `verdicts` in order,
/// then approving.
fn scripted(verdicts: Vec<Verdict>) -> Arc<ChainReviewStage> {
    Arc::new(ChainReviewStage::new(vec![Arc::new(ScriptedReview {
        verdicts: Mutex::new(verdicts.into()),
    })]))
}

/// A reviewer chain of one [`EscalatingReview`].
fn escalating(times: u32) -> Arc<ChainReviewStage> {
    Arc::new(ChainReviewStage::new(vec![Arc::new(EscalatingReview {
        remaining: Mutex::new(times),
    })]))
}

/// Seed one `pending` task on the fast lane.
///
/// `max_plans` travels in the payload rather than through the spawn
/// helpers, because the lane's cap comes from `DEFAULT_MAX_PLANS_FAST` and
/// not from `spawn_scheduler`'s arguments. Setting it low means a bug that
/// loops instead of suspending hits the cap in a few plans instead of
/// running to the lane default.
async fn seed_task(pool: &sqlx::PgPool) -> i64 {
    insert_pending(
        pool,
        Lane::Fast,
        serde_json::json!({"instruction": "x", "max_plans": 3}),
    )
    .await
    .expect("insert")
}

/// Seed one `pending` task whose payload is **channel-originated**, built by
/// calling the real producer rather than hand-writing the JSON.
///
/// `run_one` derives `TaskContext.origin` from exactly this payload via
/// `destination_from_task_payload`, so a rename on either side has to fail
/// a test rather than silently stop every escalation being delivered.
async fn seed_channel_task(pool: &sqlx::PgPool) -> i64 {
    let mut payload = build_channel_task_payload(&IncomingMessage {
        channel: ChannelId("matrix".into()),
        peer: PeerId("@me:srv".into()),
        conversation: ConversationId("!room:srv".into()),
        body: "book the flight".into(),
        evidence: None,
    });
    // Same low cap as `seed_task`, for the same reason. The channel producer
    // does not write one (a real channel task takes the lane default).
    payload["max_plans"] = serde_json::json!(3);
    insert_pending(pool, Lane::Fast, payload).await.expect("insert")
}

/// Poll `tasks.state` until it equals `want`, or give up after `secs` and
/// return whatever it last saw. A fixed sleep would pass or fail on
/// machine speed rather than on behaviour.
async fn await_state(pool: &sqlx::PgPool, task_id: i64, want: &str, secs: u64) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        let s = tasks::observe_state(pool, task_id).await.expect("state");
        if s == want || std::time::Instant::now() > deadline {
            return s;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn count_asks_for(pool: &sqlx::PgPool, task_id: i64) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM asks WHERE task_id = $1")
        .bind(task_id)
        .fetch_one(pool)
        .await
        .expect("count asks")
}

/// The `reason` on the one `ask.undelivered` row naming `task_id`.
async fn undelivered_reason(pool: &sqlx::PgPool, task_id: i64) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT payload->>'reason' FROM audit_log \
         WHERE action = $1 AND payload->>'task_id' = $2::text ORDER BY id DESC LIMIT 1",
    )
    .bind(ACTION_ASK_UNDELIVERED)
    .bind(task_id)
    .fetch_optional(pool)
    .await
    .expect("read ask.undelivered reason")
    .flatten()
}

/// Rows of one `action` naming `task_id` in their payload.
async fn count_audit_rows(pool: &sqlx::PgPool, action: &str, task_id: i64) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM audit_log \
         WHERE action = $1 AND payload->>'task_id' = $2::text",
    )
    .bind(action)
    .bind(task_id)
    .fetch_one(pool)
    .await
    .expect("count audit rows")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_escalated_plan_suspends_the_task_and_writes_no_finalize_row() {
    let Some((pool, _cluster)) = bring_up_pg("suspend").await else {
        return; // [SKIP]
    };
    let task_id = seed_task(&pool).await;
    let handle = spawn_test_scheduler(&pool, escalating(1));

    assert_eq!(
        await_state(&pool, task_id, "awaiting_operator", 20).await,
        "awaiting_operator",
    );

    // The load-bearing negative: `drain_lane` must NOT have finalized.
    let finalize_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE action = $1 AND payload->>'task_id' = $2::text",
    )
    .bind(ACTION_TASK_FINALIZE)
    .bind(task_id)
    .fetch_one(&pool)
    .await
    .expect("count finalize rows");
    assert_eq!(
        finalize_rows, 0,
        "a suspended task has not finished and must not be finalized",
    );

    let pending = kastellan_db::asks::list_pending(&pool, 10).await.expect("list");
    assert_eq!(pending.len(), 1, "exactly one ask was raised");
    assert_eq!(pending[0].task_id, task_id);
    assert!(
        pending[0].plan_digest.is_some(),
        "the raised ask binds to the digest of the plan that escalated",
    );

    handle.shutdown().await;
}

/// `TaskContext.origin` must be derived from the task's own payload, inside
/// `run_one`, and must reach `raise_and_suspend`.
///
/// **The line this exists for is `task_exec.rs`'s `origin:
/// destination_from_task_payload(&task.payload)`.** Mutating it to `origin:
/// None` left the entire 3396-test suite green: every other full-scheduler
/// e2e here seeds a payload with no routing metadata, and
/// `scheduler_asks_e2e` reaches `raise_and_suspend` directly with a
/// destination it computed itself, never through `run_one`. On a live host
/// the mutation makes every escalation audit
/// `ask.undelivered{reason: task_has_no_channel_origin}` and the operator's
/// room stays silent — indistinguishable in CI from the feature working,
/// and identical to the pre-slice behaviour.
///
/// **The assertion is the REASON, not the presence of a row.** This
/// scheduler is spawned with `outbox: None`, so `deliver_ask` returns
/// `Undelivered` either way — but with `no_channel_configured` only if it
/// got past the origin check first. The two labels can differ *only* if
/// `ctx.origin` was genuinely computed from the payload, which is why one
/// string comparison pins the whole wiring.
///
/// The second task is the contrast that makes the first assertion mean
/// something: same scheduler, same run, a payload with no routing metadata,
/// and it must land on the other label.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_channel_tasks_escalation_is_delivered_against_its_own_origin() {
    let Some((pool, _cluster)) = bring_up_pg("origin").await else {
        return; // [SKIP]
    };
    let channel_task = seed_channel_task(&pool).await;
    let plain_task = seed_task(&pool).await;

    // Escalate every plan, so both tasks suspend and neither resumes.
    // `spawn_with` passes `outbox: None` — a channel-less daemon.
    let handle = spawn_test_scheduler(&pool, escalating(u32::MAX));

    assert_eq!(
        await_state(&pool, channel_task, "awaiting_operator", 20).await,
        "awaiting_operator",
    );
    assert_eq!(
        await_state(&pool, plain_task, "awaiting_operator", 20).await,
        "awaiting_operator",
    );

    assert_eq!(
        undelivered_reason(&pool, channel_task).await.as_deref(),
        Some(REASON_NO_CHANNEL),
        "a channel-originated task's escalation must fail delivery on the MISSING \
         CHANNEL, not on a missing origin — `{REASON_NO_ORIGIN}` here means `run_one` \
         never derived `TaskContext.origin` from the payload, and on a host that does \
         have a channel the operator would simply never be asked",
    );
    assert_eq!(
        undelivered_reason(&pool, plain_task).await.as_deref(),
        Some(REASON_NO_ORIGIN),
        "a task with no routing metadata really has no origin — without this the \
         assertion above could pass on a `deliver_ask` that ignored `dest` entirely",
    );

    handle.shutdown().await;
}

/// **The outbox must survive the trip from `spawn_scheduler` to the wire.**
///
/// The test above deliberately spawns with `outbox: None` and asserts on
/// the `reason` label, so it is insensitive to the outbox argument itself.
/// That left a seven-hop thread — `main` → `spawn_scheduler` → `lane_loop`
/// → `drain_lane` → `run_one` → `run_to_terminal` → `raise_and_suspend` →
/// `deliver_ask` — with **no test anywhere passing `Some`**: all five
/// `spawn_scheduler` call sites in tests and all 18 `run_to_terminal` call
/// sites passed `None`, and the one test proving delivery works calls
/// `raise_and_suspend` directly, bypassing the runner entirely.
///
/// So `Some(outbox.clone())` in `main.rs`, or either `outbox.as_deref()` in
/// `runner.rs`, could be mutated to `None` with the whole ~3400-test suite
/// still green, while every live escalation audited `ask.undelivered` and
/// the operator's room stayed silent. That is precisely the `origin: None`
/// defect the whole-branch review caught, one to three call frames further
/// out; the fix wave closed only the `task_exec` half.
///
/// The assertion is the **rendered message on the receiver**, not an audit
/// row: an `ask.delivered` row proves the outcome mapping, whereas a body
/// carrying a token that parses back out proves the whole chain — the
/// destination came from the task's own payload, the nonce reached the
/// renderer, and the message was queued to the channel the task came from.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_raised_ask_travels_the_whole_runner_to_the_registered_channel() {
    let Some((pool, _cluster)) = bring_up_pg("outbox").await else {
        return; // [SKIP]
    };
    let channel_task = seed_channel_task(&pool).await;

    // A real outbox with a real registered queue — the same shape the bus
    // registers. `ChannelOutbox` hands no `Sender` back out, so a drained
    // receiver IS the perfect fake and no trait double is needed.
    let outbox = Arc::new(ChannelOutbox::new());
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    outbox.register(ChannelId("matrix".into()), tx);

    let handle = spawn_scheduler(
        pool.clone(),
        Arc::new(TestFormulator { vary: false, calls: Arc::new(Mutex::new(0)) }),
        escalating(u32::MAX),
        Arc::new(NoopDispatcher),
        Arc::new(kastellan_core::entity_extraction::NoOpEntityExtractor::new()),
        Arc::new(NoOpEmbedder::new()),
        Some(Arc::clone(&outbox)),
    );

    // Bounded: an unbounded `recv().await` would hang the Linux gate on
    // regression rather than failing it.
    let sent = tokio::time::timeout(std::time::Duration::from_secs(20), rx.recv())
        .await
        .expect("the ask must reach the registered channel within 20s")
        .expect("the outbox sender must still be open");

    assert_eq!(sent.channel.0, "matrix", "delivered to the task's own channel");
    assert_eq!(sent.peer.0, "@me:srv", "addressed to the peer the task came from");
    assert_eq!(sent.conversation.0, "!room:srv", "into the conversation it came from");
    assert!(
        sent.body.contains(&format!("task {channel_task}")),
        "the message names its task: {}",
        sent.body,
    );

    // The delivered token must be the one that actually resolves the ask —
    // the two halves are rendered and stored by different code, so parsing
    // it back out of the wire body is what pins them together.
    let line = sent
        .body
        .lines()
        .find(|l| l.trim_start().starts_with("/approve "))
        .expect("the message offers an /approve line");
    let cmd = parse_ask_command(line.trim()).expect("the offered line parses as a command");
    let ask = kastellan_db::asks::list_pending(&pool, 10).await.expect("list")[0].clone();
    assert!(
        kastellan_db::asks::resolve_with_nonce(
            &pool,
            &kastellan_db::asks::Nonce::from_wire(cmd.token),
            &kastellan_db::asks::Claimant::new("matrix", "@me:srv"),
            &serde_json::json!({"choice": "approve"}),
        )
        .await
        .expect("resolve")
        .is_some(),
        "the token delivered on the wire must resolve ask {}",
        ask.id,
    );

    assert_eq!(
        undelivered_reason(&pool, channel_task).await,
        None,
        "a delivered ask must leave no `ask.undelivered` row — this is the assertion \
         that fails if the outbox is dropped anywhere between `main` and `deliver_ask`",
    );

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_approval_lets_the_same_plan_through_on_resume() {
    let Some((pool, _cluster)) = bring_up_pg("approve").await else {
        return; // [SKIP]
    };
    let task_id = seed_task(&pool).await;

    // The reviewer escalates EVERY plan and the formulator returns the
    // identical plan every time. That combination is load-bearing: with a
    // reviewer that stops escalating after the first plan, the resumed run
    // would simply be approved by the reviewer and the test would pass
    // whether or not the arm ever consults the operator's approval. Here
    // the replan escalates again, so the only way the task can complete is
    // the digest matching the resolved ask.
    let handle = spawn_test_scheduler(&pool, escalating(u32::MAX));

    assert_eq!(
        await_state(&pool, task_id, "awaiting_operator", 20).await,
        "awaiting_operator",
    );
    let ask = kastellan_db::asks::list_pending(&pool, 10).await.expect("list")[0].clone();
    assert!(kastellan_db::asks::resolve(
        &pool,
        ask.id,
        "operator",
        &serde_json::json!({"choice": "approve"}),
    )
    .await
    .expect("resolve"));

    assert_eq!(await_state(&pool, task_id, "completed", 20).await, "completed");
    // Exactly one ask: the approval covered the replan rather than
    // producing a second question.
    assert_eq!(
        count_asks_for(&pool, task_id).await,
        1,
        "an approved, identical replan must not re-escalate",
    );

    // The approved-proceed branch must leave a row. Without it the audit
    // trail goes `cassandra.verdict{kind=escalate}` -> step dispatch with
    // nothing between, and the digest of the plan that RAN appears only in
    // the much earlier `ask.raised` row — so nothing in the log shows that
    // the plan which executed is the plan the operator approved.
    let applied: Vec<serde_json::Value> = sqlx::query_scalar(
        "SELECT payload FROM audit_log \
         WHERE action = $1 AND payload->>'task_id' = $2::text ORDER BY id",
    )
    .bind(ACTION_ASK_APPROVAL_APPLIED)
    .bind(task_id)
    .fetch_all(&pool)
    .await
    .expect("read ask.approval_applied rows");
    assert_eq!(applied.len(), 1, "one row per approved-proceed, got {applied:?}");
    assert_eq!(applied[0].get("ask_id").and_then(|v| v.as_i64()), Some(ask.id));
    assert_eq!(
        applied[0].get("plan_digest").and_then(|v| v.as_str()),
        ask.plan_digest.as_deref(),
        "the row must carry the digest of the plan that ran, and it must be the \
         digest the operator approved — that binding is the whole point",
    );

    handle.shutdown().await;
}

/// A task that escalates at TWO different plans, each approved, must finish
/// — with exactly two asks, not an unbounded alternating stream of them.
///
/// The livelock this pins: a task holds an approval per escalation, but the
/// read was `LIMIT 1` and the context held one `Option<Ask>`, so approving
/// the second question made the first approval invisible. The resumed run
/// re-asked plan A, approving THAT made plan B's approval the stale one,
/// and the two alternated forever — `resume_budget` grants a fresh plan
/// allowance on every resume, so nothing but the ask deadline ended it.
///
/// The count assertion is the load-bearing one. With the defect the task
/// never reaches `completed` AND a third ask is raised; either assertion
/// alone would catch it, but the count says *what* went wrong.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_escalations_at_two_plans_both_approved_finish_the_task() {
    let Some((pool, _cluster)) = bring_up_pg("twoasks").await else {
        return; // [SKIP]
    };
    let task_id = seed_task(&pool).await;

    // Plan A, then plan B, then a terminal plan — keyed on the index within
    // the run, so each run re-derives the plan it was suspended on. The
    // reviewer escalates the first four plans it sees, which is exactly the
    // four non-terminal reviews across the three runs (1 + 2 + 1); the
    // fifth, the terminal plan on the last run, is approved so the task can
    // end.
    //
    // The third run reviews ONE non-terminal plan rather than two because it
    // restores the completed plan A (spec D11) and resumes at plan B instead
    // of re-deriving plan A. Before D11 this number was 5.
    let handle = spawn_test_scheduler_per_run_index(&pool, escalating(4));

    // --- first escalation: plan A.
    assert_eq!(
        await_state(&pool, task_id, "awaiting_operator", 20).await,
        "awaiting_operator",
    );
    let first = kastellan_db::asks::list_pending(&pool, 10).await.expect("list")[0].clone();
    assert!(kastellan_db::asks::resolve(
        &pool,
        first.id,
        "operator",
        &serde_json::json!({"choice": "approve"}),
    )
    .await
    .expect("resolve A"));

    // --- second escalation: plan B. The resumed run gets past plan A on
    // the first approval and stops on the NEXT plan, which is a different
    // digest and therefore a genuinely new question.
    assert_eq!(
        await_state(&pool, task_id, "awaiting_operator", 20).await,
        "awaiting_operator",
    );
    let second = kastellan_db::asks::list_pending(&pool, 10).await.expect("list")[0].clone();
    assert_ne!(second.id, first.id, "the second ask must be a new row");
    assert_ne!(
        second.plan_digest, first.plan_digest,
        "the two escalations must be about genuinely different plans, or this test \
         proves nothing",
    );
    assert!(kastellan_db::asks::resolve(
        &pool,
        second.id,
        "operator",
        &serde_json::json!({"choice": "approve"}),
    )
    .await
    .expect("resolve B"));

    // --- both approvals are live at once, so the run walks plan A, plan B,
    // and then the terminal plan without asking anything again.
    assert_eq!(
        await_state(&pool, task_id, "completed", 30).await,
        "completed",
        "with only the newest approval kept, plan A re-asks and the task alternates \
         between two questions until its ask deadline",
    );
    assert_eq!(
        count_asks_for(&pool, task_id).await,
        2,
        "one ask per escalation and no more — a third means the earlier approval \
         went invisible when the later one landed",
    );
    // Both approvals were actually consulted — each exactly once, on the run
    // that reached its plan. (Before D11 this was 3: the third run restarted
    // from an empty history, re-derived plan A, and re-applied plan A's
    // approval on the way back to plan B — re-running plan A's steps with
    // it.)
    assert_eq!(
        count_audit_rows(&pool, ACTION_ASK_APPROVAL_APPLIED, task_id).await,
        2,
        "plan A on the second run, plan B on the third",
    );

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_denial_terminates_the_task_without_replanning() {
    let Some((pool, _cluster)) = bring_up_pg("deny").await else {
        return; // [SKIP]
    };
    let task_id = seed_task(&pool).await;

    // The formulator counts its calls so we can assert the resumed run
    // never asked for a plan.
    let (handle, formulate_calls) = spawn_test_scheduler_counting(&pool, escalating(1));

    assert_eq!(
        await_state(&pool, task_id, "awaiting_operator", 20).await,
        "awaiting_operator",
    );
    let calls_at_suspend = *formulate_calls.lock().unwrap();
    let ask = kastellan_db::asks::list_pending(&pool, 10).await.expect("list")[0].clone();
    assert!(kastellan_db::asks::resolve(
        &pool,
        ask.id,
        "operator",
        &serde_json::json!({"choice": "deny"}),
    )
    .await
    .expect("resolve"));

    assert_eq!(await_state(&pool, task_id, "blocked", 20).await, "blocked");
    assert_eq!(
        *formulate_calls.lock().unwrap(),
        calls_at_suspend,
        "a denied task must terminate BEFORE planning — this is the assertion that \
         fails if the denial only bound to the plan digest",
    );

    let t = tasks::get(&pool, task_id).await.expect("get").expect("a task");
    let r = t.result.expect("a denied task has a result");
    assert_eq!(r.get("kind").and_then(|v| v.as_str()), Some("denied"));
    assert_eq!(r.get("ask_id").and_then(|v| v.as_i64()), Some(ask.id));

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_different_replan_raises_a_second_ask_rather_than_riding_the_first_approval() {
    let Some((pool, _cluster)) = bring_up_pg("differs").await else {
        return; // [SKIP]
    };
    let task_id = seed_task(&pool).await;

    // Every plan escalates, and the formulator returns a DIFFERENT plan
    // each time (it varies `parameters`, which the digest includes — not
    // `context`, which it excludes).
    let handle = spawn_test_scheduler_varying(&pool, escalating(5));

    assert_eq!(
        await_state(&pool, task_id, "awaiting_operator", 20).await,
        "awaiting_operator",
    );
    let first = kastellan_db::asks::list_pending(&pool, 10).await.expect("list")[0].clone();
    assert!(kastellan_db::asks::resolve(
        &pool,
        first.id,
        "operator",
        &serde_json::json!({"choice": "approve"}),
    )
    .await
    .expect("resolve"));

    // The replan differs, so the approval must not cover it.
    assert_eq!(
        await_state(&pool, task_id, "awaiting_operator", 20).await,
        "awaiting_operator",
    );
    assert_eq!(
        count_asks_for(&pool, task_id).await,
        2,
        "an approval binds to a digest, not to a task",
    );

    handle.shutdown().await;
}

/// The resumed run formulates against the history it had at suspend time,
/// not against an empty one (spec D11).
///
/// **What this proves, exactly.** The scripted run completes plan 1 (its
/// step is dispatched), escalates on plan 2, and suspends. Before D11 the
/// resumed run rebuilt `TaskContext` with `plans: vec![]`, so the
/// formulator saw zero completed plans and re-derived plan 1 — dispatching
/// its step a second time. If plan 1 had sent an email, the operator's
/// approval would have sent it twice.
///
/// The assertion is the recorded `ctx.plans.len()` sequence: `[0, 1, 1, 2]`
/// with the restore, `[0, 1, 0, 1, 2]` without it. The third entry is the
/// first call of the resumed run, and it is the one that matters.
///
/// **What this does NOT prove.** Restoring the history makes the planner
/// *aware* of what it already did; it does not make re-execution
/// impossible. A planner that re-emits an identical step still dispatches
/// it. The dispatch assertion below holds because THIS formulator keys on
/// the history — it is a consequence of the restore, not an independent
/// idempotency guarantee, which is out of scope (spec D11, "What this does
/// NOT claim").
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resumed_run_formulates_against_the_history_it_had_at_suspend_time() {
    let Some((pool, _cluster)) = bring_up_pg("history").await else {
        return; // [SKIP]
    };
    let task_id = seed_task(&pool).await;

    // Approve plan 1 so its step actually runs, then escalate on plan 2 so
    // the suspension has a non-empty history to carry. Everything after is
    // approved, so the resumed run is free to walk forward.
    let (handle, seen, dispatched) = spawn_test_scheduler_recording_history(
        &pool,
        scripted(vec![
            Verdict::Approve,
            Verdict::Escalate("needs a human".to_string(), Severity::High),
        ]),
    );

    assert_eq!(
        await_state(&pool, task_id, "awaiting_operator", 20).await,
        "awaiting_operator",
    );
    assert_eq!(
        *seen.lock().unwrap(),
        vec![0, 1],
        "the pre-suspension run formulated twice: once with no history, once with plan 1          completed",
    );

    // The suspension carries that history on the ask itself.
    let ask = kastellan_db::asks::list_pending(&pool, 10).await.expect("list")[0].clone();
    let carried = ask.resume_state.as_ref().expect("the ask carries the run's state");
    assert_eq!(
        carried["plans"].as_array().map(Vec::len),
        Some(1),
        "one completed plan travels with the suspension, got {carried}",
    );

    assert!(kastellan_db::asks::resolve(
        &pool,
        ask.id,
        "operator",
        &serde_json::json!({"choice": "approve"}),
    )
    .await
    .expect("resolve"));

    assert_eq!(await_state(&pool, task_id, "completed", 30).await, "completed");

    assert_eq!(
        *seen.lock().unwrap(),
        vec![0, 1, 1, 2],
        "the resumed run's FIRST formulation must see the one plan the task had already          completed; a leading 0 there means it started from an empty history and          re-formulated work it had already done",
    );

    // The consequence that makes the property worth having: plan 1's step
    // ran once across the whole task, not once per run.
    let params = dispatched.lock().unwrap().clone();
    assert_eq!(
        params.iter().filter(|p| *p == &serde_json::json!({"n": 1})).count(),
        1,
        "plan 1's step must not be dispatched again on the resumed run, got {params:?}",
    );

    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_plan_count_is_monotonic_across_a_suspend_and_resume() {
    let Some((pool, _cluster)) = bring_up_pg("plancount").await else {
        return; // [SKIP]
    };
    let task_id = seed_task(&pool).await;

    // Always-escalating, identical plan — same reasoning as the approval
    // test: the resumed run must reach completion via the approval, not by
    // the reviewer changing its mind.
    let handle = spawn_test_scheduler(&pool, escalating(u32::MAX));

    assert_eq!(
        await_state(&pool, task_id, "awaiting_operator", 20).await,
        "awaiting_operator",
    );
    let before = tasks::get(&pool, task_id).await.unwrap().unwrap().plan_count;
    assert!(before >= 1, "at least one plan ran before the escalation");

    let ask = kastellan_db::asks::list_pending(&pool, 10).await.expect("list")[0].clone();
    assert!(kastellan_db::asks::resolve(
        &pool,
        ask.id,
        "operator",
        &serde_json::json!({"choice": "approve"}),
    )
    .await
    .expect("resolve"));
    assert_eq!(await_state(&pool, task_id, "completed", 20).await, "completed");

    let after = tasks::get(&pool, task_id).await.unwrap().unwrap().plan_count;
    assert!(
        after > before,
        "plan_count must not rewind across a resume (was {before}, now {after})",
    );

    handle.shutdown().await;
}
