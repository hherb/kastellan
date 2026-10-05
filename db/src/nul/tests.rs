//! Unit tests for [`super`]: the record escape and the identity refusal.
//! The escape's finer cases (key collisions, multi-byte neighbours) are
//! pinned through the audit layer in `audit/nul_escape/tests.rs`; the
//! live-cluster proof for the non-audit columns is
//! `db/tests/nul_non_audit_e2e.rs`.

use serde_json::json;

use super::*;
use crate::DbError;

/// `␀`, spelled once so a fixture cannot drift from the constant.
const E: char = NUL_ESCAPE;

/// The detector must see a NUL in a key and deep in a value, and must NOT
/// see one in the six-character text `\u0000` or in the glyph itself.
#[test]
fn json_contains_nul_finds_real_nuls_only() {
    assert!(json_contains_nul(&json!("a\0")));
    assert!(json_contains_nul(&json!({"k\0": 1})));
    assert!(json_contains_nul(&json!({"a": [{"b": "x\0"}]})));
    assert!(!json_contains_nul(&json!({"a": "\\u0000"})));
    assert!(!json_contains_nul(&json!({"a": format!("x{E}")})));
    assert!(!json_contains_nul(&json!([null, true, 1.5, {"k": []}])));
}

/// A clean value passes; the error names the column and never the value,
/// which may be a hostile peer's id.
#[test]
fn refuse_nul_in_text_names_the_column_only() {
    assert!(refuse_nul_in_text("pairings.peer", "@alice:example.org").is_ok());
    assert!(refuse_nul_in_text("pairings.peer", "").is_ok());
    match refuse_nul_in_text("pairings.peer", "@mallory\0:x") {
        Err(DbError::NulRefused { column }) => assert_eq!(column, "pairings.peer"),
        other => panic!("expected NulRefused, got {other:?}"),
    }
    let msg = refuse_nul_in_text("pairings.peer", "@mallory\0:x").unwrap_err().to_string();
    assert!(msg.contains("pairings.peer"), "{msg}");
    assert!(!msg.contains("mallory"), "the value must not leak into the error: {msg}");
}

/// The JSON refusal walks keys as well as values.
#[test]
fn refuse_nul_in_json_walks_keys_and_values() {
    assert!(refuse_nul_in_json("tasks.payload", &json!({"instruction": "hi"})).is_ok());
    for bad in [json!({"k\0": 1}), json!({"a": ["\0"]}), json!("\0")] {
        match refuse_nul_in_json("tasks.payload", &bad) {
            Err(DbError::NulRefused { column }) => assert_eq!(column, "tasks.payload"),
            other => panic!("expected NulRefused for {bad}, got {other:?}"),
        }
    }
}

/// The record escape: every NUL replaced and counted, nothing deleted, and
/// — unlike the audit escape — NO marker key added, because these columns
/// are read back by code that expects their own shape.
#[test]
fn escape_json_replaces_and_counts_without_a_marker() {
    let (out, n) = escape_json(json!({"k\0": ["a\0\0b"], "clean": "x"}));
    assert_eq!(n, 3);
    assert_eq!(out, json!({format!("k{E}"): [format!("a{E}{E}b")], "clean": "x"}));
    assert!(!json_contains_nul(&out));
    assert!(out.get(crate::audit::NUL_ESCAPED_KEY).is_none(), "{out}");
}

/// A clean value comes back unchanged with a zero count, so a clean row is
/// stored exactly as it was before #818.
#[test]
fn escape_json_leaves_a_clean_value_alone() {
    let v = json!({"state": "completed", "n": 3, "list": ["a", {"b": null}]});
    assert_eq!(escape_json(v.clone()), (v, 0));
}

/// Every identity / knowledge writer refuses a NUL **before any SQL**, and
/// the two pairing lookups answer "not paired" without one.
///
/// The pool is lazy and points at a port nothing listens on, so any call
/// that reached Postgres would come back as a connection error instead of
/// the asserted value — which is what makes this a proof that the check
/// runs first, on every machine, with no cluster. The live-cluster proof
/// that a refused write leaves no row is `db/tests/nul_non_audit_e2e.rs`.
#[tokio::test]
async fn identity_writers_refuse_before_any_sql() {
    use crate::memories::{insert_memory, insert_memory_at_layer, MemoryLayer};
    use crate::graph::Graph;
    use crate::{pairings, tasks};

    // A short acquire timeout so a regression fails in half a second, not 30.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(500))
        .connect_lazy("postgres://invalid:invalid@127.0.0.1:1/none")
        .expect("lazy pool construction does not connect");
    let refused = |r: Result<(), DbError>, column: &str| match r {
        Err(DbError::NulRefused { column: c }) => assert_eq!(c, column),
        other => panic!("expected NulRefused {{ column: {column} }}, got {other:?}"),
    };

    refused(
        tasks::insert_pending(&pool, tasks::Lane::Fast, json!({"peer": "a\0"})).await.map(drop),
        "tasks.payload",
    );
    refused(insert_memory(&pool, "a\0", &json!({}), None).await.map(drop), "memories.body");
    refused(
        insert_memory_at_layer(&pool, "ok", &json!({"k\0": 1}), None, MemoryLayer::Skill)
            .await
            .map(drop),
        "memories.metadata",
    );
    refused(pairings::insert_pairing(&pool, "matrix", "a\0", "code").await.map(drop), "pairings.peer");
    refused(pairings::insert_pairing(&pool, "m\0", "a", "code").await.map(drop), "pairings.channel");
    refused(
        pairings::insert_pairing_with_token(&pool, "email", "a\0@x", "op", None).await.map(drop),
        "pairings.peer",
    );
    refused(pairings::revoke_pairing(&pool, "matrix", "a\0").await.map(drop), "pairings.peer");
    refused(
        pairings::claim_code(&pool, "hash", "matrix/a\0").await.map(drop),
        "pairing_codes.consumed_by",
    );
    refused(
        pairings::insert_code(&pool, "hash", Some("label\0"), 10).await.map(drop),
        "pairing_codes.label",
    );

    let graph = crate::graph::PgGraph::new(&pool);
    refused(graph.upsert_entity("person", "Bo\0b", &json!({})).await.map(drop), "entities.name");
    refused(graph.upsert_entity("per\0", "Bob", &json!({})).await.map(drop), "entities.kind");
    refused(graph.upsert_entity("person", "Bob", &json!({"k\0": 1})).await.map(drop), "entities.attrs");
    refused(graph.upsert_relation(1, 2, "kn\0ows", &json!({})).await.map(drop), "relations.kind");
    refused(graph.upsert_relation(1, 2, "knows", &json!(["\0"])).await.map(drop), "relations.attrs");

    assert!(!pairings::is_paired(&pool, "matrix", "a\0").await.expect("answered without SQL"));
    assert_eq!(
        pairings::token_hash_for(&pool, "matrix", "a\0").await.expect("answered without SQL"),
        None
    );
}
