//! End-to-end smoke for [`hhagent_core::memory::l0_seed`] — the
//! L0 (meta-rule) seed loader and its paired read-side helper.
//!
//! Each scenario brings up its own per-test Postgres cluster (same
//! recipe as `memory_recall_e2e.rs` and `memory_layers_e2e.rs`) so
//! seeded rows cannot drift between scenarios.
//!
//! Skips silently with `[SKIP]` lines on hosts without Postgres or a
//! reachable supervisor; `cargo test -- --nocapture` to see them.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::Path;

use hhagent_core::memory::l0_seed::{
    seed_l0_from_rules, L0Rule,
};
use hhagent_db::memories::load_active_l0;
use hhagent_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime")
}

fn seed_path() -> &'static Path {
    Path::new("seeds/memory/l0_meta_rules.toml")
}

fn make_rule(id: &str, body: &str) -> L0Rule {
    L0Rule {
        id: id.to_string(),
        body: body.to_string(),
        tags: Vec::new(),
    }
}

#[test]
fn seed_from_rules_writes_new_rows() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l0n-d",
        "l0n-l",
        &format!("hhagent-supervisor-test-pg-l0new-{suffix}"),
    );

    rt().block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l0-seed-new"}),
        )
        .await
        .expect("probe");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let rules = vec![
            make_rule("rule_a", "first body"),
            make_rule("rule_b", "second body"),
        ];
        let report = seed_l0_from_rules(&pool, seed_path(), "src-sha-1", &rules)
            .await
            .expect("seed");

        assert_eq!(report.rules_loaded, 2);
        assert_eq!(report.new_rows_written, 2);
        assert_eq!(report.unchanged_skipped, 0);

        let active = load_active_l0(&pool, 64).await.expect("load");
        assert_eq!(active.len(), 2);
        // Both rules visible; bodies match.
        let bodies: std::collections::HashSet<&str> =
            active.iter().map(|m| m.body.as_str()).collect();
        assert!(bodies.contains("first body"));
        assert!(bodies.contains("second body"));
        // Layer is L0 / Meta.
        for m in &active {
            assert_eq!(
                m.layer,
                hhagent_db::memories::MemoryLayer::Meta,
                "all active L0 rows must report layer=Meta"
            );
        }
        // Metadata keys present.
        for m in &active {
            let meta = m.metadata.as_object().expect("metadata object");
            assert!(meta.contains_key("l0_rule_id"));
            assert!(meta.contains_key("body_sha256"));
            assert!(meta.contains_key("tags"));
            assert!(meta.contains_key("source_path"));
        }

        pool.close().await;
    });
}

#[test]
fn seed_from_rules_is_idempotent_on_unchanged_input() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l0i-d",
        "l0i-l",
        &format!("hhagent-supervisor-test-pg-l0idem-{suffix}"),
    );

    rt().block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l0-idempotent"}),
        )
        .await
        .expect("probe");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let rules = vec![
            make_rule("rule_a", "first body"),
            make_rule("rule_b", "second body"),
        ];

        let r1 = seed_l0_from_rules(&pool, seed_path(), "sha", &rules)
            .await
            .expect("seed-1");
        assert_eq!(r1.new_rows_written, 2);
        assert_eq!(r1.unchanged_skipped, 0);

        // Same input again → zero new rows.
        let r2 = seed_l0_from_rules(&pool, seed_path(), "sha", &rules)
            .await
            .expect("seed-2");
        assert_eq!(r2.new_rows_written, 0);
        assert_eq!(r2.unchanged_skipped, 2);

        let active = load_active_l0(&pool, 64).await.expect("load");
        assert_eq!(active.len(), 2);

        pool.close().await;
    });
}

#[test]
fn seed_from_rules_writes_new_row_on_edited_body() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l0e-d",
        "l0e-l",
        &format!("hhagent-supervisor-test-pg-l0edit-{suffix}"),
    );

    rt().block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l0-edit"}),
        )
        .await
        .expect("probe");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let rules_v1 = vec![
            make_rule("rule_a", "original body"),
            make_rule("rule_b", "untouched body"),
        ];
        let r1 = seed_l0_from_rules(&pool, seed_path(), "sha-1", &rules_v1)
            .await
            .expect("seed-v1");
        assert_eq!(r1.new_rows_written, 2);

        // Edit rule_a body, re-seed.
        let rules_v2 = vec![
            make_rule("rule_a", "edited body"),
            make_rule("rule_b", "untouched body"),
        ];
        let r2 = seed_l0_from_rules(&pool, seed_path(), "sha-2", &rules_v2)
            .await
            .expect("seed-v2");
        assert_eq!(r2.new_rows_written, 1); // rule_a got a new row
        assert_eq!(r2.unchanged_skipped, 1); // rule_b already there

        // Active set has 2 rows; rule_a body is the edited one.
        let active = load_active_l0(&pool, 64).await.expect("load");
        assert_eq!(active.len(), 2);
        let mut by_rule_id: std::collections::HashMap<String, String> = Default::default();
        for m in &active {
            let rid = m.metadata["l0_rule_id"].as_str().expect("rule_id").to_string();
            by_rule_id.insert(rid, m.body.clone());
        }
        assert_eq!(by_rule_id.get("rule_a").map(String::as_str), Some("edited body"));
        assert_eq!(by_rule_id.get("rule_b").map(String::as_str), Some("untouched body"));

        // Total memories at layer 0 is 3 (rule_a v1 + rule_a v2 + rule_b).
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM memories WHERE layer = 0",
        )
        .fetch_one(&pool)
        .await
        .expect("count");
        assert_eq!(count, 3, "edited rule must leave its old row behind for audit");

        pool.close().await;
    });
}
