//! End-to-end smoke for the per-user Postgres bring-up + every
//! `hhagent-db` runtime module.
//!
//! Five tests share one PG cluster bring-up recipe (initdb → auto.conf
//! → supervisor install/start → wait Active + socket). The recipe
//! itself lives in [`hhagent_tests_common::bring_up_pg_cluster`]; this
//! file's tests pin downstream behaviour against fresh clusters.
//!
//! Skips silently with `[SKIP]` lines on hosts that can't run the test
//! (no Postgres install found, no reachable supervisor); run
//! `cargo test -- --nocapture` to see them.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::time::Duration;

use hhagent_supervisor::ServiceStatus;
use hhagent_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix, wait_for_status,
};

#[test]
fn postgres_install_start_select_one_uninstall() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "pg-d",
        "pg-l",
        &format!("hhagent-supervisor-test-pg-{suffix}"),
    );

    // SELECT 1 over the UDS. This is the proof that the whole stack
    // agrees: data dir, config overrides, peer auth, socket dir
    // permissions, supervisor lifecycle.
    let psql = bin_dir.join("psql");
    assert!(psql.exists(), "psql at {}", psql.display());
    let select_out = std::process::Command::new(&psql)
        .arg("-h")
        .arg(&cluster.socket_dir)
        .arg("-U")
        .arg(&cluster.conn_spec.user)
        .arg("-d")
        .arg("postgres")
        .arg("-At") // -A unaligned, -t tuples-only — output is just the value
        .arg("-c")
        .arg("SELECT 1")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        // Make sure no host PG config bleeds into the test.
        .env("PGPASSFILE", "/dev/null")
        .env("PGSERVICEFILE", "/dev/null")
        .env("PGSYSCONFDIR", "/dev/null")
        .output()
        .expect("spawn psql");
    assert!(
        select_out.status.success(),
        "psql SELECT 1 failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&select_out.stdout),
        String::from_utf8_lossy(&select_out.stderr),
    );
    let out = String::from_utf8_lossy(&select_out.stdout);
    assert_eq!(
        out.trim(),
        "1",
        "psql -At -c 'SELECT 1' should print exactly '1', got: {out}",
    );

    // ---------- stop / uninstall ----------
    // Explicit stop+uninstall (not just relying on the ServiceGuard's
    // Drop) so the test asserts the full lifecycle reaches `Inactive`
    // then `NotInstalled` before the function returns.
    cluster.sup.stop(&cluster.service_name).expect("stop postgres service");
    wait_for_status(
        cluster.sup.as_ref(),
        &cluster.service_name,
        |s| s == ServiceStatus::Inactive,
        Duration::from_secs(15),
    )
    .expect("postgres should reach Inactive within 15s of stop");

    cluster
        .sup
        .uninstall(&cluster.service_name)
        .expect("uninstall postgres service");
    assert_eq!(
        cluster.sup.status(&cluster.service_name).expect("status post-uninstall"),
        ServiceStatus::NotInstalled,
    );

    // PgCluster::Drop wipes the data + log temp dirs.
}

/// End-to-end smoke for the runtime probe and the `Graph` trait.
///
/// Pipeline (mirrors what `core/src/main.rs::bring_up_database` does
/// every time the daemon starts, plus a Graph round-trip):
///
///   1. Bring up a per-test PG cluster.
///   2. Run `db::probe::run` once. This exercises:
///        * The maintenance-DB connect.
///        * `CREATE DATABASE hhagent` (first-boot branch).
///        * Reconnect to `hhagent`.
///        * `MIGRATOR.run` — pulls in `0001_init.sql`.
///        * The `audit_log` insert.
///   3. Run `db::probe::run` a *second* time. The CREATE DATABASE
///      branch must short-circuit (the lookup already finds the row),
///      and migrations must be a no-op (sqlx's `_sqlx_migrations`
///      checksum check). A fresh `audit_log` row appears, proving
///      idempotency without rewriting state.
///   4. Connect with sqlx and exercise `PgGraph`:
///        * `upsert_entity` two nodes (kind=`person`).
///        * Re-`upsert_entity` the first node — id stays stable
///          (ON CONFLICT (kind,name) DO UPDATE).
///        * `upsert_relation` one edge.
///        * `get_entity` round-trip.
///        * `neighbors` returns the second node.
///        * `path` finds the 1-hop path.
///   5. Sanity-check the `audit_log` row count is exactly 2 (the
///      two probe runs above), so no spurious writes are happening
///      in either probe path or the graph round-trip.
#[test]
fn probe_runs_migrations_and_graph_happy_path() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "probe-d",
        "probe-l",
        &format!("hhagent-supervisor-test-pg-probe-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    rt.block_on(async {
        // First run — exercises the CREATE DATABASE + migrations branches.
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "run": 1}),
        )
        .await
        .expect("first probe run");

        // Second run — must be a no-op except for the audit row.
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "run": 2}),
        )
        .await
        .expect("second probe run (idempotency)");

        // ---------- Graph trait round-trip ----------
        use hhagent_db::graph::{Graph, PgGraph};
        let pool = sqlx::postgres::PgPool::connect_with(cluster.conn_spec.to_pg_connect_options())
            .await
            .expect("pool connect");
        let g = PgGraph::new(&pool);

        let alice = g
            .upsert_entity("person", "alice", &serde_json::json!({"role": "engineer"}))
            .await
            .expect("upsert alice");
        let bob = g
            .upsert_entity("person", "bob", &serde_json::json!({}))
            .await
            .expect("upsert bob");
        assert!(alice > 0 && bob > 0 && alice != bob);

        // Re-upsert alice — id must stay stable (ON CONFLICT key is
        // (kind,name); a regression that flipped to INSERT-only or
        // changed the key would change the id).
        let alice_again = g
            .upsert_entity("person", "alice", &serde_json::json!({"role": "tlm"}))
            .await
            .expect("upsert alice again");
        assert_eq!(alice, alice_again);

        let edge_id = g
            .upsert_relation(alice, bob, "knows", &serde_json::json!({}))
            .await
            .expect("upsert relation");
        assert!(edge_id > 0);

        let fetched = g.get_entity("person", "alice").await.expect("get alice");
        let fetched = fetched.expect("alice should exist");
        assert_eq!(fetched.id, alice);
        assert_eq!(fetched.kind, "person");
        assert_eq!(fetched.name, "alice");
        assert_eq!(fetched.attrs["role"], "tlm");

        let neighbors = g
            .neighbors(alice, Some("knows"), 100)
            .await
            .expect("neighbors");
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].id, bob);

        let neighbors_unfiltered = g
            .neighbors(alice, None, 100)
            .await
            .expect("neighbors unfiltered");
        assert_eq!(neighbors_unfiltered.len(), 1);
        assert_eq!(neighbors_unfiltered[0].id, bob);

        let path = g.path(alice, bob, 5).await.expect("path");
        let path = path.expect("path should exist");
        assert_eq!(path.len(), 2);
        assert_eq!(path[0].id, alice);
        assert_eq!(path[1].id, bob);

        let no_path = g.path(bob, alice, 5).await.expect("path bob->alice");
        assert!(no_path.is_none(), "path should not exist in reverse direction");

        let row: (i64,) = sqlx::query_as("SELECT count(*) FROM audit_log")
            .fetch_one(&pool)
            .await
            .expect("count audit_log");
        assert_eq!(row.0, 2, "expected exactly 2 audit_log rows (one per probe run)");

        pool.close().await;
    });
}

/// Verify the runtime-role split from migration `0002_runtime_role.sql`.
///
/// The migration creates `hhagent_runtime` (NOSUPERUSER, NOCREATEROLE,
/// NOCREATEDB, NOLOGIN, NOINHERIT), grants membership to the OS user,
/// grants `SELECT, INSERT` on `audit_log`, and explicitly REVOKEs
/// `UPDATE, DELETE, TRUNCATE` from it. After `db::probe::run` applies
/// the migration and switches into the runtime role for its own
/// `audit_log` insert, this test connects on a fresh connection,
/// `SET ROLE`s, and proves the contract:
///
///   * `audit_log` INSERT succeeds under the runtime role.
///   * `audit_log` UPDATE fails with `permission denied` (SQLSTATE 42501).
///   * `audit_log` DELETE fails with `permission denied`.
///   * `memories` full CRUD succeeds.
///   * The role exists with the expected `pg_roles` flags and the OS
///     user is recorded in `pg_auth_members` as a member.
///   * Final `audit_log` row count is exactly 2 (probe row + our test
///     INSERT) — no UPDATE silently rewrote the probe row, no DELETE
///     vanished it.
#[test]
fn runtime_role_audit_log_revoke_is_enforced() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "runrole-d",
        "runrole-l",
        &format!("hhagent-supervisor-test-pg-runtime-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    rt.block_on(async {
        // Probe applies migrations 0001 + 0002 and writes one audit row
        // already under SET ROLE. The role + grants now exist.
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "purpose": "runtime-role-revoke"}),
        )
        .await
        .expect("probe run");

        // Pool connects as the OS user (= cluster superuser). We then
        // SET ROLE on a single acquired connection so all subsequent
        // statements run as the runtime role for that connection only.
        let pool = sqlx::postgres::PgPool::connect_with(cluster.conn_spec.to_pg_connect_options())
            .await
            .expect("pool connect");

        // ---------- role shape pin ----------
        // The four boolean flags here pin the contract from
        // `0002_runtime_role.sql`'s CREATE ROLE statement. A regression
        // that flipped any of these (e.g. accidentally adding LOGIN)
        // would silently weaken the boundary; the test is louder than
        // a code-review catch.
        let row: (String, bool, bool, bool, bool, bool) = sqlx::query_as(
            "SELECT rolname, rolcanlogin, rolsuper, rolinherit, rolcreaterole, rolcreatedb \
             FROM pg_roles WHERE rolname = 'hhagent_runtime'",
        )
        .fetch_one(&pool)
        .await
        .expect("hhagent_runtime row in pg_roles");
        assert_eq!(row.0, "hhagent_runtime");
        assert!(!row.1, "hhagent_runtime must be NOLOGIN (rolcanlogin=false)");
        assert!(!row.2, "hhagent_runtime must be NOSUPERUSER (rolsuper=false)");
        assert!(!row.3, "hhagent_runtime must be NOINHERIT (rolinherit=false)");
        assert!(!row.4, "hhagent_runtime must be NOCREATEROLE (rolcreaterole=false)");
        assert!(!row.5, "hhagent_runtime must be NOCREATEDB (rolcreatedb=false)");

        // The OS user (cluster superuser) MUST be a member of the
        // runtime role — otherwise SET ROLE fails for the daemon. The
        // join walks the role-membership graph: r1 = role being granted
        // (hhagent_runtime), r2 = role receiving the grant (= current_user
        // in our setup, which is the OS user under peer auth).
        let (member_count,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM pg_auth_members am \
             JOIN pg_roles r1 ON am.roleid = r1.oid \
             JOIN pg_roles r2 ON am.member = r2.oid \
             WHERE r1.rolname = 'hhagent_runtime' \
               AND r2.rolname = current_user",
        )
        .fetch_one(&pool)
        .await
        .expect("pg_auth_members lookup");
        assert_eq!(
            member_count, 1,
            "OS user must be a member of hhagent_runtime so SET ROLE works"
        );

        // ---------- SET ROLE on a held connection ----------
        let mut held = pool.acquire().await.expect("acquire connection");
        sqlx::query(&hhagent_db::conn::set_role_runtime_statement())
            .execute(&mut *held)
            .await
            .expect("SET ROLE hhagent_runtime");

        // ---------- positive path: INSERT into audit_log ----------
        let inserted: (i64,) = sqlx::query_as(
            "INSERT INTO audit_log (actor, action, payload) \
             VALUES ($1, $2, $3) RETURNING id",
        )
        .bind("test")
        .bind("revoke-check")
        .bind(serde_json::json!({"phase": "positive"}))
        .fetch_one(&mut *held)
        .await
        .expect("INSERT audit_log under runtime role");
        let row_id = inserted.0;

        // ---------- negative path 1: UPDATE rejected ----------
        let upd_err = sqlx::query("UPDATE audit_log SET payload = $1 WHERE id = $2")
            .bind(serde_json::json!({"tampered": true}))
            .bind(row_id)
            .execute(&mut *held)
            .await
            .expect_err("UPDATE audit_log must be rejected under runtime role");
        let upd_msg = upd_err.to_string();
        assert!(
            upd_msg.contains("permission denied"),
            "expected 'permission denied' in error, got: {upd_msg}"
        );

        // ---------- negative path 2: DELETE rejected ----------
        let del_err = sqlx::query("DELETE FROM audit_log WHERE id = $1")
            .bind(row_id)
            .execute(&mut *held)
            .await
            .expect_err("DELETE audit_log must be rejected under runtime role");
        let del_msg = del_err.to_string();
        assert!(
            del_msg.contains("permission denied"),
            "expected 'permission denied' in error, got: {del_msg}"
        );

        // ---------- positive path: full CRUD on memories ----------
        let mem: (i64,) = sqlx::query_as(
            "INSERT INTO memories (body) VALUES ($1) RETURNING id",
        )
        .bind("hello")
        .fetch_one(&mut *held)
        .await
        .expect("INSERT memories under runtime role");
        let mem_id = mem.0;

        sqlx::query("UPDATE memories SET body = $1 WHERE id = $2")
            .bind("world")
            .bind(mem_id)
            .execute(&mut *held)
            .await
            .expect("UPDATE memories under runtime role");

        let body: (String,) = sqlx::query_as("SELECT body FROM memories WHERE id = $1")
            .bind(mem_id)
            .fetch_one(&mut *held)
            .await
            .expect("SELECT memories under runtime role");
        assert_eq!(body.0, "world");

        sqlx::query("DELETE FROM memories WHERE id = $1")
            .bind(mem_id)
            .execute(&mut *held)
            .await
            .expect("DELETE memories under runtime role");

        // ---------- final audit row count ----------
        drop(held);
        let (audit_count,): (i64,) = sqlx::query_as("SELECT count(*) FROM audit_log")
            .fetch_one(&pool)
            .await
            .expect("count audit_log");
        assert_eq!(
            audit_count, 2,
            "expected exactly 2 audit_log rows (probe row + test INSERT); \
             a different number means UPDATE/DELETE may have leaked through"
        );

        pool.close().await;
    });
}

/// Verify the runtime pool, the `audit::insert` helper, and the
/// `audit_log_inserted` NOTIFY trigger from migration `0003`.
///
/// What this proves end-to-end:
///   * `pool::connect_runtime_pool` opens a pool whose `after_connect`
///     hook runs `SET ROLE hhagent_runtime`. UPDATE/DELETE on
///     `audit_log` via the pool fail with `permission denied` —
///     proof that role drop actually happened.
///   * The 0003 trigger fires AFTER INSERT and emits a NOTIFY on
///     channel `audit_log_inserted` carrying the new row's `id`.
///   * `PgListener` on a separate dedicated connection receives the
///     NOTIFY within ≤ 2 s of the INSERT.
///   * `audit::fetch_by_id` round-trips the inserted row.
///   * `audit::truncate_payload` is wired into `audit::insert`: an
///     8 KiB payload is replaced with the `_truncated` envelope before
///     storage, and `fetch_by_id` returns the envelope (not the
///     original).
#[test]
fn audit_helpers_pool_and_notify_round_trip() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "audpool-d",
        "audpool-l",
        &format!("hhagent-supervisor-test-pg-audit-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    rt.block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "purpose": "audit-helpers"}),
        )
        .await
        .expect("probe run");

        // Pool with after_connect SET ROLE.
        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("connect runtime pool");

        // Negative-path proof that pool connections run as the runtime
        // role: UPDATE on `audit_log` must fail. Under the bootstrap
        // superuser this would succeed; the failure is what tells us
        // SET ROLE actually ran in `after_connect`.
        let upd_err = sqlx::query(
            "UPDATE audit_log SET payload = $1 \
             WHERE id = (SELECT min(id) FROM audit_log)",
        )
        .bind(serde_json::json!({"tampered": true}))
        .execute(&pool)
        .await
        .expect_err("UPDATE under runtime-role pool must be rejected");
        assert!(
            upd_err.to_string().contains("permission denied"),
            "expected 'permission denied' from runtime-role pool: {upd_err}"
        );

        // ---------- attach listener BEFORE the watched insert ----------
        let mut listener = sqlx::postgres::PgListener::connect_with(&pool)
            .await
            .expect("PgListener connect");
        listener
            .listen("audit_log_inserted")
            .await
            .expect("LISTEN audit_log_inserted");

        // ---------- write a row via audit::insert ----------
        let inserted_id = hhagent_db::audit::insert(
            &pool,
            "tool:test",
            "call",
            serde_json::json!({"req": {"argv": ["echo", "hi"]}, "ms": 7}),
        )
        .await
        .expect("audit::insert under runtime-role pool");
        assert!(inserted_id > 0);

        // ---------- listener receives the NOTIFY ----------
        let notif = tokio::time::timeout(Duration::from_secs(2), listener.recv())
            .await
            .expect("NOTIFY must arrive within 2 s of audit_log INSERT")
            .expect("recv() returned a notification, not an error");
        assert_eq!(notif.channel(), "audit_log_inserted");
        let payload_id: i64 = notif
            .payload()
            .parse()
            .expect("NOTIFY payload must be a parseable i64 row id");
        assert_eq!(
            payload_id, inserted_id,
            "NOTIFY payload must equal the inserted row's id"
        );

        // ---------- fetch_by_id round-trip ----------
        let row = hhagent_db::audit::fetch_by_id(&pool, inserted_id)
            .await
            .expect("fetch_by_id");
        assert_eq!(row.id, inserted_id);
        assert_eq!(row.actor, "tool:test");
        assert_eq!(row.action, "call");
        assert_eq!(
            row.payload,
            serde_json::json!({"req": {"argv": ["echo", "hi"]}, "ms": 7})
        );

        // ---------- truncation: an 8 KiB payload is replaced ----------
        let big = "x".repeat(8192);
        let truncated_id = hhagent_db::audit::insert(
            &pool,
            "tool:test",
            "call",
            serde_json::Value::String(big),
        )
        .await
        .expect("audit::insert with big payload");
        let truncated = hhagent_db::audit::fetch_by_id(&pool, truncated_id)
            .await
            .expect("fetch_by_id for truncated row");
        let env = truncated
            .payload
            .as_object()
            .expect("truncated payload must be an object");
        assert_eq!(env.get("_truncated"), Some(&serde_json::Value::Bool(true)));
        assert!(env.contains_key("sha256"));
        assert!(env.contains_key("len"));

        // Drop the listener before pool.close() — PgListener holds a
        // checked-out PoolConnection; pool.close() blocks until every
        // permit is released, so listeners still in scope at close-time
        // deadlock the test.
        drop(listener);
        pool.close().await;
    });
}

/// End-to-end lifecycle test for `db::tasks`.
///
/// Exercises the full `tasks` API against a per-test PG cluster with
/// all six migrations applied (0001–0006). The test runs under the
/// runtime role via `connect_runtime_pool` (same as production).
///
/// Scenarios covered:
///
///   1. `insert_pending` + `claim_one` round-trip with `tasks_inserted`
///      NOTIFY confirmation — proves the trigger fires and the lane
///      filter is respected.
///   2. `observe_state` + `finalize` with `tasks_completed` NOTIFY —
///      proves the completion trigger fires and `result` + `finished_at`
///      are persisted.
///   3. `mark_cancelled` + idempotency.
///   4. `sweep_crashed` + idempotency — a running task whose lease is
///      forcibly back-dated is picked up by the sweep; a second sweep
///      returns 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tasks_lifecycle_e2e() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "lc-d",
        "lc-l",
        &format!("hhagent-supervisor-test-pg-lc-{suffix}"),
    );

    // Probe applies migrations 0001–0006 (tasks table + triggers).
    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "tasks-lifecycle"}),
    )
    .await
    .expect("probe run");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("connect runtime pool");

    use hhagent_db::tasks::{
        claim_one, finalize, get, insert_pending, mark_cancelled, observe_state, sweep_crashed,
        Lane,
    };

    // ── 1. Subscribe to listeners BEFORE inserting (race-safe) ──────
    let mut inserted_listener = sqlx::postgres::PgListener::connect_with(&pool)
        .await
        .expect("PgListener connect for tasks_inserted");
    inserted_listener
        .listen("tasks_inserted")
        .await
        .expect("LISTEN tasks_inserted");

    let mut completed_listener = sqlx::postgres::PgListener::connect_with(&pool)
        .await
        .expect("PgListener connect for tasks_completed");
    completed_listener
        .listen("tasks_completed")
        .await
        .expect("LISTEN tasks_completed");

    // ── 2. insert_pending → claim_one round trip ─────────────────────
    let id = insert_pending(&pool, Lane::Fast, serde_json::json!({"instruction": "ping"}))
        .await
        .expect("insert_pending");

    let n = tokio::time::timeout(Duration::from_secs(2), inserted_listener.recv())
        .await
        .expect("tasks_inserted notify timeout")
        .expect("tasks_inserted recv error");
    assert_eq!(
        n.payload(),
        id.to_string(),
        "tasks_inserted payload must equal the new task id"
    );

    let claimed = claim_one(&pool, Lane::Fast, 60)
        .await
        .expect("claim_one")
        .expect("claim_one returned None");
    assert_eq!(claimed.id, id);
    assert_eq!(claimed.state, "running");
    assert!(claimed.started_at.is_some(), "started_at must be set after claim");
    assert!(
        claimed.lease_expires_at.is_some(),
        "lease_expires_at must be set after claim"
    );

    // ── 3. observe and finalize ──────────────────────────────────────
    assert_eq!(
        observe_state(&pool, id).await.expect("observe_state"),
        "running",
        "observe_state must see running after claim"
    );

    finalize(
        &pool,
        id,
        "completed",
        Some(serde_json::json!({"kind": "text", "body": "pong"})),
    )
    .await
    .expect("finalize");

    let n = tokio::time::timeout(Duration::from_secs(2), completed_listener.recv())
        .await
        .expect("tasks_completed notify timeout")
        .expect("tasks_completed recv error");
    assert_eq!(
        n.payload(),
        id.to_string(),
        "tasks_completed payload must equal the finalized task id"
    );

    let task = get(&pool, id)
        .await
        .expect("get")
        .expect("task not found after finalize");
    assert_eq!(task.state, "completed");
    assert_eq!(
        task.result,
        Some(serde_json::json!({"kind": "text", "body": "pong"})),
        "result must match what was passed to finalize"
    );
    assert!(task.finished_at.is_some(), "finished_at must be set after finalize");

    // ── 4. mark_cancelled on a separate row ──────────────────────────
    // Widened 2026-05-13 to return Option<Task> via RETURNING so producer-
    // side callers can build an audit-row payload without a follow-up
    // SELECT. Some(task) = a row was flipped to cancelled; None = the
    // row was already terminal or did not exist.
    let id2 = insert_pending(&pool, Lane::Long, serde_json::json!({"instruction": "x"}))
        .await
        .expect("insert_pending id2");
    let cancelled = mark_cancelled(&pool, id2).await.expect("mark_cancelled");
    let task2 = cancelled.expect("mark_cancelled must return Some(task) for a pending row");
    assert_eq!(task2.id, id2, "RETURNING shape pins row identity");
    assert_eq!(task2.state, "cancelled", "post-update state is 'cancelled'");
    assert_eq!(task2.lane, Lane::Long, "RETURNING shape pins lane round-trip");
    assert_eq!(task2.plan_count, 0, "fresh pending task has plan_count=0");
    assert!(task2.finished_at.is_some(), "finished_at set on cancel");
    assert_eq!(
        observe_state(&pool, id2).await.expect("observe_state id2"),
        "cancelled"
    );

    assert!(
        mark_cancelled(&pool, id2)
            .await
            .expect("mark_cancelled idempotent")
            .is_none(),
        "mark_cancelled on an already-cancelled row must return None"
    );

    // ── 5. sweep_crashed ─────────────────────────────────────────────
    let id3 = insert_pending(&pool, Lane::Fast, serde_json::json!({"instruction": "y"}))
        .await
        .expect("insert_pending id3");
    let _ = claim_one(&pool, Lane::Fast, 60)
        .await
        .expect("claim_one id3")
        .expect("claim_one returned None for id3");

    sqlx::query("UPDATE tasks SET lease_expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(id3)
        .execute(&pool)
        .await
        .expect("back-date lease_expires_at");

    let swept = sweep_crashed(&pool).await.expect("sweep_crashed");
    assert_eq!(
        swept.len(),
        1,
        "sweep_crashed must find exactly one expired lease"
    );
    // The returned row carries the full metadata the audit-emission layer
    // needs to construct a `scheduler/task.crashed` lifecycle row without
    // a second SELECT.
    assert_eq!(swept[0].id, id3, "swept row must carry the original task id");
    assert_eq!(swept[0].lane, Lane::Fast, "swept row must preserve lane");
    assert_eq!(
        swept[0].state, "crashed",
        "swept row must reflect the post-UPDATE state (RETURNING returns the new value)"
    );
    assert_eq!(swept[0].plan_count, 0, "freshly-claimed task has plan_count=0");
    assert!(
        swept[0].finished_at.is_some(),
        "RETURNING must include the now()-stamped finished_at the UPDATE set"
    );
    assert_eq!(
        observe_state(&pool, id3).await.expect("observe_state id3"),
        "crashed"
    );

    assert!(
        sweep_crashed(&pool)
            .await
            .expect("sweep_crashed idempotent")
            .is_empty(),
        "second sweep_crashed must find nothing"
    );

    drop(inserted_listener);
    drop(completed_listener);
    pool.close().await;
}

/// End-to-end happy path for `db::secrets`.
///
/// Asserts:
/// 1. put + get round-trip — plaintext byte-for-byte; AAD populated.
/// 2. list returns metadata only.
/// 3. UPSERT semantics — second put replaces ciphertext + nonce.
/// 4. delete is idempotent (returns false on absent rows).
/// 5. AAD-mismatch detection on a row-name swap.
/// 6. Ciphertext-tamper detection (GCM auth tag).
/// 7. 0004 CHECK rejects empty AAD at the DB layer.
#[test]
fn secrets_put_get_list_delete_round_trip() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "secrets-d",
        "secrets-l",
        &format!("hhagent-supervisor-test-pg-secrets-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    rt.block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "purpose": "secrets-e2e"}),
        )
        .await
        .expect("probe run");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("connect runtime pool");

        let key_provider = hhagent_db::secrets::MapKeyProvider::new(
            "test-key-id-v1",
            [0x42u8; hhagent_db::secrets::KEY_LEN],
        );

        // ---------- 1. put + get round-trip ----------
        let pt_a: &[u8] = b"super-secret-token-A";
        hhagent_db::secrets::put(&pool, &key_provider, "imap_password", pt_a, None)
            .await
            .expect("put initial secret");

        let recovered =
            hhagent_db::secrets::get(&pool, &key_provider, "imap_password", None)
                .await
                .expect("get round-trip");
        assert_eq!(&*recovered, pt_a, "round-trip plaintext mismatch");

        // ---------- 2. list returns metadata only ----------
        hhagent_db::secrets::put(
            &pool,
            &key_provider,
            "anthropic_api_key",
            b"ak-zzz",
            None,
        )
        .await
        .expect("put second secret");
        let listing = hhagent_db::secrets::list(&pool).await.expect("list");
        assert_eq!(listing.len(), 2);
        // ORDER BY name ASC: "anthropic_api_key" < "imap_password"
        assert_eq!(listing[0].name, "anthropic_api_key");
        assert_eq!(listing[1].name, "imap_password");
        assert_eq!(listing[0].key_id, "test-key-id-v1");
        assert_eq!(listing[1].key_id, "test-key-id-v1");

        // ---------- 3. UPSERT semantics ----------
        let pt_a2: &[u8] = b"super-secret-token-A-rotated";
        hhagent_db::secrets::put(&pool, &key_provider, "imap_password", pt_a2, None)
            .await
            .expect("upsert second time");
        let recovered2 =
            hhagent_db::secrets::get(&pool, &key_provider, "imap_password", None)
                .await
                .expect("get after upsert");
        assert_eq!(&*recovered2, pt_a2, "upsert did not replace plaintext");
        let listing_after = hhagent_db::secrets::list(&pool).await.unwrap();
        assert_eq!(listing_after.len(), 2, "upsert must not duplicate");

        // ---------- 4. delete ----------
        let removed = hhagent_db::secrets::delete(&pool, "imap_password")
            .await
            .expect("delete");
        assert!(removed, "delete reported no row removed");
        let removed_again = hhagent_db::secrets::delete(&pool, "imap_password")
            .await
            .expect("delete idempotent");
        assert!(
            !removed_again,
            "delete of absent row must return false (idempotent)"
        );
        let err = hhagent_db::secrets::get(&pool, &key_provider, "imap_password", None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, hhagent_db::secrets::SecretsError::NotFound(_)),
            "expected NotFound after delete: {err:?}"
        );

        // ---------- 5. AAD-mismatch detection ----------
        hhagent_db::secrets::put(
            &pool,
            &key_provider,
            "swap_target",
            b"original-plaintext",
            None,
        )
        .await
        .expect("put swap_target");
        sqlx::query("UPDATE secrets SET name = $1 WHERE name = $2")
            .bind("swap_target_renamed")
            .bind("swap_target")
            .execute(&pool)
            .await
            .expect("simulate row rename");
        let mismatch_err = hhagent_db::secrets::get(
            &pool,
            &key_provider,
            "swap_target_renamed",
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                mismatch_err,
                hhagent_db::secrets::SecretsError::AadMismatch
            ),
            "renamed row must surface AadMismatch, got: {mismatch_err:?}"
        );

        // ---------- 6. ciphertext-tamper detection ----------
        hhagent_db::secrets::put(
            &pool,
            &key_provider,
            "tamper_target",
            b"original-plaintext",
            None,
        )
        .await
        .expect("put tamper_target");
        sqlx::query(
            "UPDATE secrets \
             SET ciphertext = set_byte(ciphertext, 0, get_byte(ciphertext, 0) # 1) \
             WHERE name = $1",
        )
        .bind("tamper_target")
        .execute(&pool)
        .await
        .expect("flip ciphertext byte");
        let tamper_err = hhagent_db::secrets::get(
            &pool,
            &key_provider,
            "tamper_target",
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                tamper_err,
                hhagent_db::secrets::SecretsError::DecryptFailed
            ),
            "tampered ciphertext must surface DecryptFailed, got: {tamper_err:?}"
        );

        // ---------- 7. 0004 CHECK enforces non-empty AAD ----------
        let check_err = sqlx::query(
            "INSERT INTO secrets (name, ciphertext, nonce, aad, key_id) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind("empty_aad_should_fail")
        .bind(&[0u8; 16][..])
        .bind(&[0u8; 12][..])
        .bind(&[0u8; 0][..])
        .bind("k")
        .execute(&pool)
        .await
        .expect_err("INSERT with empty aad must be rejected by 0004 CHECK");
        let msg = check_err.to_string();
        assert!(
            msg.contains("secrets_aad_nonempty") || msg.contains("check constraint"),
            "expected 0004 CHECK constraint violation, got: {msg}"
        );

        pool.close().await;
    });
}

// ─── Graph lane: memory_entities + deleted_memories (0007 + 0008) ────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_entities_link_round_trip_and_idempotency() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "mel-d",
        "mel-l",
        &format!("hhagent-pg-mel-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "memory-entities-link"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("pool");

    use hhagent_db::graph::Graph;

    // Seed: 1 memory, 3 entities.
    let mem_id = hhagent_db::memories::insert_memory(
        &pool,
        "alpha body",
        &serde_json::json!({}),
        None,
    )
    .await
    .expect("insert memory");

    let graph = hhagent_db::graph::PgGraph::new(&pool);
    let e1 = graph
        .upsert_entity("person", "alice", &serde_json::json!({}))
        .await
        .expect("upsert e1");
    let e2 = graph
        .upsert_entity("person", "bob", &serde_json::json!({}))
        .await
        .expect("upsert e2");
    let e3 = graph
        .upsert_entity("object", "cat", &serde_json::json!({}))
        .await
        .expect("upsert e3");

    // First link: both new.
    let n = hhagent_db::memories::link_memory_to_entities(&pool, mem_id, &[e1, e2])
        .await
        .expect("link 1");
    assert_eq!(n, 2, "first link of 2 fresh entities must insert 2 rows");

    // Re-link same pair: idempotent.
    let n = hhagent_db::memories::link_memory_to_entities(&pool, mem_id, &[e1, e2])
        .await
        .expect("link 2");
    assert_eq!(n, 0, "re-link of existing pairs must insert 0 rows");

    // Mixed (one new, one dupe): only the new one counts.
    let n = hhagent_db::memories::link_memory_to_entities(&pool, mem_id, &[e1, e3])
        .await
        .expect("link 3");
    assert_eq!(n, 1, "mixed re-link + new must insert 1 row");

    // Final count via raw SQL — defends against the helper's return
    // value lying about idempotency.
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM memory_entities WHERE memory_id = $1",
    )
    .bind(mem_id)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(row.0, 3, "memory_entities must hold exactly 3 distinct rows");

    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_entities_cascade_on_entity_delete() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "mec-d",
        "mec-l",
        &format!("hhagent-pg-mec-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "memory-entities-cascade"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("pool");

    use hhagent_db::graph::Graph;

    let mem_id = hhagent_db::memories::insert_memory(
        &pool,
        "bravo body",
        &serde_json::json!({}),
        None,
    )
    .await
    .expect("insert memory");
    let graph = hhagent_db::graph::PgGraph::new(&pool);
    let e_id = graph
        .upsert_entity("person", "alice", &serde_json::json!({}))
        .await
        .expect("upsert");

    hhagent_db::memories::link_memory_to_entities(&pool, mem_id, &[e_id])
        .await
        .expect("link");

    // Deleting the entity cascades to memory_entities.
    sqlx::query("DELETE FROM entities WHERE id = $1")
        .bind(e_id)
        .execute(&pool)
        .await
        .expect("delete entity");

    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM memory_entities WHERE entity_id = $1",
    )
    .bind(e_id)
    .fetch_one(&pool)
    .await
    .expect("count links");
    assert_eq!(row.0, 0, "entity delete must cascade to memory_entities");

    // Memory itself is untouched (cascade flows downward only).
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM memories WHERE id = $1")
        .bind(mem_id)
        .fetch_one(&pool)
        .await
        .expect("count memory");
    assert_eq!(row.0, 1, "memory survives entity cascade");

    // And not in deleted_memories.
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM deleted_memories WHERE id = $1",
    )
    .bind(mem_id)
    .fetch_one(&pool)
    .await
    .expect("count deleted");
    assert_eq!(row.0, 0, "memory not deleted, so deleted_memories has no row");

    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_delete_writes_deleted_memories_row() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "mda-d",
        "mda-l",
        &format!("hhagent-pg-mda-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "memory-delete-audit"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("pool");

    // Build a memory with an embedding so we exercise the full row shape.
    // Deterministic seeded vector via tests-common.
    let emb = hhagent_tests_common::text_to_embedding("delete-audit-fixture");
    let metadata = serde_json::json!({"k": "v"});
    let mem_id = hhagent_db::memories::insert_memory(
        &pool,
        "audit body",
        &metadata,
        Some(&emb),
    )
    .await
    .expect("insert memory");

    let before: (time::OffsetDateTime,) =
        sqlx::query_as("SELECT created_at FROM memories WHERE id = $1")
            .bind(mem_id)
            .fetch_one(&pool)
            .await
            .expect("fetch created_at");
    let original_created_at = before.0;

    // Delete it.
    sqlx::query("DELETE FROM memories WHERE id = $1")
        .bind(mem_id)
        .execute(&pool)
        .await
        .expect("delete memory");

    // Audit row exists with matching shape.
    let row: (i64, String, serde_json::Value, time::OffsetDateTime, time::OffsetDateTime) =
        sqlx::query_as(
            "SELECT id, body, metadata, created_at, deleted_at \
             FROM deleted_memories WHERE id = $1",
        )
        .bind(mem_id)
        .fetch_one(&pool)
        .await
        .expect("fetch deleted");
    assert_eq!(row.0, mem_id);
    assert_eq!(row.1, "audit body");
    assert_eq!(row.2, metadata);
    assert_eq!(row.3, original_created_at, "created_at preserved verbatim");

    let now = time::OffsetDateTime::now_utc();
    let drift = (now - row.4).whole_seconds().abs();
    assert!(drift < 5, "deleted_at must be within 5s of now (drift = {drift}s)");

    // Verify the embedding column was copied by the trigger. We don't
    // decode the vector itself (no pgvector Rust crate dep — see
    // db/src/memories.rs module docs) but a NOT NULL check is enough
    // to confirm the trigger function included the column in its
    // INSERT (it would have been NULL by default if omitted).
    let embedding_present: (bool,) = sqlx::query_as(
        "SELECT (embedding IS NOT NULL) FROM deleted_memories WHERE id = $1",
    )
    .bind(mem_id)
    .fetch_one(&pool)
    .await
    .expect("fetch embedding presence");
    assert!(
        embedding_present.0,
        "trigger must have copied non-null embedding into deleted_memories"
    );

    // Positive INSERT path: runtime role can INSERT directly into
    // deleted_memories. The trigger above used this same GRANT
    // (SECURITY INVOKER → runs as runtime), so this both pins the
    // GRANT shape AND defends against a future migration regression
    // that revokes INSERT and silently breaks the trigger.
    let ins = sqlx::query(
        "INSERT INTO deleted_memories (id, body, metadata, created_at) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(mem_id + 1_000_000) // disjoint id so we don't collide with the trigger-inserted row
    .bind("direct-insert-fixture")
    .bind(serde_json::json!({}))
    .bind(original_created_at)
    .execute(&pool)
    .await;
    assert!(
        ins.is_ok(),
        "direct INSERT into deleted_memories as runtime role must succeed (GRANT shape): {ins:?}"
    );

    // Append-only invariant: runtime cannot UPDATE or DELETE deleted_memories.
    let upd = sqlx::query("UPDATE deleted_memories SET body = 'tampered' WHERE id = $1")
        .bind(mem_id)
        .execute(&pool)
        .await;
    assert!(upd.is_err(), "UPDATE on deleted_memories must be denied to runtime");

    let del = sqlx::query("DELETE FROM deleted_memories WHERE id = $1")
        .bind(mem_id)
        .execute(&pool)
        .await;
    assert!(del.is_err(), "DELETE on deleted_memories must be denied to runtime");

    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_allowlists_round_trip_and_grant_shape() {
    use hhagent_db::pool::connect_runtime_pool;
    use hhagent_db::probe::run as probe_run;
    use hhagent_db::tool_allowlists::{
        add, list_all, list_for_tool, list_for_tool_full, remove, AllowlistEntry,
        ToolAllowlistError,
    };
    use hhagent_tests_common::{bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix};

    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ta-d",
        "ta-l",
        &format!("hhagent-postgres-tool-allowlists-e2e-{suffix}"),
    );

    probe_run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "tool-allowlists-e2e"}),
    )
    .await
    .expect("probe run");
    let pool = connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    // (1) Idempotent add.
    let inserted = add(&pool, "shell-exec", "/usr/bin/echo", "test").await.unwrap();
    assert!(inserted, "first add must INSERT");
    let inserted2 = add(&pool, "shell-exec", "/usr/bin/echo", "test").await.unwrap();
    assert!(!inserted2, "duplicate add must be a no-op");

    // (2) list_for_tool returns one entry.
    let v = list_for_tool(&pool, "shell-exec").await.unwrap();
    assert_eq!(v, vec!["/usr/bin/echo".to_string()]);

    // (3) A second entry under the same tool.
    let inserted3 = add(&pool, "shell-exec", "/bin/sh", "test").await.unwrap();
    assert!(inserted3);
    let v2 = list_for_tool(&pool, "shell-exec").await.unwrap();
    assert_eq!(v2, vec!["/bin/sh".to_string(), "/usr/bin/echo".to_string()],
        "list_for_tool must order argv0 ascending");

    // (4) list_all surfaces metadata.
    let all: Vec<AllowlistEntry> = list_all(&pool).await.unwrap();
    assert_eq!(all.len(), 2);
    for row in &all {
        assert_eq!(row.tool, "shell-exec");
        assert_eq!(row.created_by, "test");
    }

    // (4b) list_for_tool_full returns the full row shape, server-side
    // filtered (`WHERE tool = $1`). Seed a row under a second tool so the
    // filter is non-trivial.
    add(&pool, "other-tool", "/usr/bin/true", "test").await.unwrap();
    let shell_rows = list_for_tool_full(&pool, "shell-exec").await.unwrap();
    assert_eq!(shell_rows.len(), 2, "must include both shell-exec rows");
    assert!(
        shell_rows.iter().all(|r| r.tool == "shell-exec"),
        "rows leaked from other tool: {shell_rows:?}",
    );
    let other_rows = list_for_tool_full(&pool, "other-tool").await.unwrap();
    assert_eq!(other_rows.len(), 1);
    assert_eq!(other_rows[0].argv0, "/usr/bin/true");
    assert_eq!(other_rows[0].created_by, "test");
    let missing = list_for_tool_full(&pool, "no-such-tool").await.unwrap();
    assert!(missing.is_empty(), "unknown tool must return no rows: {missing:?}");
    // Reject malformed tool names at the validator, same contract as
    // list_for_tool / add / remove.
    assert!(matches!(
        list_for_tool_full(&pool, "bad name").await,
        Err(ToolAllowlistError::InvalidToolName),
    ));
    // Drop the seeded row so the rest of the test sees the pre-existing
    // 2-row state.
    remove(&pool, "other-tool", "/usr/bin/true").await.unwrap();

    // (5) Idempotent remove.
    let removed = remove(&pool, "shell-exec", "/usr/bin/echo").await.unwrap();
    assert!(removed);
    let removed2 = remove(&pool, "shell-exec", "/usr/bin/echo").await.unwrap();
    assert!(!removed2, "second remove must be a no-op");

    // (6) GRANT shape: UPDATE on tool_allowlists denied to hhagent_runtime.
    // SET ROLE explicitly in the same transaction so the test isn't
    // sensitive to pool reuse.
    let mut conn = pool.acquire().await.unwrap();
    sqlx::query("SET ROLE hhagent_runtime")
        .execute(&mut *conn)
        .await
        .unwrap();
    let update_res = sqlx::query("UPDATE tool_allowlists SET argv0 = '/x' WHERE tool = 'shell-exec'")
        .execute(&mut *conn)
        .await;
    let err = update_res.expect_err("UPDATE on tool_allowlists must be denied");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("permission denied") || msg.contains("denied for table"),
        "unexpected error message: {msg}"
    );
    drop(conn);

    // (7) CHECK constraint: relative argv0 rejected by Postgres even
    // when the Rust validator is bypassed.
    let bad = sqlx::query("INSERT INTO tool_allowlists (tool, argv0, created_by) VALUES ('shell-exec', 'echo', 'test')")
        .execute(&pool)
        .await;
    let bad_err = bad.expect_err("relative argv0 must be CHECK-rejected");
    let bad_msg = bad_err.to_string().to_lowercase();
    assert!(
        bad_msg.contains("check") || bad_msg.contains("violates"),
        "unexpected error: {bad_msg}"
    );

    // (7b) CHECK constraint: `..` *segment* in argv0 rejected by Postgres
    // even when the Rust validator is bypassed. Closes the path-confusion
    // bypass at the SQL layer (defense-in-depth for direct DB writers).
    for dotdot in [
        "/usr/bin/../bin/echo",
        "/..",
        "/../bin/echo",
        "/usr/bin/echo/..",
    ] {
        let res = sqlx::query("INSERT INTO tool_allowlists (tool, argv0, created_by) VALUES ('shell-exec', $1, 'test')")
            .bind(dotdot)
            .execute(&pool)
            .await;
        let err = res.expect_err(
            "argv0 with `..` segment must be CHECK-rejected by Postgres",
        );
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("check") || msg.contains("violates"),
            "argv0 {dotdot:?} rejected for unexpected reason: {msg}"
        );
    }
    // Conversely, `..` *within* a segment must be accepted (the validator
    // explicitly permits filenames like `/usr/bin/foo..bar`, so the SQL
    // CHECK must not over-reject and break legitimate paths).
    sqlx::query("INSERT INTO tool_allowlists (tool, argv0, created_by) VALUES ('shell-exec-test-dotdot', '/usr/bin/foo..bar', 'test')")
        .execute(&pool)
        .await
        .expect("`..` within a segment must pass the CHECK");

    // (8) Validator gate: add() rejects a malformed argv0 before the DB
    // sees it. Confirms the public API uses the validator, not just the
    // SQL CHECK constraint.
    let bad_argv0 = add(&pool, "shell-exec", "echo", "test").await;
    assert!(matches!(bad_argv0, Err(ToolAllowlistError::InvalidArgv0)),
        "expected InvalidArgv0; got {bad_argv0:?}");
    let bad_tool = add(&pool, "shell exec", "/usr/bin/echo", "test").await;
    assert!(matches!(bad_tool, Err(ToolAllowlistError::InvalidToolName)),
        "expected InvalidToolName; got {bad_tool:?}");

    drop(pool);
    drop(cluster);
}

/// Pin that `tasks.state = 'refused'` passes the CHECK constraint added
/// by migration `0012_tasks_state_refused.sql`, and that invalid state
/// values are still rejected.
///
/// Scenarios:
///   1. Positive: UPDATE a row to `state = 'refused'` succeeds and the
///      value round-trips correctly.
///   2. Negative: UPDATE to `state = 'garbage'` is still rejected by
///      the widened `tasks_state_check` CHECK constraint.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tasks_state_refused_passes_check_constraint() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ref-d",
        "ref-l",
        &format!("hhagent-supervisor-test-pg-refused-{suffix}"),
    );

    // Probe applies all migrations (0001–0012) and sets up roles/grants.
    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "refused-state-check"}),
    )
    .await
    .expect("probe run");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("connect runtime pool");

    // Seed a pending task via raw SQL.
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO tasks (lane, state, payload) \
         VALUES ('fast', 'pending', '{}'::jsonb) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("seed pending task");

    // ── Positive: 'refused' accepted by the widened CHECK constraint ─────
    let ok = sqlx::query(
        "UPDATE tasks SET state = 'refused', finished_at = now() WHERE id = $1",
    )
    .bind(id)
    .execute(&pool)
    .await;
    assert!(
        ok.is_ok(),
        "UPDATE to state='refused' should succeed (migration 0012 widens the CHECK); got {ok:?}"
    );

    let final_state: String = sqlx::query_scalar("SELECT state::text FROM tasks WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("read back state");
    assert_eq!(final_state, "refused", "state must round-trip as 'refused'");

    // ── Negative: 'garbage' still rejected ───────────────────────────────
    let err = sqlx::query("UPDATE tasks SET state = 'garbage' WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await;
    assert!(
        err.is_err(),
        "UPDATE to state='garbage' must be rejected by tasks_state_check; got {err:?}"
    );

    pool.close().await;
}

/// Migration 0013 — every new memory row gets `layer = 2` (Stable) by
/// default. The plain `insert_memory` call site (which has no layer
/// argument) is the one production callers use today; the default flows
/// from the column-level `DEFAULT 2` in the migration, not from any
/// Rust default — so this test pins the DB-layer contract, not the
/// Rust-API contract.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memories_layer_default_is_stable() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ml-d",
        "ml-l",
        &format!("hhagent-pg-mlayer-default-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "memory-layer-default"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("pool");

    let mem_id = hhagent_db::memories::insert_memory(
        &pool,
        "default-layer body",
        &serde_json::json!({}),
        None,
    )
    .await
    .expect("insert memory");

    let layer: i16 = sqlx::query_scalar("SELECT layer FROM memories WHERE id = $1")
        .bind(mem_id)
        .fetch_one(&pool)
        .await
        .expect("fetch layer");
    assert_eq!(
        layer, 2,
        "fresh insert_memory must default to layer = 2 (Stable / L2)"
    );

    pool.close().await;
}

/// `insert_memory_at_layer` round-trips each non-L0 layer, and the L0
/// admin path `seed_meta_memory` round-trips the L0 case. `load_layer`
/// filters strictly by layer (no cross-layer leakage). The L0
/// rejection contract is asserted at the bottom of this test (kept in
/// one place to avoid spinning up a second PG cluster — the rejection
/// short-circuits before any SQL is issued).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn insert_memory_at_layer_round_trip() {
    use hhagent_db::memories::{
        insert_memory_at_layer, load_layer, seed_meta_memory, MemoryLayer,
    };

    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "mr-d",
        "mr-l",
        &format!("hhagent-pg-mlayer-round-trip-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "memory-layer-round-trip"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("pool");

    let l0_id = seed_meta_memory(&pool, "meta-l0", &serde_json::json!({}), None)
        .await
        .expect("seed L0");

    let non_l0 = [
        (MemoryLayer::Index, "index-l1"),
        (MemoryLayer::Stable, "stable-l2"),
        (MemoryLayer::Skill, "skill-l3"),
        (MemoryLayer::Digest, "digest-l4"),
    ];

    let mut inserted_ids: Vec<(MemoryLayer, i64, &str)> = Vec::with_capacity(5);
    inserted_ids.push((MemoryLayer::Meta, l0_id, "meta-l0"));
    for (layer, body) in non_l0.iter().copied() {
        let id = insert_memory_at_layer(&pool, body, &serde_json::json!({}), None, layer)
            .await
            .expect("insert at layer");
        inserted_ids.push((layer, id, body));
    }

    // load_layer(L1) returns exactly the L1 row.
    let l1 = load_layer(&pool, MemoryLayer::Index, 100)
        .await
        .expect("load_layer L1");
    assert_eq!(l1.len(), 1, "L1 must return exactly the one L1 row");
    assert_eq!(l1[0].body, "index-l1");

    // load_layer(L3) returns exactly the L3 row.
    let l3 = load_layer(&pool, MemoryLayer::Skill, 100)
        .await
        .expect("load_layer L3");
    assert_eq!(l3.len(), 1, "L3 must return exactly the one L3 row");
    assert_eq!(l3[0].body, "skill-l3");

    // No cross-layer leakage: each layer query returns its row only.
    for (layer, _id, body) in inserted_ids.iter().copied() {
        let rows = load_layer(&pool, layer, 100)
            .await
            .expect("load_layer for fixture");
        assert_eq!(
            rows.len(),
            1,
            "layer {layer:?} must return exactly its one fixture row"
        );
        assert_eq!(rows[0].body, body);
    }

    // Policy: insert_memory_at_layer must reject L0 (Meta) — the only
    // legitimate L0 writer is seed_meta_memory above. The rejection
    // happens before any SQL is issued, so we exercise it on the same
    // pool to avoid spinning up a separate cluster.
    let rejected = insert_memory_at_layer(
        &pool,
        "l0 via agent-loop path (forbidden)",
        &serde_json::json!({}),
        None,
        MemoryLayer::Meta,
    )
    .await;
    match rejected {
        Err(hhagent_db::DbError::PolicyViolation(msg)) => {
            assert!(
                msg.contains("L0") && msg.contains("seed_meta_memory"),
                "PolicyViolation message must name L0 and the correct admin path; got: {msg}"
            );
        }
        Err(other) => panic!("expected DbError::PolicyViolation, got {other:?}"),
        Ok(id) => panic!("L0 write via insert_memory_at_layer must be rejected; got id {id}"),
    }

    // The rejection must not have created any row in `memories`.
    let l0_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memories WHERE layer = 0")
        .fetch_one(&pool)
        .await
        .expect("count L0 rows");
    assert_eq!(
        l0_count, 1,
        "exactly one L0 row from seed_meta_memory; rejected insert must not have leaked into memories"
    );

    pool.close().await;
}

/// The deleted_memories AFTER DELETE trigger (migrations 0008 + 0014)
/// must carry the `layer` column into the audit row so post-deletion
/// forensics can tell whether a deleted row was a load-bearing L1
/// pointer or a routine L2 fact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memory_delete_preserves_layer_in_audit() {
    use hhagent_db::memories::{insert_memory_at_layer, MemoryLayer};

    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "md-d",
        "md-l",
        &format!("hhagent-pg-mlayer-delete-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "memory-layer-delete-audit"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("pool");

    let mem_id = insert_memory_at_layer(
        &pool,
        "l1 routing pointer",
        &serde_json::json!({}),
        None,
        MemoryLayer::Index,
    )
    .await
    .expect("insert L1 memory");

    sqlx::query("DELETE FROM memories WHERE id = $1")
        .bind(mem_id)
        .execute(&pool)
        .await
        .expect("delete memory");

    let audit_layer: i16 =
        sqlx::query_scalar("SELECT layer FROM deleted_memories WHERE id = $1")
            .bind(mem_id)
            .fetch_one(&pool)
            .await
            .expect("fetch deleted_memories.layer");
    assert_eq!(
        audit_layer, 1,
        "AFTER DELETE trigger must copy the source row's layer (L1 = 1) into the audit row"
    );

    pool.close().await;
}

/// `delete_memory_at_layer` happy path: insert an L1 row, delete it via
/// the layer-guarded helper, assert `true` returned; second call returns
/// `false` (row already gone).
///
/// Also verifies the AFTER DELETE trigger (migration 0008) journals the
/// deletion into `deleted_memories`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_memory_at_layer_happy_path() {
    use hhagent_db::memories::{delete_memory_at_layer, insert_memory_at_layer, MemoryLayer};

    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "dml-d",
        "dml-l",
        &format!("hhagent-pg-del-at-layer-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"purpose": "delete-memory-at-layer-happy"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("pool");

    // Seed an L1 row.
    let id = insert_memory_at_layer(
        &pool,
        "l1-body-to-delete",
        &serde_json::json!({"source": "operator"}),
        None,
        MemoryLayer::Index,
    )
    .await
    .expect("insert L1 row");

    // First delete: must return true (row existed and matched layer).
    let deleted = delete_memory_at_layer(&pool, id, MemoryLayer::Index)
        .await
        .expect("delete L1 row");
    assert!(deleted, "first delete must return true — row matched id + layer");

    // Second delete: must return false (row is gone).
    let deleted_again = delete_memory_at_layer(&pool, id, MemoryLayer::Index)
        .await
        .expect("delete again (idempotent call)");
    assert!(
        !deleted_again,
        "second delete must return false — row already gone"
    );

    // AFTER DELETE trigger (migration 0008) must have journalled the row.
    let audit_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM deleted_memories WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("count deleted_memories");
    assert_eq!(
        audit_count, 1,
        "AFTER DELETE trigger must have written exactly one deleted_memories row"
    );

    pool.close().await;
}

/// `delete_memory_at_layer` wrong-layer guard: inserting an L2 (Stable)
/// row and calling `delete_memory_at_layer` with `MemoryLayer::Index`
/// must return `false` and leave the row untouched in `memories`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_memory_at_layer_rejects_wrong_layer() {
    use hhagent_db::memories::{delete_memory_at_layer, fetch_by_ids, insert_memory, MemoryLayer};

    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "dwl-d",
        "dwl-l",
        &format!("hhagent-pg-del-wrong-layer-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"purpose": "delete-memory-at-layer-wrong-layer"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("pool");

    // Seed an L2 (Stable) row via insert_memory (DB DEFAULT 2).
    let id = insert_memory(
        &pool,
        "stable-body",
        &serde_json::json!({}),
        None,
    )
    .await
    .expect("insert L2 row");

    // Attempt to delete it via the L1 guard — must be rejected.
    let deleted = delete_memory_at_layer(&pool, id, MemoryLayer::Index)
        .await
        .expect("delete wrong-layer call");
    assert!(
        !deleted,
        "wrong-layer guard must return false (L2 row not touched by L1 DELETE)"
    );

    // The L2 row must still exist.
    let rows = fetch_by_ids(&pool, &[id]).await.expect("fetch_by_ids");
    assert_eq!(
        rows.len(),
        1,
        "L2 row must survive the wrong-layer guard; fetch_by_ids must return it"
    );
    assert_eq!(rows[0].id, id);

    pool.close().await;
}

// ─── Migration 0015: entity_kinds + quarantine + name_norm ────────────
//
// These five tests pin the shape introduced by
// `0015_entity_kinds_and_quarantine.sql`:
//
//   1. Schema check (`migration_0015_seeds_entity_kinds_and_adds_quarantine`):
//      20 seed kinds, `undefined` present, `quarantine` DEFAULT TRUE,
//      `name_norm` NOT NULL, FK `entities_kind_fk` exists, unique index
//      `entities_kind_name_norm_idx` exists.
//   2. Dedup behaviour (`entities_upsert_dedup_by_name_norm`):
//      two inserts with the same `name_norm` dedup to one row; the
//      first writer's display `name` is preserved.
//   3. FK fallback (`kind_delete_sets_default_to_undefined`):
//      deleting a kind reparents existing entities to `undefined`
//      (ON DELETE SET DEFAULT path).
//   4. FK guard (`entities_kind_fk_blocks_unknown_kind`):
//      INSERT with an unknown kind is rejected by the FK.
//   5. Cascade vs. quarantine (`relation_persists_when_endpoints_quarantined`):
//      relations between quarantined entities persist; deleting an
//      endpoint still cascades the edge (0001's ON DELETE CASCADE).
//
// All five share the existing cluster-bring-up pattern
// (`skip_if_no_supervisor` + `pg_bin_dir_or_skip` + `bring_up_pg_cluster`).
// "Runtime pool" = `pool::connect_runtime_pool`; "admin pool" = a fresh
// `PgPool::connect_with(cluster.conn_spec.to_pg_connect_options())`
// (i.e. the OS user = cluster superuser, no SET ROLE) used for
// operations that require privileges the runtime role doesn't have
// (deleting from `entity_kinds`, deleting entities owned by other
// roles, etc.).

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_0015_seeds_entity_kinds_and_adds_quarantine() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "m15-d",
        "m15-l",
        &format!("hhagent-pg-m15-shape-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "migration-0015-shape"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    // entity_kinds present + 20 seed rows.
    let n_kinds: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM entity_kinds")
        .fetch_one(&pool)
        .await
        .expect("count entity_kinds");
    assert_eq!(n_kinds, 20, "migration seeds 20 default kinds");

    // 'undefined' specifically present (FK fallback target).
    let n_undefined: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM entity_kinds WHERE kind = 'undefined'",
    )
    .fetch_one(&pool)
    .await
    .expect("count undefined");
    assert_eq!(n_undefined, 1, "'undefined' kind must exist for FK fallback");

    // entities.quarantine column present with DEFAULT TRUE.
    let col_default: String = sqlx::query_scalar(
        "SELECT column_default FROM information_schema.columns \
         WHERE table_name='entities' AND column_name='quarantine'",
    )
    .fetch_one(&pool)
    .await
    .expect("query quarantine default");
    assert!(
        col_default.starts_with("true"),
        "quarantine DEFAULT TRUE; got {col_default}"
    );

    // entities.name_norm column present, NOT NULL.
    let nullable: String = sqlx::query_scalar(
        "SELECT is_nullable FROM information_schema.columns \
         WHERE table_name='entities' AND column_name='name_norm'",
    )
    .fetch_one(&pool)
    .await
    .expect("query name_norm nullable");
    assert_eq!(nullable, "NO", "name_norm must be NOT NULL");

    // FK from entities.kind exists.
    let n_fks: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.table_constraints \
         WHERE table_name='entities' AND constraint_name='entities_kind_fk' \
           AND constraint_type='FOREIGN KEY'",
    )
    .fetch_one(&pool)
    .await
    .expect("query fk");
    assert_eq!(n_fks, 1, "entities_kind_fk must exist");

    // Unique index on (kind, name_norm) exists.
    let n_uniq: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_indexes \
         WHERE tablename='entities' AND indexname='entities_kind_name_norm_idx'",
    )
    .fetch_one(&pool)
    .await
    .expect("query unique idx");
    assert_eq!(n_uniq, 1, "entities_kind_name_norm_idx must exist");

    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entities_upsert_dedup_by_name_norm() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "m15-d",
        "m15-l",
        &format!("hhagent-pg-m15-dedup-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "migration-0015-dedup"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    // Insert "Dr Smith"; second insert with "DR SMITH" (different
    // display, same name_norm) must hit ON CONFLICT and NOT create
    // a second row. Display form (`name`) preserves the FIRST insert.
    let id1: i64 = sqlx::query_scalar(
        "INSERT INTO entities (kind, name, name_norm, quarantine) \
         VALUES ('person', 'Dr Smith', 'dr smith', TRUE) \
         ON CONFLICT (kind, name_norm) DO NOTHING \
         RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("first insert");

    let id2_opt: Option<i64> = sqlx::query_scalar(
        "INSERT INTO entities (kind, name, name_norm, quarantine) \
         VALUES ('person', 'DR SMITH', 'dr smith', TRUE) \
         ON CONFLICT (kind, name_norm) DO NOTHING \
         RETURNING id",
    )
    .fetch_optional(&pool)
    .await
    .expect("second insert");
    assert!(
        id2_opt.is_none(),
        "second insert with same name_norm must conflict"
    );

    // Existing row's display name still 'Dr Smith' (first writer wins).
    let display: String = sqlx::query_scalar("SELECT name FROM entities WHERE id = $1")
        .bind(id1)
        .fetch_one(&pool)
        .await
        .expect("fetch display");
    assert_eq!(display, "Dr Smith", "first writer's display preserved");

    // Final row count = 1 (defends against a future regression that
    // silently relaxed the unique constraint).
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM entities WHERE kind='person' AND name_norm='dr smith'",
    )
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(n, 1, "exactly one row for (person, 'dr smith')");

    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kind_delete_sets_default_to_undefined() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "m15-d",
        "m15-l",
        &format!("hhagent-pg-m15-fkdef-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "migration-0015-fk-default"}),
    )
    .await
    .expect("probe");

    // Admin pool: OS user / cluster superuser (no SET ROLE). Needed
    // because the runtime role's write privileges on `entity_kinds`
    // were revoked in migration 0016 — operator-only writes by design.
    // See the dedicated permission-denied test below.
    let admin = sqlx::postgres::PgPool::connect_with(cluster.conn_spec.to_pg_connect_options())
        .await
        .expect("admin pool");

    // Seed a custom kind + an entity of that kind.
    sqlx::query("INSERT INTO entity_kinds (kind) VALUES ('test_temp_kind')")
        .execute(&admin)
        .await
        .expect("insert temp kind");
    let ent_id: i64 = sqlx::query_scalar(
        "INSERT INTO entities (kind, name, name_norm, quarantine) \
         VALUES ('test_temp_kind', 'X', 'x', TRUE) RETURNING id",
    )
    .fetch_one(&admin)
    .await
    .expect("insert entity");

    // Delete the kind (FK ON DELETE SET DEFAULT → 'undefined').
    sqlx::query("DELETE FROM entity_kinds WHERE kind = 'test_temp_kind'")
        .execute(&admin)
        .await
        .expect("delete kind");

    let reparented: String = sqlx::query_scalar("SELECT kind FROM entities WHERE id = $1")
        .bind(ent_id)
        .fetch_one(&admin)
        .await
        .expect("fetch reparented");
    assert_eq!(
        reparented, "undefined",
        "FK ON DELETE SET DEFAULT must reparent"
    );

    admin.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entities_kind_fk_blocks_unknown_kind() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "m15-d",
        "m15-l",
        &format!("hhagent-pg-m15-fkblk-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "migration-0015-fk-block"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    let r = sqlx::query(
        "INSERT INTO entities (kind, name, name_norm, quarantine) \
         VALUES ('this_kind_does_not_exist', 'X', 'x', TRUE)",
    )
    .execute(&pool)
    .await;
    assert!(r.is_err(), "insert of unknown kind must fail FK constraint");
    let err = format!("{:?}", r.unwrap_err());
    assert!(
        err.contains("entities_kind_fk") || err.to_lowercase().contains("foreign key"),
        "FK error expected; got: {err}"
    );

    pool.close().await;
}

/// Migration 0016: the runtime role must NOT be able to write to
/// `entity_kinds` — adding a kind is an operator-deliberate act, not
/// something the agent / extractor / any runtime path should be allowed
/// to do silently. 0002's `ALTER DEFAULT PRIVILEGES` would otherwise
/// hand the runtime role full CRUD on every new table; 0016 REVOKEs
/// INSERT/UPDATE/DELETE/TRUNCATE on `entity_kinds` to restore the
/// "operator-only writes" invariant 0015's comment claimed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entity_kinds_writes_denied_to_runtime_role() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "m16-d",
        "m16-l",
        &format!("hhagent-pg-m16-revoke-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "migration-0016-revoke-entity-kinds-writes"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    // SELECT must still work (extractor's KindsCache depends on it).
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM entity_kinds")
        .fetch_one(&pool)
        .await
        .expect("runtime SELECT on entity_kinds must succeed");
    assert!(n >= 20, "0015 seeds at least 20 kinds; got {n}");

    // INSERT must be rejected with permission denied.
    let r = sqlx::query("INSERT INTO entity_kinds (kind) VALUES ('runtime_should_not_insert')")
        .execute(&pool)
        .await;
    let err = format!("{:?}", r.expect_err("runtime INSERT must be denied"));
    assert!(
        err.to_lowercase().contains("permission denied"),
        "expected permission-denied; got: {err}",
    );

    // UPDATE must also be rejected.
    let r = sqlx::query("UPDATE entity_kinds SET description = 'tampered' WHERE kind = 'person'")
        .execute(&pool)
        .await;
    let err = format!("{:?}", r.expect_err("runtime UPDATE must be denied"));
    assert!(
        err.to_lowercase().contains("permission denied"),
        "expected permission-denied; got: {err}",
    );

    // DELETE must also be rejected.
    let r = sqlx::query("DELETE FROM entity_kinds WHERE kind = 'person'")
        .execute(&pool)
        .await;
    let err = format!("{:?}", r.expect_err("runtime DELETE must be denied"));
    assert!(
        err.to_lowercase().contains("permission denied"),
        "expected permission-denied; got: {err}",
    );

    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relation_persists_when_endpoints_quarantined() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "m15-d",
        "m15-l",
        &format!("hhagent-pg-m15-relq-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "migration-0015-relation-quarantine"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    // Two quarantined entities + a relation between them.
    let head: i64 = sqlx::query_scalar(
        "INSERT INTO entities (kind, name, name_norm, quarantine) \
         VALUES ('person', 'Alpha', 'alpha', TRUE) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("head");
    let tail: i64 = sqlx::query_scalar(
        "INSERT INTO entities (kind, name, name_norm, quarantine) \
         VALUES ('disease', 'Beta', 'beta', TRUE) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("tail");
    let _rel: i64 = sqlx::query_scalar(
        "INSERT INTO relations (src_id, dst_id, kind) VALUES ($1, $2, 'treats') RETURNING id",
    )
    .bind(head)
    .bind(tail)
    .fetch_one(&pool)
    .await
    .expect("relation");

    // Relation row exists.
    let n_rels: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM relations WHERE src_id=$1 AND dst_id=$2 AND kind='treats'",
    )
    .bind(head)
    .bind(tail)
    .fetch_one(&pool)
    .await
    .expect("count rel");
    assert_eq!(
        n_rels, 1,
        "relation between quarantined endpoints must persist"
    );

    // Deleting one endpoint cascades the relation. Use the admin pool
    // because the head row was inserted by the runtime role under the
    // shared cluster; either pool can DELETE here, but the admin pool
    // mirrors the plan's "operator-driven" framing.
    let admin = sqlx::postgres::PgPool::connect_with(cluster.conn_spec.to_pg_connect_options())
        .await
        .expect("admin pool");
    sqlx::query("DELETE FROM entities WHERE id = $1")
        .bind(head)
        .execute(&admin)
        .await
        .expect("delete head");

    let n_rels_after: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM relations WHERE src_id=$1 AND dst_id=$2",
    )
    .bind(head)
    .bind(tail)
    .fetch_one(&pool)
    .await
    .expect("count rel after");
    assert_eq!(n_rels_after, 0, "relation must cascade-delete with endpoint");

    admin.close().await;
    pool.close().await;
}

// ─── entity_kinds module: KindsCache + fetch_kinds ────────────────────
//
// Three tests pin the behaviour of the cache wrapper introduced for
// the v2 entity extractor:
//
//   1. `entity_kinds_cache_returns_seeded_list`: first call to a
//      fresh cache returns all 20 seeded kinds, including the FK
//      fallback target (`undefined`), a representative single-word
//      kind (`person`), and a multi-word kind (`phone number` — the
//      space is load-bearing and easy to silently regress).
//   2. `entity_kinds_fetch_kinds_orders_alphabetically`: the raw
//      `fetch_kinds` helper returns rows in `ORDER BY kind`, so
//      callers can rely on deterministic order without re-sorting.
//   3. `entity_kinds_cache_hits_warm_does_not_re_query`: two calls in
//      quick succession return structurally-equal Vecs. We can't
//      observe "no SQL issued" from outside the cache, so the test
//      pins return-stability as a proxy.
//
// Same cluster-bring-up pattern as the migration-0015 tests above.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entity_kinds_cache_returns_seeded_list() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ek-d",
        "ek-l",
        &format!("hhagent-pg-ek-seeded-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "entity-kinds-cache-seeded"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    let cache = hhagent_db::entity_kinds::KindsCache::new();
    let kinds = cache.list_kinds(&pool).await.expect("list_kinds");

    assert_eq!(
        kinds.len(),
        20,
        "migration 0015 seeds exactly 20 kinds; got {} ({:?})",
        kinds.len(),
        kinds
    );
    assert!(
        kinds.iter().any(|k| k == "undefined"),
        "'undefined' must be present (FK fallback target); got {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| k == "person"),
        "'person' must be present; got {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| k == "phone number"),
        "'phone number' (with space) must be present; got {kinds:?}"
    );

    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entity_kinds_fetch_kinds_orders_alphabetically() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ek-d",
        "ek-l",
        &format!("hhagent-pg-ek-order-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "entity-kinds-fetch-order"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    let kinds = hhagent_db::entity_kinds::fetch_kinds(&pool)
        .await
        .expect("fetch_kinds");

    let mut sorted = kinds.clone();
    sorted.sort();
    assert_eq!(
        kinds, sorted,
        "fetch_kinds must return rows in ORDER BY kind; got {kinds:?}"
    );

    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entity_kinds_cache_hits_warm_does_not_re_query() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ek-d",
        "ek-l",
        &format!("hhagent-pg-ek-warm-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "entity-kinds-cache-warm"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    let cache = hhagent_db::entity_kinds::KindsCache::new();
    let first = cache.list_kinds(&pool).await.expect("list_kinds #1");
    let second = cache.list_kinds(&pool).await.expect("list_kinds #2");
    assert_eq!(
        first, second,
        "back-to-back list_kinds within TTL must return identical Vecs"
    );

    pool.close().await;
}

// ─── graph_search quarantine filter (Task 4) ─────────────────────────

/// Production callers pass `include_quarantined=false`; only the
/// promoted side of the entity table should contribute memory ids to
/// the graph lane. A memory linked exclusively to quarantined entities
/// must NOT surface even when its entity_id appears in the seed set.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_search_excludes_quarantined_by_default() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "gsq-d",
        "gsq-l",
        &format!("hhagent-pg-gsq-excl-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "graph_search-excludes-quarantined"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    // Two entities: one promoted (quarantine=FALSE), one quarantined.
    // Inserted via raw SQL because `Graph::upsert_entity` doesn't
    // expose the quarantine column (defaults TRUE, promotion is the
    // future operator path).
    let ent_promoted: i64 = sqlx::query_scalar(
        "INSERT INTO entities (kind, name, name_norm, quarantine) \
         VALUES ('person', 'Alice Promoted', 'alice promoted', FALSE) \
         RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("insert promoted entity");

    let ent_quar: i64 = sqlx::query_scalar(
        "INSERT INTO entities (kind, name, name_norm, quarantine) \
         VALUES ('person', 'Bob Quarantined', 'bob quarantined', TRUE) \
         RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("insert quarantined entity");

    // Two memories, one linked to each entity.
    let mem_promoted = hhagent_db::memories::insert_memory(
        &pool,
        "memory linked to promoted entity",
        &serde_json::json!({}),
        None,
    )
    .await
    .expect("insert mem_promoted");

    let mem_quar = hhagent_db::memories::insert_memory(
        &pool,
        "memory linked to quarantined entity",
        &serde_json::json!({}),
        None,
    )
    .await
    .expect("insert mem_quar");

    hhagent_db::memories::link_memory_to_entities(&pool, mem_promoted, &[ent_promoted])
        .await
        .expect("link mem_promoted");
    hhagent_db::memories::link_memory_to_entities(&pool, mem_quar, &[ent_quar])
        .await
        .expect("link mem_quar");

    // Production call: include_quarantined=false.
    let hits = hhagent_db::memories::graph_search(
        &pool,
        &[ent_promoted, ent_quar],
        10,
        false,
    )
    .await
    .expect("graph_search");

    assert_eq!(
        hits,
        vec![mem_promoted],
        "graph_search with include_quarantined=false must drop memories \
         whose only linked entities are quarantined"
    );

    pool.close().await;
}

/// The operator-review CLI path passes `include_quarantined=true` so
/// reviewers can see what the v2 extractor staged. Confirm the flag
/// genuinely overrides the filter, including for entities still in
/// their default quarantined state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_search_includes_quarantined_when_flag_true() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "gsq-d",
        "gsq-l",
        &format!("hhagent-pg-gsq-incl-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "graph_search-includes-quarantined"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    let ent_quar: i64 = sqlx::query_scalar(
        "INSERT INTO entities (kind, name, name_norm, quarantine) \
         VALUES ('person', 'Carol Quarantined', 'carol quarantined', TRUE) \
         RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("insert quarantined entity");

    let mem = hhagent_db::memories::insert_memory(
        &pool,
        "memory linked to quarantined entity (operator path)",
        &serde_json::json!({}),
        None,
    )
    .await
    .expect("insert mem");

    hhagent_db::memories::link_memory_to_entities(&pool, mem, &[ent_quar])
        .await
        .expect("link mem");

    // Operator path: include_quarantined=true.
    let hits = hhagent_db::memories::graph_search(&pool, &[ent_quar], 10, true)
        .await
        .expect("graph_search");

    assert_eq!(
        hits,
        vec![mem],
        "graph_search with include_quarantined=true must surface \
         memories linked only to quarantined entities"
    );

    pool.close().await;
}

// ─── Entities review surface (0015) ─────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entities_list_filters_by_state_kind_and_since() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "elf-d",
        "elf-l",
        &format!("hhagent-pg-elf-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "entities-list-filters"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("pool");

    use hhagent_db::entities::{list_entities, EntityState, ListFilter};
    use time::OffsetDateTime;

    // Seed 4 entities — 2 quarantined (different kinds), 1 approved, 1 old.
    sqlx::query(
        "INSERT INTO entities (kind, name, name_norm, quarantine) VALUES
        ('person', 'Quar Alice', 'quar alice', TRUE),
        ('place',  'Quar Mosman', 'quar mosman', TRUE),
        ('person', 'OK Bob', 'ok bob', FALSE),
        ('person', 'Old Carol', 'old carol', TRUE)",
    )
    .execute(&pool)
    .await
    .unwrap();
    // Back-date Old Carol so the --since filter excludes it.
    sqlx::query(
        "UPDATE entities SET created_at = now() - interval '7 days' WHERE name = 'Old Carol'",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Default filter (quarantined, limit 50, no other filters).
    let rows = list_entities(&pool, &ListFilter::default()).await.unwrap();
    let names: std::collections::BTreeSet<&str> =
        rows.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains("Quar Alice"));
    assert!(names.contains("Quar Mosman"));
    assert!(names.contains("Old Carol"));
    assert!(
        !names.contains("OK Bob"),
        "approved entity must not appear in default filter"
    );
    assert_eq!(rows.len(), 3, "expected 3 quarantined rows, got {}", rows.len());

    // Filter by kind=person.
    let rows = list_entities(
        &pool,
        &ListFilter {
            kind: Some("person".into()),
            ..ListFilter::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(rows.len(), 2, "expected 2 quarantined persons");
    for r in &rows {
        assert_eq!(r.kind, "person");
    }

    // Filter by state=approved.
    let rows = list_entities(
        &pool,
        &ListFilter {
            state: EntityState::Approved,
            ..ListFilter::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "OK Bob");
    assert!(!rows[0].quarantine);

    // Filter by since = now - 1 day. Old Carol must be excluded.
    let cutoff = OffsetDateTime::now_utc() - time::Duration::days(1);
    let rows = list_entities(
        &pool,
        &ListFilter {
            since: Some(cutoff),
            ..ListFilter::default()
        },
    )
    .await
    .unwrap();
    let names: std::collections::BTreeSet<&str> =
        rows.iter().map(|r| r.name.as_str()).collect();
    assert!(
        !names.contains("Old Carol"),
        "back-dated row must be excluded by --since"
    );
    assert!(names.contains("Quar Alice"));

    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entities_list_min_mentions_filter_uses_join_count() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "emm-d",
        "emm-l",
        &format!("hhagent-pg-emm-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "entities-min-mentions"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("pool");

    use hhagent_db::entities::{list_entities, ListFilter};

    // Seed 1 entity with 0 mentions and 1 entity with 2 mentions.
    sqlx::query(
        "INSERT INTO entities (kind, name, name_norm) VALUES
        ('person', 'Zero', 'zero'),
        ('person', 'Two',  'two')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let two_id: i64 = sqlx::query_scalar("SELECT id FROM entities WHERE name = 'Two'")
        .fetch_one(&pool)
        .await
        .unwrap();

    // Two memories linked only to the 'Two' entity.
    use hhagent_db::memories::{insert_memory_at_layer, MemoryLayer};
    let mem1 = insert_memory_at_layer(
        &pool,
        "body 1",
        &serde_json::json!({}),
        None,
        MemoryLayer::Stable,
    )
    .await
    .unwrap();
    let mem2 = insert_memory_at_layer(
        &pool,
        "body 2",
        &serde_json::json!({}),
        None,
        MemoryLayer::Stable,
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO memory_entities (memory_id, entity_id) VALUES ($1, $2), ($3, $2)",
    )
    .bind(mem1)
    .bind(two_id)
    .bind(mem2)
    .execute(&pool)
    .await
    .unwrap();

    // min_mentions=1 — only 'Two' qualifies.
    let rows = list_entities(
        &pool,
        &ListFilter {
            min_mentions: 1,
            ..ListFilter::default()
        },
    )
    .await
    .unwrap();
    let names: std::collections::BTreeSet<&str> =
        rows.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains("Two"));
    assert!(!names.contains("Zero"));

    // Verify mention_count is surfaced correctly.
    let two_row = rows.iter().find(|r| r.name == "Two").unwrap();
    assert_eq!(two_row.mention_count, 2);

    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entities_approve_flips_quarantine_and_is_idempotent() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "eaf-d",
        "eaf-l",
        &format!("hhagent-pg-eaf-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "entities-approve"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("pool");

    use hhagent_db::entities::{approve_entity, ApproveOutcome};

    sqlx::query("INSERT INTO entities (kind, name, name_norm) VALUES ('person', 'Approve Me', 'approve me')")
        .execute(&pool).await.unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM entities WHERE name = 'Approve Me'")
        .fetch_one(&pool).await.unwrap();

    // First call: Approved.
    match approve_entity(&pool, id).await.unwrap() {
        ApproveOutcome::Approved { kind, name } => {
            assert_eq!(kind, "person");
            assert_eq!(name, "Approve Me");
        }
        other => panic!("expected Approved, got {other:?}"),
    }
    // DB state must reflect the flip.
    let quarantine: bool = sqlx::query_scalar("SELECT quarantine FROM entities WHERE id = $1")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert!(!quarantine);

    // Second call: AlreadyApproved.
    assert!(matches!(approve_entity(&pool, id).await.unwrap(), ApproveOutcome::AlreadyApproved));

    // Unknown id: NotFound.
    assert!(matches!(approve_entity(&pool, 999_999).await.unwrap(), ApproveOutcome::NotFound));

    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entities_reject_cascades_memory_entities_and_returns_count() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "erc-d",
        "erc-l",
        &format!("hhagent-pg-erc-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "entities-reject"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("pool");

    use hhagent_db::entities::{reject_entity, RejectOutcome};
    use hhagent_db::memories::{insert_memory_at_layer, MemoryLayer};

    sqlx::query("INSERT INTO entities (kind, name, name_norm) VALUES ('person', 'Reject Me', 'reject me')")
        .execute(&pool).await.unwrap();
    let entity_id: i64 = sqlx::query_scalar("SELECT id FROM entities WHERE name = 'Reject Me'")
        .fetch_one(&pool).await.unwrap();

    // Link two memories to the entity.
    let mem1 = insert_memory_at_layer(&pool, "body one", &serde_json::json!({}), None, MemoryLayer::Stable).await.unwrap();
    let mem2 = insert_memory_at_layer(&pool, "body two", &serde_json::json!({}), None, MemoryLayer::Stable).await.unwrap();
    sqlx::query("INSERT INTO memory_entities (memory_id, entity_id) VALUES ($1, $3), ($2, $3)")
        .bind(mem1).bind(mem2).bind(entity_id)
        .execute(&pool).await.unwrap();

    match reject_entity(&pool, entity_id).await.unwrap() {
        RejectOutcome::Rejected { kind, name, mentions_dropped } => {
            assert_eq!(kind, "person");
            assert_eq!(name, "Reject Me");
            assert_eq!(mentions_dropped, 2);
        }
        other => panic!("expected Rejected, got {other:?}"),
    }

    // Entity is gone.
    let entity_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM entities WHERE id = $1")
        .bind(entity_id).fetch_one(&pool).await.unwrap();
    assert_eq!(entity_count, 0);
    // memory_entities rows cascaded.
    let me_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memory_entities WHERE entity_id = $1")
        .bind(entity_id).fetch_one(&pool).await.unwrap();
    assert_eq!(me_count, 0);
    // Memory rows themselves survive.
    let mem_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memories WHERE id IN ($1, $2)")
        .bind(mem1).bind(mem2).fetch_one(&pool).await.unwrap();
    assert_eq!(mem_count, 2);

    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entities_reject_returns_not_found_on_unknown_id() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ernf-d",
        "ernf-l",
        &format!("hhagent-pg-ernf-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "entities-reject-notfound"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("pool");

    use hhagent_db::entities::{reject_entity, RejectOutcome};
    assert!(matches!(
        reject_entity(&pool, 999_999).await.unwrap(),
        RejectOutcome::NotFound
    ));

    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entities_merge_retargets_links_and_drops_duplicates() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "emr-d",
        "emr-l",
        &format!("hhagent-pg-emr-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "entities-merge-retarget"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("pool");

    use hhagent_db::entities::merge_entities;
    use hhagent_db::memories::{insert_memory_at_layer, MemoryLayer};

    // 3 person entities: 'Smith' is the canonical row; 'SMITH' is a
    // near-duplicate the operator wants to merge in. The (kind, name_norm)
    // unique constraint from migration 0015 would reject two rows with
    // name_norm='smith', so the seed uses an artificial 'smith_2' value.
    // A real extractor wouldn't produce two such rows — but the merge
    // operation must still handle whatever the operator finds in the DB.
    sqlx::query("INSERT INTO entities (kind, name, name_norm) VALUES
        ('person', 'Smith',     'smith'),
        ('person', 'SMITH',     'smith_2'),
        ('person', 'Dr. Smith', 'dr smith')")
        .execute(&pool).await.unwrap();
    let keep:  i64 = sqlx::query_scalar("SELECT id FROM entities WHERE name = 'Smith'").fetch_one(&pool).await.unwrap();
    let drop_a: i64 = sqlx::query_scalar("SELECT id FROM entities WHERE name = 'SMITH'").fetch_one(&pool).await.unwrap();
    let drop_b: i64 = sqlx::query_scalar("SELECT id FROM entities WHERE name = 'Dr. Smith'").fetch_one(&pool).await.unwrap();

    // 4 memories. mem1 -> keep only; mem2 -> drop_a + keep (the duplicate);
    // mem3 -> drop_a only (a unique retarget); mem4 -> drop_b only.
    let mem1 = insert_memory_at_layer(&pool, "m1", &serde_json::json!({}), None, MemoryLayer::Stable).await.unwrap();
    let mem2 = insert_memory_at_layer(&pool, "m2", &serde_json::json!({}), None, MemoryLayer::Stable).await.unwrap();
    let mem3 = insert_memory_at_layer(&pool, "m3", &serde_json::json!({}), None, MemoryLayer::Stable).await.unwrap();
    let mem4 = insert_memory_at_layer(&pool, "m4", &serde_json::json!({}), None, MemoryLayer::Stable).await.unwrap();
    sqlx::query("INSERT INTO memory_entities (memory_id, entity_id) VALUES
        ($1, $5), ($2, $5), ($2, $6), ($3, $6), ($4, $7)")
        .bind(mem1).bind(mem2).bind(mem3).bind(mem4)
        .bind(keep).bind(drop_a).bind(drop_b)
        .execute(&pool).await.unwrap();

    let outcome = merge_entities(&pool, keep, &[drop_a, drop_b]).await.unwrap();
    // mem2 was linked to BOTH drop_a and keep — that's the duplicate.
    // mem3 was linked only to drop_a — that retargets to keep.
    // mem4 was linked only to drop_b — that retargets to keep.
    assert_eq!(outcome.links_retargeted, 2,
        "expected 2 unique-link retargets (mem3+mem4), got {outcome:?}");
    assert_eq!(outcome.links_dropped_as_duplicate, 1,
        "expected 1 duplicate dropped (mem2), got {outcome:?}");
    assert_eq!(outcome.kept_id, keep);
    assert_eq!(outcome.kept_kind, "person");
    assert_eq!(outcome.kept_name, "Smith");

    // drop_a + drop_b rows are gone.
    let drop_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM entities WHERE id IN ($1, $2)")
        .bind(drop_a).bind(drop_b).fetch_one(&pool).await.unwrap();
    assert_eq!(drop_count, 0);

    // keep is linked to mem1, mem2, mem3, mem4 (all distinct).
    let kept_links: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memory_entities WHERE entity_id = $1")
        .bind(keep).fetch_one(&pool).await.unwrap();
    assert_eq!(kept_links, 4);

    pool.close().await;
}

/// Pins the documented semantic difference between
/// `links_retargeted` (distinct memories newly visible from keep) and
/// `links_dropped_as_duplicate` (memory_entities ROWS absorbed by the
/// ON CONFLICT DO NOTHING). When one memory is linked to BOTH drops
/// AND to keep, it contributes 2 to `dup_count` (one row per drop)
/// but 0 to `links_retargeted` (keep already had this memory). The
/// merge-happy-path test above exercises the 1:1 case where the two
/// counters coincide; this test exercises the multi-drop overlap that
/// makes them diverge.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entities_merge_dup_count_sums_rows_across_multiple_drops() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "emd-d",
        "emd-l",
        &format!("hhagent-pg-emd-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "entities-merge-dup-rows"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("pool");

    use hhagent_db::entities::merge_entities;
    use hhagent_db::memories::{insert_memory_at_layer, MemoryLayer};

    sqlx::query("INSERT INTO entities (kind, name, name_norm) VALUES
        ('person', 'Smith',  'smith'),
        ('person', 'SMITH',  'smith_2'),
        ('person', 'smithy', 'smithy')")
        .execute(&pool).await.unwrap();
    let keep:   i64 = sqlx::query_scalar("SELECT id FROM entities WHERE name = 'Smith'").fetch_one(&pool).await.unwrap();
    let drop_a: i64 = sqlx::query_scalar("SELECT id FROM entities WHERE name = 'SMITH'").fetch_one(&pool).await.unwrap();
    let drop_b: i64 = sqlx::query_scalar("SELECT id FROM entities WHERE name = 'smithy'").fetch_one(&pool).await.unwrap();

    // One memory linked to keep + drop_a + drop_b. The ON CONFLICT will
    // absorb two rows (drop_a→keep already exists, drop_b→keep already
    // exists), but the memory is not newly visible from keep — keep
    // already had the link.
    let mem = insert_memory_at_layer(&pool, "m", &serde_json::json!({}), None, MemoryLayer::Stable).await.unwrap();
    sqlx::query("INSERT INTO memory_entities (memory_id, entity_id) VALUES ($1, $2), ($1, $3), ($1, $4)")
        .bind(mem).bind(keep).bind(drop_a).bind(drop_b)
        .execute(&pool).await.unwrap();

    let outcome = merge_entities(&pool, keep, &[drop_a, drop_b]).await.unwrap();
    assert_eq!(outcome.links_retargeted, 0,
        "keep already had this memory — nothing newly visible: {outcome:?}");
    assert_eq!(outcome.links_dropped_as_duplicate, 2,
        "memory was linked to both drops; dup_count counts rows, not memories: {outcome:?}");

    // keep retains exactly one link to mem after the merge.
    let kept_links: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memory_entities WHERE entity_id = $1")
        .bind(keep).fetch_one(&pool).await.unwrap();
    assert_eq!(kept_links, 1);

    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entities_merge_refuses_cross_kind_and_keep_in_drop_list() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "emx-d",
        "emx-l",
        &format!("hhagent-pg-emx-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "entities-merge-cross-kind"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("pool");

    use hhagent_db::entities::{merge_entities, EntitiesError};

    sqlx::query("INSERT INTO entities (kind, name, name_norm) VALUES
        ('person', 'Alice',  'alice'),
        ('place',  'Sydney', 'sydney')")
        .execute(&pool).await.unwrap();
    let alice:  i64 = sqlx::query_scalar("SELECT id FROM entities WHERE name = 'Alice'").fetch_one(&pool).await.unwrap();
    let sydney: i64 = sqlx::query_scalar("SELECT id FROM entities WHERE name = 'Sydney'").fetch_one(&pool).await.unwrap();

    // Cross-kind merge — refuse with KindMismatch.
    let err = merge_entities(&pool, alice, &[sydney]).await.unwrap_err();
    match err {
        EntitiesError::KindMismatch { keep_id, keep_kind, drop_id, drop_kind } => {
            assert_eq!(keep_id, alice);
            assert_eq!(keep_kind, "person");
            assert_eq!(drop_id, sydney);
            assert_eq!(drop_kind, "place");
        }
        other => panic!("expected KindMismatch, got {other:?}"),
    }
    // Both entities still exist (rollback worked).
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM entities WHERE id IN ($1, $2)")
        .bind(alice).bind(sydney).fetch_one(&pool).await.unwrap();
    assert_eq!(count, 2);

    // Keep in drop list — refuse with KeepInDropList (pure-helper path).
    let err = merge_entities(&pool, alice, &[alice]).await.unwrap_err();
    assert!(matches!(err, EntitiesError::KeepInDropList(id) if id == alice));

    // Unknown drop id — refuse with NotFound.
    let err = merge_entities(&pool, alice, &[999_999]).await.unwrap_err();
    assert!(matches!(err, EntitiesError::NotFound(id) if id == 999_999));

    pool.close().await;
}

// ─── Migration 0017: relation_kinds + relations.kind FK ────────────────
//
// Three tests pin the migration's load-bearing pieces, symmetric to
// the three migration-0015 / 0016 tests above:
//
//   1. Schema check (`migration_0017_seeds_relation_kinds_and_adds_fk`):
//      the lookup table exists, 19 seed kinds are present (1
//      `undefined` fallback + 18 starter relations), `undefined` is
//      among them, `relations_kind_fk` is wired, and
//      `fetch_relation_kinds` returns the same set the cache will use.
//
//   2. FK behaviour (`relation_kinds_fk_rejects_unknown_kind_and_sets_default_on_delete`):
//      INSERT into `relations` with an unknown `kind` is rejected;
//      deleting a kind that's referenced by a row sets that row's kind
//      to `'undefined'` (the ON DELETE SET DEFAULT contract). Uses the
//      superuser channel for the DELETE on `relation_kinds` because
//      the runtime role's INSERT/UPDATE/DELETE on `relation_kinds` is
//      revoked (covered by the third test).
//
//   3. GRANT/REVOKE shape (`relation_kinds_writes_denied_to_runtime_role`):
//      runtime role can SELECT but not INSERT/UPDATE/DELETE on
//      `relation_kinds` — same operator-managed posture as
//      `entity_kinds` post-0016.

/// 0017 introduces the `relation_kinds` lookup table seeded with 17
/// starter relations, adds a `relations.kind` FK + `SET DEFAULT
/// 'undefined'` on delete, and locks the runtime role out of writes.
/// Mirrors the migration-0015 + migration-0016 contract for
/// `entity_kinds`. This test pins the *schema* end of that contract;
/// the FK behaviour and the REVOKE behaviour each have their own
/// dedicated tests below.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_0017_seeds_relation_kinds_and_adds_fk() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "m17-d",
        "m17-l",
        &format!("hhagent-pg-m17-shape-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "migration-0017-relation-kinds-shape"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    // relation_kinds present with 19 seeds (1 fallback + 18 starter).
    let n_kinds: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM relation_kinds")
        .fetch_one(&pool)
        .await
        .expect("count relation_kinds");
    assert_eq!(n_kinds, 19, "migration seeds 19 default relation kinds");

    // 'undefined' specifically present — load-bearing target of the
    // FK's ON DELETE SET DEFAULT.
    let n_undefined: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM relation_kinds WHERE kind = 'undefined'",
    )
    .fetch_one(&pool)
    .await
    .expect("count undefined");
    assert_eq!(n_undefined, 1, "'undefined' kind must exist for FK fallback");

    // FK on relations.kind exists.
    let n_fks: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.table_constraints \
         WHERE table_name='relations' AND constraint_name='relations_kind_fk' \
           AND constraint_type='FOREIGN KEY'",
    )
    .fetch_one(&pool)
    .await
    .expect("query fk");
    assert_eq!(n_fks, 1, "relations_kind_fk must exist");

    // relations.kind DEFAULT 'undefined' — required so ON DELETE SET
    // DEFAULT lands on the FK target.
    let col_default: String = sqlx::query_scalar(
        "SELECT column_default FROM information_schema.columns \
         WHERE table_name='relations' AND column_name='kind'",
    )
    .fetch_one(&pool)
    .await
    .expect("query relations.kind default");
    assert!(
        col_default.contains("'undefined'"),
        "relations.kind DEFAULT must be 'undefined'; got {col_default}",
    );

    // `fetch_relation_kinds` is the source-of-truth for the
    // `RelationKindsCache`. Pin it returns the same 19 seeds the
    // direct COUNT(*) saw and includes the load-bearing ones.
    let kinds = hhagent_db::relation_kinds::fetch_relation_kinds(&pool)
        .await
        .expect("fetch_relation_kinds");
    assert_eq!(kinds.len(), 19, "fetch returned {} kinds, want 19", kinds.len());
    for required in ["undefined", "treats", "located in", "associated with", "owns", "knows"] {
        assert!(
            kinds.iter().any(|k| k == required),
            "kinds must contain {required:?}; got {kinds:?}",
        );
    }

    pool.close().await;
}

/// `relations_kind_fk` is configured `ON DELETE SET DEFAULT` with
/// default `'undefined'`. Pin both halves: (a) inserting a row with an
/// unknown kind is rejected, and (b) deleting a referenced kind from
/// the lookup table rewrites the dependent row to `'undefined'`
/// instead of cascading or erroring.
///
/// The DELETE on `relation_kinds` runs via the cluster's superuser
/// connection (the runtime role has no DELETE — that contract is
/// pinned by `relation_kinds_writes_denied_to_runtime_role` below).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relation_kinds_fk_rejects_unknown_kind_and_sets_default_on_delete() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "m17-d",
        "m17-l",
        &format!("hhagent-pg-m17-fk-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "migration-0017-fk"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    // Seed two unquarantined entities so we can form a real edge.
    let src_id: i64 = sqlx::query_scalar(
        "INSERT INTO entities (kind, name, name_norm, quarantine) \
         VALUES ('person', 'Alice', 'alice', FALSE) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("insert src entity");
    let dst_id: i64 = sqlx::query_scalar(
        "INSERT INTO entities (kind, name, name_norm, quarantine) \
         VALUES ('disease', 'Asthma', 'asthma', FALSE) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("insert dst entity");

    // (a) Unknown relation kind must be rejected by the FK.
    let r = sqlx::query(
        "INSERT INTO relations (src_id, dst_id, kind) VALUES ($1, $2, 'no_such_kind')",
    )
    .bind(src_id)
    .bind(dst_id)
    .execute(&pool)
    .await;
    let err = format!("{:?}", r.expect_err("unknown kind must violate FK"));
    assert!(
        err.to_lowercase().contains("foreign key"),
        "expected FK-violation error; got: {err}",
    );

    // Insert a real edge using a seeded kind — should succeed.
    let rel_id: i64 = sqlx::query_scalar(
        "INSERT INTO relations (src_id, dst_id, kind) VALUES ($1, $2, 'treats') RETURNING id",
    )
    .bind(src_id)
    .bind(dst_id)
    .fetch_one(&pool)
    .await
    .expect("insert relation with seeded kind");

    pool.close().await;

    // (b) Delete the kind via a superuser connection; the dependent
    // relations row must have its kind rewritten to 'undefined' by
    // the FK's ON DELETE SET DEFAULT clause.
    let admin_pool = sqlx::postgres::PgPool::connect_with(cluster.conn_spec.to_pg_connect_options())
        .await
        .expect("admin pool");
    sqlx::query("DELETE FROM relation_kinds WHERE kind = 'treats'")
        .execute(&admin_pool)
        .await
        .expect("admin DELETE on relation_kinds");
    let after_kind: String =
        sqlx::query_scalar("SELECT kind FROM relations WHERE id = $1")
            .bind(rel_id)
            .fetch_one(&admin_pool)
            .await
            .expect("read relation kind after delete");
    assert_eq!(
        after_kind, "undefined",
        "ON DELETE SET DEFAULT must rewrite to 'undefined'; got {after_kind:?}",
    );
    admin_pool.close().await;
}

/// `relation_kinds` is operator-managed, mirroring `entity_kinds`
/// post-0016: runtime role gets SELECT only. INSERT/UPDATE/DELETE/
/// TRUNCATE must be refused with `permission denied` so a compromised
/// extractor cannot widen the vocabulary without operator review.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relation_kinds_writes_denied_to_runtime_role() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "m17-d",
        "m17-l",
        &format!("hhagent-pg-m17-revoke-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "migration-0017-revoke-relation-kinds-writes"}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    // SELECT must still work (extractor's RelationKindsCache depends on it).
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM relation_kinds")
        .fetch_one(&pool)
        .await
        .expect("runtime SELECT on relation_kinds must succeed");
    assert!(n >= 19, "0017 seeds at least 19 kinds; got {n}");

    // INSERT must be rejected with permission denied.
    let r = sqlx::query("INSERT INTO relation_kinds (kind) VALUES ('runtime_should_not_insert')")
        .execute(&pool)
        .await;
    let err = format!("{:?}", r.expect_err("runtime INSERT must be denied"));
    assert!(
        err.to_lowercase().contains("permission denied"),
        "expected permission-denied; got: {err}",
    );

    // UPDATE must also be rejected.
    let r = sqlx::query("UPDATE relation_kinds SET description = 'tampered' WHERE kind = 'treats'")
        .execute(&pool)
        .await;
    let err = format!("{:?}", r.expect_err("runtime UPDATE must be denied"));
    assert!(
        err.to_lowercase().contains("permission denied"),
        "expected permission-denied; got: {err}",
    );

    // DELETE must also be rejected.
    let r = sqlx::query("DELETE FROM relation_kinds WHERE kind = 'treats'")
        .execute(&pool)
        .await;
    let err = format!("{:?}", r.expect_err("runtime DELETE must be denied"));
    assert!(
        err.to_lowercase().contains("permission denied"),
        "expected permission-denied; got: {err}",
    );

    pool.close().await;
}

// ─── relation_kinds operator CLI surface ───────────────────────────────
//
// The next four tests cover the `hhagent-cli relations kinds {add,
// remove, list}` substrate (`db::relation_kinds::{add, remove, list_all}`
// + `db::pool::connect_admin_pool`). They pin three load-bearing
// invariants:
//
//   1. Admin pool can write where runtime pool cannot
//      (`admin_pool_can_write_relation_kinds_while_runtime_pool_cannot`).
//      The whole point of `connect_admin_pool` is to bypass the runtime
//      role's REVOKE for vocabulary-management CLIs; this test pins
//      both halves of that asymmetry against one cluster.
//   2. `add` is idempotent
//      (`relation_kinds_add_is_idempotent_and_persists_description`).
//      The CLI relies on `Ok(true)` vs `Ok(false)` to decide whether to
//      emit an audit row, and on the description being preserved across
//      the round-trip.
//   3. `remove` refuses to delete the FK fallback
//      (`relation_kinds_remove_rejects_undefined_sentinel`). Deleting
//      `'undefined'` would break `ON DELETE SET DEFAULT` for every
//      historical row whose original kind was later removed.
//   4. `list_all` returns rows in `kind ASC` order
//      (`relation_kinds_list_all_returns_seeded_rows_ordered_by_kind`).
//      Stable ordering is part of the operator-facing output contract.

/// Admin pool can write to `relation_kinds`; runtime pool cannot.
///
/// Pins the privilege-bypass property that `connect_admin_pool` exists
/// to deliver. Without this asymmetry the operator-CLI vocabulary slice
/// would have to either widen the runtime role's GRANT shape (defeating
/// migration 0017's REVOKE) or introduce a second non-superuser role
/// (more operational complexity).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_pool_can_write_relation_kinds_while_runtime_pool_cannot() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "rk-ap-d",
        "rk-ap-l",
        &format!("hhagent-pg-relation-kinds-admin-pool-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "admin_pool_can_write_relation_kinds"}),
    )
    .await
    .expect("probe");

    // Runtime-role pool: SELECT works, INSERT denied.
    let runtime_pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM relation_kinds")
        .fetch_one(&runtime_pool)
        .await
        .expect("runtime SELECT");
    assert!(n >= 19, "expected at least the 19 seed rows; got {n}");

    let runtime_add = hhagent_db::relation_kinds::add(
        &runtime_pool,
        "operator-only-kind",
        Some("should be denied to runtime role"),
    )
    .await;
    let err = format!("{:?}", runtime_add.expect_err("runtime add must be denied"));
    assert!(
        err.to_lowercase().contains("permission denied"),
        "expected permission-denied from runtime pool; got: {err}",
    );

    // Admin-pool: same call succeeds.
    let admin_pool = hhagent_db::pool::connect_admin_pool(&cluster.conn_spec)
        .await
        .expect("admin pool");
    let inserted = hhagent_db::relation_kinds::add(
        &admin_pool,
        "operator-only-kind",
        Some("operator-added via admin pool"),
    )
    .await
    .expect("admin add succeeds");
    assert!(inserted, "first admin add must INSERT one row");

    let still_there: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM relation_kinds WHERE kind = 'operator-only-kind'",
    )
    .fetch_one(&admin_pool)
    .await
    .expect("verify operator-only-kind present");
    assert_eq!(still_there, 1, "admin INSERT must have landed exactly one row");

    drop(runtime_pool);
    admin_pool.close().await;
}

/// `add` returns `Ok(true)` on first INSERT, `Ok(false)` on a re-add of
/// the same kind, and preserves the description across the round-trip.
/// Both `Some(desc)` and `None` (NULL) paths exercised.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relation_kinds_add_is_idempotent_and_persists_description() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "rk-add-d",
        "rk-add-l",
        &format!("hhagent-pg-relation-kinds-add-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "relation_kinds_add_idempotent"}),
    )
    .await
    .expect("probe");

    let admin_pool = hhagent_db::pool::connect_admin_pool(&cluster.conn_spec)
        .await
        .expect("admin pool");

    // First add — description present.
    let first = hhagent_db::relation_kinds::add(
        &admin_pool,
        "supervises",
        Some("management relation: subject supervises object"),
    )
    .await
    .expect("first add");
    assert!(first, "first add must INSERT");

    // Re-add — idempotent: no second INSERT, no description rewrite.
    let second = hhagent_db::relation_kinds::add(&admin_pool, "supervises", None)
        .await
        .expect("idempotent re-add");
    assert!(!second, "re-add must be a no-op");

    // Description must be the original Some(...), NOT overwritten by
    // the second call's None.
    let desc: Option<String> = sqlx::query_scalar(
        "SELECT description FROM relation_kinds WHERE kind = 'supervises'",
    )
    .fetch_one(&admin_pool)
    .await
    .expect("read description");
    assert_eq!(
        desc.as_deref(),
        Some("management relation: subject supervises object"),
        "ON CONFLICT DO NOTHING must preserve the original description"
    );

    // Add a different kind with no description.
    let third = hhagent_db::relation_kinds::add(&admin_pool, "tagged_with", None)
        .await
        .expect("add nondesc");
    assert!(third);
    let desc2: Option<String> = sqlx::query_scalar(
        "SELECT description FROM relation_kinds WHERE kind = 'tagged_with'",
    )
    .fetch_one(&admin_pool)
    .await
    .expect("read nondesc");
    assert_eq!(desc2, None, "None description must persist as SQL NULL");

    admin_pool.close().await;
}

/// `remove('undefined')` must be rejected up front (not allowed through
/// to Postgres) because deleting the FK fallback would break
/// `ON DELETE SET DEFAULT` for any historical row whose kind was later
/// removed. Belt-and-braces: pin both the typed error AND that the row
/// is still present in the DB afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relation_kinds_remove_rejects_undefined_sentinel() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "rk-rm-d",
        "rk-rm-l",
        &format!("hhagent-pg-relation-kinds-remove-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "relation_kinds_remove_undefined_reject"}),
    )
    .await
    .expect("probe");

    let admin_pool = hhagent_db::pool::connect_admin_pool(&cluster.conn_spec)
        .await
        .expect("admin pool");

    let r = hhagent_db::relation_kinds::remove(
        &admin_pool,
        hhagent_db::relation_kinds::RELATION_KIND_UNDEFINED,
    )
    .await;
    let err = r.expect_err("undefined removal must be rejected");
    assert!(
        matches!(
            err,
            hhagent_db::relation_kinds::RelationKindError::RemovalOfUndefinedRejected
        ),
        "expected RemovalOfUndefinedRejected; got: {err:?}",
    );

    let still_there: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM relation_kinds WHERE kind = 'undefined'",
    )
    .fetch_one(&admin_pool)
    .await
    .expect("verify undefined still present");
    assert_eq!(
        still_there, 1,
        "'undefined' row must still be present after rejected remove",
    );

    // A legitimate remove of a non-sentinel kind on the same cluster
    // must still work (proves the rejection is targeted, not a
    // permission/wiring problem masquerading as a sentinel reject).
    let inserted =
        hhagent_db::relation_kinds::add(&admin_pool, "throwaway_kind_for_remove_e2e", None)
            .await
            .expect("seed throwaway");
    assert!(inserted);
    let removed =
        hhagent_db::relation_kinds::remove(&admin_pool, "throwaway_kind_for_remove_e2e")
            .await
            .expect("remove throwaway");
    assert!(removed, "real remove must succeed");
    let removed_again =
        hhagent_db::relation_kinds::remove(&admin_pool, "throwaway_kind_for_remove_e2e")
            .await
            .expect("idempotent re-remove");
    assert!(!removed_again, "second remove of same kind must be no-op");

    admin_pool.close().await;
}

/// `list_all` returns every row ordered by `kind ASC`. The seed list
/// alone covers the contract (19 rows, deterministic order). One added
/// row threads in mid-alphabet to prove the ordering is dynamic, not a
/// hard-coded seed-list result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relation_kinds_list_all_returns_seeded_rows_ordered_by_kind() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "rk-ls-d",
        "rk-ls-l",
        &format!("hhagent-pg-relation-kinds-list-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "relation_kinds_list_all"}),
    )
    .await
    .expect("probe");

    let admin_pool = hhagent_db::pool::connect_admin_pool(&cluster.conn_spec)
        .await
        .expect("admin pool");

    // Seed-only baseline: 19 rows.
    let baseline = hhagent_db::relation_kinds::list_all(&admin_pool)
        .await
        .expect("list_all baseline");
    assert_eq!(baseline.len(), 19, "0017 seeds 19 kinds");

    // Pin ascending order on baseline.
    for w in baseline.windows(2) {
        assert!(
            w[0].kind <= w[1].kind,
            "kinds must be sorted ascending: {:?} then {:?}",
            w[0].kind,
            w[1].kind,
        );
    }

    // Add a kind that sorts mid-alphabet so the ordering check is
    // not a tautology of the static seed list.
    hhagent_db::relation_kinds::add(&admin_pool, "mentions", Some("test-only marker"))
        .await
        .expect("seed marker kind");

    let after = hhagent_db::relation_kinds::list_all(&admin_pool)
        .await
        .expect("list_all after");
    assert_eq!(after.len(), 20, "post-add count must be 20");
    for w in after.windows(2) {
        assert!(w[0].kind <= w[1].kind, "still sorted ascending after add");
    }
    // The added row must round-trip with its description intact.
    let marker = after.iter().find(|e| e.kind == "mentions").expect("marker present");
    assert_eq!(marker.description.as_deref(), Some("test-only marker"));

    admin_pool.close().await;
}

// ─── entity_kinds operator CLI surface ─────────────────────────────────
//
// Mirror of the relation_kinds tests above, covering the same four
// load-bearing invariants for the `hhagent-cli entities kinds
// {add,remove,list}` substrate (`db::entity_kinds::{add, remove,
// list_all}` + the shared `db::pool::connect_admin_pool`):
//
//   1. Admin pool can write where runtime pool cannot
//      (`admin_pool_can_write_entity_kinds_while_runtime_pool_cannot`).
//   2. `add` is idempotent
//      (`entity_kinds_add_is_idempotent_and_persists_description`).
//   3. `remove` refuses to delete the FK fallback
//      (`entity_kinds_remove_rejects_undefined_sentinel`).
//   4. `list_all` returns rows in `kind ASC` order
//      (`entity_kinds_list_all_returns_seeded_rows_ordered_by_kind`).
//
// Mostly mechanical mirror of the relation_kinds suite; the cluster-
// per-test cost dominates, the assertions are nearly identical, and
// running both suites against the same migration set proves the
// connect_admin_pool helper handles both REVOKE-protected tables
// (0016 entity_kinds + 0017 relation_kinds) uniformly.

/// Admin pool can write to `entity_kinds`; runtime pool cannot.
/// Twin of `admin_pool_can_write_relation_kinds_while_runtime_pool_cannot`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_pool_can_write_entity_kinds_while_runtime_pool_cannot() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ek-ap-d",
        "ek-ap-l",
        &format!("hhagent-pg-entity-kinds-admin-pool-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "admin_pool_can_write_entity_kinds"}),
    )
    .await
    .expect("probe");

    // Runtime-role pool: SELECT works, INSERT denied.
    let runtime_pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM entity_kinds")
        .fetch_one(&runtime_pool)
        .await
        .expect("runtime SELECT");
    // Migration 0015 seeds 20 rows (1 undefined + 19 starters).
    assert!(n >= 20, "expected at least the 20 seed rows; got {n}");

    let runtime_add = hhagent_db::entity_kinds::add(
        &runtime_pool,
        "operator-only-entity-kind",
        Some("should be denied to runtime role"),
    )
    .await;
    let err = format!("{:?}", runtime_add.expect_err("runtime add must be denied"));
    assert!(
        err.to_lowercase().contains("permission denied"),
        "expected permission-denied from runtime pool; got: {err}",
    );

    // Admin-pool: same call succeeds.
    let admin_pool = hhagent_db::pool::connect_admin_pool(&cluster.conn_spec)
        .await
        .expect("admin pool");
    let inserted = hhagent_db::entity_kinds::add(
        &admin_pool,
        "operator-only-entity-kind",
        Some("operator-added via admin pool"),
    )
    .await
    .expect("admin add succeeds");
    assert!(inserted, "first admin add must INSERT one row");

    let still_there: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM entity_kinds WHERE kind = 'operator-only-entity-kind'",
    )
    .fetch_one(&admin_pool)
    .await
    .expect("verify operator-only-entity-kind present");
    assert_eq!(still_there, 1, "admin INSERT must have landed exactly one row");

    drop(runtime_pool);
    admin_pool.close().await;
}

/// `add` returns `Ok(true)` on first INSERT, `Ok(false)` on a re-add of
/// the same kind, and preserves the description across the round-trip.
/// Twin of `relation_kinds_add_is_idempotent_and_persists_description`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entity_kinds_add_is_idempotent_and_persists_description() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ek-add-d",
        "ek-add-l",
        &format!("hhagent-pg-entity-kinds-add-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "entity_kinds_add_idempotent"}),
    )
    .await
    .expect("probe");

    let admin_pool = hhagent_db::pool::connect_admin_pool(&cluster.conn_spec)
        .await
        .expect("admin pool");

    let first = hhagent_db::entity_kinds::add(
        &admin_pool,
        "research_subject",
        Some("clinical-trial-context individual"),
    )
    .await
    .expect("first add");
    assert!(first, "first add must INSERT");

    // Re-add with None description — must NOT overwrite the original
    // Some(...).
    let second = hhagent_db::entity_kinds::add(&admin_pool, "research_subject", None)
        .await
        .expect("idempotent re-add");
    assert!(!second, "re-add must be a no-op");

    let desc: Option<String> = sqlx::query_scalar(
        "SELECT description FROM entity_kinds WHERE kind = 'research_subject'",
    )
    .fetch_one(&admin_pool)
    .await
    .expect("read description");
    assert_eq!(
        desc.as_deref(),
        Some("clinical-trial-context individual"),
        "ON CONFLICT DO NOTHING must preserve the original description"
    );

    // None description from the start persists as SQL NULL.
    let third = hhagent_db::entity_kinds::add(&admin_pool, "site_visit", None)
        .await
        .expect("add nondesc");
    assert!(third);
    let desc2: Option<String> = sqlx::query_scalar(
        "SELECT description FROM entity_kinds WHERE kind = 'site_visit'",
    )
    .fetch_one(&admin_pool)
    .await
    .expect("read nondesc");
    assert_eq!(desc2, None);

    admin_pool.close().await;
}

/// `remove('undefined')` must be rejected up front with the typed
/// `RemovalOfUndefinedRejected` error; the DB row must survive; a
/// legitimate non-sentinel remove on the same cluster still works.
/// Twin of `relation_kinds_remove_rejects_undefined_sentinel`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entity_kinds_remove_rejects_undefined_sentinel() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ek-rm-d",
        "ek-rm-l",
        &format!("hhagent-pg-entity-kinds-remove-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "entity_kinds_remove_undefined_reject"}),
    )
    .await
    .expect("probe");

    let admin_pool = hhagent_db::pool::connect_admin_pool(&cluster.conn_spec)
        .await
        .expect("admin pool");

    let r = hhagent_db::entity_kinds::remove(
        &admin_pool,
        hhagent_db::entity_kinds::ENTITY_KIND_UNDEFINED,
    )
    .await;
    let err = r.expect_err("undefined removal must be rejected");
    assert!(
        matches!(
            err,
            hhagent_db::entity_kinds::EntityKindError::RemovalOfUndefinedRejected
        ),
        "expected RemovalOfUndefinedRejected; got: {err:?}",
    );

    let still_there: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM entity_kinds WHERE kind = 'undefined'",
    )
    .fetch_one(&admin_pool)
    .await
    .expect("verify undefined still present");
    assert_eq!(still_there, 1);

    // Sanity: non-sentinel kinds ARE removable on the same cluster.
    // Use an operator-added throwaway so we don't disturb the
    // production-seed list (entity_kinds seeds are referenced by the
    // production hot path; removing one would surprise downstream
    // assertions in other tests if they shared the cluster, which
    // they don't, but defence-in-depth is cheap).
    let inserted =
        hhagent_db::entity_kinds::add(&admin_pool, "throwaway_kind_for_remove_e2e", None)
            .await
            .expect("seed throwaway");
    assert!(inserted);
    let removed =
        hhagent_db::entity_kinds::remove(&admin_pool, "throwaway_kind_for_remove_e2e")
            .await
            .expect("remove throwaway");
    assert!(removed, "real remove must succeed");
    let removed_again =
        hhagent_db::entity_kinds::remove(&admin_pool, "throwaway_kind_for_remove_e2e")
            .await
            .expect("idempotent re-remove");
    assert!(!removed_again);

    admin_pool.close().await;
}

/// `list_all` returns every row ordered by `kind ASC`. 0015 seeds 20
/// rows; a mid-alphabet operator addition lands sorted into the
/// middle of the list (proves the ordering is dynamic not a hard-
/// coded result). Twin of
/// `relation_kinds_list_all_returns_seeded_rows_ordered_by_kind`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entity_kinds_list_all_returns_seeded_rows_ordered_by_kind() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ek-ls-d",
        "ek-ls-l",
        &format!("hhagent-pg-entity-kinds-list-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "entity_kinds_list_all"}),
    )
    .await
    .expect("probe");

    let admin_pool = hhagent_db::pool::connect_admin_pool(&cluster.conn_spec)
        .await
        .expect("admin pool");

    // Seed-only baseline: 20 rows (1 undefined + 19 starters per 0015).
    let baseline = hhagent_db::entity_kinds::list_all(&admin_pool)
        .await
        .expect("list_all baseline");
    assert_eq!(baseline.len(), 20, "0015 seeds 20 entity kinds");

    for w in baseline.windows(2) {
        assert!(
            w[0].kind <= w[1].kind,
            "kinds must be sorted ascending: {:?} then {:?}",
            w[0].kind,
            w[1].kind,
        );
    }

    // Mid-alphabet add proves ordering is dynamic.
    hhagent_db::entity_kinds::add(&admin_pool, "intermediate", Some("test-only marker"))
        .await
        .expect("seed marker kind");

    let after = hhagent_db::entity_kinds::list_all(&admin_pool)
        .await
        .expect("list_all after");
    assert_eq!(after.len(), 21, "post-add count must be 21");
    for w in after.windows(2) {
        assert!(w[0].kind <= w[1].kind, "still sorted ascending after add");
    }
    let marker = after.iter().find(|e| e.kind == "intermediate").expect("marker present");
    assert_eq!(marker.description.as_deref(), Some("test-only marker"));

    admin_pool.close().await;
}

// ─── kinds `list` runtime-pool path (#111 item 1) ──────────────────────
//
// Both `entities kinds list` and `relations kinds list` previously rode
// on `connect_admin_pool` even though the operation is SELECT-only and
// the runtime role has SELECT granted (migrations 0015 + 0017).
// Switching the production code to `connect_runtime_pool` lets the
// `list` action work for operators without cluster-superuser
// peer-auth — useful in a future deployment where peer-auth ≠ admin
// role. These two tests pin the SELECT-via-runtime-pool path as
// load-bearing now that the production code depends on it.
//
// **What these tests do NOT pin:** the original
// `admin_pool_can_write_*_while_runtime_pool_cannot` tests pin the
// asymmetric write-privilege shape (admin = full CRUD; runtime =
// SELECT only). Together with the new tests below the pool privilege
// surface is fully covered: admin can read+write; runtime can read,
// cannot write.

/// Runtime-role pool can read the full `relation_kinds` table via
/// `list_all`. Pins the SELECT-via-runtime-pool path used by
/// `hhagent-cli relations kinds list` post-#111 item 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relation_kinds_list_all_via_runtime_pool_returns_seed_rows() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "rk-rl-d",
        "rk-rl-l",
        &format!("hhagent-pg-relation-kinds-runtime-list-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "relation_kinds_list_all_via_runtime_pool"}),
    )
    .await
    .expect("probe");

    let runtime_pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    let entries = hhagent_db::relation_kinds::list_all(&runtime_pool)
        .await
        .expect("list_all via runtime pool must succeed (GRANT SELECT covers it)");
    assert_eq!(entries.len(), 19, "0017 seeds 19 relation kinds");
    // Spot-check the FK-fallback sentinel is present (a missing
    // 'undefined' row would break the `relations.kind` FK).
    assert!(
        entries.iter().any(|e| e.kind == "undefined"),
        "the 'undefined' FK fallback must be in the seed set",
    );

    runtime_pool.close().await;
}

/// Runtime-role pool can read the full `entity_kinds` table via
/// `list_all`. Pins the SELECT-via-runtime-pool path used by
/// `hhagent-cli entities kinds list` post-#111 item 1. Twin of
/// `relation_kinds_list_all_via_runtime_pool_returns_seed_rows`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entity_kinds_list_all_via_runtime_pool_returns_seed_rows() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ek-rl-d",
        "ek-rl-l",
        &format!("hhagent-pg-entity-kinds-runtime-list-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "entity_kinds_list_all_via_runtime_pool"}),
    )
    .await
    .expect("probe");

    let runtime_pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    let entries = hhagent_db::entity_kinds::list_all(&runtime_pool)
        .await
        .expect("list_all via runtime pool must succeed (GRANT SELECT covers it)");
    assert_eq!(entries.len(), 20, "0015 seeds 20 entity kinds");
    assert!(
        entries.iter().any(|e| e.kind == "undefined"),
        "the 'undefined' fallback must be in the seed set",
    );

    runtime_pool.close().await;
}

// ─── Graph::walk_outbound_edges / walk_inbound_edges (Next-TODO Item 21) ───
//
// These tests pin the shape of the new operator-facing graph-walking
// methods that back `hhagent-cli relations show <entity-id>`. The
// fixture builds a small clinical-style graph:
//
//     dr_smith --[treats]----> asthma --[has_symptom]--> wheezing
//     dr_smith --[prescribed]-> salbutamol
//     patient_jane --[consulted]-> dr_smith   (inbound to dr_smith)
//     dr_smith --[employed by]-> clinic_a
//     [cycle test] alice --[knows]-> bob --[knows]-> alice
//
// Each test re-seeds against its own per-test cluster so they can run
// in parallel without colliding on a shared PG.

/// Helper: bring up a per-test PG cluster + run probe + return a
/// runtime-role pool. The 5 walk-tests below share this shape so
/// extracting it keeps each test focused on the assertion it owns.
async fn bring_up_pg_for_walk_test(test_label: &str) -> (PgClusterGuard, sqlx::PgPool) {
    let bin_dir = pg_bin_dir_or_skip().expect("pg bin dir (skip handled by caller)");
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        &format!("{test_label}-d"),
        &format!("{test_label}-l"),
        &format!("hhagent-pg-{test_label}-{suffix}"),
    );

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": test_label}),
    )
    .await
    .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("pool");

    (cluster, pool)
}

/// Local re-alias of the cluster type so the helper above has a
/// concrete return type. The actual struct lives in
/// `hhagent_tests_common`; we don't move it.
type PgClusterGuard = hhagent_tests_common::PgCluster;

/// Empty seed (no outbound edges) returns an empty Vec — no
/// `Option::None` ambiguity, no SQL-level NULL row, no panic.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn walk_outbound_edges_empty_seed_returns_empty() {
    if skip_if_no_supervisor() {
        return;
    }
    if pg_bin_dir_or_skip().is_none() {
        return;
    }

    let (_cluster, pool) = bring_up_pg_for_walk_test("walk-empty").await;
    let g = hhagent_db::graph::PgGraph::new(&pool);

    use hhagent_db::graph::Graph;
    let lonely = g
        .upsert_entity("person", "loner", &serde_json::json!({}))
        .await
        .expect("upsert lonely");

    let edges = g
        .walk_outbound_edges(lonely, 5, 10_000)
        .await
        .expect("walk_outbound_edges");

    assert!(
        edges.is_empty(),
        "isolated entity has no outbound edges; got {edges:?}",
    );

    // Same property for inbound — the seed has nothing pointing to it.
    let edges = g
        .walk_inbound_edges(lonely, 5, 10_000)
        .await
        .expect("walk_inbound_edges");
    assert!(
        edges.is_empty(),
        "isolated entity has no inbound edges; got {edges:?}",
    );

    pool.close().await;
}

/// `max_depth == 0` is a degenerate call shape (no edges to walk).
/// Returns an empty Vec without a DB round-trip — the short-circuit is
/// load-bearing for callers that thread `max_depth` from `--depth N`
/// without re-validating zero.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn walk_outbound_edges_max_depth_zero_returns_empty() {
    if skip_if_no_supervisor() {
        return;
    }
    if pg_bin_dir_or_skip().is_none() {
        return;
    }

    let (_cluster, pool) = bring_up_pg_for_walk_test("walk-d0").await;
    let g = hhagent_db::graph::PgGraph::new(&pool);

    use hhagent_db::graph::Graph;
    // Seed a real edge so the test would fail noisily if max_depth=0
    // *did* somehow surface a row.
    let a = g.upsert_entity("person", "a", &serde_json::json!({})).await.unwrap();
    let b = g.upsert_entity("person", "b", &serde_json::json!({})).await.unwrap();
    g.upsert_relation(a, b, "knows", &serde_json::json!({})).await.unwrap();

    assert_eq!(
        g.walk_outbound_edges(a, 0, 10_000).await.unwrap().len(),
        0,
        "max_depth=0 must return empty",
    );
    assert_eq!(
        g.walk_inbound_edges(b, 0, 10_000).await.unwrap().len(),
        0,
        "max_depth=0 must return empty (inbound)",
    );

    pool.close().await;
}

/// 1-hop outbound walk returns the seed's direct edges with full
/// endpoint metadata, sorted by edge id (ascending) within the depth=1
/// band. Cross-checks edge `kind`, both endpoints' `kind`/`name`/
/// `quarantine` columns, and that the seed (`dr_smith`) is the `src`
/// of every emitted edge.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn walk_outbound_edges_one_hop_returns_direct_edges() {
    if skip_if_no_supervisor() {
        return;
    }
    if pg_bin_dir_or_skip().is_none() {
        return;
    }

    let (_cluster, pool) = bring_up_pg_for_walk_test("walk-1hop").await;
    let g = hhagent_db::graph::PgGraph::new(&pool);

    use hhagent_db::graph::Graph;
    // Seed in deterministic insertion order so edge_id ordering is
    // predictable.
    // All kinds used here are pre-seeded vocabulary: `person` /
    // `disease` / `drug` from 0015's entity_kinds; `treats` /
    // `prescribed` from 0017's relation_kinds. The FK on
    // entities.kind + relations.kind would reject anything else.
    let dr = g
        .upsert_entity("person", "Dr Smith", &serde_json::json!({}))
        .await
        .expect("upsert dr_smith");
    let asthma = g
        .upsert_entity("disease", "asthma", &serde_json::json!({}))
        .await
        .expect("upsert asthma");
    let salbutamol = g
        .upsert_entity("drug", "salbutamol", &serde_json::json!({}))
        .await
        .expect("upsert salbutamol");

    // Flip dr_smith out of quarantine so we can pin both states in one test.
    sqlx::query("UPDATE entities SET quarantine = FALSE WHERE id = $1")
        .bind(dr)
        .execute(&pool)
        .await
        .expect("approve dr_smith");

    let e1 = g.upsert_relation(dr, asthma, "treats", &serde_json::json!({})).await.unwrap();
    let e2 = g.upsert_relation(dr, salbutamol, "prescribed", &serde_json::json!({})).await.unwrap();
    assert!(e1 < e2, "insertion order should give e1 < e2 for stable depth-1 ordering");

    let edges = g
        .walk_outbound_edges(dr, 1, 10_000)
        .await
        .expect("walk_outbound_edges");

    assert_eq!(edges.len(), 2, "two 1-hop outbound edges from dr_smith");

    // First edge: depth=1, src=dr_smith (NOT quarantined), dst=asthma (quarantined), kind=treats.
    assert_eq!(edges[0].depth, 1);
    assert_eq!(edges[0].edge_id, e1);
    assert_eq!(edges[0].src_id, dr);
    assert_eq!(edges[0].src_kind, "person");
    assert_eq!(edges[0].src_name, "Dr Smith");
    assert!(!edges[0].src_quarantine, "dr_smith was approved before the walk");
    assert_eq!(edges[0].dst_id, asthma);
    assert_eq!(edges[0].dst_kind, "disease");
    assert_eq!(edges[0].dst_name, "asthma");
    assert!(edges[0].dst_quarantine, "asthma was never approved");
    assert_eq!(edges[0].kind, "treats");

    // Second edge — same shape, different kind/dst.
    assert_eq!(edges[1].depth, 1);
    assert_eq!(edges[1].edge_id, e2);
    assert_eq!(edges[1].dst_id, salbutamol);
    assert_eq!(edges[1].kind, "prescribed");

    pool.close().await;
}

/// N-hop walk reaches depth N's edges but stops there — `depth ≤
/// max_depth` is the contract. Fixture: a chain of 3 edges
/// (`dr_smith → asthma → wheezing → cough`). With `max_depth=2` we
/// must see exactly the first two; with `max_depth=3` all three.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn walk_outbound_edges_respects_max_depth_bound() {
    if skip_if_no_supervisor() {
        return;
    }
    if pg_bin_dir_or_skip().is_none() {
        return;
    }

    let (_cluster, pool) = bring_up_pg_for_walk_test("walk-depth").await;
    let g = hhagent_db::graph::PgGraph::new(&pool);

    use hhagent_db::graph::Graph;
    // All entity/relation kinds are pre-seeded vocabulary.
    let a = g.upsert_entity("person", "Dr Smith", &serde_json::json!({})).await.unwrap();
    let b = g.upsert_entity("disease", "asthma", &serde_json::json!({})).await.unwrap();
    let c = g.upsert_entity("symptom", "wheezing", &serde_json::json!({})).await.unwrap();
    let d = g.upsert_entity("symptom", "cough", &serde_json::json!({})).await.unwrap();
    g.upsert_relation(a, b, "treats", &serde_json::json!({})).await.unwrap();
    g.upsert_relation(b, c, "has symptom", &serde_json::json!({})).await.unwrap();
    g.upsert_relation(c, d, "associated with", &serde_json::json!({})).await.unwrap();

    let d2 = g.walk_outbound_edges(a, 2, 10_000).await.unwrap();
    assert_eq!(d2.len(), 2, "max_depth=2 returns 2 edges");
    assert_eq!(d2[0].depth, 1);
    assert_eq!(d2[1].depth, 2);

    let d3 = g.walk_outbound_edges(a, 3, 10_000).await.unwrap();
    assert_eq!(d3.len(), 3, "max_depth=3 returns all 3 edges");
    assert_eq!(d3[2].depth, 3);
    assert_eq!(d3[2].dst_id, d);
    assert_eq!(d3[2].kind, "associated with");

    pool.close().await;
}

/// A cycle in the graph (`alice → bob → alice`) must not diverge the
/// recursive CTE. The visited-set tracking in the CTE refuses to
/// re-enter a previously-visited node on the same path; the second
/// edge (`bob → alice`) is dropped at the recursive step because
/// `alice` is already in `visited`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn walk_outbound_edges_terminates_on_cycle() {
    if skip_if_no_supervisor() {
        return;
    }
    if pg_bin_dir_or_skip().is_none() {
        return;
    }

    let (_cluster, pool) = bring_up_pg_for_walk_test("walk-cycle").await;
    let g = hhagent_db::graph::PgGraph::new(&pool);

    use hhagent_db::graph::Graph;
    let alice = g.upsert_entity("person", "alice", &serde_json::json!({})).await.unwrap();
    let bob = g.upsert_entity("person", "bob", &serde_json::json!({})).await.unwrap();

    let e_ab = g.upsert_relation(alice, bob, "knows", &serde_json::json!({})).await.unwrap();
    let _e_ba = g.upsert_relation(bob, alice, "knows", &serde_json::json!({})).await.unwrap();

    // With max_depth=5 and a 2-node cycle, we expect just the depth=1
    // outbound edge from alice (alice → bob); the recursive step would
    // try (bob → alice) but `alice` is already visited so it's
    // filtered. Without the visited-set guard this would diverge until
    // LIMIT clipped it.
    let edges = g.walk_outbound_edges(alice, 5, 10_000).await.unwrap();
    assert_eq!(edges.len(), 1, "cycle must not produce extra rows");
    assert_eq!(edges[0].edge_id, e_ab);
    assert_eq!(edges[0].depth, 1);

    pool.close().await;
}

/// Inbound walk surfaces edges that *point to* the seed; each
/// `WalkedEdge` keeps the canonical `(src, kind, dst)` orientation as
/// it lives in `relations`, so a caller can mix outbound + inbound
/// results without having to invert orientations.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn walk_inbound_edges_preserves_canonical_orientation() {
    if skip_if_no_supervisor() {
        return;
    }
    if pg_bin_dir_or_skip().is_none() {
        return;
    }

    let (_cluster, pool) = bring_up_pg_for_walk_test("walk-inbound").await;
    let g = hhagent_db::graph::PgGraph::new(&pool);

    use hhagent_db::graph::Graph;
    // `associated with` + `refers to` are pre-seeded relation kinds.
    // `patient` is a pre-seeded entity kind.
    let dr = g.upsert_entity("person", "Dr Smith", &serde_json::json!({})).await.unwrap();
    let jane = g.upsert_entity("patient", "Jane Doe", &serde_json::json!({})).await.unwrap();
    let referrer = g.upsert_entity("person", "Dr Brown", &serde_json::json!({})).await.unwrap();

    // jane --[associated with]-> dr  (clinical consult relation)
    let e1 = g.upsert_relation(jane, dr, "associated with", &serde_json::json!({})).await.unwrap();
    // referrer --[refers to]-> jane (depth-2 inbound from dr's POV)
    let _e2 = g.upsert_relation(referrer, jane, "refers to", &serde_json::json!({})).await.unwrap();

    // 1-hop inbound: just the jane→dr edge.
    let d1 = g.walk_inbound_edges(dr, 1, 10_000).await.unwrap();
    assert_eq!(d1.len(), 1);
    assert_eq!(d1[0].edge_id, e1);
    assert_eq!(d1[0].depth, 1);
    assert_eq!(d1[0].src_id, jane, "canonical src is still jane");
    assert_eq!(d1[0].dst_id, dr, "canonical dst is still dr");
    assert_eq!(d1[0].kind, "associated with");

    // 2-hop inbound: also referrer→jane (which arrives upstream of dr).
    let d2 = g.walk_inbound_edges(dr, 2, 10_000).await.unwrap();
    assert_eq!(d2.len(), 2);
    assert_eq!(d2[1].depth, 2);
    assert_eq!(d2[1].src_id, referrer);
    assert_eq!(d2[1].dst_id, jane);
    assert_eq!(d2[1].kind, "refers to");

    pool.close().await;
}

/// `limit` is honoured SQL-side. Fixture: seed entity with 20
/// outbound edges; pass `limit=5` and confirm exactly 5 rows return.
/// Defends against a future refactor that accidentally drops the
/// `LIMIT $3` clause (or moves it to client-side).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn walk_outbound_edges_honours_limit_argument() {
    if skip_if_no_supervisor() {
        return;
    }
    if pg_bin_dir_or_skip().is_none() {
        return;
    }

    let (_cluster, pool) = bring_up_pg_for_walk_test("walk-limit").await;
    let g = hhagent_db::graph::PgGraph::new(&pool);

    use hhagent_db::graph::Graph;
    let hub = g.upsert_entity("person", "hub", &serde_json::json!({})).await.unwrap();
    for i in 0..20 {
        let other = g
            .upsert_entity("person", &format!("p{i:02}"), &serde_json::json!({}))
            .await
            .unwrap();
        g.upsert_relation(hub, other, "knows", &serde_json::json!({})).await.unwrap();
    }

    let edges = g.walk_outbound_edges(hub, 1, 5).await.unwrap();
    assert_eq!(edges.len(), 5, "LIMIT 5 must clip to 5 rows");

    let edges = g.walk_outbound_edges(hub, 1, 100).await.unwrap();
    assert_eq!(edges.len(), 20, "LIMIT >> fan-out returns all rows");

    pool.close().await;
}

/// Diamond-topology regression pin for [issue
/// #114](https://github.com/hherb/hhagent/issues/114): a unique
/// `edge_id` reachable by *multiple* paths from the seed must appear
/// exactly once in the result, anchored to its **shortest-path
/// depth**.
///
/// ### Fixture
///
/// ```text
///   A --[knows]--> B
///   A --[knows]--> C
///   B --[knows]--> C   (creates the diamond)
///   C --[knows]--> D
/// ```
///
/// Walking outbound from `A` at `max_depth = 3`:
///
/// - depth 1: `A→B`, `A→C` (one row each — direct neighbours).
/// - depth 2: from `A→B`'s frontier: `B→C`; from `A→C`'s frontier: `C→D`.
/// - depth 3: from `A→B→C`'s frontier: `C→D` *again* — same `edge_id`,
///   reached via the longer A-B-C path. The visited-set blocks cycles
///   per-path but does NOT prevent the same edge from being surfaced
///   via two different paths.
///
/// **Pre-fix:** the outer SELECT projects one row per traversal, so
/// `edge_CD` appears twice (depth=2 via A-C, depth=3 via A-B-C).
///
/// **Post-fix:** `DISTINCT ON (edge_id) ORDER BY edge_id, depth ASC`
/// keeps the shortest-depth row per unique `edge_id`. `edge_CD`
/// appears exactly once with `depth=2`.
///
/// Mirrored for inbound from `D` for parity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn walk_edges_dedupes_diamond_topology_to_shortest_depth() {
    if skip_if_no_supervisor() {
        return;
    }
    if pg_bin_dir_or_skip().is_none() {
        return;
    }

    let (_cluster, pool) = bring_up_pg_for_walk_test("walk-dmnd").await;
    let g = hhagent_db::graph::PgGraph::new(&pool);

    use hhagent_db::graph::Graph;
    let a = g.upsert_entity("person", "a", &serde_json::json!({})).await.unwrap();
    let b = g.upsert_entity("person", "b", &serde_json::json!({})).await.unwrap();
    let c = g.upsert_entity("person", "c", &serde_json::json!({})).await.unwrap();
    let d = g.upsert_entity("person", "d", &serde_json::json!({})).await.unwrap();
    let e_ab = g.upsert_relation(a, b, "knows", &serde_json::json!({})).await.unwrap();
    let e_ac = g.upsert_relation(a, c, "knows", &serde_json::json!({})).await.unwrap();
    let e_bc = g.upsert_relation(b, c, "knows", &serde_json::json!({})).await.unwrap();
    let e_cd = g.upsert_relation(c, d, "knows", &serde_json::json!({})).await.unwrap();

    // Outbound walk from A — 4 unique edges, no duplicates.
    let outbound = g.walk_outbound_edges(a, 3, 10_000).await.unwrap();
    let outbound_ids: Vec<i64> = outbound.iter().map(|e| e.edge_id).collect();
    let mut unique_outbound = outbound_ids.clone();
    unique_outbound.sort();
    unique_outbound.dedup();
    assert_eq!(
        outbound_ids.len(),
        unique_outbound.len(),
        "outbound walk must surface each unique edge_id exactly once; \
         got duplicates: {outbound_ids:?}",
    );
    assert_eq!(outbound.len(), 4, "expected 4 unique edges; got {outbound:?}");

    // The diamond's "bottom" edge C→D is reachable by two paths
    // (A-C-D shortest, A-B-C-D longest). The kept row must carry the
    // shortest depth.
    let cd_rows: Vec<&hhagent_db::graph::WalkedEdge> =
        outbound.iter().filter(|e| e.edge_id == e_cd).collect();
    assert_eq!(cd_rows.len(), 1, "edge_CD must appear exactly once");
    assert_eq!(
        cd_rows[0].depth, 2,
        "edge_CD must be anchored to its shortest path depth (A-C-D, depth=2), \
         not the longer A-B-C-D (depth=3)",
    );

    // The other three edges are each reached by exactly one path; pin
    // their depths anyway as a guardrail on the rest of the result.
    let depth_for = |id: i64| outbound.iter().find(|e| e.edge_id == id).map(|e| e.depth);
    assert_eq!(depth_for(e_ab), Some(1), "A→B is a direct neighbour");
    assert_eq!(depth_for(e_ac), Some(1), "A→C is a direct neighbour");
    assert_eq!(depth_for(e_bc), Some(2), "B→C reached via A-B path");

    // Mirror: walking INBOUND from D should also dedupe — same diamond,
    // same shortest-depth invariant. C→D is the only incoming edge to D,
    // and B→C / A→C / A→B all sit upstream. At max_depth=3 the inbound
    // walk should surface all 4 unique edges, with no duplicates and
    // C→D anchored at depth=1 (it points directly at D).
    let inbound = g.walk_inbound_edges(d, 3, 10_000).await.unwrap();
    let inbound_ids: Vec<i64> = inbound.iter().map(|e| e.edge_id).collect();
    let mut unique_inbound = inbound_ids.clone();
    unique_inbound.sort();
    unique_inbound.dedup();
    assert_eq!(
        inbound_ids.len(),
        unique_inbound.len(),
        "inbound walk must surface each unique edge_id exactly once; \
         got duplicates: {inbound_ids:?}",
    );

    pool.close().await;
}
