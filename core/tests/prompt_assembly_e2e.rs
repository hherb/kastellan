//! End-to-end smoke for [`hhagent_core::prompt_assembly::PgSystemPromptBuilder`].
//!
//! Each scenario brings up its own per-test Postgres cluster (same
//! recipe as `memory_l0_seed_e2e.rs` and `memory_layers_e2e.rs`) so
//! seeded rows cannot drift between scenarios.
//!
//! Skips silently with `[SKIP]` lines on hosts without Postgres or a
//! reachable supervisor; `cargo test -- --nocapture` to see them.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use hhagent_core::prompt_assembly::{PgSystemPromptBuilder, SystemPromptBuilder};
use hhagent_db::memories::{insert_memory_at_layer, seed_meta_memory, MemoryLayer};
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

#[test]
fn pg_builder_build_against_seeded_db() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "pas-d",
        "pas-l",
        &format!("hhagent-supervisor-test-pg-pa-seeded-{suffix}"),
    );

    rt().block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "prompt-assembly-seeded"}),
        )
        .await
        .expect("probe");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // Seed 2 L0 rules — each metadata carries an `l0_rule_id` key
        // so `load_active_l0` (which filters on the key) returns them.
        for (rule_id, body) in [("never_rm_rf", "L0 RULE ONE"), ("refusal_terminal", "L0 RULE TWO")] {
            let meta = serde_json::json!({
                "l0_rule_id": rule_id,
                "body_sha256": format!("sha-{rule_id}"),
                "source_path": "test",
                "tags": ["test"],
            });
            seed_meta_memory(&pool, body, &meta, None)
                .await
                .expect("seed L0");
        }

        // Seed 1 L1 row using the non-policy-restricted writer.
        insert_memory_at_layer(
            &pool,
            "L1 INSIGHT ONE",
            &serde_json::json!({}),
            None,
            MemoryLayer::Index,
        )
        .await
        .expect("insert L1");

        let builder = PgSystemPromptBuilder::new(pool.clone());
        let result = builder.build("BASE BODY").await.expect("build");

        assert_eq!(result.l0_count, 2, "two L0 rows seeded: {result:?}");
        assert_eq!(result.l1_count, 1, "one L1 row seeded: {result:?}");
        assert_eq!(result.recalled_count, 0,
                   "build() with no recall context defaults to recalled_count = 0; got: {result:?}");
        let s = &result.system_prompt;
        assert!(s.starts_with("<l0_meta_rules>\n"),
                "L0 section first; got:\n{s}");
        assert!(s.contains("- L0 RULE ONE\n"), "L0 rule one missing in:\n{s}");
        assert!(s.contains("- L0 RULE TWO\n"), "L0 rule two missing in:\n{s}");
        assert!(s.contains("<l1_insights>\n- L1 INSIGHT ONE\n</l1_insights>"),
                "L1 section missing/wrong shape; got:\n{s}");
        assert!(s.contains("<base>\nBASE BODY\n</base>\n"),
                "base section missing; got:\n{s}");

        // Positional ordering: L0 before L1 before base. The pure
        // assembler's unit tests cover this, but the e2e test pins
        // the same contract end-to-end so a future PgSystemPromptBuilder
        // regression can't silently reorder sections.
        let l0_end = s.find("</l0_meta_rules>").expect("L0 close tag");
        let l1_start = s.find("<l1_insights>").expect("L1 open tag");
        let base_start = s.find("<base>").expect("base open tag");
        assert!(l0_end < l1_start, "L0 must precede L1; offsets {l0_end}/{l1_start}");
        assert!(l1_start < base_start, "L1 must precede base; offsets {l1_start}/{base_start}");

        pool.close().await;
    });
}

#[test]
fn pg_builder_build_with_empty_db_returns_base_only() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "pae-d",
        "pae-l",
        &format!("hhagent-supervisor-test-pg-pa-empty-{suffix}"),
    );

    rt().block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "prompt-assembly-empty"}),
        )
        .await
        .expect("probe");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let builder = PgSystemPromptBuilder::new(pool.clone());
        let result = builder.build("BASE BODY").await.expect("build");

        assert_eq!(result.l0_count, 0, "no rows seeded: {result:?}");
        assert_eq!(result.l1_count, 0, "no rows seeded: {result:?}");
        assert_eq!(result.recalled_count, 0,
                   "build() with no recall context defaults to recalled_count = 0; got: {result:?}");
        assert_eq!(
            result.system_prompt, "<base>\nBASE BODY\n</base>\n",
            "empty-DB build must return just the <base> block"
        );

        pool.close().await;
    });
}
