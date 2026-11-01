//! Inner-loop tests for a planning call that times out (#774) or comes
//! back empty (#923): the loop must retry when a cheaper request (after a
//! timeout) or the same request (after an empty answer) can still produce
//! an answer, must not throw away gathered work, and must say what happened
//! when it cannot. Cases 12–14 are the fast lane's wall-clock ceiling
//! (#922), which a run reaches between plans and answers like the plan cap.
//!
//! The decisions are unit-tested in `llm_failure/tests.rs`; these run the
//! real `run_to_terminal` against a real Postgres and a formulator whose
//! timeouts are **real** `reqwest` request timeouts (a router pointed at a
//! listener that accepts and never answers), because a hand-built error
//! could satisfy `is_request_timeout` in a way production never produces.
//!
//! Skips silently when Postgres / the supervisor are unavailable, like the
//! other PG-backed tests in this module.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::tests::inner_loop_test_stubs::{one_step_plan, terminal_plan, OkDispatcher};
use super::*;
use crate::cassandra::types::Plan;
use crate::scheduler::agent::{AgentError, FormulationMeta, PlanFormulator};

/// A genuine request timeout out of the real router: the backend accepts
/// the connection and never replies.
async fn real_request_timeout() -> AgentError {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let hold = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.expect("accept");
        tokio::time::sleep(Duration::from_secs(10)).await;
        drop(sock);
    });
    let router = kastellan_llm_router::Router::new(kastellan_llm_router::RouterConfig {
        local_url: url,
        timeout: Duration::from_millis(150),
        ..Default::default()
    })
    .expect("router");
    let req = kastellan_llm_router::ChatRequest::new(
        "m",
        vec![kastellan_llm_router::ChatMessage::user("hi")],
    );
    let err = router.send(&req).await.expect_err("must time out");
    hold.abort();
    let err = AgentError::Router(err);
    assert!(err.is_request_timeout(), "fixture must be a real request timeout: {err}");
    err
}

/// One scripted formulator call.
enum Next {
    /// Boxed: `Plan` is ~440 bytes and the other variants carry nothing
    /// (`clippy::large_enum_variant`).
    Plan(Box<Plan>),
    Timeout,
    /// An answer that is not a plan — re-asked once through the reformat
    /// door when the script has a `reformat` answer (#945), else fails.
    Decode,
    /// An answer with no content at all (#923) — retried once, unless it is
    /// itself the turn's retry.
    Empty,
    /// A plan that takes this long to formulate — runs a task past its
    /// deadline with a real clock.
    SlowPlan(Box<Plan>, Duration),
}

/// Returns scripted results by call order, and records which door each
/// call came through.
struct TimeoutScript {
    script: Mutex<VecDeque<Next>>,
    /// The answer for `formulate_synthesis_without_thinking`; `None` models
    /// a formulator with no cheaper attempt (thinking already off).
    retry: Mutex<Option<Next>>,
    /// The answer for `formulate_reformat` (#945); `None` models a
    /// formulator with no re-ask, the trait's default.
    reformat: Mutex<Option<Next>>,
    /// The replies each re-ask was shown, in order.
    reformat_replies: Mutex<Vec<String>>,
    synthesis_calls: AtomicUsize,
    retry_calls: AtomicUsize,
}

impl TimeoutScript {
    fn new(script: Vec<Next>, retry: Option<Next>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into()),
            retry: Mutex::new(retry),
            reformat: Mutex::new(None),
            reformat_replies: Mutex::new(Vec::new()),
            synthesis_calls: AtomicUsize::new(0),
            retry_calls: AtomicUsize::new(0),
        })
    }
}

async fn resolve(next: Option<Next>) -> Result<(Plan, FormulationMeta), AgentError> {
    match next {
        Some(Next::Plan(p)) => Ok((*p, meta())),
        Some(Next::SlowPlan(p, d)) => {
            tokio::time::sleep(d).await;
            Ok((*p, meta()))
        }
        Some(Next::Timeout) => Err(real_request_timeout().await),
        Some(Next::Empty) => Err(AgentError::Decode {
            detail: "EOF while parsing a value at line 1 column 0".into(),
            raw: String::new(),
            usage: Default::default(),
            latency_ms: 4321,
        }),
        Some(Next::Decode) | None => Err(AgentError::Decode {
            detail: "scripted decode failure".into(),
            raw: "Sure! Here is what I would do.".into(),
            usage: Default::default(),
            // Non-zero so case 6 proves the row carries the error's own latency, not a default 0.
            latency_ms: 4321,
        }),
    }
}

#[async_trait::async_trait]
impl PlanFormulator for TimeoutScript {
    async fn formulate_plan(&self, _ctx: &TaskContext) -> Result<(Plan, FormulationMeta), AgentError> {
        let next = self.script.lock().unwrap().pop_front();
        resolve(next).await
    }

    async fn formulate_synthesis(
        &self,
        ctx: &TaskContext,
    ) -> Result<(Plan, FormulationMeta), AgentError> {
        self.synthesis_calls.fetch_add(1, Ordering::SeqCst);
        self.formulate_plan(ctx).await
    }

    async fn formulate_synthesis_without_thinking(
        &self,
        _ctx: &TaskContext,
    ) -> Option<Result<(Plan, FormulationMeta), AgentError>> {
        let next = self.retry.lock().unwrap().take()?;
        self.retry_calls.fetch_add(1, Ordering::SeqCst);
        Some(resolve(Some(next)).await)
    }

    async fn formulate_reformat(
        &self,
        _ctx: &TaskContext,
        _synth_turn: bool,
        reply: &str,
    ) -> Option<Result<(Plan, FormulationMeta), AgentError>> {
        let next = self.reformat.lock().unwrap().take()?;
        self.reformat_replies.lock().unwrap().push(reply.to_owned());
        Some(resolve(Some(next)).await)
    }
}

fn meta() -> FormulationMeta {
    FormulationMeta {
        prompt_name: "agent_planner".into(),
        prompt_sha256: "test".into(),
        llm_model: "test-model".into(),
        llm_backend: "local".into(),
        latency_ms: 1,
        retry_count: 0,
        assembled_prompt_sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
        l0_count: 0,
        l1_count: 0,
        skill_count: 0,
        recalled_memory_ids: Vec::new(),
        recall_count: 0,
        recall_query_sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
        graph_seed_entity_ids: Vec::new(),
        graph_seed_count: 0,
        graph_seed_source: crate::entity_extraction::SeedSource::None,
        usage: Default::default(),
    }
}

/// Insert + claim a fresh Fast task. `max_plans` is generous so only a
/// timeout — never the cap — can trigger the synthesis turn.
async fn claim_ctx(pool: &sqlx::PgPool) -> TaskContext {
    let id = kastellan_db::tasks::insert_pending(pool, kastellan_db::tasks::Lane::Fast, serde_json::json!({}))
        .await
        .unwrap();
    let _ = kastellan_db::tasks::claim_one(pool, kastellan_db::tasks::Lane::Fast, 60)
        .await
        .unwrap()
        .unwrap();
    TaskContext {
        task_id: id,
        lane: kastellan_db::tasks::Lane::Fast,
        instruction: "the 5 most recent domestic bookings, as a table".into(),
        classification_floor: crate::cassandra::types::DataClass::Public,
        classification_floor_source: ClassificationFloorSource::Default,
        classification_floor_signals: vec![],
        plans: vec![],
        advisories: vec![],
        blocks: vec![],
        plan_count: 0,
        max_plans: 10,
        deadline: None,
        resolved_asks: Vec::new(),
        origin: None,
        conversation: Vec::new(),
        conversation_task_ids: Some(Vec::new()),
    }
}

async fn run(pool: &sqlx::PgPool, f: Arc<TimeoutScript>) -> (i64, InnerLoopResult) {
    run_until(pool, f, None).await
}

/// [`run`] with a wall-clock deadline on the task.
async fn run_until(
    pool: &sqlx::PgPool,
    f: Arc<TimeoutScript>,
    deadline: Option<std::time::Instant>,
) -> (i64, InnerLoopResult) {
    let review = Arc::new(crate::cassandra::review::ChainReviewStage::new(vec![Arc::new(
        crate::cassandra::review::NoopReviewStage,
    )]));
    let mut ctx = claim_ctx(pool).await;
    ctx.deadline = deadline;
    let task_id = ctx.task_id;
    let r = super::run_to_terminal(pool, f, review, Arc::new(OkDispatcher), ctx, None)
        .await
        .unwrap();
    (task_id, r)
}

/// `agent/<action>` rows for one task, as their payloads, oldest first.
async fn agent_rows(pool: &sqlx::PgPool, task_id: i64, action: &str) -> Vec<serde_json::Value> {
    sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT payload FROM audit_log WHERE actor = 'agent' AND action = $1 \
         AND (payload->>'task_id')::bigint = $2 ORDER BY id",
    )
    .bind(action)
    .bind(task_id)
    .fetch_all(pool)
    .await
    .expect("audit query")
}

fn failed_detail(r: &InnerLoopResult) -> &str {
    match &r.outcome {
        Outcome::Failed(d) => d,
        o => panic!("expected Failed, got {o:?}"),
    }
}

fn completed_body(r: &InnerLoopResult) -> String {
    match &r.outcome {
        Outcome::Completed(v) => v["body"].as_str().unwrap_or_default().to_string(),
        o => panic!("expected Completed, got {o:?}"),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_planning_timeout_after_gathering_answers_from_what_was_gathered() {
    if kastellan_tests_common::skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = kastellan_tests_common::pg_bin_dir_or_skip() else {
        return;
    };
    let suffix = format!("iltm-{}", kastellan_tests_common::unique_suffix());
    let service_name = format!("kastellan-sched-test-pg-{suffix}");
    let cluster = tokio::task::block_in_place(|| {
        kastellan_tests_common::bring_up_pg_cluster(&bin_dir, "it-d", "it-l", &service_name)
    });
    kastellan_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"purpose": "inner-loop-timeout-unit"}),
    )
    .await
    .ok();
    // The cluster is up, so a failed connect is a failure, not a skip.
    let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("connect runtime pool");

    // ── 1. A normal turn times out after a gather → synthesis turn, answered.
    let f = TimeoutScript::new(
        vec![
            Next::Plan(Box::new(one_step_plan())),
            Next::Timeout,
            Next::Plan(Box::new(terminal_plan("Five bookings: …"))),
        ],
        None,
    );
    let (task_id, r) = run(&pool, f.clone()).await;
    assert_eq!(completed_body(&r), "Five bookings: …");
    assert_eq!(f.synthesis_calls.load(Ordering::SeqCst), 1, "the answer came from the synthesis door");
    assert_eq!(r.plan_count, 2, "the timed-out call produced no plan");
    assert_eq!(r.failed_llm_calls, 1, "…but it is a model call, counted separately");
    assert_eq!(r.dispatch_count, 1);
    let failed = agent_rows(&pool, task_id, "plan.formulate_failed").await;
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["request_timeout"], true);
    assert_eq!(failed[0]["synth_turn"], false);
    assert!(failed[0].get("llm_usage").is_none(), "a timeout got no completion to cost: {}", failed[0]);

    // ── 2. The synthesis turn itself times out → one retry, which answers.
    let f = TimeoutScript::new(
        vec![Next::Plan(Box::new(one_step_plan())), Next::Timeout, Next::Timeout],
        Some(Next::Plan(Box::new(terminal_plan("from the retry")))),
    );
    let (task_id, r) = run(&pool, f.clone()).await;
    assert_eq!(completed_body(&r), "from the retry");
    assert_eq!(f.retry_calls.load(Ordering::SeqCst), 1);
    assert_eq!(r.failed_llm_calls, 2, "the planning timeout and the synthesis timeout");
    let failed = agent_rows(&pool, task_id, "plan.formulate_failed").await;
    assert_eq!(failed.len(), 2, "{failed:?}");
    assert_eq!(failed[1]["synth_turn"], true);
    assert_eq!(failed[1]["thinking_suppressed_retry"], false, "the FIRST synthesis attempt timed out");
    let plans = agent_rows(&pool, task_id, "plan.formulate").await;
    assert_eq!(plans.last().unwrap()["retry_count"], 1, "the retry's row says it was one");
    assert_eq!(plans[0]["retry_count"], 0);

    // ── 3. Synthesis times out and no cheaper attempt exists → a worded failure.
    let f = TimeoutScript::new(
        vec![Next::Plan(Box::new(one_step_plan())), Next::Timeout, Next::Timeout],
        None,
    );
    let (task_id, r) = run(&pool, f.clone()).await;
    let d = failed_detail(&r);
    assert!(d.starts_with("The language model ran out of time before it could write an answer"), "{d}");
    assert!(d.contains("1 tool call (1 succeeded): read ×1."), "{d}");
    assert!(d.contains("[llm: router: HTTP transport error") && d.contains("timed out"), "{d}");
    assert_eq!(f.synthesis_calls.load(Ordering::SeqCst), 1, "never a second synthesis turn");
    assert_eq!(r.failed_llm_calls, 2);
    assert_eq!(agent_rows(&pool, task_id, "plan.formulate_failed").await.len(), 2, "one row per failed call, never two");

    // ── 4. The retry times out too → the same worded failure, after exactly one retry.
    let f = TimeoutScript::new(
        vec![Next::Plan(Box::new(one_step_plan())), Next::Timeout, Next::Timeout],
        Some(Next::Timeout),
    );
    let (task_id, r) = run(&pool, f.clone()).await;
    assert!(failed_detail(&r).starts_with("The language model ran out of time"), "{}", failed_detail(&r));
    assert_eq!(f.retry_calls.load(Ordering::SeqCst), 1);
    assert_eq!(r.failed_llm_calls, 3, "planning timeout, synthesis timeout, retry timeout");
    let failed = agent_rows(&pool, task_id, "plan.formulate_failed").await;
    assert_eq!(failed.len(), 3, "{failed:?}");
    assert_eq!(failed[2]["thinking_suppressed_retry"], true);

    // ── 5. A timeout before anything was gathered → worded failure, no synthesis.
    let f = TimeoutScript::new(vec![Next::Timeout], Some(Next::Plan(Box::new(terminal_plan("unused")))));
    let (_, r) = run(&pool, f.clone()).await;
    let d = failed_detail(&r);
    assert!(d.contains("no tool was called"), "{d}");
    assert_eq!(f.synthesis_calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.retry_calls.load(Ordering::SeqCst), 0, "the retry is for synthesis turns only");
    assert_eq!(r.failed_llm_calls, 1);

    // ── 6. A non-plan answer after a gather fails at once, in words: no retry, no synthesis.
    let f = TimeoutScript::new(
        vec![
            Next::Plan(Box::new(one_step_plan())),
            Next::Decode,
            Next::Plan(Box::new(terminal_plan("never reached"))),
        ],
        None,
    );
    let (task_id, r) = run(&pool, f.clone()).await;
    let d = failed_detail(&r);
    assert!(d.starts_with("The language model answered with something that is not a plan"), "{d}");
    assert!(d.ends_with("[llm: plan decode failed: scripted decode failure]"), "{d}");
    assert_eq!(f.synthesis_calls.load(Ordering::SeqCst), 0);
    assert_eq!(r.failed_llm_calls, 1, "a final non-timeout failure is a model call too");
    let failed = agent_rows(&pool, task_id, "plan.formulate_failed").await;
    assert_eq!(failed[0]["request_timeout"], false);
    // The completion that failed to decode is costed on its row (#923); a
    // timeout's row (case 1) has no completion, so no such keys.
    assert_eq!(failed[0]["latency_ms"], 4321, "{}", failed[0]);
    assert!(failed[0]["llm_usage"].is_object(), "{}", failed[0]);
    // ...and so is what the model said instead of a plan, as stored (#945).
    assert_eq!(failed[0]["raw_head"], "Sure! Here is what I would do.", "{}", failed[0]);

    // ── 7. A timeout-forced synthesis that asks for more tools must not claim
    //       the plan cap, which was never reached (#774 review).
    let f = TimeoutScript::new(
        vec![Next::Plan(Box::new(one_step_plan())), Next::Timeout, Next::Plan(Box::new(one_step_plan()))],
        None,
    );
    let (_, r) = run(&pool, f.clone()).await;
    let d = failed_detail(&r);
    assert!(d.starts_with("A planning call ran out of time"), "{d}");
    assert!(!d.contains("plan_iteration_cap_exceeded"), "{d}");
    assert_eq!(r.dispatch_count, 1, "the synthesis turn's steps must not run");

    // ── 8. An empty answer is asked again once; the retry's plan is used (#923).
    let f = TimeoutScript::new(
        vec![Next::Empty, Next::Plan(Box::new(terminal_plan("after an empty answer")))],
        None,
    );
    let (task_id, r) = run(&pool, f.clone()).await;
    assert_eq!(completed_body(&r), "after an empty answer");
    assert_eq!(f.synthesis_calls.load(Ordering::SeqCst), 0, "a planning turn retries through the PLAN door");
    assert_eq!(r.failed_llm_calls, 1, "the empty answer is a model call");
    let failed = agent_rows(&pool, task_id, "plan.formulate_failed").await;
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["thinking_suppressed_retry"], false, "{}", failed[0]);
    let plans = agent_rows(&pool, task_id, "plan.formulate").await;
    assert_eq!(plans.last().unwrap()["retry_count"], 1, "the retry's row says it was one");

    // ── 9. Empty twice → a worded failure after exactly one retry; the
    //       third scripted answer is never asked for.
    let f = TimeoutScript::new(
        vec![Next::Empty, Next::Empty, Next::Plan(Box::new(terminal_plan("never reached")))],
        None,
    );
    let (task_id, r) = run(&pool, f.clone()).await;
    let d = failed_detail(&r);
    assert!(d.starts_with("The language model returned an empty answer"), "{d}");
    assert!(d.ends_with("[llm: plan decode failed: EOF while parsing a value at line 1 column 0]"), "{d}");
    assert_eq!(r.failed_llm_calls, 2);
    let failed = agent_rows(&pool, task_id, "plan.formulate_failed").await;
    assert_eq!(failed.len(), 2, "one row per failed call: {failed:?}");
    assert_eq!(failed[1]["thinking_suppressed_retry"], false, "a plain retry: {}", failed[1]);
    assert_eq!(r.plan_count, 0);
    assert_eq!(f.synthesis_calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.script.lock().unwrap().len(), 1, "the third answer is never asked for");

    // ── 10. An empty synthesis answer is retried through the SYNTHESIS door.
    let f = TimeoutScript::new(
        vec![
            Next::Plan(Box::new(one_step_plan())),
            Next::Timeout,
            Next::Empty,
            Next::Plan(Box::new(terminal_plan("synthesis retry"))),
        ],
        None,
    );
    let (task_id, r) = run(&pool, f.clone()).await;
    assert_eq!(completed_body(&r), "synthesis retry");
    assert_eq!(f.synthesis_calls.load(Ordering::SeqCst), 2, "both synthesis attempts used the synthesis door");
    assert_eq!(f.retry_calls.load(Ordering::SeqCst), 0, "an empty answer is not a timeout: thinking stays as configured");
    assert_eq!(r.failed_llm_calls, 2, "the planning timeout and the empty synthesis answer");
    let failed = agent_rows(&pool, task_id, "plan.formulate_failed").await;
    assert_eq!(failed.len(), 2, "{failed:?}");
    assert_eq!(failed[1]["synth_turn"], true, "{}", failed[1]);
    let plans = agent_rows(&pool, task_id, "plan.formulate").await;
    assert_eq!(plans.last().unwrap()["retry_count"], 1, "the retry's row says it was one");

    // ── 11. The thinking-suppressed synthesis retry comes back empty: the
    //        turn's one retry is spent, so it is not asked again.
    let f = TimeoutScript::new(
        vec![
            Next::Plan(Box::new(one_step_plan())),
            Next::Timeout,
            Next::Timeout,
            Next::Plan(Box::new(terminal_plan("never reached"))),
        ],
        Some(Next::Empty),
    );
    let (task_id, r) = run(&pool, f.clone()).await;
    let d = failed_detail(&r);
    assert!(d.starts_with("The language model returned an empty answer"), "{d}");
    assert_eq!(f.retry_calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.synthesis_calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.script.lock().unwrap().len(), 1, "no plain retry after the thinking-off one");
    assert_eq!(r.failed_llm_calls, 3);
    let failed = agent_rows(&pool, task_id, "plan.formulate_failed").await;
    assert_eq!(failed.len(), 3, "{failed:?}");
    assert_eq!(failed[2]["thinking_suppressed_retry"], true, "{}", failed[2]);

    // ── 12. The deadline passes during a gathering plan: the next turn is the
    //        forced synthesis, exactly as at the plan cap (#922).
    let slow = || Next::SlowPlan(Box::new(one_step_plan()), Duration::from_millis(300));
    let soon = || Some(std::time::Instant::now() + Duration::from_millis(100));
    let f = TimeoutScript::new(vec![slow(), Next::Plan(Box::new(terminal_plan("out of time")))], None);
    let (_, r) = run_until(&pool, f.clone(), soon()).await;
    assert_eq!(completed_body(&r), "out of time");
    assert_eq!(f.synthesis_calls.load(Ordering::SeqCst), 1, "the answer came through the synthesis door");
    assert_eq!(r.dispatch_count, 1);
    assert_eq!(r.plan_count, 2, "gather turn + synthesis turn, far below max_plans");

    // ── 13. Nothing gathered when the deadline passes: fails like the cap,
    //        naming the ceiling, and asks the planner nothing.
    let f = TimeoutScript::new(vec![Next::Plan(Box::new(terminal_plan("never reached")))], None);
    let (_, r) = run_until(&pool, f.clone(), Some(std::time::Instant::now())).await;
    assert_eq!(failed_detail(&r), "plan_time_ceiling_exceeded");
    assert_eq!(r.plan_count, 0);
    assert_eq!(f.script.lock().unwrap().len(), 1, "no formulation after the deadline");

    // ── 14. The synthesis turn after the deadline still plans a step.
    let f = TimeoutScript::new(vec![slow(), Next::Plan(Box::new(one_step_plan()))], None);
    let (_, r) = run_until(&pool, f.clone(), soon()).await;
    assert_eq!(
        failed_detail(&r),
        "plan_time_ceiling_exceeded; forced synthesis did not produce a final answer"
    );
    assert_eq!(r.dispatch_count, 1, "the synthesis turn's step never ran");

    // ── 15. A non-plan answer is re-asked once with the model's own reply,
    //        and the re-ask's plan answers the task (#945).
    let f = TimeoutScript::new(vec![Next::Plan(Box::new(one_step_plan())), Next::Decode], None);
    *f.reformat.lock().unwrap() = Some(Next::Plan(Box::new(terminal_plan("reformatted"))));
    let (task_id, r) = run(&pool, f.clone()).await;
    assert_eq!(completed_body(&r), "reformatted");
    assert_eq!(*f.reformat_replies.lock().unwrap(), ["Sure! Here is what I would do."], "shown its own reply");
    assert_eq!(r.failed_llm_calls, 1);
    let failed = agent_rows(&pool, task_id, "plan.formulate_failed").await;
    assert_eq!(failed.len(), 1, "one row for the non-plan answer: {failed:?}");
    let plans = agent_rows(&pool, task_id, "plan.formulate").await;
    assert_eq!(plans.last().unwrap()["retry_count"], 1, "the re-ask's row says it was one");

    // ── 16. The re-ask is not a plan either: the turn's one retry is spent,
    //        and the task fails in the same words as before #945.
    let f = TimeoutScript::new(
        vec![Next::Plan(Box::new(one_step_plan())), Next::Decode, Next::Plan(Box::new(terminal_plan("never reached")))],
        None,
    );
    *f.reformat.lock().unwrap() = Some(Next::Decode);
    let (task_id, r) = run(&pool, f.clone()).await;
    let d = failed_detail(&r);
    assert!(d.starts_with("The language model answered with something that is not a plan"), "{d}");
    assert_eq!(f.reformat_replies.lock().unwrap().len(), 1, "re-asked once, never twice");
    assert_eq!(f.script.lock().unwrap().len(), 1, "no plain retry after the re-ask");
    assert_eq!(r.failed_llm_calls, 2);
    let failed = agent_rows(&pool, task_id, "plan.formulate_failed").await;
    assert_eq!(failed.len(), 2, "one row per failed call: {failed:?}");
    assert_eq!(failed[1]["thinking_suppressed_retry"], true, "the re-ask runs with thinking off: {}", failed[1]);
}
