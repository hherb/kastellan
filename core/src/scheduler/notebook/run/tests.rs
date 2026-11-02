//! The notebook loop against a real Postgres, a scripted model and a
//! scripted tool host (#922). One cluster, one task per case. Skips like the
//! other PG-backed scheduler tests when Postgres or the supervisor is
//! unavailable (run with `KASTELLAN_PG_BIN_DIR` set on the Mac).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kastellan_db::notebook::{self as db, EntryKind};
use kastellan_llm_router::ThinkingPolicy;
use serde_json::{json, Value};

use super::*;
use crate::cassandra::review::{ReviewStage, ReviewStageContext};
use crate::cassandra::types::{Plan, PlannedStep, Verdict};
use crate::scheduler::agent::{AgentError, FormulationMeta};
use crate::scheduler::inner_loop::ClassificationFloorSource;
use crate::scheduler::notebook::formulator::UserMessage;

const RUNAWAY: &str = "<runaway>";

// ── scripted model ──────────────────────────────────────────────────────

/// Returns `replies` in order, and records each turn's rendered message and
/// whether it was a re-ask. `on_first_call` runs during the first turn (an
/// operator typing a correction while the model thinks).
struct Script {
    replies: Mutex<VecDeque<String>>,
    messages: Mutex<Vec<String>>,
    reasks: Mutex<Vec<String>>,
    thinking: Mutex<Vec<ThinkingPolicy>>,
    correction_during_first_turn: Option<(sqlx::PgPool, i64, &'static str)>,
}

impl Script {
    fn new(replies: &[Value]) -> Self {
        Self::raw(&replies.iter().map(Value::to_string).collect::<Vec<_>>())
    }

    fn raw(replies: &[String]) -> Self {
        Self {
            replies: Mutex::new(replies.iter().cloned().collect()),
            messages: Mutex::new(Vec::new()),
            reasks: Mutex::new(Vec::new()),
            thinking: Mutex::new(Vec::new()),
            correction_during_first_turn: None,
        }
    }

    fn messages(&self) -> Vec<String> {
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
}

fn meta() -> FormulationMeta {
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

/// Approves everything except a step whose tool is `forbidden`, and records
/// the instruction it was shown for each plan.
#[derive(Default)]
struct RecordingReview {
    instructions: Mutex<Vec<String>>,
}

#[async_trait]
impl ReviewStage for RecordingReview {
    fn name(&self) -> &str {
        "recording"
    }

    async fn review(&self, plan: &Plan, ctx: &ReviewStageContext<'_>) -> Verdict {
        self.instructions.lock().unwrap().push(ctx.instruction.to_string());
        if plan.steps.iter().any(|s| s.tool == "forbidden") {
            Verdict::Block("forbidden tool".into())
        } else {
            Verdict::Approve
        }
    }
}

/// A step whose method is `fail` fails; every other returns its own name.
#[derive(Default)]
struct Tools {
    dispatched: Mutex<Vec<String>>,
}

#[async_trait]
impl StepDispatcher for Tools {
    async fn dispatch_step(&self, _task_id: i64, step: &PlannedStep) -> StepOutcome {
        self.dispatched.lock().unwrap().push(step.method.clone());
        if step.method == "fail" {
            StepOutcome::Err { code: "POLICY_DENIED".into(), detail: "not allowed".into() }
        } else {
            StepOutcome::Ok(json!({"result": format!("output of {}", step.method)}))
        }
    }
}

// ── reply builders ──────────────────────────────────────────────────────

fn steps(ids: &[u32]) -> Value {
    ids.iter().map(|i| json!({"id": i, "purpose": format!("p{i}"), "tool": "mail", "done_when": "d"})).collect()
}

fn call(step: u32, method: &str) -> Value {
    json!({"step": step, "tool": "mail", "method": method, "parameters": {"q": step},
           "returns": "r", "done_when": "d", "classification": "Personal"})
}

fn tick(step: u32, finding: &str) -> Value {
    json!({"step": step, "note": format!("did {step}"), "summary": format!("after {step}"),
           "findings": [{"text": finding, "source": format!("msg-{step}")}]})
}

// ── harness ─────────────────────────────────────────────────────────────

async fn new_task(pool: &sqlx::PgPool, max_plans: u32) -> TaskContext {
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

struct Ran {
    task_id: i64,
    result: InnerLoopResult,
    review: Arc<RecordingReview>,
    tools: Arc<Tools>,
}

async fn run(pool: &sqlx::PgPool, script: &Script, ctx: TaskContext) -> Ran {
    let review = Arc::new(RecordingReview::default());
    let chain = Arc::new(ChainReviewStage::new(vec![review.clone() as Arc<dyn ReviewStage>]));
    let tools = Arc::new(Tools::default());
    let task_id = ctx.task_id;
    let result = run_notebook_to_terminal(pool, script, chain, tools.clone(), ctx, None).await.unwrap();
    Ran { task_id, result, review, tools }
}

async fn kinds(pool: &sqlx::PgPool, task_id: i64) -> Vec<EntryKind> {
    db::entries(pool, task_id, 0).await.unwrap().into_iter().map(|e| e.kind).collect()
}

fn body(r: &InnerLoopResult) -> String {
    match &r.outcome {
        Outcome::Completed(v) => v["body"].as_str().unwrap_or_default().to_string(),
        o => panic!("expected Completed, got {o:?}"),
    }
}

/// The resolved `data_ceiling` of the task's last `plan.formulate` row.
async fn last_ceiling(pool: &sqlx::PgPool, task_id: i64) -> Value {
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_notebook_loop() {
    if kastellan_tests_common::skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = kastellan_tests_common::pg_bin_dir_or_skip() else { return };
    let suffix = format!("nbl-{}", kastellan_tests_common::unique_suffix());
    let service = format!("kastellan-sched-test-pg-{suffix}");
    let cluster =
        tokio::task::block_in_place(|| kastellan_tests_common::bring_up_pg_cluster(&bin_dir, "nb-d", "nb-l", &service));
    kastellan_db::probe::run(&cluster.conn_spec, "core", "startup", json!({"purpose": "notebook-loop-unit"}))
        .await
        .ok();
    // The cluster is up, so a failed connect is a failure, not a skip.
    let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec).await.expect("runtime pool");

    a_three_step_task_ticks_each_step_and_answers_from_findings(&pool).await;
    a_failed_step_is_followed_by_a_replan_turn(&pool).await;
    a_blocked_call_is_a_failed_step(&pool).await;
    an_invalid_reply_is_re_asked_once_then_fails_in_words(&pool).await;
    a_correction_reaches_the_reviewer_and_the_next_turn(&pool).await;
    a_resumed_task_does_not_redispatch_ticked_steps(&pool).await;
    infeasible_completes_with_the_reason(&pool).await;
    a_step_replanned_away_still_counts_toward_the_ceiling(&pool).await;
    tasks_correct_adds_to_a_running_task_only(&pool).await;
    a_runaway_reply_after_gathering_spends_the_answer_turn(&pool).await;
    a_call_on_the_answer_turn_is_re_asked(&pool).await;
    the_turn_cap_is_twice_max_plans(&pool).await;
}

async fn a_three_step_task_ticks_each_step_and_answers_from_findings(pool: &sqlx::PgPool) {
    let script = Script::new(&[
        json!({"steps": steps(&[1, 2, 3]), "call": call(1, "a")}),
        json!({"tick": tick(1, "invoice A"), "call": call(2, "b")}),
        json!({"tick": tick(2, "invoice B"), "call": call(3, "c")}),
        json!({"tick": tick(3, "invoice C"), "answer": {"body": "A, B and C", "data_ceiling": "Public"}}),
    ]);
    let r = run(pool, &script, new_task(pool, 5).await).await;
    assert_eq!(body(&r.result), "A, B and C");
    use EntryKind::*;
    assert_eq!(kinds(pool, r.task_id).await, vec![Steps, Tick, Tick, Tick, Answer]);
    assert_eq!(*r.tools.dispatched.lock().unwrap(), ["a", "b", "c"]);
    assert_eq!((r.result.dispatch_count, r.result.plan_count), (3, 4));
    // C1: the model declared Public, but every call touched Personal data.
    assert_eq!(last_ceiling(pool, r.task_id).await, "Personal");
    // Each step turn shows the result of the call made for it — and only that.
    let m = script.messages();
    assert!(m[1].contains("output of a") && !m[1].contains("output of b"), "{}", m[1]);
    assert!(m[3].contains("output of c") && !m[3].contains("output of a"), "{}", m[3]);
    assert!(m[3].contains("invoice A") && m[3].contains("invoice B"), "earlier findings stay: {}", m[3]);
}

async fn a_failed_step_is_followed_by_a_replan_turn(pool: &sqlx::PgPool) {
    let script = Script::new(&[
        json!({"steps": steps(&[1, 2]), "call": call(1, "fail")}),
        json!({"steps": steps(&[3]), "call": call(3, "ok")}),
        json!({"tick": tick(3, "found"), "answer": {"body": "done"}}),
    ]);
    let r = run(pool, &script, new_task(pool, 5).await).await;
    assert_eq!(body(&r.result), "done");
    use EntryKind::*;
    assert_eq!(kinds(pool, r.task_id).await, vec![Steps, FailedStep, Steps, Tick, Answer]);
    let replan = &script.messages()[1];
    let v: Value = serde_json::from_str(replan).unwrap();
    assert_eq!(v["last_failure"]["step"], 1, "{replan}");
    assert_eq!(v["last_failure"]["result"]["code"], "POLICY_DENIED");
    assert_eq!(v["notebook"]["steps"][0]["status"], "failed");
}

async fn a_blocked_call_is_a_failed_step(pool: &sqlx::PgPool) {
    let mut forbidden = call(1, "x");
    forbidden["tool"] = json!("forbidden");
    let script = Script::new(&[
        json!({"steps": steps(&[1, 2]), "call": forbidden}),
        json!({"call": call(2, "ok")}),
        json!({"tick": tick(2, "f"), "answer": {"body": "done"}}),
    ]);
    let r = run(pool, &script, new_task(pool, 5).await).await;
    assert_eq!(body(&r.result), "done");
    use EntryKind::*;
    assert_eq!(kinds(pool, r.task_id).await, vec![Steps, FailedStep, Tick, Answer]);
    assert_eq!(*r.tools.dispatched.lock().unwrap(), ["ok"], "a blocked call never runs");
    let v: Value = serde_json::from_str(&script.messages()[1]).unwrap();
    assert_eq!(v["blocks"][0], "forbidden tool", "the replan turn sees why");
}

async fn an_invalid_reply_is_re_asked_once_then_fails_in_words(pool: &sqlx::PgPool) {
    // A tick for a step that is not current: refused, re-asked with the
    // reason, and the corrected reply is used.
    let script = Script::new(&[
        json!({"steps": steps(&[1, 2]), "call": call(1, "a")}),
        json!({"tick": tick(2, "x"), "call": call(2, "b")}),
        json!({"tick": tick(1, "x"), "answer": {"body": "fixed"}}),
    ]);
    let r = run(pool, &script, new_task(pool, 5).await).await;
    assert_eq!(body(&r.result), "fixed");
    assert_eq!(r.result.failed_llm_calls, 1);
    assert!(script.reasks.lock().unwrap()[0].contains("not the current step 1"));

    // Twice unusable: the task fails, saying why.
    let script = Script::raw(&["no json here".into(), "still none".into()]);
    let r = run(pool, &script, new_task(pool, 5).await).await;
    match &r.result.outcome {
        Outcome::Failed(d) => assert!(!d.is_empty(), "a worded failure"),
        o => panic!("expected Failed, got {o:?}"),
    }
    assert_eq!(r.result.failed_llm_calls, 2, "the first reply and the re-ask");
    assert_eq!(script.reasks.lock().unwrap().len(), 1, "re-asked exactly once");
    assert!(kinds(pool, r.task_id).await.is_empty(), "nothing usable was written");
}

async fn a_correction_reaches_the_reviewer_and_the_next_turn(pool: &sqlx::PgPool) {
    let ctx = new_task(pool, 5).await;
    let mut script = Script::new(&[
        json!({"steps": steps(&[1, 2]), "call": call(1, "a")}),
        json!({"tick": tick(1, "x"), "call": call(2, "b")}),
        json!({"tick": tick(2, "y"), "answer": {"body": "done"}}),
    ]);
    script.correction_during_first_turn = Some((pool.clone(), ctx.task_id, "the other account"));
    let r = run(pool, &script, ctx).await;
    assert_eq!(body(&r.result), "done");
    let seen = r.review.instructions.lock().unwrap().clone();
    assert!(!seen[0].contains("Operator correction"), "the first plan was reviewed before it: {}", seen[0]);
    for later in &seen[1..] {
        assert!(later.ends_with("\n\nOperator correction: the other account"), "{later}");
    }
    let m = script.messages();
    let shown = |s: &str| serde_json::from_str::<Value>(s).unwrap().get("corrections").cloned();
    assert_eq!(shown(&m[1]), Some(json!([{"text": "the other account"}])), "shown on the next turn");
    assert_eq!(shown(&m[2]), None, "…exactly once");
}

async fn a_resumed_task_does_not_redispatch_ticked_steps(pool: &sqlx::PgPool) {
    let ctx = new_task(pool, 5).await;
    let t = ctx.task_id;
    db::append(pool, t, EntryKind::Steps, json!({"steps": steps(&[1, 2, 3])})).await.unwrap();
    for i in [1u32, 2] {
        let mut p = tick(i, "f");
        p["classification"] = json!("Public");
        db::append(pool, t, EntryKind::Tick, p).await.unwrap();
    }
    // Asking for a ticked step is refused by the check, never dispatched.
    let script = Script::new(&[
        json!({"call": call(1, "again")}),
        json!({"call": call(3, "third")}),
        json!({"tick": tick(3, "z"), "answer": {"body": "done"}}),
    ]);
    let r = run(pool, &script, ctx).await;
    assert_eq!(body(&r.result), "done");
    assert_eq!(*r.tools.dispatched.lock().unwrap(), ["third"]);
    let v: Value = serde_json::from_str(&script.messages()[0]).unwrap();
    assert_eq!(v["current_step"]["id"], 3, "the resumed turn starts at the first pending step");
}

async fn infeasible_completes_with_the_reason(pool: &sqlx::PgPool) {
    let script = Script::new(&[json!({"steps": steps(&[1]), "infeasible": {"reason": "no tool reads faxes"}})]);
    let r = run(pool, &script, new_task(pool, 5).await).await;
    assert_eq!(body(&r.result), "no tool reads faxes");
    assert_eq!(kinds(pool, r.task_id).await, vec![EntryKind::Steps, EntryKind::Infeasible]);
}

async fn the_turn_cap_is_twice_max_plans(pool: &sqlx::PgPool) {
    // max_plans 2 → 4 turns, each a call; then the one forced-answer turn.
    let mut replies = vec![json!({"steps": steps(&[1, 2, 3, 4, 5]), "call": call(1, "s1")})];
    for i in 1..=3u32 {
        replies.push(json!({"tick": tick(i, "f"), "call": call(i + 1, &format!("s{}", i + 1))}));
    }
    replies.push(json!({"tick": tick(4, "f"), "answer": {"body": "from the findings"}}));
    let script = Script::new(&replies);
    let r = run(pool, &script, new_task(pool, 2).await).await;
    assert_eq!(body(&r.result), "from the findings");
    assert_eq!(r.tools.dispatched.lock().unwrap().len(), 4);
    assert_eq!(r.result.plan_count, 5);
    let last: Value = serde_json::from_str(script.messages().last().unwrap()).unwrap();
    assert_eq!(last["turn_budget"], json!({"directive": ANSWER_DIRECTIVE}));
}

/// C1: step 1's call touched Personal data, then the model replanned it away
/// without a tick. No entry records that class; the answer's ceiling must
/// still carry it.
async fn a_step_replanned_away_still_counts_toward_the_ceiling(pool: &sqlx::PgPool) {
    let script = Script::new(&[
        json!({"steps": steps(&[1]), "call": call(1, "peek")}),
        json!({"steps": steps(&[2]), "answer": {"body": "nothing", "data_ceiling": "Public"}}),
    ]);
    let r = run(pool, &script, new_task(pool, 5).await).await;
    assert_eq!(body(&r.result), "nothing");
    assert_eq!(last_ceiling(pool, r.task_id).await, "Personal");
}

async fn tasks_correct_adds_to_a_running_task_only(pool: &sqlx::PgPool) {
    use crate::cli_audit::{correct_and_audit, CorrectOutcome, ACTION_TASK_CORRECTED};
    let running = new_task(pool, 5).await.task_id;
    assert_eq!(correct_and_audit(pool, running, "the other account").await.unwrap(), CorrectOutcome::Added { seq: 1 });
    let e = db::entries(pool, running, 0).await.unwrap();
    assert_eq!((e[0].kind, e[0].payload["text"].as_str()), (EntryKind::Correction, Some("the other account")));
    let row: Value = sqlx::query_scalar(
        "SELECT payload FROM audit_log WHERE actor = 'cli' AND action = $1 AND (payload->>'task_id')::bigint = $2",
    )
    .bind(ACTION_TASK_CORRECTED)
    .bind(running)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(row, json!({"task_id": running, "seq": 1, "chars": 17}), "the row never carries the text");

    let pending = kastellan_db::tasks::insert_pending(pool, kastellan_db::tasks::Lane::Long, json!({})).await.unwrap();
    assert_eq!(correct_and_audit(pool, pending, "x").await.unwrap(), CorrectOutcome::NotRunning);
    assert_eq!(correct_and_audit(pool, i64::MAX, "x").await.unwrap(), CorrectOutcome::NotRunning);
    assert!(db::entries(pool, pending, 0).await.unwrap().is_empty());
}

/// DGX task 269: a turn spent its whole `max_tokens` thinking and said
/// nothing. With findings in hand that is the forced-answer turn, written
/// with thinking off — not a failure that throws the findings away.
async fn a_runaway_reply_after_gathering_spends_the_answer_turn(pool: &sqlx::PgPool) {
    let script = Script::raw(&[
        json!({"steps": steps(&[1, 2]), "call": call(1, "a")}).to_string(),
        RUNAWAY.into(),
        json!({"tick": tick(1, "invoice A"), "answer": {"body": "A"}}).to_string(),
    ]);
    let r = run(pool, &script, new_task(pool, 5).await).await;
    assert_eq!(body(&r.result), "A");
    assert_eq!(r.result.failed_llm_calls, 1);
    let last: Value = serde_json::from_str(script.messages().last().unwrap()).unwrap();
    assert_eq!(last["turn_budget"], json!({"directive": ANSWER_DIRECTIVE}));
    assert_eq!(script.thinking.lock().unwrap().last(), Some(&ThinkingPolicy::Suppress));

    // Before anything was gathered there is nothing to answer from: it fails.
    let script = Script::raw(&[RUNAWAY.into()]);
    let r = run(pool, &script, new_task(pool, 5).await).await;
    assert!(matches!(r.result.outcome, Outcome::Failed(_)), "{:?}", r.result.outcome);
}

/// DGX task 273: on the forced-answer turn the model called again. That is
/// re-asked with the reason, and the answer it then gives is used.
async fn a_call_on_the_answer_turn_is_re_asked(pool: &sqlx::PgPool) {
    let script = Script::new(&[
        json!({"steps": steps(&[1, 2, 3]), "call": call(1, "a")}),
        json!({"tick": tick(1, "f"), "call": call(2, "b")}),
        json!({"tick": tick(2, "g"), "call": call(3, "c")}),
        json!({"tick": tick(2, "g"), "answer": {"body": "f and g"}}),
    ]);
    // max_plans 1 → 2 turns; turn 3 is the forced answer.
    let r = run(pool, &script, new_task(pool, 1).await).await;
    assert_eq!(body(&r.result), "f and g");
    assert_eq!(*r.tools.dispatched.lock().unwrap(), ["a", "b"], "the answer turn's call never ran");
    assert!(script.reasks.lock().unwrap()[0].contains("must end the task"), "{:?}", script.reasks.lock().unwrap());
}
