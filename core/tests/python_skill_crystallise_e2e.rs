//! PG-gated integration test for the Python-skill crystallise direct writer:
//! [`kastellan_core::memory::l3py_crystallise::crystallise_python_skill`].
//!
//! Skips silently with `[SKIP]` on hosts without Postgres or a reachable
//! supervisor. Run `cargo test -- --nocapture` to see skip lines.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use kastellan_core::cassandra::types::PythonSkillCandidate;
use kastellan_core::memory::l3_crystallise::L3Source;
use kastellan_core::memory::l3py_crystallise::{crystallise_python_skill, PyWriteOutcome};
use kastellan_db::memories::MemoryLayer;
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
