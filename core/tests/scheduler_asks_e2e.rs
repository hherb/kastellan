//! PG e2e for `scheduler::asks` — the raise/expire wiring and its audit rows.
//!
//! Skips silently with `[SKIP]` on hosts without Postgres; run with
//! `-- --nocapture` to see whether it ran.
//!
//! Issue #15 will eventually hoist the bring-up helpers into a shared
//! fixture; until then we copy and adapt the recipe from
//! `core/tests/scheduler_inner_loop_e2e.rs`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use kastellan_core::cassandra::types::{DataClass, Plan, Severity};
use kastellan_core::channel::ask_message::{destination_from_task_payload, parse_ask_command, AskChoice};
use kastellan_core::channel::ingest::build_channel_task_payload;
use kastellan_core::channel::outbox::ChannelOutbox;
use kastellan_core::channel::{ChannelId, ConversationId, IncomingMessage, PeerId};
use kastellan_core::scheduler::asks;
use kastellan_core::scheduler::audit::{
    ACTION_ASK_DELIVERED, ACTION_ASK_DELIVERY_FAILED, ACTION_ASK_EXPIRED, ACTION_ASK_RAISED,
};
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
    let service_name = format!("kastellan-sched-test-pg-asks-{suffix}");
    let cluster = tokio::task::block_in_place(|| {
        bring_up_pg_cluster(&bin_dir, "asks-d", "asks-l", &service_name)
    });

    kastellan_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "scheduler-asks"}),
    )
    .await
    .ok()?;

    let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .ok()?;

    Some((pool, cluster))
}

/// A minimal terminal plan varying only in `context` — one of the four
/// fields `plan_digest` deliberately EXCLUDES. Mirrors
/// `task_complete_plan` in `core/tests/scheduler_lanes_e2e.rs:147`.
fn plan_with_context(context: &str) -> Plan {
    Plan {
        context: context.into(),
        decision: "task_complete".into(),
        rationale: "done".into(),
        steps: vec![],
        result: Some(serde_json::json!({"kind": "text", "body": "ok"})),
        data_ceiling: Some(DataClass::Public),
        refused: None,
        floor_request: None,
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
        python_skill: None,
    }
}

/// A channel-originated task payload, built by CALLING
/// `channel::ingest::build_channel_task_payload` rather than hand-writing the
/// JSON literal.
///
/// **This is one of the two cross-crate pins this file exists to carry.**
/// `db::asks::resolve_with_nonce`'s entitlement guard matches literal SQL
/// string keys (`kind`/`channel`/`peer`) against this payload's shape, and
/// `db` cannot depend on `core` to import the producer's constant — nothing
/// else in the codebase can see both sides at once. Hand-writing the JSON
/// here would leave a rename in `build_channel_task_payload` unnoticed by
/// every test on either side (every unit test on the `db` side stays green
/// against its own literal; every unit test on the `core` side stays green
/// against its own struct) while every real approval on a live host started
/// failing closed, silently. Calling the real function makes a rename fail
/// THIS test instead.
fn channel_payload() -> serde_json::Value {
    build_channel_task_payload(&IncomingMessage {
        channel: ChannelId("matrix".into()),
        peer: PeerId("@me:srv".into()),
        conversation: ConversationId("!room:srv".into()),
        body: "book the flight".into(),
        evidence: None,
    })
}

async fn seed_running_task(pool: &sqlx::PgPool) -> i64 {
    let id = insert_pending(pool, Lane::Fast, serde_json::json!({"instruction": "x", "max_plans": 3}))
        .await
        .expect("insert");
    tasks::claim_one(pool, Lane::Fast, 60).await.expect("claim").expect("a task");
    id
}

async fn audit_actions_for(pool: &sqlx::PgPool, task_id: i64) -> Vec<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT action FROM audit_log \
         WHERE payload->>'task_id' = $1::text ORDER BY id ASC",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await
    .expect("audit read")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raise_and_suspend_suspends_the_task_and_audits_it() {
    let Some((pool, _cluster)) = bring_up_pg("raise").await else { return };
    let task_id = seed_running_task(&pool).await;

    // A non-empty `resume_state`: the run's history at suspend time, which
    // the resume restores so the task does not re-formulate — and re-run —
    // the iterations it already completed (spec D11).
    let run_state = serde_json::json!({
        "plans": [{"plan": {"decision": "act"}, "outcomes": ["ok"]}],
        "advisories": ["watch the tone"],
        "blocks": [],
    });
    let ask_id = asks::raise_and_suspend(
        &pool, task_id, &plan_with_context("send the mail"),
        "this sends mail to a stranger", Severity::High, Some(&run_state),
        None, None,
    ).await.expect("raise_and_suspend");

    assert_eq!(
        tasks::observe_state(&pool, task_id).await.expect("state"),
        "awaiting_operator",
    );
    let ask = kastellan_db::asks::get(&pool, ask_id).await.expect("get").expect("an ask");
    assert_eq!(ask.state, "pending");
    assert_eq!(ask.kind, asks::ASK_KIND_PLAN_APPROVAL);
    assert_eq!(ask.body, "this sends mail to a stranger");
    assert_eq!(ask.options, serde_json::json!(["approve", "deny"]));
    assert!(ask.plan_digest.is_some(), "a plan_approval ask must bind to a digest");
    assert_eq!(
        ask.resume_state.as_ref(), Some(&run_state),
        "the suspension must carry the run's history, verbatim — `run_one` restores \
         the resumed TaskContext from exactly this value",
    );

    assert!(
        audit_actions_for(&pool, task_id).await.iter().any(|a| a == ACTION_ASK_RAISED),
        "an ask.raised row must be written",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_digest_recorded_is_the_digest_of_the_plan_passed_in() {
    let Some((pool, _cluster)) = bring_up_pg("digest").await else { return };
    let a = seed_running_task(&pool).await;
    let ask_a = asks::raise_and_suspend(
        &pool, a, &plan_with_context("plan one"), "c", Severity::Medium, None,
        None, None,
    ).await.expect("raise a");

    let b = seed_running_task(&pool).await;
    let ask_b = asks::raise_and_suspend(
        &pool, b, &plan_with_context("plan two"), "c", Severity::Medium, None,
        None, None,
    ).await.expect("raise b");

    let da = kastellan_db::asks::get(&pool, ask_a).await.unwrap().unwrap().plan_digest;
    let db_ = kastellan_db::asks::get(&pool, ask_b).await.unwrap().unwrap().plan_digest;
    // `context` is one of the four fields the digest EXCLUDES, so two plans
    // differing only in it must digest identically. This pins that
    // raise_and_suspend really calls plan_digest rather than hashing
    // something convenient.
    assert_eq!(da, db_, "context is excluded from the digest (slice 1a D2)");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raising_against_a_task_that_is_not_running_is_an_error() {
    let Some((pool, _cluster)) = bring_up_pg("notrunning").await else { return };
    let task_id = insert_pending(&pool, Lane::Fast, serde_json::json!({"instruction": "x", "max_plans": 3}))
        .await.expect("insert");
    // Never claimed, so still `pending`.
    let err = asks::raise_and_suspend(
        &pool, task_id, &plan_with_context("x"), "c", Severity::Low, None,
        None, None,
    ).await;
    assert!(err.is_err(), "raising against a non-running task must fail, not orphan an ask");
    assert_eq!(tasks::observe_state(&pool, task_id).await.expect("state"), "pending");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sweep_expired_and_audit_fails_the_task_and_writes_one_row_each() {
    let Some((pool, _cluster)) = bring_up_pg("sweep").await else { return };
    let task_id = seed_running_task(&pool).await;

    // A one-second deadline, honoured through the documented env knob so
    // the test exercises the same path production does.
    std::env::set_var(asks::ASK_DEADLINE_ENV, "1");
    let ask_id = asks::raise_and_suspend(
        &pool, task_id, &plan_with_context("x"), "c", Severity::Low, None,
        None, None,
    ).await.expect("raise");
    std::env::remove_var(asks::ASK_DEADLINE_ENV);

    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let swept = asks::sweep_expired_and_audit(&pool).await.expect("sweep");
    assert_eq!(swept, 1);

    assert_eq!(tasks::observe_state(&pool, task_id).await.expect("state"), "failed");
    let t = tasks::get(&pool, task_id).await.expect("get").expect("a task");
    assert_eq!(
        t.result.as_ref().and_then(|r| r.get("detail")).and_then(|d| d.as_str()),
        Some(kastellan_db::asks::ASK_TIMEOUT_DETAIL),
    );
    assert_eq!(
        kastellan_db::asks::get(&pool, ask_id).await.unwrap().unwrap().state,
        "expired",
    );
    assert!(
        audit_actions_for(&pool, task_id).await.iter().any(|a| a == ACTION_ASK_EXPIRED),
        "an ask.expired row must be written",
    );

    // Idempotent: a second sweep finds nothing and writes nothing.
    assert_eq!(asks::sweep_expired_and_audit(&pool).await.expect("sweep 2"), 0);
    let expired_rows = audit_actions_for(&pool, task_id).await
        .iter().filter(|a| *a == ACTION_ASK_EXPIRED).count();
    assert_eq!(expired_rows, 1, "a second sweep must not duplicate the audit row");
}

/// The whole loop, end to end against a live Postgres: a channel task
/// escalates, the ask is delivered to the outbox carrying a token, that
/// token resolves the ask when presented by the task's own peer, and the
/// task returns to `pending`.
///
/// **This is the only test that proves the delivered token is the token
/// that resolves.** Every pure test on either side passes with the two
/// halves disagreeing — the renderer could print one thing and the
/// resolver expect another, and both suites stay green.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_raised_ask_is_delivered_and_its_token_resolves_it() {
    let Some((pool, _cluster)) = bring_up_pg("deliver").await else { return };

    let payload = channel_payload();
    let task_id = insert_pending(&pool, Lane::Fast, payload.clone()).await.expect("insert");
    tasks::claim_one(&pool, Lane::Fast, 60).await.expect("claim").expect("a task");

    let outbox = ChannelOutbox::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    outbox.register(ChannelId("matrix".into()), tx);

    let dest = destination_from_task_payload(&payload).expect("destination");
    let ask_id = asks::raise_and_suspend(
        &pool, task_id, &plan_with_context("send the money"),
        "sends money to a stranger", Severity::High, None,
        Some(&outbox), Some(&dest),
    )
    .await
    .expect("raise + deliver");

    // The delivery carried a usable command, into the right room.
    //
    // Bounded, and that matters more than it looks: `outbox` still holds the
    // `Sender`, so a `deliver_ask` regressed to a no-op never closes the
    // channel and `rx.recv()` returns neither a message nor `None` — it
    // waits forever. `cargo test` has no per-test timeout, so an unbounded
    // await here would HANG the Linux gate instead of failing it.
    let sent = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
        .await
        .expect(
            "no ask reached the outbox within 10s — `raise_and_suspend` delivered nothing; \
             this timeout is what turns that regression into a failure instead of a hang",
        )
        .expect("the ask was delivered");
    let kastellan_core::channel::bus::Queued::Ask { out: sent, .. } = sent else {
        panic!("a raised ask is queued as an ask, with no reply to claim: {sent:?}");
    };
    assert_eq!(sent.conversation.0, "!room:srv");
    let approve = sent
        .body
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("/approve"))
        .expect("an approve command was offered")
        .to_string();
    let cmd = parse_ask_command(&approve).expect("the offered command parses");

    // ... and that token resolves the ask, for the task's own peer.
    let owner = kastellan_db::asks::Claimant::new("matrix", "@me:srv");
    // The submitted choice is derived from the wire vocabulary
    // (`AskChoice::as_str()`), not retyped as `"approve"` — the second
    // cross-crate pin this file carries. `render_ask` prints `/approve
    // <token>`, `raise_and_suspend` builds `asks.options` by calling
    // `AskChoice::{Approve,Deny}::as_str()`, and `resolve_with_nonce`
    // validates the submitted choice against exactly that array. A literal
    // here would stay green even if the wire vocabulary and the stored
    // options drifted apart, because it would happen to match today's
    // spelling on both sides by coincidence rather than by construction.
    let resolved = kastellan_db::asks::resolve_with_nonce(
        &pool,
        &kastellan_db::asks::Nonce::from_wire(cmd.token),
        &owner,
        &serde_json::json!({"choice": AskChoice::Approve.as_str()}),
    )
    .await
    .expect("resolve")
    .expect("the delivered token resolves the ask it was delivered for");
    assert_eq!(resolved.ask_id, ask_id);
    assert_eq!(resolved.task_id, task_id);
    assert_eq!(tasks::observe_state(&pool, task_id).await.expect("state"), "pending");

    assert!(
        audit_actions_for(&pool, task_id).await.iter().any(|a| a == ACTION_ASK_DELIVERED),
        "a delivered ask must leave an ask.delivered row",
    );
}

/// A delivery failure must not cost the ask. The registry has no channel,
/// so `try_deliver` fails — and the ask must still be committed, the task
/// still suspended, and `kastellan-cli inbox` still able to answer it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_delivery_still_leaves_a_durable_answerable_ask() {
    let Some((pool, _cluster)) = bring_up_pg("delivfail").await else { return };

    let payload = channel_payload();
    let task_id = insert_pending(&pool, Lane::Fast, payload.clone()).await.expect("insert");
    tasks::claim_one(&pool, Lane::Fast, 60).await.expect("claim").expect("a task");

    let empty_outbox = ChannelOutbox::new(); // nothing registered
    let dest = destination_from_task_payload(&payload).expect("destination");
    let ask_id = asks::raise_and_suspend(
        &pool, task_id, &plan_with_context("send the money"),
        "sends money to a stranger", Severity::High, None,
        Some(&empty_outbox), Some(&dest),
    )
    .await
    .expect("a delivery failure must not fail the raise");

    assert_eq!(kastellan_db::asks::get(&pool, ask_id).await.unwrap().unwrap().state, "pending");
    assert_eq!(
        tasks::observe_state(&pool, task_id).await.expect("state"),
        "awaiting_operator",
    );
    assert!(
        audit_actions_for(&pool, task_id).await.iter().any(|a| a == ACTION_ASK_DELIVERY_FAILED),
        "an undelivered ask must leave the compensating row",
    );
    assert!(
        kastellan_db::asks::resolve(
            &pool, ask_id, "hherb", &serde_json::json!({"choice": AskChoice::Approve.as_str()}),
        ).await.unwrap(),
        "the CLI must still be able to answer it",
    );
}
