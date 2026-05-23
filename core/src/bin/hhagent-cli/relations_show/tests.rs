//! Unit tests for the `relations show` CLI handler.
//!
//! Lifted from an inline `#[cfg(test)] mod tests` block in `relations_show.rs`
//! to keep the production file under the 500-LOC soft cap. The body is
//! byte-identical to what it was inline; `use super::*` still resolves to
//! the parent `relations_show` module per the Rust 2018 sibling-directory
//! module pattern.

use super::*;

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

// --- parse_show_args ----------------------------------------------

#[test]
fn parse_show_args_id_only_uses_defaults() {
    let parsed = parse_show_args(&args(&["42"])).unwrap();
    assert_eq!(parsed, (42, DEFAULT_SHOW_DEPTH, ShowFormat::Plain));
}

#[test]
fn parse_show_args_accepts_negative_id_as_i64() {
    // BIGSERIAL is i64; negative ids are syntactically valid even
    // though no production row has one. The parser delegates the
    // existence check to the DB layer (which returns "not found").
    let parsed = parse_show_args(&args(&["-1"])).unwrap();
    assert_eq!(parsed.0, -1);
}

#[test]
fn parse_show_args_accepts_depth() {
    let parsed = parse_show_args(&args(&["42", "--depth", "3"])).unwrap();
    assert_eq!(parsed, (42, 3, ShowFormat::Plain));
}

#[test]
fn parse_show_args_accepts_format_json() {
    let parsed = parse_show_args(&args(&["42", "--format", "json"])).unwrap();
    assert_eq!(parsed, (42, DEFAULT_SHOW_DEPTH, ShowFormat::Json));
}

#[test]
fn parse_show_args_accepts_format_plain_explicit() {
    let parsed = parse_show_args(&args(&["42", "--format", "plain"])).unwrap();
    assert_eq!(parsed, (42, DEFAULT_SHOW_DEPTH, ShowFormat::Plain));
}

#[test]
fn parse_show_args_accepts_depth_and_format_in_either_order() {
    let a = parse_show_args(&args(&["42", "--depth", "2", "--format", "json"])).unwrap();
    let b = parse_show_args(&args(&["42", "--format", "json", "--depth", "2"])).unwrap();
    assert_eq!(a, b);
    assert_eq!(a, (42, 2, ShowFormat::Json));
}

#[test]
fn parse_show_args_rejects_empty() {
    let err = parse_show_args(&[]).unwrap_err();
    assert!(err.contains("usage"), "expected usage line: {err}");
}

#[test]
fn parse_show_args_rejects_non_integer_id() {
    let err = parse_show_args(&args(&["not-a-number"])).unwrap_err();
    assert!(err.contains("invalid entity-id"), "got: {err}");
}

#[test]
fn parse_show_args_rejects_depth_zero() {
    let err = parse_show_args(&args(&["42", "--depth", "0"])).unwrap_err();
    assert!(
        err.contains("--depth 0"),
        "expected explicit depth=0 diagnostic: {err}",
    );
}

#[test]
fn parse_show_args_rejects_depth_above_cap() {
    let too_deep = hhagent_db::graph::MAX_WALK_DEPTH + 1;
    let err = parse_show_args(&args(&["42", "--depth", &too_deep.to_string()])).unwrap_err();
    assert!(
        err.contains("exceeds cap"),
        "expected cap-exceeded diagnostic: {err}",
    );
}

#[test]
fn parse_show_args_rejects_dangling_depth() {
    let err = parse_show_args(&args(&["42", "--depth"])).unwrap_err();
    assert!(
        err.contains("--depth requires a value"),
        "expected dangling-depth diagnostic: {err}",
    );
}

#[test]
fn parse_show_args_rejects_unknown_format() {
    let err = parse_show_args(&args(&["42", "--format", "xml"])).unwrap_err();
    assert!(
        err.contains("not recognised"),
        "expected unknown-format diagnostic: {err}",
    );
}

#[test]
fn parse_show_args_rejects_dangling_format() {
    let err = parse_show_args(&args(&["42", "--format"])).unwrap_err();
    assert!(
        err.contains("--format requires a value"),
        "expected dangling-format diagnostic: {err}",
    );
}

#[test]
fn parse_show_args_rejects_unknown_flag() {
    let err = parse_show_args(&args(&["42", "--bogus", "x"])).unwrap_err();
    assert!(
        err.contains("unrecognised argument"),
        "expected unknown-flag diagnostic: {err}",
    );
}

// --- endpoint_str (renderer helper) -------------------------------

#[test]
fn endpoint_str_strips_quarantine_tag_when_approved() {
    assert_eq!(
        endpoint_str("person", "Dr Smith", false),
        r#"(person, "Dr Smith")"#,
    );
}

#[test]
fn endpoint_str_adds_quarantine_tag_when_quarantined() {
    assert_eq!(
        endpoint_str("disease", "asthma", true),
        r#"(disease, "asthma") [Q]"#,
    );
}

#[test]
fn endpoint_str_escapes_embedded_double_quote() {
    // Entity names allow arbitrary TEXT (no character-set CHECK), so
    // a name like `Dr "Bob" Smith` is legal. The plain rendering must
    // escape the inner quotes so naive regex parsers of the output
    // don't miscount the closing quote.
    assert_eq!(
        endpoint_str("person", r#"Dr "Bob" Smith"#, false),
        r#"(person, "Dr \"Bob\" Smith")"#,
    );
}

#[test]
fn endpoint_str_escapes_backslash_before_quote() {
    // Backslashes must be escaped first; otherwise `name\"` would
    // produce ambiguous-to-parse `name\\"` (escaped backslash + raw
    // quote vs raw backslash + escaped quote). The two-pass replace
    // gives the unambiguous result.
    assert_eq!(
        endpoint_str("k", r#"a\b"c"#, false),
        r#"(k, "a\\b\"c")"#,
    );
}

// --- edge_to_json (JSON shape pin) --------------------------------

#[test]
fn edge_to_json_emits_canonical_fields() {
    use hhagent_db::graph::WalkedEdge;
    let e = WalkedEdge {
        depth: 2,
        edge_id: 17,
        src_id: 10,
        src_kind: "person".into(),
        src_name: "Dr Smith".into(),
        src_quarantine: false,
        dst_id: 20,
        dst_kind: "disease".into(),
        dst_name: "asthma".into(),
        dst_quarantine: true,
        kind: "treats".into(),
    };
    let line = edge_to_json("outbound", &e);
    let v: serde_json::Value = serde_json::from_str(&line).expect("valid JSON");
    // Field-by-field pin so a future renderer change that drops or
    // renames a field trips this test rather than silently breaking
    // downstream `jq` consumers.
    assert_eq!(v["type"], "edge");
    assert_eq!(v["direction"], "outbound");
    assert_eq!(v["depth"], 2);
    assert_eq!(v["edge_id"], 17);
    assert_eq!(v["kind"], "treats");
    assert_eq!(v["src"]["id"], 10);
    assert_eq!(v["src"]["kind"], "person");
    assert_eq!(v["src"]["name"], "Dr Smith");
    assert_eq!(v["src"]["quarantine"], false);
    assert_eq!(v["dst"]["id"], 20);
    assert_eq!(v["dst"]["kind"], "disease");
    assert_eq!(v["dst"]["name"], "asthma");
    assert_eq!(v["dst"]["quarantine"], true);
}
