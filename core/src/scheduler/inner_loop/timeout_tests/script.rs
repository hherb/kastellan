//! The scripted formulator and the run/read helpers the timeout tests share,
//! moved out of `timeout_tests.rs` (over the 500-line cap). Items and the
//! `TimeoutScript` fields gain `pub(super)`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::super::tests::inner_loop_test_stubs::OkDispatcher;
use super::super::*;
use crate::cassandra::types::Plan;
use crate::scheduler::agent::{AgentError, FormulationMeta, PlanFormulator};

/// A genuine request timeout out of the real router: the backend accepts
/// the connection and never replies.
pub(super) async fn real_request_timeout() -> AgentError {
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
pub(super) enum Next {
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
pub(super) struct TimeoutScript {
    pub(super) script: Mutex<VecDeque<Next>>,
    /// The answer for `formulate_synthesis_without_thinking`; `None` models
    /// a formulator with no cheaper attempt (thinking already off).
    pub(super) retry: Mutex<Option<Next>>,
    /// The answer for `formulate_reformat` (#945); `None` models a
    /// formulator with no re-ask, the trait's default.
    pub(super) reformat: Mutex<Option<Next>>,
    /// Every `formulate_reformat` call, `(synth_turn, reply)`, in order —
    /// recorded before the scripted answer is taken, so a second call is
    /// counted even though it finds no answer left.
    pub(super) reformat_replies: Mutex<Vec<(bool, String)>>,
    pub(super) synthesis_calls: AtomicUsize,
    pub(super) retry_calls: AtomicUsize,
}

impl TimeoutScript {
    pub(super) fn new(script: Vec<Next>, retry: Option<Next>) -> Arc<Self> {
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

pub(super) async fn resolve(next: Option<Next>) -> Result<(Plan, FormulationMeta), AgentError> {
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
        synth_turn: bool,
        reply: &str,
    ) -> Option<Result<(Plan, FormulationMeta), AgentError>> {
        self.reformat_replies.lock().unwrap().push((synth_turn, reply.to_owned()));
        let next = self.reformat.lock().unwrap().take()?;
        Some(resolve(Some(next)).await)
    }
}

pub(super) fn meta() -> FormulationMeta {
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
pub(super) async fn claim_ctx(pool: &sqlx::PgPool) -> TaskContext {
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

pub(super) async fn run(pool: &sqlx::PgPool, f: Arc<TimeoutScript>) -> (i64, InnerLoopResult) {
    run_until(pool, f, None).await
}

/// [`run`] with a wall-clock deadline on the task.
pub(super) async fn run_until(
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
pub(super) async fn agent_rows(pool: &sqlx::PgPool, task_id: i64, action: &str) -> Vec<serde_json::Value> {
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

pub(super) fn failed_detail(r: &InnerLoopResult) -> &str {
    match &r.outcome {
        Outcome::Failed(d) => d,
        o => panic!("expected Failed, got {o:?}"),
    }
}

pub(super) fn completed_body(r: &InnerLoopResult) -> String {
    match &r.outcome {
        Outcome::Completed(v) => v["body"].as_str().unwrap_or_default().to_string(),
        o => panic!("expected Completed, got {o:?}"),
    }
}
