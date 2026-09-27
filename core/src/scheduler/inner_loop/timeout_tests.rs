//! Inner-loop tests for a planning call that times out (#774): the loop
//! must not throw away gathered work when a cheaper request can still
//! produce an answer, and must say what happened when it cannot.
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
    Plan(Plan),
    Timeout,
    /// A non-timeout failure — must keep the old `llm: …` behaviour.
    Decode,
}

/// Returns scripted results by call order, and records which door each
/// call came through.
struct TimeoutScript {
    script: Mutex<VecDeque<Next>>,
    /// The answer for `formulate_synthesis_without_thinking`; `None` models
    /// a formulator with no cheaper attempt (thinking already off).
    retry: Mutex<Option<Next>>,
    synthesis_calls: AtomicUsize,
    retry_calls: AtomicUsize,
}

impl TimeoutScript {
    fn new(script: Vec<Next>, retry: Option<Next>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into()),
            retry: Mutex::new(retry),
            synthesis_calls: AtomicUsize::new(0),
            retry_calls: AtomicUsize::new(0),
        })
    }
}

async fn resolve(next: Option<Next>) -> Result<(Plan, FormulationMeta), AgentError> {
    match next {
        Some(Next::Plan(p)) => Ok((p, meta())),
        Some(Next::Timeout) => Err(real_request_timeout().await),
        Some(Next::Decode) | None => Err(AgentError::Decode {
            detail: "scripted decode failure".into(),
            raw: String::new(),
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
        resolved_asks: Vec::new(),
        origin: None,
        conversation: Vec::new(),
        conversation_task_ids: Some(Vec::new()),
    }
}

async fn run(pool: &sqlx::PgPool, f: Arc<TimeoutScript>) -> InnerLoopResult {
    let review = Arc::new(crate::cassandra::review::ChainReviewStage::new(vec![Arc::new(
        crate::cassandra::review::NoopReviewStage,
    )]));
    let ctx = claim_ctx(pool).await;
    super::run_to_terminal(pool, f, review, Arc::new(OkDispatcher), ctx, None)
        .await
        .unwrap()
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
    let Ok(pool) = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec).await else {
        return;
    };

    // ── 1. A normal turn times out after a gather → synthesis turn, answered.
    let f = TimeoutScript::new(
        vec![
            Next::Plan(one_step_plan()),
            Next::Timeout,
            Next::Plan(terminal_plan("Five bookings: …")),
        ],
        None,
    );
    let r = run(&pool, f.clone()).await;
    assert_eq!(completed_body(&r), "Five bookings: …");
    assert_eq!(f.synthesis_calls.load(Ordering::SeqCst), 1, "the answer came from the synthesis door");
    assert_eq!(r.plan_count, 2, "the timed-out call produced no plan and is not counted");
    assert_eq!(r.dispatch_count, 1);

    // ── 2. The synthesis turn itself times out → one retry, which answers.
    let f = TimeoutScript::new(
        vec![Next::Plan(one_step_plan()), Next::Timeout, Next::Timeout],
        Some(Next::Plan(terminal_plan("from the retry"))),
    );
    let r = run(&pool, f.clone()).await;
    assert_eq!(completed_body(&r), "from the retry");
    assert_eq!(f.retry_calls.load(Ordering::SeqCst), 1);

    // ── 3. Synthesis times out and no cheaper attempt exists → a worded failure.
    let f = TimeoutScript::new(
        vec![Next::Plan(one_step_plan()), Next::Timeout, Next::Timeout],
        None,
    );
    let r = run(&pool, f.clone()).await;
    let d = failed_detail(&r);
    assert!(d.starts_with("The language model ran out of time before it could write an answer"), "{d}");
    assert!(d.contains("1 tool call (1 succeeded): read ×1."), "{d}");
    assert!(d.contains("[llm: router: HTTP transport error") && d.contains("timed out"), "{d}");
    assert_eq!(f.synthesis_calls.load(Ordering::SeqCst), 1, "never a second synthesis turn");

    // ── 4. The retry times out too → the same worded failure, after exactly one retry.
    let f = TimeoutScript::new(
        vec![Next::Plan(one_step_plan()), Next::Timeout, Next::Timeout],
        Some(Next::Timeout),
    );
    let r = run(&pool, f.clone()).await;
    assert!(failed_detail(&r).starts_with("The language model ran out of time"), "{}", failed_detail(&r));
    assert_eq!(f.retry_calls.load(Ordering::SeqCst), 1);

    // ── 5. A timeout before anything was gathered → worded failure, no synthesis.
    let f = TimeoutScript::new(vec![Next::Timeout], Some(Next::Plan(terminal_plan("unused"))));
    let r = run(&pool, f.clone()).await;
    let d = failed_detail(&r);
    assert!(d.contains("no tool was called"), "{d}");
    assert_eq!(f.synthesis_calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.retry_calls.load(Ordering::SeqCst), 0, "the retry is for synthesis turns only");

    // ── 6. A NON-timeout failure after a gather is unchanged: `llm: …`, no synthesis.
    let f = TimeoutScript::new(vec![Next::Plan(one_step_plan()), Next::Decode], None);
    let r = run(&pool, f.clone()).await;
    assert!(failed_detail(&r).starts_with("llm: plan decode failed"), "{}", failed_detail(&r));
    assert_eq!(f.synthesis_calls.load(Ordering::SeqCst), 0);
}
