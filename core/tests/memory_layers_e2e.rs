//! End-to-end smoke for [`kastellan_core::memory::layers::load_l1`] —
//! the storage-level L1 insight-index reader.
//!
//! Each test brings up its own per-test Postgres cluster (same
//! recipe `memory_recall_e2e.rs` uses) so the rows seeded here cannot
//! drift between scenarios. The L1 contract under test:
//!
//!   1. Empty L1 returns `Ok(vec![])` — explicitly not an error.
//!   2. `load_l1` returns only L1 rows, newest-first, ignoring rows at
//!      every other layer.
//!   3. The `cap_rows` knob hard-caps the row count.
//!   4. The `cap_bytes` knob hard-caps the cumulative body length; an
//!      over-budget single row is dropped (with a `tracing::warn!`).
//!   5. `load_l1_default` is observationally equivalent to `load_l1`
//!      with the published default caps — the convenience wrapper
//!      cannot silently empty the L1 block via a fat-fingered `0`.
//!
//! Skips silently with `[SKIP]` lines on hosts without Postgres or a
//! reachable supervisor; `cargo test -- --nocapture` to see them.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use kastellan_core::memory::layers::{
    load_l1, load_l1_default, L1_DEFAULT_CAP_BYTES, L1_DEFAULT_CAP_ROWS,
};
use kastellan_db::memories::{insert_memory_at_layer, seed_meta_memory, MemoryLayer};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};

/// Single-runtime helper — every scenario uses the same shape.
fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime")
}

#[test]
fn load_l1_empty_returns_empty_vec() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l1e-d",
        "l1e-l",
        &format!("kastellan-supervisor-test-pg-l1empty-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "load-l1-empty"}),
        )
        .await
        .expect("probe");

        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let l1 = load_l1(&pool, L1_DEFAULT_CAP_ROWS, L1_DEFAULT_CAP_BYTES)
            .await
            .expect("load_l1 must succeed on empty corpus");
        assert!(
            l1.is_empty(),
            "empty corpus must return empty Vec, not an error; got {} rows",
            l1.len()
        );

        pool.close().await;
    });
}

#[test]
fn load_l1_returns_only_l1_rows_newest_first() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l1o-d",
        "l1o-l",
        &format!("kastellan-supervisor-test-pg-l1only-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "load-l1-only-l1"}),
        )
        .await
        .expect("probe");

        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // One row per layer. Order of insertion does not affect L1's
        // newest-first ordering when there's exactly one L1 row, but
        // it does pin the cross-layer no-leakage contract: even
        // though L0/L2/L3/L4 rows are written *after* the L1 row,
        // load_l1 still returns only the L1 row. L0 goes through
        // `seed_meta_memory` (insert_memory_at_layer rejects L0 by
        // contract — see `seed_meta_memory` doc).
        let l1_body = "l1 routing pointer to skill X";
        let _ = insert_memory_at_layer(&pool, l1_body, &serde_json::json!({}), None, MemoryLayer::Index)
            .await
            .expect("insert L1");
        seed_meta_memory(&pool, "meta rule", &serde_json::json!({}), None)
            .await
            .expect("seed L0");
        for (layer, body) in [
            (MemoryLayer::Stable, "stable fact"),
            (MemoryLayer::Skill, "skill template"),
            (MemoryLayer::Digest, "session digest"),
        ] {
            insert_memory_at_layer(&pool, body, &serde_json::json!({}), None, layer)
                .await
                .expect("insert non-L1");
        }

        let l1 = load_l1(&pool, L1_DEFAULT_CAP_ROWS, L1_DEFAULT_CAP_BYTES)
            .await
            .expect("load_l1");
        assert_eq!(l1.len(), 1, "exactly one L1 row inserted; got {}", l1.len());
        assert_eq!(l1[0].body, l1_body, "load_l1 must return the L1 row, not a foreign-layer body");
        assert_eq!(l1[0].layer, MemoryLayer::Index, "row.layer must be Index");

        pool.close().await;
    });
}

#[test]
fn load_l1_respects_row_cap() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l1r-d",
        "l1r-l",
        &format!("kastellan-supervisor-test-pg-l1row-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "load-l1-row-cap"}),
        )
        .await
        .expect("probe");

        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // Five L1 rows. created_at is set by DEFAULT now() at INSERT
        // time, so inserting sequentially guarantees the later inserts
        // sort newer-first.
        for i in 0..5 {
            insert_memory_at_layer(
                &pool,
                &format!("l1 row #{i}"),
                &serde_json::json!({}),
                None,
                MemoryLayer::Index,
            )
            .await
            .expect("insert L1");
        }

        let l1 = load_l1(&pool, 3, L1_DEFAULT_CAP_BYTES)
            .await
            .expect("load_l1 with cap_rows=3");
        assert_eq!(l1.len(), 3, "cap_rows=3 must hard-cap to 3 rows; got {}", l1.len());

        // newest-first: the rows we get back must be #4, #3, #2 in
        // that order (since #0..#4 were inserted in monotonic
        // created_at order).
        assert_eq!(l1[0].body, "l1 row #4", "row 0 must be newest (#4)");
        assert_eq!(l1[1].body, "l1 row #3", "row 1 must be #3");
        assert_eq!(l1[2].body, "l1 row #2", "row 2 must be #2");

        pool.close().await;
    });
}

#[test]
fn load_l1_respects_byte_cap() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l1b-d",
        "l1b-l",
        &format!("kastellan-supervisor-test-pg-l1byte-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "load-l1-byte-cap"}),
        )
        .await
        .expect("probe");

        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // Three rows of ~2 KiB each. The default cap_rows is far above
        // 3 — what we're testing is the byte cap.
        let body_2k = "X".repeat(2048);
        for _ in 0..3 {
            insert_memory_at_layer(
                &pool,
                &body_2k,
                &serde_json::json!({}),
                None,
                MemoryLayer::Index,
            )
            .await
            .expect("insert 2k L1 row");
        }

        // At 4 KiB total budget: row #1 (2048) fits → 2048 used;
        // row #2 would push to 4096 — *not over* the 4096 cap by the
        // strict `>` check — so it fits. Row #3 would push to 6144,
        // over cap → break. Expected: 2 rows.
        let l1 = load_l1(&pool, 32, 4096).await.expect("load_l1 with cap_bytes=4096");
        assert_eq!(
            l1.len(),
            2,
            "two 2-KiB rows fit under a 4-KiB cap; the third must be dropped. got {} rows",
            l1.len()
        );

        // At 100-byte budget: the first row alone (2048 > 100) so the
        // byte loop breaks before pushing it; an over-budget *single*
        // row also emits `tracing::warn!` (not asserted here — the
        // warn-on-drop branch is exercised, the side effect is for
        // operator logs). Expected: 0 rows.
        let l1 = load_l1(&pool, 32, 100).await.expect("load_l1 with cap_bytes=100");
        assert_eq!(
            l1.len(),
            0,
            "no L1 row fits under a 100-byte cap; got {} rows",
            l1.len()
        );

        pool.close().await;
    });
}

#[test]
fn load_l1_default_matches_explicit_default_caps() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l1d-d",
        "l1d-l",
        &format!("kastellan-supervisor-test-pg-l1default-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "load-l1-default"}),
        )
        .await
        .expect("probe");

        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // Three small L1 rows so neither cap can plausibly intervene
        // — the parity claim is observable only when the budget is not
        // the limiting factor in either call.
        for i in 0..3 {
            insert_memory_at_layer(
                &pool,
                &format!("l1 row #{i}"),
                &serde_json::json!({}),
                None,
                MemoryLayer::Index,
            )
            .await
            .expect("insert L1");
        }

        let via_default = load_l1_default(&pool).await.expect("load_l1_default");
        let via_explicit = load_l1(&pool, L1_DEFAULT_CAP_ROWS, L1_DEFAULT_CAP_BYTES)
            .await
            .expect("load_l1 with explicit defaults");

        assert_eq!(
            via_default.len(),
            via_explicit.len(),
            "row count must match between load_l1_default and explicit-default call"
        );
        for (a, b) in via_default.iter().zip(via_explicit.iter()) {
            assert_eq!(a.id, b.id, "id must match for the same prefix slot");
            assert_eq!(a.body, b.body, "body must match for the same prefix slot");
            assert_eq!(a.layer, b.layer, "layer must match for the same prefix slot");
        }

        pool.close().await;
    });
}
