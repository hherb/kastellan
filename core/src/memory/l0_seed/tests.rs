//! Unit tests for [`super`] — the L0 (meta-rule) seed loader.
//!
//! Lifted verbatim (de-indented one level) from the inline
//! `#[cfg(test)] mod tests` block that used to live at the tail of
//! `l0_seed.rs`, following the established Rust-2018 sibling-module
//! pattern (cf. `inner_loop/tests.rs`, `replay/tests.rs`,
//! `injection_guard/tests.rs`). `use super::*` resolves to the parent
//! `l0_seed` module, so every production item these tests exercise
//! (`parse_l0_rules`, `seed_l0_from_file`, `load_l0_active`, the
//! `L0_MAX_*` / `L0_DEFAULT_*` consts, …) stays reachable exactly as
//! before. Pure-mechanical move — zero behaviour change.

use super::*;
use std::path::Path;

fn p() -> &'static Path {
    Path::new("test/fixture.toml")
}

// --- parse_l0_rules ------------------------------------------------

#[test]
fn parse_valid_minimal_one_rule() {
    let toml = r#"
[[rule]]
id = "never_rm_rf"
body = "Never invoke rm -rf without explicit confirmation."
"#;
    let rules = parse_l0_rules(p(), toml).expect("parse");
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].id, "never_rm_rf");
    assert_eq!(
        rules[0].body,
        "Never invoke rm -rf without explicit confirmation."
    );
    assert!(rules[0].tags.is_empty());
}

#[test]
fn parse_valid_multi_rule_preserves_order() {
    let toml = r#"
[[rule]]
id = "a_rule"
body = "first"
[[rule]]
id = "b_rule"
body = "second"
[[rule]]
id = "c_rule"
body = "third"
"#;
    let rules = parse_l0_rules(p(), toml).expect("parse");
    let ids: Vec<&str> = rules.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, vec!["a_rule", "b_rule", "c_rule"]);
}

#[test]
fn parse_rejects_missing_id() {
    let toml = r#"
[[rule]]
body = "no id here"
"#;
    let err = parse_l0_rules(p(), toml).expect_err("must fail");
    assert!(matches!(err, L0Error::TomlParse { .. }), "got {err:?}");
}

#[test]
fn parse_rejects_missing_body() {
    let toml = r#"
[[rule]]
id = "no_body"
"#;
    let err = parse_l0_rules(p(), toml).expect_err("must fail");
    assert!(matches!(err, L0Error::TomlParse { .. }), "got {err:?}");
}

#[test]
fn parse_rejects_empty_body() {
    let toml = r#"
[[rule]]
id = "blank"
body = "   "
"#;
    let err = parse_l0_rules(p(), toml).expect_err("must fail");
    match err {
        L0Error::Validation { detail, .. } => {
            assert!(detail.contains("blank"), "got {detail}");
            assert!(detail.contains("empty"), "got {detail}");
        }
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[test]
fn parse_rejects_oversize_body_and_accepts_exact_cap() {
    let body_1024 = "a".repeat(L0_MAX_BODY_BYTES);
    let body_1025 = "a".repeat(L0_MAX_BODY_BYTES + 1);

    let pass = format!(
        "[[rule]]\nid = \"big_a\"\nbody = \"{}\"\n",
        body_1024
    );
    let rules = parse_l0_rules(p(), &pass).expect("1024 must pass");
    assert_eq!(rules.len(), 1);

    let fail = format!(
        "[[rule]]\nid = \"big_b\"\nbody = \"{}\"\n",
        body_1025
    );
    let err = parse_l0_rules(p(), &fail).expect_err("1025 must fail");
    match err {
        L0Error::Validation { detail, .. } => {
            assert!(detail.contains("1025"), "got {detail}");
            assert!(detail.contains("1024"), "got {detail}");
        }
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[test]
fn parse_rejects_duplicate_id() {
    let toml = r#"
[[rule]]
id = "dup"
body = "first"
[[rule]]
id = "dup"
body = "second"
"#;
    let err = parse_l0_rules(p(), toml).expect_err("must fail");
    match err {
        L0Error::Validation { detail, .. } => {
            assert!(detail.contains("duplicate"), "got {detail}");
            assert!(detail.contains("dup"), "got {detail}");
        }
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[test]
fn parse_rejects_bad_id_charset() {
    for bad in ["With-Dashes", "UPPER_CASE", "with space", "trailing!"] {
        let toml = format!("[[rule]]\nid = \"{}\"\nbody = \"x\"\n", bad);
        let err = parse_l0_rules(p(), &toml).expect_err(&format!("{bad} must fail"));
        match err {
            L0Error::Validation { detail, .. } => {
                assert!(detail.contains(bad), "got {detail}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }
}

#[test]
fn parse_rejects_empty_id() {
    let toml = "[[rule]]\nid = \"\"\nbody = \"x\"\n";
    let err = parse_l0_rules(p(), toml).expect_err("must fail");
    match err {
        L0Error::Validation { detail, .. } => {
            assert!(detail.contains("empty"), "got {detail}");
        }
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[test]
fn parse_rejects_unknown_top_level_key() {
    let toml = "[rules]\nfoo = 1\n";
    let err = parse_l0_rules(p(), toml).expect_err("must fail");
    assert!(matches!(err, L0Error::TomlParse { .. }), "got {err:?}");
}

#[test]
fn parse_rejects_unknown_rule_key() {
    let toml = r#"
[[rule]]
id = "x"
body = "y"
tag = ["a"]
"#;
    let err = parse_l0_rules(p(), toml).expect_err("must fail");
    assert!(matches!(err, L0Error::TomlParse { .. }), "got {err:?}");
}

#[test]
fn parse_empty_file_is_ok() {
    let rules = parse_l0_rules(p(), "").expect("parse empty");
    assert!(rules.is_empty());
}

#[test]
fn parse_tags_optional_and_default_empty() {
    let toml = "[[rule]]\nid = \"a\"\nbody = \"x\"\n";
    let rules = parse_l0_rules(p(), toml).expect("parse");
    assert!(rules[0].tags.is_empty());
}

#[test]
fn parse_rejects_empty_tag_string() {
    let toml = r#"
[[rule]]
id = "with_blank_tag"
body = "ok"
tags = ["", "real_tag"]
"#;
    let err = parse_l0_rules(p(), toml).expect_err("must fail");
    match err {
        L0Error::Validation { detail, .. } => {
            assert!(detail.contains("with_blank_tag"), "got {detail}");
            assert!(detail.contains("empty"), "got {detail}");
        }
        other => panic!("expected Validation, got {other:?}"),
    }
}

// --- pure helpers --------------------------------------------------

#[test]
fn build_l0_metadata_pins_key_set() {
    use std::collections::BTreeSet;
    let meta = build_l0_metadata(
        "rid",
        "abc",
        &["t1".to_string(), "t2".to_string()],
        Path::new("seeds/memory/l0_meta_rules.toml"),
    );
    let obj = meta.as_object().expect("object");
    let keys: BTreeSet<&str> = obj.keys().map(String::as_str).collect();
    let expected: BTreeSet<&str> = ["l0_rule_id", "body_sha256", "tags", "source_path"]
        .into_iter()
        .collect();
    assert_eq!(
        keys, expected,
        "metadata key set drifted; this is a wire-shape change"
    );
    assert_eq!(obj["l0_rule_id"], "rid");
    assert_eq!(obj["body_sha256"], "abc");
    assert_eq!(obj["tags"], serde_json::json!(["t1", "t2"]));
    assert_eq!(
        obj["source_path"], "seeds/memory/l0_meta_rules.toml"
    );
}

#[test]
fn compute_body_sha256_is_stable_and_whitespace_sensitive() {
    let h1 = compute_body_sha256("hello world");
    let h2 = compute_body_sha256("hello world");
    let h3 = compute_body_sha256("hello world\n");
    assert_eq!(h1, h2);
    assert_ne!(h1, h3, "trailing newline must change the hash");
    assert_eq!(h1.len(), 64, "sha256 hex is 64 chars");
    assert!(h1.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
}

/// Known-answer test for the hex encoder. A nibble-swap or
/// off-by-one bug in `hex_encode_lower` would pass every other
/// hash test (stable, whitespace-sensitive, correct length,
/// lowercase-hex) while silently corrupting every body_sha256
/// the loader writes. Pin against the canonical empty-string
/// SHA-256 to catch that class of regression.
#[test]
fn compute_body_sha256_matches_known_answer_for_empty_string() {
    assert_eq!(
        compute_body_sha256(""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    );
}

#[test]
fn l0_default_caps_pin() {
    assert_eq!(L0_DEFAULT_CAP_ROWS, 64);
    assert_eq!(L0_DEFAULT_CAP_BYTES, 8192);
}

#[test]
fn l0_max_constants_pin() {
    assert_eq!(L0_MAX_BODY_BYTES, 1024);
    assert_eq!(L0_MAX_ID_LEN, 64);
}
