//! PG-gated e2e for issue #816: an audit row whose payload carries a NUL
//! must still land.
//!
//! Postgres refuses U+0000 anywhere in `jsonb` (`unsupported Unicode escape
//! sequence`) and in `text` (an invalid byte sequence). A compromised channel worker is in scope
//! (`docs/threat-model.md`), and it controls strings that end up in audit
//! payloads — a peer id, a message id, a tool result. Before #816 a single
//! NUL in one of them failed the whole insert, so a hostile peer could erase
//! every row about itself. `kastellan_db::audit::insert` now escapes each NUL
//! to U+2400 (`␀`) and counts them under `audit::NUL_ESCAPED_KEY`.
//!
//! The test opens with a **positive control**: the same payload, bound raw
//! (bypassing `audit::insert`), must still be refused. Without it, a future
//! Postgres that accepted NUL would leave the rest of this file passing
//! while proving nothing about the escape.
//!
//! Skip-as-pass without a supervisor/PG (root CI container, a Mac without
//! `KASTELLAN_PG_BIN_DIR`); live wherever a cluster can be brought up, and
//! demanded by the `pg` profile of `scripts/run-e2e-gate.sh`.

use kastellan_db::audit::{self, NUL_ESCAPE, NUL_ESCAPED_KEY, PAYLOAD_MAX_BYTES};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};

#[test]
fn audit_rows_carrying_nul_land_with_the_escape_counted() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "nul-d",
        "nul-l",
        &format!("kastellan-supervisor-test-pg-nul-{suffix}"),
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
            serde_json::json!({"version": "test", "purpose": "audit-nul-e2e"}),
        )
        .await
        .expect("probe run");

        // The runtime pool is the role every production audit write uses.
        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("runtime pool");

        // The shape #816 names: a worker-supplied peer id with a NUL in it,
        // plus a NUL in a nested value and in an object KEY (a tool result
        // is arbitrary worker JSON, so keys are worker-controlled too).
        let hostile = serde_json::json!({
            "channel": "matrix",
            "peer": "@mallory\u{0}:example.org",
            "result": {"k\u{0}ey": ["a\u{0}\u{0}b"]},
        });

        // ── Positive control: raw NUL really is refused. ──
        let raw = sqlx::query(
            "INSERT INTO audit_log (actor, action, payload) VALUES ($1, $2, $3)",
        )
        .bind("core")
        .bind("channel.rejected_unpaired")
        .bind(&hostile)
        .execute(&pool)
        .await;
        let err = raw.expect_err(
            "a raw NUL in jsonb must be refused — if this ever passes, the \
             premise of #816 changed and the escape below proves nothing",
        );
        assert!(
            err.to_string().contains("unsupported Unicode escape sequence"),
            "the control must fail for the NUL, not for something else: {err}"
        );

        // ── The fix: the same payload through audit::insert lands. ──
        let id = audit::insert(&pool, "core", "channel.rejected_unpaired", hostile)
            .await
            .expect("audit::insert must land a payload carrying NUL (#816)");
        let row = audit::fetch_by_id(&pool, id).await.expect("read the row back");

        let esc = NUL_ESCAPE;
        assert_eq!(
            row.payload["peer"],
            serde_json::json!(format!("@mallory{esc}:example.org")),
            "the NUL is replaced by a visible glyph, never deleted"
        );
        assert_eq!(
            row.payload["result"][format!("k{esc}ey")],
            serde_json::json!([format!("a{esc}{esc}b")]),
            "keys and nested values are escaped alike"
        );
        assert_eq!(
            row.payload[NUL_ESCAPED_KEY],
            serde_json::json!(4),
            "every replaced NUL is counted, so a row says it was rewritten"
        );

        // ── The tool path applies the storage form TWICE. ──
        // `core::tool_host::audit_sink::AuditSink::insert` runs
        // `truncate_payload` so a test double sees the stored form, then
        // `PgAuditSink` calls `audit::insert`, which runs it again. A second
        // pass that dropped the count would store `␀` with nothing saying it
        // had been a NUL — the first draft of #816 did exactly that.
        let pre = audit::truncate_payload(serde_json::json!({"result": "a\u{0}b"}));
        let id = audit::insert(&pool, "tool:x", "call", pre)
            .await
            .expect("a pre-truncated payload lands");
        let row = audit::fetch_by_id(&pool, id).await.expect("read back");
        assert_eq!(row.payload["result"], serde_json::json!(format!("a{esc}b")));
        assert_eq!(
            row.payload[NUL_ESCAPED_KEY],
            serde_json::json!(1),
            "the count survives the second pass: {}",
            row.payload
        );

        // ── actor / action are `text`, which refuses NUL too. ──
        let id = audit::insert(&pool, "tool:x\u{0}", "ca\u{0}ll", serde_json::json!({}))
            .await
            .expect("a NUL in actor/action must not lose the row either");
        let row = audit::fetch_by_id(&pool, id).await.expect("read back");
        assert_eq!(row.actor, format!("tool:x{esc}"));
        assert_eq!(row.action, format!("ca{esc}ll"));
        // The count describes the payload only; actor and action have no
        // payload slot of their own, so their glyph is their record.
        assert!(row.payload.get(NUL_ESCAPED_KEY).is_none(), "{}", row.payload);

        // ── Over the cap: the count rides through truncation. ──
        // The body is replaced by a fingerprint envelope; the count is a
        // preserved key, so the row still says it was rewritten. The
        // request summary's head is cut from the escaped request, so it
        // shows `␀` like the rest of the row.
        let big = serde_json::json!({
            "req": {"argv": ["echo", "x\u{0}y"]},
            "result": "z".repeat(PAYLOAD_MAX_BYTES),
        });
        let id = audit::insert(&pool, "tool:shell", "call", big)
            .await
            .expect("an oversized payload carrying NUL must land too");
        let row = audit::fetch_by_id(&pool, id).await.expect("read back");
        assert!(audit::is_truncation_envelope(&row.payload), "{}", row.payload);
        assert_eq!(row.payload[NUL_ESCAPED_KEY], serde_json::json!(1), "{}", row.payload);
        let head = row.payload[audit::REQ_SUMMARY_KEY]["head"]
            .as_str()
            .expect("a req_summary head was stored");
        assert!(head.contains(&format!("x{esc}y")), "{head}");

        pool.close().await;
    });
}
