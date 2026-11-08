//! The agent-raised plan features on the notebook loop (#996: ported from
//! the classic loop before it is retired, #993). The reply carries each one
//! on its `answer` (or `call`); the shared `execute::review_and_dispatch`
//! acts on it exactly as on a classic plan. Each runs on the cluster
//! `super::tests::the_notebook_loop` brings up.

use serde_json::json;

use crate::scheduler::notebook::test_harness::*;

const INSIGHT: &str = "mail.search needs a date range or it pages the whole archive";

/// A completed answer's `l1_insight` reaches `terminal_l1_insight` (the field
/// the runner promotes to L1 from) and its `plan.formulate` row.
pub(super) async fn an_answers_l1_insight_reaches_the_runner(pool: &sqlx::PgPool) {
    let script = Script::new(&[
        json!({"steps": steps(&[1]), "call": call(1, "a")}),
        json!({"tick": tick(1, "invoice A"), "answer": {"body": "A", "l1_insight": INSIGHT}}),
    ]);
    let r = run(pool, &script, new_task(pool, 3).await).await;
    assert_eq!(body(&r.result), "A");
    assert_eq!(r.result.terminal_l1_insight.as_deref(), Some(INSIGHT));
    let formulated = rows(pool, r.task_id, "agent", "plan.formulate").await;
    assert_eq!(formulated.last().unwrap()["l1_insight"], INSIGHT, "{formulated:#?}");
}

/// A refused answer ends the task `Refused`, so its insight is dropped, as on
/// a classic plan: only a completed task teaches a lesson.
pub(super) async fn a_refused_answers_l1_insight_is_dropped(pool: &sqlx::PgPool) {
    let refused = json!({"principle": 1, "reason": "physical_harm"});
    let script = Script::new(&[json!({"steps": steps(&[1]), "answer": {"body": "no", "refused": refused, "l1_insight": INSIGHT}})]);
    let r = run(pool, &script, new_task(pool, 3).await).await;
    assert_eq!(r.result.outcome.final_state(), Some("refused"), "{:?}", r.result.outcome);
    assert_eq!(r.result.terminal_l1_insight, None);
}

/// An answer set aside on a routine turn is written again on a thinking turn:
/// the insight is the rewritten answer's, never the discarded one's.
pub(super) async fn a_rewritten_answer_keeps_only_its_own_insight(pool: &sqlx::PgPool) {
    let mut script = Script::new(&[
        json!({"steps": steps(&[1]), "call": call(1, "a")}),
        json!({"tick": tick(1, "invoice A"), "answer": {"body": "draft", "l1_insight": "the draft's lesson"}}),
        json!({"answer": {"body": "A", "l1_insight": INSIGHT}}),
    ]);
    script.thinks = false;
    let r = run(pool, &script, new_task(pool, 3).await).await;
    assert_eq!(body(&r.result), "A");
    assert_eq!(r.result.terminal_l1_insight.as_deref(), Some(INSIGHT));

    // A rewrite with no lesson of its own does not inherit the draft's.
    let mut script = Script::new(&[
        json!({"steps": steps(&[1]), "call": call(1, "a")}),
        json!({"tick": tick(1, "invoice A"), "answer": {"body": "draft", "l1_insight": "the draft's lesson"}}),
        json!({"answer": {"body": "A"}}),
    ]);
    script.thinks = false;
    let r = run(pool, &script, new_task(pool, 3).await).await;
    assert_eq!(body(&r.result), "A");
    assert_eq!(r.result.terminal_l1_insight, None);
}

/// A skill over the `call` this test task makes (`mail`'s `"a"`).
fn skill() -> serde_json::Value {
    json!({
        "name": "list_invoices",
        "description": "List the invoices matching a query",
        "parameters": [{"name": "q", "description": "the search"}],
        "steps": [{"tool": "mail", "method": "a", "parameters": {"q": "{{q}}"}}],
    })
}

/// A completed answer's `l3_skill`, after a call was dispatched, reaches
/// `terminal_l3_skill`, which the runner crystallises from, and its
/// `plan.formulate` row.
pub(super) async fn an_answers_l3_skill_reaches_the_runner(pool: &sqlx::PgPool) {
    let script = Script::new(&[
        json!({"steps": steps(&[1]), "call": call(1, "a")}),
        json!({"tick": tick(1, "invoice A"), "answer": {"body": "A", "l3_skill": skill()}}),
    ]);
    let r = run(pool, &script, new_task(pool, 3).await).await;
    assert_eq!(body(&r.result), "A");
    assert_eq!(r.result.terminal_l3_skill.map(|s| s.name).as_deref(), Some("list_invoices"));
    let formulated = rows(pool, r.task_id, "agent", "plan.formulate").await;
    assert_eq!(formulated.last().unwrap()["l3_skill"]["name"], "list_invoices", "{formulated:#?}");
}

/// The forced answer is a wrap-up under the turn cap, not a procedure shown
/// to work, so its skill is dropped (`execute`'s `synth_turn` gate).
pub(super) async fn a_forced_answers_l3_skill_is_dropped(pool: &sqlx::PgPool) {
    let script = Script::new(&[
        json!({"steps": steps(&[1, 2, 3]), "call": call(1, "a")}),
        json!({"tick": tick(1, "f"), "call": call(2, "b")}),
        json!({"tick": tick(2, "g"), "answer": {"body": "f and g", "l3_skill": skill()}}),
    ]);
    // max_plans 1 → 2 turns; turn 3 is the forced answer.
    let r = run(pool, &script, new_task(pool, 1).await).await;
    assert_eq!(body(&r.result), "f and g");
    assert_eq!(r.result.terminal_l3_skill, None);
}

/// A Python skill. These scripts' calls are `mail` ones: the grounding gate
/// needs a dispatched call, not a `python-exec` call of this code.
fn python_skill() -> serde_json::Value {
    json!({"name": "sum_ints", "description": "Sum the integers", "code": "print(sum([1, 2]))\n"})
}

/// A completed answer's `python_skill`, after a call was dispatched, reaches
/// `terminal_python_skill`, which the runner crystallises from, and its
/// `plan.formulate` row. The notebook twin of the classic
/// `inner_loop/tests.rs::terminal_python_skill_captured_under_grounding_gate`.
pub(super) async fn an_answers_python_skill_reaches_the_runner(pool: &sqlx::PgPool) {
    let script = Script::new(&[
        json!({"steps": steps(&[1]), "call": call(1, "a")}),
        json!({"tick": tick(1, "invoice A"), "answer": {"body": "A", "python_skill": python_skill()}}),
    ]);
    let r = run(pool, &script, new_task(pool, 3).await).await;
    assert_eq!(body(&r.result), "A");
    assert_eq!(r.result.terminal_python_skill.map(|s| s.name).as_deref(), Some("sum_ints"));
    let formulated = rows(pool, r.task_id, "agent", "plan.formulate").await;
    // No compact top-level key as `l3_skill` has: the row's whole `plan` carries it.
    assert_eq!(formulated.last().unwrap()["plan"]["python_skill"]["name"], "sum_ints", "{formulated:#?}");
}

/// The grounding gate: an answer on the first turn made no call, so its
/// Python skill is dropped; so is a forced answer's (`synth_turn`).
pub(super) async fn an_ungrounded_or_forced_python_skill_is_dropped(pool: &sqlx::PgPool) {
    let script = Script::new(&[json!({"steps": steps(&[1]), "answer": {"body": "A", "python_skill": python_skill()}})]);
    let r = run(pool, &script, new_task(pool, 3).await).await;
    assert_eq!(body(&r.result), "A");
    assert_eq!(r.result.terminal_python_skill, None);

    let script = Script::new(&[
        json!({"steps": steps(&[1, 2, 3]), "call": call(1, "a")}),
        json!({"tick": tick(1, "f"), "call": call(2, "b")}),
        json!({"tick": tick(2, "g"), "answer": {"body": "f and g", "python_skill": python_skill()}}),
    ]);
    // max_plans 1 → 2 turns; turn 3 is the forced answer.
    let r = run(pool, &script, new_task(pool, 1).await).await;
    assert_eq!(body(&r.result), "f and g");
    assert_eq!(r.result.terminal_python_skill, None);
}

/// `call(1, "a")` with a `floor_request` and its own `classification`.
fn raising_call(class: &str) -> serde_json::Value {
    let mut c = call(1, "a");
    c["floor_request"] = json!("ClinicalConfidential");
    c["classification"] = json!(class);
    c
}

/// A call's `floor_request` raises the task floor before review (the
/// notebook twin of the classic `agent_floor_request_higher_than_producer_
/// elevates_ctx`): its `plan.formulate` row, and the answer's after it, carry
/// the raised floor and `agent_raised`; the answer's ceiling covers it.
pub(super) async fn a_calls_floor_request_raises_the_task_floor(pool: &sqlx::PgPool) {
    let script = Script::new(&[
        json!({"steps": steps(&[1]), "call": raising_call("ClinicalConfidential")}),
        json!({"tick": tick(1, "invoice A"), "answer": {"body": "A"}}),
    ]);
    let r = run_policed(pool, &script, new_task(pool, 3).await).await;
    assert_eq!(body(&r.result), "A");
    assert_eq!(*r.tools.dispatched.lock().unwrap(), ["a"]);
    let formulated = rows(pool, r.task_id, "agent", "plan.formulate").await;
    assert_eq!(formulated.len(), 2, "{formulated:#?}");
    for row in &formulated {
        assert_eq!(row["classification_floor"], "ClinicalConfidential", "{row:#?}");
        assert_eq!(row["classification_floor_source"], "agent_raised", "{row:#?}");
    }
    assert_eq!(last_ceiling(pool, r.task_id).await, "ClinicalConfidential");
    assert!(script.messages()[1].contains(r#""classification_floor":"ClinicalConfidential""#), "the render shows it");
}

/// The classic `agent_floor_raise_chain_blocks_low_classification_step` on
/// the notebook: a call that raises the floor but classifies itself below it
/// is blocked by I2 under the real `DeterministicPolicy`, and never runs.
pub(super) async fn a_call_below_its_raised_floor_is_blocked(pool: &sqlx::PgPool) {
    let script = Script::new(&[
        json!({"steps": steps(&[1]), "call": raising_call("Personal")}),
        json!({"answer": {"body": "blocked"}}),
    ]);
    let r = run_policed(pool, &script, new_task(pool, 3).await).await;
    assert_eq!(body(&r.result), "blocked");
    assert!(r.tools.dispatched.lock().unwrap().is_empty(), "a blocked call is not dispatched");
    let formulated = rows(pool, r.task_id, "agent", "plan.formulate").await;
    assert_eq!(formulated.len(), 2, "{formulated:#?}");
    // The block keeps the raise: the answer after it is reviewed at it.
    for row in &formulated {
        assert_eq!(row["classification_floor"], "ClinicalConfidential", "{row:#?}");
        assert_eq!(row["classification_floor_source"], "agent_raised", "{row:#?}");
    }
    assert_eq!(last_ceiling(pool, r.task_id).await, "ClinicalConfidential");
    let next = &script.messages()[1];
    assert!(next.contains("step_classification_below_floor"), "the block is shown back: {next}");
}

/// A repeated call is not dispatched (#922), but its `floor_request` still
/// raises the floor: the raise only tightens review, and dropping it would
/// let the answer go out below the class the agent declared.
pub(super) async fn a_repeated_calls_floor_request_still_raises_the_floor(pool: &sqlx::PgPool) {
    let script = Script::new(&[
        json!({"steps": steps(&[1]), "call": call(1, "a")}),
        json!({"call": raising_call("ClinicalConfidential")}),
        json!({"tick": tick(1, "invoice A"), "answer": {"body": "A"}}),
    ]);
    let r = run_policed(pool, &script, new_task(pool, 3).await).await;
    assert_eq!(body(&r.result), "A");
    assert_eq!(*r.tools.dispatched.lock().unwrap(), ["a"], "the repeat never ran");
    let formulated = rows(pool, r.task_id, "agent", "plan.formulate").await;
    let answer = formulated.last().unwrap();
    assert_eq!(answer["classification_floor"], "ClinicalConfidential", "{formulated:#?}");
    assert_eq!(answer["classification_floor_source"], "agent_raised", "{formulated:#?}");
    assert_eq!(last_ceiling(pool, r.task_id).await, "ClinicalConfidential");
}

/// Pin a two-step skill over the harness's `mail` calls — `"a"`, then a read
/// of `{"id": q}` (which fails for `q` = `"bad"`) — as an operator would; its
/// id.
async fn seed_pinned_skill(pool: &sqlx::PgPool, name: &str) -> i64 {
    use kastellan_db::memories::{insert_memory_at_layer, set_skill_trust, MemoryLayer};
    let template = json!({
        "name": name, "description": "d",
        "parameters": [{"name": "q", "description": "the search"}],
        "steps": [{"tool": "mail", "method": "a", "parameters": {"q": "{{q}}"}},
                  {"tool": "mail", "method": "b", "parameters": {"id": "{{q}}"}}],
    });
    let metadata = json!({"template": template, "trust": "pinned", "body_sha256": "deadbeef"});
    let id = insert_memory_at_layer(pool, "skill body", &metadata, None, MemoryLayer::Skill).await.unwrap();
    set_skill_trust(pool, id, "pinned").await.unwrap();
    id
}

/// A call for step 1 that runs the skill `name` with `q`.
fn invoke_call(name: &str, q: &str) -> serde_json::Value {
    json!({"step": 1, "invoke_skill": {"name": name, "args": {"q": q}},
           "returns": "r", "done_when": "d", "classification": "Personal"})
}

/// The `scheduler / <action>` rows naming skill `name` (they carry no task id).
async fn invoke_rows(pool: &sqlx::PgPool, action: &str, name: &str) -> Vec<serde_json::Value> {
    sqlx::query_scalar(
        "SELECT payload FROM audit_log WHERE actor = 'scheduler' AND action = $1 \
         AND payload->>'skill_name' = $2 ORDER BY id",
    )
    .bind(action)
    .bind(name)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// A call's `invoke_skill` runs the pinned skill's steps (the notebook twin
/// of the classic `agent_invoke_pinned_skill_expands_and_dispatches`): the
/// reviewer sees the concrete steps, the next turn sees every step's result,
/// and the lifecycle is audited. The answer's `l3_skill` is then dropped
/// (`invoke_driven_task_suppresses_recrystallisation`'s twin): a task that
/// ran a skill never crystallises one.
pub(super) async fn an_invoked_skill_runs_its_steps(pool: &sqlx::PgPool) {
    let memory_id = seed_pinned_skill(pool, "fetch_invoices").await;
    let script = Script::new(&[
        json!({"steps": steps(&[1]), "call": invoke_call("fetch_invoices", "invoices")}),
        json!({"tick": tick(1, "invoices A and B"), "answer": {"body": "A and B", "l3_skill": skill()}}),
    ]);
    let r = run(pool, &script, new_task(pool, 3).await).await;
    assert_eq!(body(&r.result), "A and B");
    assert_eq!(*r.tools.dispatched.lock().unwrap(), ["a", "b"]);
    assert_eq!(r.review.plans.lock().unwrap()[0], ["a", "b"], "reviewed as the skill's steps");
    let next = &script.messages()[1];
    assert!(next.contains("output of a") && next.contains("text of invoices"), "every step's result is shown: {next}");
    assert_eq!(r.result.terminal_l3_skill, None, "an invoke-driven task never re-crystallises");
    let invoked = invoke_rows(pool, "l3.invoked", "fetch_invoices").await;
    assert_eq!(invoked.len(), 1, "{invoked:#?}");
    assert_eq!(invoked[0]["memory_id"], memory_id);
    let outcome = invoke_rows(pool, "l3.invoke_outcome", "fetch_invoices").await;
    assert_eq!(outcome.len(), 1, "{outcome:#?}");
    assert_eq!(outcome[0]["steps_executed"], 2, "{outcome:#?}");

    // A later step that fails fails the call's step, though the first ran:
    // the next turn replans from it.
    let script = Script::new(&[
        json!({"steps": steps(&[1]), "call": invoke_call("fetch_invoices", "bad")}),
        json!({"answer": {"body": "the read failed"}}),
    ]);
    let r = run(pool, &script, new_task(pool, 3).await).await;
    assert_eq!(body(&r.result), "the read failed");
    assert_eq!(*r.tools.dispatched.lock().unwrap(), ["a", "b"]);
    assert!(kinds(pool, r.task_id).await.contains(&kastellan_db::notebook::EntryKind::FailedStep));
    assert!(script.messages()[1].contains("last_failure"), "{}", script.messages()[1]);
}

/// A skill that is not pinned (here: none of that name) is refused before
/// review (`agent_invoke_unknown_skill_refuses_then_replans`'s twin): nothing
/// runs, the refusal is audited and shown back, the step fails, and the next
/// turn replans.
pub(super) async fn an_unknown_skill_is_refused_and_the_next_turn_replans(pool: &sqlx::PgPool) {
    let script = Script::new(&[
        json!({"steps": steps(&[1]), "call": invoke_call("no_such_skill", "invoices")}),
        json!({"answer": {"body": "no skill to run"}}),
    ]);
    let r = run(pool, &script, new_task(pool, 3).await).await;
    assert_eq!(body(&r.result), "no skill to run");
    assert!(r.tools.dispatched.lock().unwrap().is_empty(), "a refused invoke runs nothing");
    assert!(r.review.plans.lock().unwrap().iter().all(|p| p.is_empty()), "never reviewed as steps");
    assert_eq!(invoke_rows(pool, "l3.invoke_rejected", "no_such_skill").await.len(), 1);
    let next = &script.messages()[1];
    assert!(next.contains("invoke_rejected"), "the refusal is shown back: {next}");
    assert!(kinds(pool, r.task_id).await.contains(&kastellan_db::notebook::EntryKind::FailedStep));
}
