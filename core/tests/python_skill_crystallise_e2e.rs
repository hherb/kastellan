//! PG-gated integration tests for Python-skill crystallisation: the direct
//! writer [`kastellan_core::memory::l3py_crystallise::crystallise_python_skill`],
//! and the scheduler path, where a notebook `answer` carries the skill (#996)
//! and `runner::drain_lane` writes it with its `l3.crystallised` audit row.
//!
//! Skips silently with `[SKIP]` on hosts without Postgres or a reachable
//! supervisor. Run `cargo test -- --nocapture` to see skip lines.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use kastellan_core::cassandra::review::{ChainReviewStage, NoopReviewStage};
use kastellan_core::cassandra::types::{DataClass, PlannedStep, PythonSkillCandidate};
use kastellan_core::entity_extraction::NoOpEntityExtractor;
use kastellan_core::memory::embedder::NoOpEmbedder;
use kastellan_core::memory::l3_crystallise::L3Source;
use kastellan_core::memory::l3py_crystallise::{crystallise_python_skill, PyWriteOutcome};
use kastellan_core::scheduler::inner_loop::{StepDispatcher, StepOutcome};
use kastellan_core::scheduler::spawn_scheduler;
use kastellan_db::memories::MemoryLayer;
use kastellan_tests_common::scripted_notebook::{answer_reply, call_reply, ScriptedNotebook};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix, PgCluster,
};

// ---------------------------------------------------------------------------
// PG bring-up helper (async, tokio context required)
// ---------------------------------------------------------------------------

async fn bring_up_pg(label: &str) -> Option<(sqlx::PgPool, PgCluster)> {
    if skip_if_no_supervisor() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let suffix = format!("{}-{}", label, unique_suffix());
    let service_name = format!("kastellan-pyx-test-pg-{suffix}");
    // data_label and log_label must be short and unique per concurrent test.
    // Use the first 4 chars of label so the socket path stays well under
    // the macOS sockaddr_un.sun_path 104-byte cap.
    let data_label = format!("{}-d", &label[..label.len().min(4)]);
    let log_label = format!("{}-l", &label[..label.len().min(4)]);
    let cluster = tokio::task::block_in_place(|| {
        bring_up_pg_cluster(&bin_dir, &data_label, &log_label, &service_name)
    });

    kastellan_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"purpose": "python-skill-crystallise-e2e"}),
    )
    .await
    .ok()?;

    let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .ok()?;

    Some((pool, cluster))
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

fn cand() -> PythonSkillCandidate {
    PythonSkillCandidate {
        name: "sum_stdin".into(),
        description: "Sum integers from stdin".into(),
        code: "import sys\nprint(sum(int(x) for x in sys.stdin))\n".into(),
    }
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crystallise_python_skill_inserts_dedups_and_stores_verbatim() {
    let Some((pool, _cluster)) = bring_up_pg("pyx").await else {
        return; // [SKIP]
    };

    let out = crystallise_python_skill(&pool, &cand(), L3Source::AgentRaised { task_id: 1 })
        .await
        .expect("crystallise ok");
    let id = match out {
        PyWriteOutcome::Inserted { memory_id } => memory_id,
        other => panic!("expected Inserted, got {other:?}"),
    };

    // Re-crystallising identical code dedups to the same row.
    let again = crystallise_python_skill(&pool, &cand(), L3Source::AgentRaised { task_id: 2 })
        .await
        .expect("re-crystallise ok");
    assert!(matches!(again, PyWriteOutcome::SkippedDuplicate { memory_id } if memory_id == id));

    // exactly one layer-3 row.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memories WHERE layer = $1")
        .bind(MemoryLayer::Skill.as_db())
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(count, 1, "dedup: only one row");

    // The stored row is kind=python, trust=untrusted, with the verbatim code + description body.
    let row: (serde_json::Value, String) =
        sqlx::query_as("SELECT metadata, body FROM memories WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row.0.get("kind").and_then(|v| v.as_str()), Some("python"));
    assert_eq!(row.0.get("trust").and_then(|v| v.as_str()), Some("untrusted"));
    assert_eq!(
        row.0.get("python").and_then(|p| p.get("code")).and_then(|v| v.as_str()),
        Some(cand().code.as_str())
    );
    assert_eq!(row.1, cand().description);

    pool.close().await;
}

/// Dispatcher that answers every step `Ok`.
struct OkDispatcher;

#[async_trait]
impl StepDispatcher for OkDispatcher {
    async fn dispatch_step(&self, _task_id: i64, _step: &PlannedStep) -> StepOutcome {
        StepOutcome::Ok(serde_json::json!("ok"))
    }
}

/// Run one notebook task through the scheduler: one `python-exec` call (of
/// [`cand`]'s code, whatever `skill` is), then an answer carrying `skill`.
/// The task must complete with that answer whatever the skill is
/// (`tasks_completed` fires on a failed task too, hence the state check).
async fn run_notebook_answering(pool: &sqlx::PgPool, skill: PythonSkillCandidate) {
    // Subscribe BEFORE inserting so the NOTIFY is not missed.
    let mut listener = sqlx::postgres::PgListener::connect_with(pool).await.expect("PgListener connect");
    listener.listen("tasks_completed").await.expect("LISTEN tasks_completed");
    let task_id = kastellan_db::tasks::insert_pending(
        pool,
        kastellan_db::tasks::Lane::Fast,
        serde_json::json!({"instruction": "test", "max_plans": 10}),
    )
    .await
    .expect("insert_pending");

    let notebook = ScriptedNotebook::new(move |ctx| {
        if ctx.plans.is_empty() {
            let step = PlannedStep {
                tool: "python-exec".into(),
                method: "python.exec".into(),
                parameters: serde_json::json!({"code": cand().code}),
                returns: "the sum".into(),
                done_when: "printed".into(),
                classification: DataClass::Public,
            };
            return call_reply(1, &step);
        }
        let mut reply: serde_json::Value = serde_json::from_str(&answer_reply("done")).unwrap();
        reply["answer"]["python_skill"] = serde_json::to_value(&skill).unwrap();
        reply.to_string()
    });
    let scheduler = spawn_scheduler(
        pool.clone(),
        Arc::new(notebook),
        Arc::new(ChainReviewStage::new(vec![Arc::new(NoopReviewStage)])),
        Arc::new(OkDispatcher),
        Arc::new(NoOpEntityExtractor::new()),
        Arc::new(NoOpEmbedder::new()),
        None,
    );
    tokio::time::timeout(Duration::from_secs(10), listener.recv())
        .await
        .expect("timed out waiting for tasks_completed NOTIFY")
        .expect("PgListener recv error");
    // The listener holds a pooled connection: `pool.close()` would wait on it forever.
    drop(listener);
    scheduler.shutdown().await;

    let (state, result): (String, Option<serde_json::Value>) =
        sqlx::query_as("SELECT state, result FROM tasks WHERE id = $1").bind(task_id).fetch_one(pool).await.unwrap();
    assert_eq!(state, "completed", "{result:?}");
    assert_eq!(result.as_ref().and_then(|r| r["body"].as_str()), Some("done"), "{result:?}");
}

async fn skill_rows(pool: &sqlx::PgPool) -> Vec<serde_json::Value> {
    let rows: Vec<(serde_json::Value,)> = sqlx::query_as("SELECT metadata FROM memories WHERE layer = $1")
        .bind(MemoryLayer::Skill.as_db())
        .fetch_all(pool)
        .await
        .unwrap();
    rows.into_iter().map(|(m,)| m).collect()
}

async fn crystallised_rows(pool: &sqlx::PgPool) -> Vec<serde_json::Value> {
    let audit = kastellan_db::audit::fetch_since(pool, 0, 100).await.unwrap();
    audit.into_iter().filter(|r| r.actor == "scheduler" && r.action == "l3.crystallised").map(|r| r.payload).collect()
}

/// The scheduler path: a notebook task makes one call, then answers with
/// `python_skill`; the runner stores it untrusted and audits it as `python`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_notebook_answers_python_skill_is_crystallised_by_the_runner() {
    let Some((pool, _cluster)) = bring_up_pg("pyr").await else {
        return; // [SKIP]
    };
    run_notebook_answering(&pool, cand()).await;

    let rows = skill_rows(&pool).await;
    assert_eq!(rows.len(), 1, "one layer-3 row");
    assert_eq!(rows[0]["kind"], "python");
    assert_eq!(rows[0]["trust"], "untrusted");
    assert_eq!(rows[0]["python"]["code"], cand().code.as_str());

    let crystallised = crystallised_rows(&pool).await;
    assert_eq!(crystallised.len(), 1, "one l3.crystallised row");
    let p = &crystallised[0];
    assert_eq!((p["kind"].as_str(), p["action"].as_str()), (Some("python"), Some("inserted")), "{p}");
    assert_eq!(p["skill_name"], "sum_stdin");

    pool.close().await;
}

/// The runner's validation on the notebook path: a skill whose code holds a
/// `secret://` reference writes no layer-3 row and no `l3.crystallised` row,
/// and the task still completes with its answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_notebook_answers_invalid_python_skill_writes_nothing() {
    let Some((pool, _cluster)) = bring_up_pg("pyi").await else {
        return; // [SKIP]
    };
    let bad = PythonSkillCandidate { code: "print('secret://api_key')\n".into(), ..cand() };
    run_notebook_answering(&pool, bad).await;

    assert_eq!(skill_rows(&pool).await, Vec::<serde_json::Value>::new(), "no layer-3 row");
    assert_eq!(crystallised_rows(&pool).await, Vec::<serde_json::Value>::new(), "no l3.crystallised row");

    pool.close().await;
}
