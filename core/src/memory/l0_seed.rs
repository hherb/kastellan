//! L0 (meta-rule) seed loader.
//!
//! L0 is the highest-priority memory layer: hard agent constraints
//! that the prompt assembler concatenates into *every* system prompt
//! regardless of the current task. This module reads a hand-edited
//! TOML file of rules, hashes each rule body, and writes a new
//! `memories` row only when the `(l0_rule_id, body_sha256)` pair is
//! not already present — append-only ledger discipline, identical to
//! the `agent_prompts` pattern.
//!
//! ## Idempotency
//!
//! Re-running the loader against an unchanged source file writes
//! zero new rows. Editing a single rule writes one new row; the old
//! version stays for audit. The read-side helper [`load_l0_active`]
//! dedups by `l0_rule_id`, returning newest-first per rule.
//!
//! ## Fail-closed on malformed input
//!
//! A TOML parse error, missing required field, duplicate `id`, or
//! oversize body is a hard error — `seed_l0_from_file` returns `Err`
//! and the daemon refuses to start. Silently coming up with a stale
//! or partial L0 set is more dangerous than not coming up at all.
//!
//! ## What this module does NOT do
//!
//! - No embeddings on L0 rows (they're pinned into every prompt; no
//!   semantic recall is involved).
//! - No prompt-assembler wiring (future slice).
//! - No hot-reload on file change (edit + restart, same cadence as
//!   `agent_prompts`).
//! - No tag-based filtering at load time (tags are stored for future
//!   ops queries).

use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use thiserror::Error;

use hhagent_db::memories::Memory;
use hhagent_db::DbError;

/// Default upper bound on the active L0 row count returned by
/// [`load_l0_active_default`].
///
/// L0 should stay small — these are agent safety rules, not a
/// knowledge base. 64 is twice the L1 cap because L0 sits even higher
/// in priority and the rules are typically shorter one-liners.
pub const L0_DEFAULT_CAP_ROWS: usize = 64;

/// Default upper bound on the cumulative byte length of L0 row bodies
/// returned by [`load_l0_active_default`].
///
/// 8 KiB ≈ 2 K tokens at typical English density; about 7% of a 30 K
/// target window. Twice the L1 byte cap because L0 is the most
/// load-bearing layer for safety; the prompt assembler will not drop
/// L0 even under token pressure.
pub const L0_DEFAULT_CAP_BYTES: usize = 8192;

/// Maximum allowed length (UTF-8 bytes) of a single rule body.
///
/// Rule bodies are expected to be a single sentence (<200 bytes
/// typical). 1024 is a generous ceiling that catches the failure mode
/// "operator pasted a paragraph of prose into the body field by
/// accident" without rejecting legitimate multi-clause rules.
pub const L0_MAX_BODY_BYTES: usize = 1024;

/// Maximum allowed length of a rule `id`.
pub const L0_MAX_ID_LEN: usize = 64;

/// Top-level TOML schema: a list of `[[rule]]` tables.
///
/// `#[serde(deny_unknown_fields)]` catches typos like a top-level
/// `[rules]` table (singular) — the wrong shape would otherwise be
/// silently ignored, producing an empty rule set on a non-empty file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct L0RulesFile {
    #[serde(default)]
    rule: Vec<L0RuleRaw>,
}

/// Per-`[[rule]]` table shape as it appears on disk.
///
/// `#[serde(deny_unknown_fields)]` catches typos like `tag = [...]`
/// (singular, missing the trailing `s`) — without this, the typo'd
/// field would be silently dropped and the rule would load with no
/// tags, hiding the operator error.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct L0RuleRaw {
    id: String,
    body: String,
    #[serde(default)]
    tags: Vec<String>,
}

/// A single parsed and validated L0 rule, ready to seed.
///
/// `id` is in canonical form (`^[a-z0-9_]+$`, non-empty,
/// `<= L0_MAX_ID_LEN` bytes). `body` is non-empty after trimming,
/// `<= L0_MAX_BODY_BYTES` UTF-8 bytes. `tags` is the original order
/// from the source file (may be empty).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct L0Rule {
    pub id: String,
    pub body: String,
    pub tags: Vec<String>,
}

/// Errors surfaced by the L0 seed pipeline.
#[derive(Debug, Error)]
pub enum L0Error {
    #[error("toml parse error in {path:?}: {source}")]
    TomlParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("validation error in {path:?}: {detail}")]
    Validation { path: PathBuf, detail: String },
    #[error("io error reading {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("db error: {0}")]
    Db(#[from] DbError),
}

/// Operator-visible summary of one seed run.
///
/// Returned from `seed_l0_from_rules` / `seed_l0_from_file` and
/// recorded into the `actor='core' action='l0.seeded'` audit row in
/// `core::main`.
#[derive(Clone, Debug, Default)]
pub struct L0SeedReport {
    /// Number of rules parsed from the source file.
    pub rules_loaded: usize,
    /// Rules whose `(l0_rule_id, body_sha256)` was not yet in
    /// `memories` and were inserted by this run.
    pub new_rows_written: usize,
    /// Rules whose `(l0_rule_id, body_sha256)` already existed; the
    /// loader skipped the insert.
    pub unchanged_skipped: usize,
    /// Path the rules came from (for the audit row + diagnostics).
    pub source_path: PathBuf,
    /// SHA-256 of the source file content (for cross-restart drift
    /// detection in the audit row).
    pub source_sha256: String,
}

/// Parse + validate the TOML rule file content.
///
/// `source_path` is only used in error diagnostics — the function
/// itself does no I/O. `Ok(vec![])` is returned for an empty (or
/// `rule`-less) input; empty is not an error.
pub fn parse_l0_rules(source_path: &Path, toml_str: &str) -> Result<Vec<L0Rule>, L0Error> {
    let raw: L0RulesFile = toml::from_str(toml_str).map_err(|e| L0Error::TomlParse {
        path: source_path.to_path_buf(),
        source: e,
    })?;

    let mut out: Vec<L0Rule> = Vec::with_capacity(raw.rule.len());
    let mut seen_ids: std::collections::HashSet<String> =
        std::collections::HashSet::with_capacity(raw.rule.len());

    for r in raw.rule {
        validate_id(source_path, &r.id)?;
        if !seen_ids.insert(r.id.clone()) {
            return Err(L0Error::Validation {
                path: source_path.to_path_buf(),
                detail: format!("duplicate rule id: {:?}", r.id),
            });
        }

        let body_trimmed = r.body.trim();
        if body_trimmed.is_empty() {
            return Err(L0Error::Validation {
                path: source_path.to_path_buf(),
                detail: format!("rule {:?}: body is empty after trimming", r.id),
            });
        }
        if r.body.len() > L0_MAX_BODY_BYTES {
            return Err(L0Error::Validation {
                path: source_path.to_path_buf(),
                detail: format!(
                    "rule {:?}: body is {} bytes (max {})",
                    r.id,
                    r.body.len(),
                    L0_MAX_BODY_BYTES
                ),
            });
        }

        for tag in &r.tags {
            if tag.is_empty() {
                return Err(L0Error::Validation {
                    path: source_path.to_path_buf(),
                    detail: format!("rule {:?}: tag is empty", r.id),
                });
            }
        }

        out.push(L0Rule {
            id: r.id,
            body: r.body,
            tags: r.tags,
        });
    }
    Ok(out)
}

fn validate_id(source_path: &Path, id: &str) -> Result<(), L0Error> {
    if id.is_empty() {
        return Err(L0Error::Validation {
            path: source_path.to_path_buf(),
            detail: "rule id is empty".to_string(),
        });
    }
    if id.len() > L0_MAX_ID_LEN {
        return Err(L0Error::Validation {
            path: source_path.to_path_buf(),
            detail: format!(
                "rule id {:?} is {} bytes (max {})",
                id,
                id.len(),
                L0_MAX_ID_LEN
            ),
        });
    }
    if !id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
        return Err(L0Error::Validation {
            path: source_path.to_path_buf(),
            detail: format!(
                "rule id {:?} contains chars outside [a-z0-9_]",
                id
            ),
        });
    }
    Ok(())
}

/// Pure: SHA-256 hex of the rule body (no extra framing). Used as the
/// `metadata.body_sha256` value for idempotency.
///
/// Whitespace-sensitive on purpose: a body that differs by one
/// trailing newline is a different row. The operator chose that
/// edit; we don't second-guess.
pub fn compute_body_sha256(body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body.as_bytes());
    hex_encode_lower(&hasher.finalize())
}

/// Compute SHA-256 of the source file content for cross-restart drift
/// detection in the `l0.seeded` audit row.
pub fn compute_source_sha256(toml_str: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(toml_str.as_bytes());
    hex_encode_lower(&hasher.finalize())
}

fn hex_encode_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Pure: build the JSON `metadata` blob for one L0 row.
///
/// Exactly 4 keys: `l0_rule_id`, `body_sha256`, `tags`,
/// `source_path`. Pinned by `build_l0_metadata_pins_key_set`.
pub fn build_l0_metadata(
    rule_id: &str,
    body_sha256: &str,
    tags: &[String],
    source_path: &Path,
) -> serde_json::Value {
    serde_json::json!({
        "l0_rule_id": rule_id,
        "body_sha256": body_sha256,
        "tags": tags,
        "source_path": source_path.to_string_lossy(),
    })
}

// ---------------------------------------------------------------------
// DB writers + readers — bodies land in Task 2 + Task 3.
// ---------------------------------------------------------------------

/// Seed parsed rules into `memories`, idempotent on
/// `(l0_rule_id, body_sha256)`.
///
/// Body shipped in Task 2.
pub async fn seed_l0_from_rules(
    pool: &PgPool,
    source_path: &Path,
    source_sha256: &str,
    rules: &[L0Rule],
) -> Result<L0SeedReport, L0Error> {
    let mut report = L0SeedReport {
        rules_loaded: rules.len(),
        new_rows_written: 0,
        unchanged_skipped: 0,
        source_path: source_path.to_path_buf(),
        source_sha256: source_sha256.to_string(),
    };

    for rule in rules {
        let body_sha256 = compute_body_sha256(&rule.body);

        // Idempotency check: does `(l0_rule_id, body_sha256)` already
        // exist at layer 0? sqlx::query_scalar with `EXISTS` returns
        // a single bool; no row construction overhead.
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS ( \
                 SELECT 1 FROM memories \
                  WHERE layer = 0 \
                    AND metadata->>'l0_rule_id' = $1 \
                    AND metadata->>'body_sha256' = $2 \
              )",
        )
        .bind(&rule.id)
        .bind(&body_sha256)
        .fetch_one(pool)
        .await
        .map_err(|e| {
            L0Error::Db(DbError::Query(format!(
                "l0 idempotency check ({}): {e}",
                rule.id
            )))
        })?;

        if exists {
            report.unchanged_skipped += 1;
            continue;
        }

        let metadata = build_l0_metadata(&rule.id, &body_sha256, &rule.tags, source_path);
        hhagent_db::memories::seed_meta_memory(pool, &rule.body, &metadata, None).await?;
        report.new_rows_written += 1;
    }

    Ok(report)
}

/// Convenience: read + parse + seed.
pub async fn seed_l0_from_file(pool: &PgPool, path: &Path) -> Result<L0SeedReport, L0Error> {
    let content = tokio::fs::read_to_string(path).await.map_err(|e| L0Error::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let source_sha256 = compute_source_sha256(&content);
    let rules = parse_l0_rules(path, &content)?;
    seed_l0_from_rules(pool, path, &source_sha256, &rules).await
}

/// Returns the currently-active L0 rule set — newest version per
/// `l0_rule_id` — newest-first, bounded by the two caps.
pub async fn load_l0_active(
    pool: &PgPool,
    cap_rows: usize,
    cap_bytes: usize,
) -> Result<Vec<Memory>, DbError> {
    if cap_rows == 0 || cap_bytes == 0 {
        return Ok(Vec::new());
    }
    let candidates = hhagent_db::memories::load_active_l0(pool, cap_rows).await?;

    let mut acc: Vec<Memory> = Vec::with_capacity(candidates.len());
    let mut bytes_used: usize = 0;
    for row in candidates {
        let row_bytes = row.body.len();
        // saturating_add: defense-in-depth against a future caller
        // somehow supplying a row whose body length wraps usize on
        // accumulation. Overflow → "definitely over the cap," the
        // safe direction. Mirrors load_l1's idiom.
        if bytes_used.saturating_add(row_bytes) > cap_bytes {
            if acc.is_empty() && row_bytes > cap_bytes {
                let rule_id = row
                    .metadata
                    .get("l0_rule_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                tracing::warn!(
                    memory_id = row.id,
                    l0_rule_id = rule_id,
                    row_bytes,
                    cap_bytes,
                    "load_l0_active: dropping L0 row whose body alone exceeds cap_bytes; \
                     prompt pinning will skip this rule"
                );
            }
            break;
        }
        bytes_used += row_bytes;
        acc.push(row);
    }
    Ok(acc)
}

/// Convenience wrapper pinning the two published defaults.
pub async fn load_l0_active_default(pool: &PgPool) -> Result<Vec<Memory>, DbError> {
    load_l0_active(pool, L0_DEFAULT_CAP_ROWS, L0_DEFAULT_CAP_BYTES).await
}

#[cfg(test)]
mod tests {
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
}
