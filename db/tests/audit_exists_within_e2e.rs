//! PG-gated e2e for `kastellan_db::audit::exists_within` (issue #906).
//!
//! The guard tier's boot refusal is thinned across daemon restarts by this
//! lookup alone, so each way it can say "no" is pinned here against real
//! Postgres: an old row, another payload, another action, another actor.
//! An old row is written with an explicit `ts` (the runtime role may INSERT
//! any column; it is UPDATE/DELETE that are revoked).
//!
//! Skip-as-pass without a supervisor/PG; demanded by the `pg` profile of
//! `scripts/run-e2e-gate.sh`.

use std::time::Duration;

use kastellan_db::audit;
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};
use serde_json::json;

const HOUR: Duration = Duration::from_secs(60 * 60);

#[test]
fn exists_within_sees_only_a_recent_matching_row() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "exw-d",
        "exw-l",
        &format!("kastellan-supervisor-test-pg-exw-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    rt.block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            json!({"version": "test", "purpose": "audit-exists-within-e2e"}),
        )
        .await
        .expect("probe run");
        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("runtime pool");

        let tau = json!({"refusal": "tau", "error_kind": null});
        let context = json!({"refusal": "context", "error_kind": null});

        // Nothing written yet.
        assert!(!audit::exists_within(&pool, "policy", "t.refused", &tau, HOUR).await.unwrap());

        // A row two hours old does not count against a one-hour window.
        sqlx::query(
            "INSERT INTO audit_log (ts, actor, action, payload) \
             VALUES (now() - interval '2 hours', 'policy', 't.refused', $1)",
        )
        .bind(&tau)
        .execute(&pool)
        .await
        .expect("insert old row");
        assert!(
            !audit::exists_within(&pool, "policy", "t.refused", &tau, HOUR).await.unwrap(),
            "a row older than the window must not count"
        );

        // A fresh row does.
        audit::insert(&pool, "policy", "t.refused", tau.clone()).await.expect("insert");
        assert!(audit::exists_within(&pool, "policy", "t.refused", &tau, HOUR).await.unwrap());

        // ...but only for its own payload, action and actor.
        assert!(
            !audit::exists_within(&pool, "policy", "t.refused", &context, HOUR).await.unwrap(),
            "another refusal kind is another row"
        );
        assert!(!audit::exists_within(&pool, "policy", "t.other", &tau, HOUR).await.unwrap());
        assert!(!audit::exists_within(&pool, "core", "t.refused", &tau, HOUR).await.unwrap());
    });
}
