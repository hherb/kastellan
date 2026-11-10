//! The notebook loop's test harness (#922): a scripted model, a recording
//! reviewer, scripted tools, reply builders and a task on a real Postgres.
//! Shared by the loop's tests and the sift tests; the sorter script, its
//! delay and the cancel hook serve the latter.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kastellan_db::notebook::{self as db, EntryKind};
use kastellan_llm_router::ThinkingPolicy;
use serde_json::{json, Value};

use super::audit::ACTION_PLAN_DISCARDED;
use super::run::*;
pub(crate) use crate::cassandra::review::ChainReviewStage;
pub(crate) use crate::cassandra::types::DataClass;
pub(crate) use crate::scheduler::inner_loop::{InnerLoopResult, Outcome, StepDispatcher, StepOutcome, TaskContext};
pub(crate) use crate::scheduler::notebook::formulator::NotebookFormulator;
use crate::cassandra::review::{ReviewStage, ReviewStageContext};
use crate::cassandra::types::{Plan, PlannedStep, Verdict};
use crate::scheduler::agent::{AgentError, FormulationMeta};
use crate::scheduler::inner_loop::ClassificationFloorSource;
use crate::scheduler::notebook::formulator::UserMessage;
pub(crate) use crate::scheduler::notebook::sift::config::SiftConfig;
use crate::scheduler::notebook::sift::sorter::Sorter;

pub(crate) const RUNAWAY: &str = "<runaway>";
/// A scripted reply that is a real request timeout ([`real_request_timeout`]).
pub(crate) const TIMEOUT: &str = "<timeout>";

/// A genuine request timeout out of the real router: the backend accepts
/// the connection and never replies.
pub(crate) async fn real_request_timeout() -> AgentError {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let hold = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.expect("accept");
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        drop(sock);
    });
    let router = kastellan_llm_router::Router::new(kastellan_llm_router::RouterConfig {
        local_url: url,
        timeout: std::time::Duration::from_millis(150),
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

// ── scripted model ──────────────────────────────────────────────────────

/// Returns `replies` in order, and records each turn's rendered message and
/// whether it was a re-ask. `on_first_call` runs during the first turn (an
/// operator typing a correction while the model thinks).
pub(crate) struct Script {
    pub(crate) replies: Mutex<VecDeque<String>>,
    pub(crate) messages: Mutex<Vec<String>>,
    pub(crate) reasks: Mutex<Vec<String>>,
    pub(crate) thinking: Mutex<Vec<ThinkingPolicy>>,
    /// What `thinks_by_default` answers: `false` is a backend configured
    /// not to think (`KASTELLAN_LLM_DISABLE_THINKING=1`).
    pub(crate) thinks: bool,
    pub(crate) correction_during_first_turn: Option<(sqlx::PgPool, i64, &'static str)>,
    /// The sorter's replies, in order (`sift`, #922); out of script it errs.
    pub(crate) sorter_replies: Mutex<VecDeque<String>>,
    /// Each message the sorter was sent.
    pub(crate) sorted: Mutex<Vec<String>>,
    pub(crate) sift_cfg: SiftConfig,
    /// How long each sorter call takes (a deadline test).
    pub(crate) sort_delay: std::time::Duration,
    /// Cancel this task during the first sorter call (an operator's cancel).
    pub(crate) cancel_during_sort: Option<(sqlx::PgPool, i64)>,
    /// How long each model turn takes (a task run past its deadline).
    pub(crate) turn_delay: std::time::Duration,
}

impl Script {
    pub(crate) fn new(replies: &[Value]) -> Self {
        Self::raw(&replies.iter().map(Value::to_string).collect::<Vec<_>>())
    }

    pub(crate) fn raw(replies: &[String]) -> Self {
        Self {
            replies: Mutex::new(replies.iter().cloned().collect()),
            messages: Mutex::new(Vec::new()),
            reasks: Mutex::new(Vec::new()),
            thinking: Mutex::new(Vec::new()),
            thinks: true,
            correction_during_first_turn: None,
            sorter_replies: Mutex::new(VecDeque::new()),
            sorted: Mutex::new(Vec::new()),
            sift_cfg: SiftConfig { max_reads: 64, read_methods: vec!["mail.read".into(), "esc.read".into()] },
            sort_delay: std::time::Duration::ZERO,
            cancel_during_sort: None,
            turn_delay: std::time::Duration::ZERO,
        }
    }

    /// The sorter answers `replies`, in order.
    pub(crate) fn sorting(self, replies: &[Value]) -> Self {
        *self.sorter_replies.lock().unwrap() = replies.iter().map(Value::to_string).collect();
        self
    }

    pub(crate) fn messages(&self) -> Vec<String> {
        self.messages.lock().unwrap().clone()
    }
}

#[async_trait]
impl NotebookFormulator for Script {
    async fn notebook_turn(
        &self,
        _ctx: &TaskContext,
        user_msg: &UserMessage<'_>,
        prior: Option<(&str, &str)>,
        thinking: ThinkingPolicy,
    ) -> Result<(String, FormulationMeta), AgentError> {
        let first = self.messages.lock().unwrap().is_empty();
        self.messages.lock().unwrap().push(user_msg(None));
        if let Some((_, reason)) = prior {
            self.reasks.lock().unwrap().push(reason.to_string());
        }
        if let (true, Some((pool, task, text))) = (first, &self.correction_during_first_turn) {
            db::append(pool, *task, EntryKind::Correction, json!({"text": text})).await.unwrap();
        }
        // Out of script: a non-blank reply that is not a notebook reply, so it
        // is re-asked once and then fails — never an empty one, which would be
        // retried as a blank completion.
        let reply = self.replies.lock().unwrap().pop_front().unwrap_or_else(|| "out of script".into());
        self.thinking.lock().unwrap().push(thinking);
        tokio::time::sleep(self.turn_delay).await;
        if reply == TIMEOUT {
            return Err(real_request_timeout().await);
        }
        // `RUNAWAY` stands for a reply cut off at `max_tokens` with nothing said.
        if reply == RUNAWAY {
            let mut m = meta();
            m.usage.finish_reason = Some("length".into());
            return Ok((String::new(), m));
        }
        Ok((reply, meta()))
    }

    fn tool_doc(&self, tool: &str) -> Option<String> {
        Some(format!("- {tool} (method: {tool}.call): a scripted tool\n"))
    }

    fn thinks_by_default(&self) -> bool {
        self.thinks
    }

    fn sorter(&self) -> Option<&dyn Sorter> {
        Some(self)
    }

    fn sift_config(&self) -> SiftConfig {
        self.sift_cfg.clone()
    }
}

#[async_trait]
impl Sorter for Script {
    async fn sort(&self, _ctx: &TaskContext, user_msg: String) -> Result<String, AgentError> {
        tokio::time::sleep(self.sort_delay).await;
        let first = self.sorted.lock().unwrap().is_empty();
        if let (true, Some((pool, task))) = (first, &self.cancel_during_sort) {
            kastellan_db::tasks::mark_cancelled(pool, *task).await.unwrap();
        }
        self.sorted.lock().unwrap().push(user_msg);
        self.sorter_replies.lock().unwrap().pop_front().ok_or_else(|| AgentError::PromptMissing)
    }
}

pub(crate) fn meta() -> FormulationMeta {
    FormulationMeta {
        prompt_name: "agent_notebook".into(),
        prompt_sha256: "test".into(),
        llm_model: "test-model".into(),
        llm_backend: "local".into(),
        latency_ms: 1,
        retry_count: 0,
        assembled_prompt_sha256: "0".repeat(64),
        l0_count: 0,
        l1_count: 0,
        skill_count: 0,
        recalled_memory_ids: Vec::new(),
        recall_count: 0,
        recall_query_sha256: "0".repeat(64),
        graph_seed_entity_ids: Vec::new(),
        graph_seed_count: 0,
        graph_seed_source: crate::entity_extraction::SeedSource::None,
        usage: Default::default(),
    }
}

// ── scripted review and tools ───────────────────────────────────────────

/// Approves everything except a step whose tool is `forbidden` (blocked) or
/// `esc` (escalated), and records the instruction it was shown and the
/// steps' methods for each plan.
#[derive(Default)]
pub(crate) struct RecordingReview {
    pub(crate) instructions: Mutex<Vec<String>>,
    pub(crate) plans: Mutex<Vec<Vec<String>>>,
}

#[async_trait]
impl ReviewStage for RecordingReview {
    fn name(&self) -> &str {
        "recording"
    }

    async fn review(&self, plan: &Plan, ctx: &ReviewStageContext<'_>) -> Verdict {
        self.instructions.lock().unwrap().push(ctx.instruction.to_string());
        self.plans.lock().unwrap().push(plan.steps.iter().map(|s| s.method.clone()).collect());
        if plan.steps.iter().any(|s| s.tool == "forbidden") {
            Verdict::Block("forbidden tool".into())
        } else if plan.steps.iter().any(|s| s.tool == "esc") {
            Verdict::Escalate("needs a human".into(), crate::cassandra::types::Severity::Medium)
        } else {
            Verdict::Approve
        }
    }
}

/// Gives every plan the same verdict.
pub(crate) struct FixedVerdict(pub(crate) Verdict);

#[async_trait]
impl ReviewStage for FixedVerdict {
    fn name(&self) -> &str {
        "fixed"
    }

    async fn review(&self, _plan: &Plan, _ctx: &ReviewStageContext<'_>) -> Verdict {
        self.0.clone()
    }
}

/// A step whose method is `fail` fails; a call with a `hits` parameter
/// returns them as `{"results": hits}` (a search, for `sift`) — stashed in
/// a real handoff cache, the planner seeing its placeholder, when the call
/// also says `"stash": true`; a call with `"purge": true` drops the task's
/// stash, as a suspend does; a read of `{"id": "bad"}` fails, a read of any
/// other `id` returns `text of <id>` (stashed, past 2 KiB of filler, when it
/// also says `"stash": true`; `"stash": "gone"` then drops the task's stash,
/// as an eviction does); every other call returns its own name.
#[derive(Default)]
pub(crate) struct Tools {
    pub(crate) dispatched: Mutex<Vec<String>>,
    pub(crate) handoff: crate::handoff::HandoffCache,
}

#[async_trait]
impl StepDispatcher for Tools {
    /// The tools a pinned skill may name (the invoke path's live re-check).
    fn known_tools(&self) -> std::collections::BTreeSet<String> {
        ["mail".to_string()].into()
    }

    /// `<m>-bare` stands for a method written without its namespace: it
    /// completes to `<m>`, as the registry completes `get_message`.
    fn qualify_method(&self, _tool: &str, method: &str) -> Option<String> {
        method.strip_suffix("-bare").map(str::to_string)
    }

    fn stashed(&self, task_id: i64, r: &crate::handoff::HandoffRef) -> Option<Value> {
        self.handoff.whole(task_id, r)
    }

    async fn dispatch_step(&self, task_id: i64, step: &PlannedStep) -> StepOutcome {
        self.dispatched.lock().unwrap().push(step.method.clone());
        if step.method == "fail" || step.parameters.get("id") == Some(&json!("bad")) {
            StepOutcome::Err { code: "POLICY_DENIED".into(), detail: "not allowed".into() }
        } else if step.parameters.get("purge") == Some(&json!(true)) {
            self.handoff.purge_task(task_id);
            StepOutcome::Ok(json!({"result": "purged"}))
        } else if let Some(hits) = step.parameters.get("hits") {
            let result = json!({"results": hits});
            match step.parameters.get("stash") == Some(&json!(true)) {
                true => StepOutcome::Ok(self.handoff.stash_if_oversized(task_id, &result, 0).expect("cap 0").placeholder),
                false => StepOutcome::Ok(result),
            }
        } else if let Some(id) = step.parameters.get("id") {
            let text = format!("text of {}", id.as_str().unwrap_or_default());
            let Some(stash) = step.parameters.get("stash") else {
                return StepOutcome::Ok(json!({"text": text}));
            };
            // A long read, stashed: its text sits past the placeholder's 1 KiB head.
            let long = json!({"text": format!("{} {text}", "filler ".repeat(300))});
            let placeholder = self.handoff.stash_if_oversized(task_id, &long, 0).expect("cap 0").placeholder;
            if stash == &json!("gone") {
                // Evicted before the sift reads it back.
                self.handoff.purge_task(task_id);
            }
            StepOutcome::Ok(placeholder)
        } else {
            StepOutcome::Ok(json!({"result": format!("output of {}", step.method)}))
        }
    }
}

// ── reply builders ──────────────────────────────────────────────────────

pub(crate) fn steps(ids: &[u32]) -> Value {
    ids.iter().map(|i| json!({"id": i, "purpose": format!("p{i}"), "tool": "mail", "done_when": "d"})).collect()
}

pub(crate) fn call(step: u32, method: &str) -> Value {
    json!({"step": step, "tool": "mail", "method": method, "parameters": {"q": step},
           "returns": "r", "done_when": "d", "classification": "Personal"})
}

pub(crate) fn tick(step: u32, finding: &str) -> Value {
    json!({"step": step, "note": format!("did {step}"), "summary": format!("after {step}"),
           "findings": [{"text": finding, "source": format!("msg-{step}")}]})
}

// ── harness ─────────────────────────────────────────────────────────────

pub(crate) async fn new_task(pool: &sqlx::PgPool, max_plans: u32) -> TaskContext {
    let id = kastellan_db::tasks::insert_pending(pool, kastellan_db::tasks::Lane::Fast, json!({})).await.unwrap();
    kastellan_db::tasks::claim_one(pool, kastellan_db::tasks::Lane::Fast, 60).await.unwrap().unwrap();
    TaskContext {
        task_id: id,
        lane: kastellan_db::tasks::Lane::Fast,
        instruction: "list my invoices".into(),
        classification_floor: DataClass::Public,
        classification_floor_source: ClassificationFloorSource::Default,
        classification_floor_signals: vec![],
        plans: vec![],
        advisories: vec![],
        blocks: vec![],
        plan_count: 0,
        max_plans,
        deadline: None,
        resolved_asks: Vec::new(),
        origin: None,
        conversation: Vec::new(),
        conversation_task_ids: Some(Vec::new()),
    }
}

pub(crate) struct Ran {
    pub(crate) task_id: i64,
    pub(crate) result: InnerLoopResult,
    pub(crate) review: Arc<RecordingReview>,
    pub(crate) tools: Arc<Tools>,
}

pub(crate) async fn run(pool: &sqlx::PgPool, script: &Script, ctx: TaskContext) -> Ran {
    run_reviewed(pool, script, ctx, None).await
}

/// [`run`], with `verdict` (when set) given to every plan by a second review
/// stage after the recorder: a reviewer that blocks an answer, say.
pub(crate) async fn run_reviewed(pool: &sqlx::PgPool, script: &Script, ctx: TaskContext, verdict: Option<Verdict>) -> Ran {
    run_staged(pool, script, ctx, verdict.map(|v| Arc::new(FixedVerdict(v)) as Arc<dyn ReviewStage>)).await
}

/// [`run`], with the production classification screen
/// (`DeterministicPolicy`: I1–I3 against the task floor) after the recorder.
pub(crate) async fn run_policed(pool: &sqlx::PgPool, script: &Script, ctx: TaskContext) -> Ran {
    run_staged(pool, script, ctx, Some(Arc::new(crate::cassandra::review::DeterministicPolicy))).await
}

async fn run_staged(pool: &sqlx::PgPool, script: &Script, ctx: TaskContext, extra: Option<Arc<dyn ReviewStage>>) -> Ran {
    let review = Arc::new(RecordingReview::default());
    let mut stages = vec![review.clone() as Arc<dyn ReviewStage>];
    stages.extend(extra);
    let chain = Arc::new(ChainReviewStage::new(stages));
    let tools = Arc::new(Tools::default());
    let task_id = ctx.task_id;
    let result = run_notebook_to_terminal(pool, script, chain, tools.clone(), ctx, None).await.unwrap();
    Ran { task_id, result, review, tools }
}

pub(crate) async fn kinds(pool: &sqlx::PgPool, task_id: i64) -> Vec<EntryKind> {
    db::entries(pool, task_id, 0).await.unwrap().into_iter().map(|e| e.kind).collect()
}

pub(crate) fn body(r: &InnerLoopResult) -> String {
    match &r.outcome {
        Outcome::Completed(v) => v["body"].as_str().unwrap_or_default().to_string(),
        o => panic!("expected Completed, got {o:?}"),
    }
}

/// The resolved `data_ceiling` of the task's last `plan.formulate` row.
pub(crate) async fn last_ceiling(pool: &sqlx::PgPool, task_id: i64) -> Value {
    let row: Value = sqlx::query_scalar(
        "SELECT payload FROM audit_log WHERE actor = 'agent' AND action = 'plan.formulate' \
         AND (payload->>'task_id')::bigint = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(task_id)
    .fetch_one(pool)
    .await
    .unwrap();
    row["plan"]["data_ceiling"].clone()
}

/// The task's audit rows with `actor` and `action`, oldest first.
pub(crate) async fn rows(pool: &sqlx::PgPool, task_id: i64, actor: &str, action: &str) -> Vec<Value> {
    sqlx::query_scalar(
        "SELECT payload FROM audit_log WHERE actor = $1 AND action = $2 \
         AND (payload->>'task_id')::bigint = $3 ORDER BY id",
    )
    .bind(actor)
    .bind(action)
    .bind(task_id)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// The task's `agent / plan.discarded` rows, oldest first.
pub(crate) async fn discarded(pool: &sqlx::PgPool, task_id: i64) -> Vec<Value> {
    sqlx::query_scalar(
        "SELECT payload FROM audit_log WHERE actor = 'agent' AND action = $1 \
         AND (payload->>'task_id')::bigint = $2 ORDER BY id",
    )
    .bind(ACTION_PLAN_DISCARDED)
    .bind(task_id)
    .fetch_all(pool)
    .await
    .unwrap()
}

