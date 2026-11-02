//! PG-gated e2e for the scrapbook (#922 `sift`, migration 0029). Skip-as-pass
//! without a supervisor/PG; live on the DGX, and on a Mac that exports
//! `KASTELLAN_PG_BIN_DIR`.

use kastellan_db::scrapbook::{append_rows, rows, NewRow, RowKind};
use kastellan_db::tasks::{insert_pending, Lane};
use kastellan_tests_common::{bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix, PgCluster};
use serde_json::json;

/// A cluster plus a runtime. Pools are built inside `rt.block_on` so they
/// drop on the runtime, before the cluster (see `asks_e2e.rs`'s `Harness`).
struct Harness {
    cluster: PgCluster,
    rt: tokio::runtime::Runtime,
}

impl Harness {
    async fn migrated_pool(&self) -> sqlx::PgPool {
        kastellan_db::probe::run(
            &self.cluster.conn_spec,
            "core",
            "startup",
            json!({"version": "test", "purpose": "scrapbook-e2e"}),
        )
        .await
        .expect("probe run");
        kastellan_db::pool::connect_admin_pool(&self.cluster.conn_spec).await.expect("admin pool")
    }
}

fn harness(tag: &str) -> Option<Harness> {
    if skip_if_no_supervisor() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        &format!("{tag}-d"),
        &format!("{tag}-l"),
        &format!("kastellan-supervisor-test-pg-{tag}-{suffix}"),
    );
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    Some(Harness { cluster, rt })
}

async fn a_task(pool: &sqlx::PgPool) -> i64 {
    insert_pending(pool, Lane::Fast, json!({"instruction": "scrapbook probe"})).await.expect("insert task")
}

fn kept(n: i64) -> NewRow {
    NewRow {
        kind: RowKind::Kept,
        source: json!({"message_id": format!("m{n}")}),
        fields: json!({"amount": n}),
        classification: "Personal".into(),
    }
}

/// Rows come back in `row` order, numbered from 1, with every column as
/// written; another sift of the same task and an unknown sift are separate.
#[test]
fn append_then_read_back_in_row_order() {
    let Some(h) = harness("sb-rows") else { return };
    h.rt.block_on(async {
        let pool = h.migrated_pool().await;
        let task = a_task(&pool).await;
        let withheld = NewRow {
            kind: RowKind::Withheld,
            source: json!({"message_id": "m9"}),
            fields: json!({}),
            classification: "Public".into(),
        };
        append_rows(&pool, task, 3, &[kept(1), withheld, kept(2)]).await.unwrap();
        append_rows(&pool, task, 4, &[kept(7)]).await.unwrap();

        let got = rows(&pool, task, 3).await.unwrap();
        let seen: Vec<(i32, RowKind, serde_json::Value)> = got.iter().map(|r| (r.row, r.kind, r.fields.clone())).collect();
        assert_eq!(
            seen,
            vec![
                (1, RowKind::Kept, json!({"amount": 1})),
                (2, RowKind::Withheld, json!({})),
                (3, RowKind::Kept, json!({"amount": 2})),
            ]
        );
        assert!(got.iter().all(|r| r.task_id == task && r.sift == 3));
        assert_eq!(got[0].source, json!({"message_id": "m1"}));
        assert_eq!(got[0].classification, "Personal");
        assert_eq!(rows(&pool, task, 4).await.unwrap().len(), 1, "a second sift keeps its own rows");
        assert!(rows(&pool, task, 99).await.unwrap().is_empty(), "an unknown sift has no rows");
    });
}

/// A sift is written once: a second append to the same `(task, sift)` would
/// collide on row 1 and is refused whole, never merged.
#[test]
fn a_second_append_to_the_same_sift_is_refused() {
    let Some(h) = harness("sb-dup") else { return };
    h.rt.block_on(async {
        let pool = h.migrated_pool().await;
        let task = a_task(&pool).await;
        append_rows(&pool, task, 1, &[kept(1)]).await.unwrap();
        assert!(append_rows(&pool, task, 1, &[kept(2), kept(3)]).await.is_err());
        assert_eq!(rows(&pool, task, 1).await.unwrap().len(), 1, "the refused append wrote nothing");
    });
}

#[test]
fn the_runtime_role_cannot_rewrite_a_row() {
    let Some(h) = harness("sb-role") else { return };
    h.rt.block_on(async {
        let pool = h.migrated_pool().await;
        let task = a_task(&pool).await;
        append_rows(&pool, task, 1, &[kept(1)]).await.unwrap();

        let mut held = pool.acquire().await.expect("acquire");
        sqlx::query(sqlx::AssertSqlSafe(kastellan_db::conn::set_role_runtime_statement()))
            .execute(&mut *held)
            .await
            .expect("SET ROLE kastellan_runtime");
        // Positive control: the role can add and read.
        sqlx::query(
            "INSERT INTO task_scrapbook (task_id, sift, row, kind, source, fields, classification) \
             VALUES ($1, 2, 1, 'kept', '{}', '{}', 'Public')",
        )
        .bind(task)
        .execute(&mut *held)
        .await
        .expect("the runtime role can append");
        sqlx::query("SELECT count(*) FROM task_scrapbook").execute(&mut *held).await.expect("and read");

        for stmt in [
            "UPDATE task_scrapbook SET fields = '{\"x\":1}' WHERE task_id = $1",
            "DELETE FROM task_scrapbook WHERE task_id = $1",
        ] {
            let err = sqlx::query(sqlx::AssertSqlSafe(stmt))
                .bind(task)
                .execute(&mut *held)
                .await
                .expect_err("the runtime role must not rewrite the scrapbook");
            let msg = err.to_string().to_lowercase();
            assert!(msg.contains("permission denied"), "{stmt}: {msg}");
        }
    });
}
