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
    load_l0_active, load_l0_active_default, seed_l0_from_file, seed_l0_from_rules,
    L0Error, L0Rule, L0_DEFAULT_CAP_BYTES, L0_DEFAULT_CAP_ROWS,
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

        let active = load_active_l0(&pool, L0_DEFAULT_CAP_ROWS).await.expect("load");
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

        let active = load_active_l0(&pool, L0_DEFAULT_CAP_ROWS).await.expect("load");
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
        let active = load_active_l0(&pool, L0_DEFAULT_CAP_ROWS).await.expect("load");
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

#[test]
fn seed_from_file_reads_parses_and_seeds() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l0f-d",
        "l0f-l",
        &format!("hhagent-supervisor-test-pg-l0file-{suffix}"),
    );

    rt().block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l0-from-file"}),
        )
        .await
        .expect("probe");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // Write a small TOML to a temp dir.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("l0.toml");
        let toml = r#"
[[rule]]
id = "from_file_a"
body = "rule A body"

[[rule]]
id = "from_file_b"
body = "rule B body"
"#;
        tokio::fs::write(&path, toml).await.expect("write toml");

        let report = seed_l0_from_file(&pool, &path).await.expect("seed");
        assert_eq!(report.rules_loaded, 2);
        assert_eq!(report.new_rows_written, 2);
        assert_eq!(report.unchanged_skipped, 0);
        assert_eq!(report.source_path, path);
        assert_eq!(report.source_sha256.len(), 64, "SHA-256 hex");

        let active = load_l0_active(&pool, L0_DEFAULT_CAP_ROWS, L0_DEFAULT_CAP_BYTES).await.expect("load");
        assert_eq!(active.len(), 2);

        pool.close().await;
    });
}

#[test]
fn seed_from_file_fails_closed_on_malformed_toml() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l0m-d",
        "l0m-l",
        &format!("hhagent-supervisor-test-pg-l0mal-{suffix}"),
    );

    rt().block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l0-malformed"}),
        )
        .await
        .expect("probe");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("bad.toml");
        // Missing body, unterminated string — toml crate must reject.
        tokio::fs::write(&path, "[[rule]]\nid = \"x\"\nbody = \"oops")
            .await
            .expect("write");

        let err = seed_l0_from_file(&pool, &path)
            .await
            .expect_err("malformed toml must fail closed");
        assert!(matches!(err, L0Error::TomlParse { .. }), "got {err:?}");

        // No rows written.
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM memories WHERE layer = 0",
        )
        .fetch_one(&pool)
        .await
        .expect("count");
        assert_eq!(count, 0, "fail-closed must write zero rows");

        pool.close().await;
    });
}

#[test]
fn load_l0_active_returns_newest_per_rule_id() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l0d-d",
        "l0d-l",
        &format!("hhagent-supervisor-test-pg-l0dedup-{suffix}"),
    );

    rt().block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l0-dedup"}),
        )
        .await
        .expect("probe");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // Seed v1, then v2 of the same rule_id.
        let v1 = vec![make_rule("ruleX", "version 1")];
        seed_l0_from_rules(&pool, seed_path(), "sha-1", &v1)
            .await
            .expect("v1");
        // Sleep 5 ms so created_at differs at microsecond resolution
        // (defense-in-depth — the `id DESC` tiebreaker would also
        // pick the newer row, but pinning on created_at is the
        // documented load_active_l0 contract).
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        let v2 = vec![make_rule("ruleX", "version 2")];
        seed_l0_from_rules(&pool, seed_path(), "sha-2", &v2)
            .await
            .expect("v2");

        let active = load_l0_active_default(&pool).await.expect("load");
        assert_eq!(active.len(), 1, "dedup must return one row per rule_id");
        assert_eq!(active[0].body, "version 2", "newest version wins");

        pool.close().await;
    });
}

#[test]
fn load_l0_active_respects_cap_rows() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l0r-d",
        "l0r-l",
        &format!("hhagent-supervisor-test-pg-l0caprows-{suffix}"),
    );

    rt().block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l0-cap-rows"}),
        )
        .await
        .expect("probe");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let rules = vec![
            make_rule("r1", "a"),
            make_rule("r2", "b"),
            make_rule("r3", "c"),
        ];
        seed_l0_from_rules(&pool, seed_path(), "sha", &rules)
            .await
            .expect("seed");

        let two = load_l0_active(&pool, 2, L0_DEFAULT_CAP_BYTES).await.expect("cap 2");
        assert_eq!(two.len(), 2, "cap_rows must trim DB-side");

        // Defense-in-depth: cap_rows = 0 returns empty.
        let zero = load_l0_active(&pool, 0, L0_DEFAULT_CAP_BYTES).await.expect("cap 0");
        assert!(zero.is_empty());

        pool.close().await;
    });
}

#[test]
fn load_l0_active_oversize_body_dropped_silently() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l0o-d",
        "l0o-l",
        &format!("hhagent-supervisor-test-pg-l0over-{suffix}"),
    );

    rt().block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l0-oversize"}),
        )
        .await
        .expect("probe");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // Seed the big body first (older); the small body second
        // (newer). load_active_l0 returns newest-first, so the small
        // body comes back at index 0, fits. The big body comes back
        // at index 1; cumulative bytes exceed cap_bytes=500 → break.
        let big_body = "x".repeat(600);
        let small_body = "y".repeat(100);

        let rules1 = vec![L0Rule {
            id: "big".to_string(),
            body: big_body.clone(),
            tags: Vec::new(),
        }];
        seed_l0_from_rules(&pool, seed_path(), "sha-big", &rules1)
            .await
            .expect("seed big");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        let rules2 = vec![L0Rule {
            id: "small".to_string(),
            body: small_body.clone(),
            tags: Vec::new(),
        }];
        seed_l0_from_rules(&pool, seed_path(), "sha-small", &rules2)
            .await
            .expect("seed small");

        // cap_bytes = 500 < big body (600). The small body (newer,
        // 100 B) comes back first and fits; the big body (older,
        // 600 B) comes back second and pushes cumulative bytes past
        // the cap → break.
        let active = load_l0_active(&pool, L0_DEFAULT_CAP_ROWS, 500).await.expect("load");
        assert_eq!(active.len(), 1, "only the small body fits");
        assert_eq!(active[0].body, small_body);

        pool.close().await;
    });
}

#[test]
fn load_l0_active_excludes_legacy_l0_rows_without_rule_id() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l0l-d",
        "l0l-l",
        &format!("hhagent-supervisor-test-pg-l0legacy-{suffix}"),
    );

    rt().block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l0-legacy"}),
        )
        .await
        .expect("probe");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // A "legacy" L0 row written directly via seed_meta_memory with
        // empty metadata (no l0_rule_id). load_active_l0 must skip it.
        hhagent_db::memories::seed_meta_memory(
            &pool,
            "legacy without rule_id",
            &serde_json::json!({}),
            None,
        )
        .await
        .expect("seed legacy");

        // A real L0 rule.
        let rules = vec![make_rule("real", "real rule body")];
        seed_l0_from_rules(&pool, seed_path(), "sha", &rules)
            .await
            .expect("seed real");

        let active = load_l0_active_default(&pool).await.expect("load");
        assert_eq!(active.len(), 1, "legacy row must be excluded");
        assert_eq!(active[0].body, "real rule body");

        // Sanity: layer-0 total is 2 (the legacy row is in the table,
        // just not in the active set).
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM memories WHERE layer = 0",
        )
        .fetch_one(&pool)
        .await
        .expect("count");
        assert_eq!(count, 2);

        pool.close().await;
    });
}

/// Final-review follow-up: `L0Error::Io` is constructed when
/// `seed_l0_from_file` is called against a path that does not exist.
/// The daemon's `l0_path.exists()` guard normally elides this branch,
/// but the API is public — pin the surface.
#[test]
fn seed_from_file_returns_io_error_on_missing_path() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l0p-d",
        "l0p-l",
        &format!("hhagent-supervisor-test-pg-l0iopath-{suffix}"),
    );

    rt().block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l0-io-missing"}),
        )
        .await
        .expect("probe");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("definitely-does-not-exist.toml");
        let err = seed_l0_from_file(&pool, &missing)
            .await
            .expect_err("missing file must fail");
        assert!(matches!(err, L0Error::Io { .. }), "got {err:?}");

        pool.close().await;
    });
}

/// Final-review follow-up: the `tracing::warn!` branch in
/// `load_l0_active` fires when the *first* (newest) row's body alone
/// exceeds `cap_bytes`. The existing oversize test seeds a smaller
/// row last (newest), so the budget breaks AFTER admitting one row.
/// This scenario keeps the accumulator empty when the budget breaks,
/// exercising the warn-and-drop branch.
#[test]
fn load_l0_active_warns_when_first_row_alone_exceeds_cap_bytes() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l0w-d",
        "l0w-l",
        &format!("hhagent-supervisor-test-pg-l0warn-{suffix}"),
    );

    rt().block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l0-warn"}),
        )
        .await
        .expect("probe");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // Single rule, 700-byte body. cap_bytes = 500 < 700 → the
        // newest (only) row trips the
        // `acc.is_empty() && row_bytes > cap_bytes` branch and is
        // dropped with a warn (not asserted in-process, but the
        // empty-result assertion proves the branch fired).
        let big_body = "z".repeat(700);
        let rules = vec![L0Rule {
            id: "lonely_big".to_string(),
            body: big_body,
            tags: Vec::new(),
        }];
        seed_l0_from_rules(&pool, seed_path(), "sha-lonely", &rules)
            .await
            .expect("seed");

        let active = load_l0_active(&pool, L0_DEFAULT_CAP_ROWS, 500)
            .await
            .expect("load");
        assert!(active.is_empty(), "first row alone over cap must be dropped");

        pool.close().await;
    });
}

/// Final-review follow-up: standalone pin on the `cap_bytes == 0`
/// fast-path. The existing `respects_cap_rows` test already covers
/// `cap_rows == 0`; this one closes the symmetric gap.
#[test]
fn load_l0_active_zero_cap_bytes_returns_empty() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l0z-d",
        "l0z-l",
        &format!("hhagent-supervisor-test-pg-l0zerobytes-{suffix}"),
    );

    rt().block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l0-zero-bytes"}),
        )
        .await
        .expect("probe");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let rules = vec![make_rule("r1", "anything")];
        seed_l0_from_rules(&pool, seed_path(), "sha", &rules)
            .await
            .expect("seed");

        let active = load_l0_active(&pool, L0_DEFAULT_CAP_ROWS, 0)
            .await
            .expect("load with cap_bytes=0");
        assert!(active.is_empty(), "cap_bytes=0 must return empty");

        pool.close().await;
    });
}
