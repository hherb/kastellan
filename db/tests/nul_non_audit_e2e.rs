//! PG-gated e2e for issue #818: a NUL in worker- or peer-influenced text
//! must not silently fail a non-audit write.
//!
//! Postgres refuses U+0000 in `jsonb` (`unsupported Unicode escape
//! sequence`) and in `text` (`invalid byte sequence for encoding "UTF8":
//! 0x00`). #816 fixed `audit_log`; this file pins the rule for the rest,
//! stated in `kastellan_db::nul`: **escape records, refuse identities.**
//!
//! * records — `tasks.result` / `tasks.turn_record` (`tasks::finalize`) and
//!   `asks.body` / `asks.resume_state` (`asks::raise`) — land with each NUL
//!   rewritten to `␀`. Before #818 a failed `finalize` left the task
//!   `running` with no reply, and a failed `raise` made escalation
//!   impossible for any run whose tool output held a NUL.
//! * identities and long-lived knowledge — `tasks.payload` (it carries the
//!   peer and conversation), `memories`, `pairings` — are refused with a
//!   typed [`DbError::NulRefused`] before any SQL runs, naming the column.
//!
//! The test opens with **positive controls**: the same values bound raw must
//! still be refused by Postgres, once for `jsonb` and once for `text`.
//! Without them a future Postgres that accepted NUL would leave the rest of
//! this file passing while proving nothing.
//!
//! Skip-as-pass without a supervisor/PG (root CI container, a Mac without
//! `KASTELLAN_PG_BIN_DIR`); live wherever a cluster can be brought up, and
//! demanded by the `pg` profile of `scripts/run-e2e-gate.sh`.

use kastellan_db::memories::{insert_memory_at_layer, MemoryLayer};
use kastellan_db::nul::NUL_ESCAPE;
use kastellan_db::tasks::{self, Lane};
use kastellan_db::{asks, pairings, DbError};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};
use serde_json::json;

/// The refusal must be the typed one, naming exactly this column.
fn assert_refused<T: std::fmt::Debug>(r: Result<T, DbError>, column: &str) {
    match r {
        Err(DbError::NulRefused { column: c }) => assert_eq!(c, column),
        other => panic!("expected NulRefused {{ column: {column} }}, got {other:?}"),
    }
}

#[test]
fn nul_in_non_audit_writes_is_escaped_in_records_and_refused_in_identities() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "nul2-d",
        "nul2-l",
        &format!("kastellan-supervisor-test-pg-nul2-{suffix}"),
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
            json!({"version": "test", "purpose": "nul-non-audit-e2e"}),
        )
        .await
        .expect("probe run");

        // The runtime pool is the role the daemon writes through; the admin
        // pool only mints the pairing code the claim below needs.
        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("runtime pool");
        let admin = kastellan_db::pool::connect_admin_pool(&cluster.conn_spec)
            .await
            .expect("admin pool");
        let esc = NUL_ESCAPE;

        // ── Positive controls: raw NUL really is refused, jsonb and text. ──
        let raw_json = sqlx::query("INSERT INTO tasks (state, lane, payload) VALUES ('pending', 'fast', $1)")
            .bind(json!({"instruction": "a\u{0}b"}))
            .execute(&pool)
            .await
            .expect_err("a raw NUL in jsonb must be refused, or this file proves nothing");
        assert!(
            raw_json.to_string().contains("unsupported Unicode escape sequence"),
            "the jsonb control must fail for the NUL: {raw_json}"
        );
        let raw_text = sqlx::query("SELECT EXISTS(SELECT 1 FROM pairings WHERE peer = $1)")
            .bind("@mallory\u{0}:example.org")
            .fetch_one(&pool)
            .await
            .expect_err("a raw NUL in a text bind must be refused, or this file proves nothing");
        assert!(
            raw_text.to_string().contains("0x00"),
            "the text control must fail for the NUL: {raw_text}"
        );

        // ── Identity: a task payload carrying a NUL is refused, typed. ──
        assert_refused(
            tasks::insert_pending(&pool, Lane::Fast, json!({"peer": "@m\u{0}:x", "instruction": "hi"}))
                .await,
            "tasks.payload",
        );

        // ── Record: finalize escapes result and turn_record, and the task
        //    really reaches its terminal state. ──
        let task_id = tasks::insert_pending(&pool, Lane::Fast, json!({"instruction": "finalize probe"}))
            .await
            .expect("insert a clean task");
        let claimed = tasks::claim_one(&pool, Lane::Fast, 60)
            .await
            .expect("claim")
            .expect("a pending task");
        assert_eq!(claimed.id, task_id);
        tasks::finalize(
            &pool,
            task_id,
            "completed",
            Some(json!({"body": "the id is <a\u{0}b>"})),
            Some(json!({"steps": [{"parameters": {"message_id": "x\u{0}y"}}]})),
        )
        .await
        .expect("finalize must land a result carrying NUL (#818)");
        let done = tasks::get(&pool, task_id).await.expect("get").expect("the task");
        assert_eq!(
            done.state, "completed",
            "before #818 the UPDATE failed and the task stayed running with no reply"
        );
        assert_eq!(done.result, Some(json!({"body": format!("the id is <a{esc}b>")})));
        let turn: serde_json::Value =
            sqlx::query_scalar("SELECT turn_record FROM tasks WHERE id = $1")
                .bind(task_id)
                .fetch_one(&pool)
                .await
                .expect("read turn_record");
        assert_eq!(turn["steps"][0]["parameters"]["message_id"], json!(format!("x{esc}y")));

        // ── Record: an ask carrying NUL in body and resume_state is raised. ──
        let ask_task = tasks::insert_pending(&pool, Lane::Fast, json!({"instruction": "ask probe"}))
            .await
            .expect("insert");
        tasks::claim_one(&pool, Lane::Fast, 60).await.expect("claim").expect("pending");
        let raised = asks::raise(
            &pool,
            ask_task,
            "plan_approval",
            "approve sending to a\u{0}b?",
            &json!(["approve", "deny"]),
            Some("digest"),
            time::OffsetDateTime::now_utc() + time::Duration::seconds(300),
            Some(&json!({"outcomes": [{"ok": {"k\u{0}": "worker said \u{0}"}}]})),
        )
        .await
        .expect("an escalation must not be made impossible by a NUL in a tool result (#818)");
        let ask = asks::get(&pool, raised.ask_id).await.expect("get").expect("the ask");
        assert_eq!(ask.body, format!("approve sending to a{esc}b?"));
        assert_eq!(
            ask.resume_state,
            Some(json!({"outcomes": [{"ok": {format!("k{esc}"): format!("worker said {esc}")}}]}))
        );

        // ── Long-lived knowledge: memories refuse, body and metadata. ──
        assert_refused(
            insert_memory_at_layer(&pool, "a\u{0}b", &json!({}), None, MemoryLayer::Skill).await,
            "memories.body",
        );
        assert_refused(
            insert_memory_at_layer(&pool, "ok", &json!({"p": "\u{0}"}), None, MemoryLayer::Skill)
                .await,
            "memories.metadata",
        );
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM memories WHERE layer = 3")
            .fetch_one(&pool)
            .await
            .expect("count");
        // Belt and braces: the typed refusals above already prove the check
        // runs before SQL; this only pins that no row slipped in regardless.
        assert_eq!(n, 0, "a refused memory leaves no row");

        // ── Identity: pairings refuse a NUL peer; a lookup answers "no". ──
        assert_refused(
            pairings::insert_pairing(&pool, "matrix", "@m\u{0}:x", "code").await,
            "pairings.peer",
        );
        assert_refused(
            pairings::insert_pairing_with_token(&admin, "email", "a\u{0}@x", "operator", None).await,
            "pairings.peer",
        );
        let code_hash = asks::sha256_hex("pairing-code-818");
        pairings::insert_code(&admin, &code_hash, None, 10).await.expect("mint a code");
        assert_refused(
            pairings::claim_code(&pool, &code_hash, "matrix/@m\u{0}:x").await,
            "pairing_codes.consumed_by",
        );
        assert!(
            pairings::any_active_code(&pool).await.expect("any_active_code"),
            "a refused claim must not consume the code"
        );
        assert_eq!(
            pairings::token_hash_for(&pool, "matrix", "@m\u{0}:x").await.expect("lookup"),
            None,
            "no stored peer can hold a NUL, so the truthful answer is 'not paired'"
        );
        assert!(!pairings::is_paired(&pool, "matrix", "@m\u{0}:x").await.expect("is_paired"));

        // ── The escape needs a UTF8 database; boot refuses any other. ──
        // `CREATE DATABASE … ENCODING` is the one way to get a non-UTF8
        // database now that `build_initdb_argv` always asks for UTF8.
        sqlx::query(
            "CREATE DATABASE kastellan_ascii ENCODING 'SQL_ASCII' \
             LC_COLLATE 'C' LC_CTYPE 'C' TEMPLATE template0",
        )
        .execute(&admin)
        .await
        .expect("create a SQL_ASCII database");
        let mut ascii = cluster.conn_spec.clone();
        ascii.database = "kastellan_ascii".into();
        match kastellan_db::probe::run(&ascii, "core", "startup", json!({})).await {
            Err(DbError::PolicyViolation(msg)) => {
                assert!(msg.contains("SQL_ASCII") && msg.contains("UTF8"), "{msg}")
            }
            other => panic!("boot must refuse a non-UTF8 database, got {other:?}"),
        }
        // …and refuse it before migrating: nothing may have been written.
        let ascii_admin = kastellan_db::pool::connect_admin_pool(&ascii)
            .await
            .expect("admin pool on the SQL_ASCII database");
        let untouched: bool = sqlx::query_scalar(
            "SELECT to_regclass('_sqlx_migrations') IS NULL AND to_regclass('audit_log') IS NULL",
        )
        .fetch_one(&ascii_admin)
        .await
        .expect("catalog lookup");
        assert!(untouched, "the encoding check must run before migrations");
        ascii_admin.close().await;

        pool.close().await;
        admin.close().await;
    });
}
