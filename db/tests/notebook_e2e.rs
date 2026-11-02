//! PG-gated e2e for the task notebook (#922, migration 0028). Skip-as-pass
//! without a supervisor/PG; live on the DGX, and on a Mac that exports
//! `KASTELLAN_PG_BIN_DIR`.

use kastellan_db::notebook::{append, entries, EntryKind, NOTIFY_CHANNEL};
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
            json!({"version": "test", "purpose": "notebook-e2e"}),
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
    insert_pending(pool, Lane::Fast, json!({"instruction": "notebook probe"})).await.expect("insert task")
}

#[test]
fn append_numbers_entries_from_one_without_gaps() {
    let Some(h) = harness("nb-seq") else { return };
    h.rt.block_on(async {
        let pool = h.migrated_pool().await;
        let task = a_task(&pool).await;
        let other = a_task(&pool).await;
        assert_eq!(append(&pool, task, EntryKind::Steps, json!({"n": 1})).await.unwrap(), 1);
        assert_eq!(append(&pool, other, EntryKind::Steps, json!({})).await.unwrap(), 1, "seq is per task");
        assert_eq!(append(&pool, task, EntryKind::Tick, json!({"n": 2})).await.unwrap(), 2);
        assert_eq!(append(&pool, task, EntryKind::Answer, json!({"n": 3})).await.unwrap(), 3);

        let all = entries(&pool, task, 0).await.unwrap();
        let seen: Vec<(i64, EntryKind, i64)> =
            all.iter().map(|e| (e.seq, e.kind, e.payload["n"].as_i64().unwrap())).collect();
        assert_eq!(seen, vec![(1, EntryKind::Steps, 1), (2, EntryKind::Tick, 2), (3, EntryKind::Answer, 3)]);
        assert!(all.iter().all(|e| e.task_id == task));

        let tail = entries(&pool, task, 2).await.unwrap();
        assert_eq!(tail.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![3]);
    });
}

/// The loop and a correction write to one task at once: no two entries may
/// share a seq, and none may be skipped.
#[test]
fn concurrent_appends_never_share_a_seq() {
    let Some(h) = harness("nb-race") else { return };
    h.rt.block_on(async {
        let pool = h.migrated_pool().await;
        let task = a_task(&pool).await;
        let writer = |kind| {
            let pool = pool.clone();
            async move {
                let mut seqs = Vec::new();
                for i in 0..20 {
                    seqs.push(append(&pool, task, kind, json!({"i": i})).await.expect("append"));
                }
                seqs
            }
        };
        let (a, b) = tokio::join!(
            tokio::spawn(writer(EntryKind::Tick)),
            tokio::spawn(writer(EntryKind::Correction))
        );
        let mut all: Vec<i64> = a.unwrap().into_iter().chain(b.unwrap()).collect();
        all.sort();
        assert_eq!(all, (1..=40).collect::<Vec<i64>>());
    });
}

#[test]
fn the_runtime_role_cannot_rewrite_an_entry() {
    let Some(h) = harness("nb-role") else { return };
    h.rt.block_on(async {
        let pool = h.migrated_pool().await;
        let task = a_task(&pool).await;
        append(&pool, task, EntryKind::Steps, json!({})).await.unwrap();

        let mut held = pool.acquire().await.expect("acquire");
        sqlx::query(sqlx::AssertSqlSafe(kastellan_db::conn::set_role_runtime_statement()))
            .execute(&mut *held)
            .await
            .expect("SET ROLE kastellan_runtime");
        // Positive control: the role can add and read.
        sqlx::query("INSERT INTO task_notebook (task_id, seq, kind, payload) VALUES ($1, 2, 'tick', '{}')")
            .bind(task)
            .execute(&mut *held)
            .await
            .expect("the runtime role can append");
        sqlx::query("SELECT count(*) FROM task_notebook").execute(&mut *held).await.expect("and read");

        for stmt in [
            "UPDATE task_notebook SET payload = '{\"x\":1}' WHERE task_id = $1",
            "DELETE FROM task_notebook WHERE task_id = $1",
        ] {
            let err = sqlx::query(sqlx::AssertSqlSafe(stmt))
                .bind(task)
                .execute(&mut *held)
                .await
                .expect_err("the runtime role must not rewrite the notebook");
            let msg = err.to_string().to_lowercase();
            assert!(msg.contains("permission denied"), "{stmt}: {msg}");
        }
    });
}

#[test]
fn an_append_notifies_task_id_and_seq() {
    let Some(h) = harness("nb-notify") else { return };
    h.rt.block_on(async {
        let pool = h.migrated_pool().await;
        let task = a_task(&pool).await;
        let mut listener = sqlx::postgres::PgListener::connect_with(&pool).await.expect("listener");
        listener.listen(NOTIFY_CHANNEL).await.expect("LISTEN");
        append(&pool, task, EntryKind::Steps, json!({})).await.unwrap();
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), listener.recv())
            .await
            .expect("a NOTIFY within 5 s")
            .expect("recv");
        assert_eq!(got.payload(), format!("{task}:1"));
    });
}

/// A NUL in a payload is escaped, not refused: losing the row would lose a
/// step the task already took.
#[test]
fn a_nul_in_a_payload_is_escaped_not_refused() {
    let Some(h) = harness("nb-nul") else { return };
    h.rt.block_on(async {
        let pool = h.migrated_pool().await;
        let task = a_task(&pool).await;
        append(&pool, task, EntryKind::Tick, json!({"note": "a\u{0}b"})).await.expect("append lands");
        let e = entries(&pool, task, 0).await.unwrap();
        assert_eq!(e[0].payload["note"], json!("a\u{2400}b"));
    });
}
