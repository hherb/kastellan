//! Inner-loop tests for a planning call that times out (#774), comes back
//! empty (#923) or answers with something that is not a plan (#945): the
//! loop must retry when a cheaper request (after a timeout), the same
//! request (after an empty answer) or a re-ask showing the model its reply
//! can still produce an answer, must not throw away gathered work, and must
//! say what happened when it cannot. Cases 12–14 are the fast lane's
//! wall-clock ceiling (#922), which a run reaches between plans and answers
//! like the plan cap; cases 15–20 are the #945 re-ask.
//!
//! The decisions are unit-tested in `llm_failure/tests.rs`; these run the
//! real `run_to_terminal` against a real Postgres and a formulator whose
//! timeouts are **real** `reqwest` request timeouts (a router pointed at a
//! listener that accepts and never answers), because a hand-built error
//! could satisfy `is_request_timeout` in a way production never produces.
//!
//! Skips silently when Postgres / the supervisor are unavailable, like the
//! other PG-backed tests in this module.

use std::sync::atomic::Ordering;
use std::time::Duration;

use super::tests::inner_loop_test_stubs::{one_step_plan, terminal_plan};
use super::*;

mod script;
use script::*;

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

    // ── 6. A non-plan answer after a gather, from a formulator with no re-ask
    //       (the trait default; `RouterAgent` has one, cases 15–20), fails at
    //       once, in words: no retry, no synthesis.
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
    assert_eq!(
        *f.reformat_replies.lock().unwrap(),
        [(false, "Sure! Here is what I would do.".to_owned())],
        "shown its own reply, on a planning turn"
    );
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

    // ── 17. The forced-synthesis turn answers in prose: its re-ask is a
    //        synthesis re-ask, and its plan answers the task.
    let f = TimeoutScript::new(vec![Next::Plan(Box::new(one_step_plan())), Next::Timeout, Next::Decode], None);
    *f.reformat.lock().unwrap() = Some(Next::Plan(Box::new(terminal_plan("synth reformatted"))));
    let (task_id, r) = run(&pool, f.clone()).await;
    assert_eq!(completed_body(&r), "synth reformatted");
    assert_eq!(f.synthesis_calls.load(Ordering::SeqCst), 1);
    let calls = f.reformat_replies.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].0, "re-asked as the synthesis turn: {calls:?}");
    let failed = agent_rows(&pool, task_id, "plan.formulate_failed").await;
    assert_eq!(failed.len(), 2, "the timeout and the prose answer: {failed:?}");
    assert_eq!(failed[1]["synth_turn"], true, "{}", failed[1]);

    // ── 18. Retries do not chain: an empty answer's retry that comes back
    //        as prose is not re-asked — the turn's one retry is spent.
    let f = TimeoutScript::new(vec![Next::Empty, Next::Decode], None);
    *f.reformat.lock().unwrap() = Some(Next::Plan(Box::new(terminal_plan("never reached"))));
    let (_, r) = run(&pool, f.clone()).await;
    let d = failed_detail(&r);
    assert!(d.starts_with("The language model answered with something that is not a plan"), "{d}");
    assert!(f.reformat_replies.lock().unwrap().is_empty(), "no re-ask after the empty retry");
    assert_eq!(r.failed_llm_calls, 2);

    // ── 19. The re-ask times out after a gather: like any planning
    //        timeout, the forced-synthesis turn answers from what was gathered.
    let f = TimeoutScript::new(
        vec![Next::Plan(Box::new(one_step_plan())), Next::Decode, Next::Plan(Box::new(terminal_plan("synth")))],
        None,
    );
    *f.reformat.lock().unwrap() = Some(Next::Timeout);
    let (task_id, r) = run(&pool, f.clone()).await;
    assert_eq!(completed_body(&r), "synth");
    assert_eq!(f.synthesis_calls.load(Ordering::SeqCst), 1);
    assert_eq!(r.failed_llm_calls, 2);
    let failed = agent_rows(&pool, task_id, "plan.formulate_failed").await;
    assert_eq!(failed.len(), 2, "{failed:?}");
    assert_eq!(failed[1]["request_timeout"], true, "{}", failed[1]);

    // ── 20. The re-ask times out with nothing gathered: the task fails in
    //        the timeout's words, and says the first answer was prose.
    let f = TimeoutScript::new(vec![Next::Decode], None);
    *f.reformat.lock().unwrap() = Some(Next::Timeout);
    let (_, r) = run(&pool, f.clone()).await;
    let d = failed_detail(&r);
    assert!(d.starts_with("The language model's first answer was not a plan, so it was asked again. "), "{d}");
    assert!(d.contains("timed out"), "the timeout query still finds it: {d}");
}
