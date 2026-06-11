//! End-to-end smoke for [`kastellan_core::prompt_assembly::PgSystemPromptBuilder`].
//!
//! Each scenario brings up its own per-test Postgres cluster (same
//! recipe as `memory_l0_seed_e2e.rs` and `memory_layers_e2e.rs`) so
//! seeded rows cannot drift between scenarios.
//!
//! Skips silently with `[SKIP]` lines on hosts without Postgres or a
//! reachable supervisor; `cargo test -- --nocapture` to see them.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use kastellan_core::prompt_assembly::{PgSystemPromptBuilder, SystemPromptBuilder};
use kastellan_db::memories::{insert_memory_at_layer, seed_meta_memory, MemoryLayer};
use kastellan_tests_common::{
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
        &format!("kastellan-supervisor-test-pg-pa-seeded-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "prompt-assembly-seeded"}),
        )
        .await
        .expect("probe");

        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
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
        &format!("kastellan-supervisor-test-pg-pa-empty-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "prompt-assembly-empty"}),
        )
        .await
        .expect("probe");

        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let builder = PgSystemPromptBuilder::new(pool.clone());
        let result = builder.build("BASE BODY").await.expect("build");

        assert_eq!(result.l0_count, 0, "no rows seeded: {result:?}");
        assert_eq!(result.l1_count, 0, "no rows seeded: {result:?}");
        assert_eq!(result.recalled_count, 0,
                   "build() with no recall context defaults to recalled_count = 0; got: {result:?}");
        // The `<handoff>` block is always present (PR #200 — planner
        // fetch_handoff surfacing), so "empty DB" no longer means a bare
        // `<base>`; it means no *memory-derived* blocks precede `<base>`.
        // Assert that structurally rather than byte-pinning the handoff text:
        // the unit tests in `assemble/tests.rs` already byte-pin it against the
        // source-of-truth `render_handoff_block()` helper, which is crate-private
        // and so not reachable from this integration test.
        assert!(
            result.system_prompt.starts_with("<handoff>\n"),
            "the always-present <handoff> block must lead the prompt; got: {result:?}"
        );
        assert!(
            result.system_prompt.ends_with("<base>\nBASE BODY\n</base>\n"),
            "the <base> block must be terminal; got: {result:?}"
        );
        for absent in ["<l0_meta_rules>", "<l1_insights>", "<skills>", "<recalled>"] {
            assert!(
                !result.system_prompt.contains(absent),
                "empty-DB build must contain no memory-derived blocks, found {absent}; got: {result:?}"
            );
        }

        pool.close().await;
    });
}

#[test]
fn pg_builder_with_recalled_renders_block_against_seeded_db() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "par-d",
        "par-l",
        &format!("kastellan-supervisor-test-pg-par-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "prompt-assembly-with-recalled"}),
        )
        .await
        .expect("probe");

        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // Empty DB → no L0/L1 sections; recalled context supplied
        // directly so we exercise the <recalled> rendering without
        // going through the real recall lane.
        let recalled = kastellan_core::recall_assembly::RecalledContext::new(
            vec![10, 20],
            vec!["RECALL ALPHA".into(), "RECALL BETA".into()],
            "a".repeat(64),
        );

        let builder = PgSystemPromptBuilder::new(pool.clone());
        let result = builder.build_with_recalled("BASE BODY", &recalled)
            .await
            .expect("build_with_recalled");

        assert_eq!(result.l0_count, 0);
        assert_eq!(result.l1_count, 0);
        assert_eq!(result.recalled_count, 2);
        let s = &result.system_prompt;
        assert!(s.contains("<recalled>\n- RECALL ALPHA\n- RECALL BETA\n</recalled>"),
                "recalled block missing/wrong shape; got:\n{s}");
        assert!(s.contains("<base>\nBASE BODY\n</base>\n"),
                "base section missing; got:\n{s}");

        // Empty-recalled fallback: build() (the legacy 1-arg shim) must
        // produce identical output to build_with_recalled(base, &empty).
        let r_via_legacy = builder.build("BASE BODY").await.expect("legacy build");
        let r_via_explicit_empty = builder
            .build_with_recalled(
                "BASE BODY",
                &kastellan_core::recall_assembly::RecalledContext::empty(),
            )
            .await
            .expect("explicit empty build");
        assert_eq!(r_via_legacy.system_prompt, r_via_explicit_empty.system_prompt,
                   "legacy build() must produce byte-identical output to build_with_recalled(base, &empty)");

        pool.close().await;
    });
}
