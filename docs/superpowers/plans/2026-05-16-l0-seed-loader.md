# L0 Seed Data Loader Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a startup-time loader that reads a hand-edited TOML file of meta-rules into L0 (Meta) rows in the `memories` table via the existing `seed_meta_memory` admin function, idempotent on `(l0_rule_id, body_sha256)`; plus the paired read-side helper `load_l0_active` that dedups by rule_id for the future prompt-assembler slice.

**Architecture:** New `core::memory::l0_seed` module with three layers: pure `parse_l0_rules(toml_str) → Vec<L0Rule>`, async DB writer `seed_l0_from_rules(pool, rules) → L0SeedReport`, and file convenience wrapper `seed_l0_from_file(pool, path)`. A new `db::memories::load_active_l0` function carries the `SELECT DISTINCT ON (metadata->>'l0_rule_id')` SQL; the core wrapper `load_l0_active` applies in-Rust byte caps mirroring `load_l1`. Wire-in in `core/src/main.rs` runs right after the prompts loader and writes one `actor='core' action='l0.seeded'` audit row.

**Tech Stack:** Rust 2021, sqlx 0.8 (PgPool), toml 0.8 (workspace dep), sha2 0.10 (workspace dep), tokio multi-thread runtime, `kastellan_tests_common` for per-test PG clusters.

**Spec:** [docs/superpowers/specs/2026-05-16-l0-seed-loader-design.md](../specs/2026-05-16-l0-seed-loader-design.md) (committed at `7153b48` on branch `feat/l0-seed-loader`).

**Branch:** `feat/l0-seed-loader` (already created, off `main` at `305941a`).

**Workspace test-count baseline:** 607 passed / 0 failed / 4 ignored. Target after this plan: ~631 passed.

---

## Pre-flight: confirm dependencies already present

`toml`, `sha2`, `sqlx`, `tracing`, `thiserror` are already in `core`'s `[dependencies]` block (verified before plan-writing). No `Cargo.toml` changes needed. Run `cargo build --workspace` once at the start to confirm.

```bash
source "$HOME/.cargo/env"
cargo build --workspace
```

Expected: clean build, no warnings.

---

## Task 1: Module scaffold + `parse_l0_rules` + pure helpers

**Files:**
- Create: `core/src/memory/l0_seed.rs`
- Modify: `core/src/memory/mod.rs` (add `pub mod l0_seed;` declaration)

This task ships the file, the types, the pure parser, and the pure helpers — everything that can be unit-tested without a database connection. The DB and file I/O surfaces land in later tasks.

### Step 1: Add module declaration

Edit `core/src/memory/mod.rs`. The existing declarations are alphabetically ordered: `embed`, `layers`, `recall`. Insert `l0_seed` between `embed` and `layers`.

- [ ] **Edit `core/src/memory/mod.rs`**

Find the existing block:

```rust
pub mod embed;
pub mod layers;
```

Replace with:

```rust
pub mod embed;
pub mod l0_seed;
pub mod layers;
```

### Step 2: Write the unit tests first (RED)

Create the new file with the test module body in place. Implementation bodies stay as `todo!()` so the tests compile-error initially, then we fill in.

- [ ] **Create `core/src/memory/l0_seed.rs` with skeleton + tests**

```rust
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

use kastellan_db::memories::Memory;
use kastellan_db::DbError;

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
    _pool: &PgPool,
    _source_path: &Path,
    _source_sha256: &str,
    _rules: &[L0Rule],
) -> Result<L0SeedReport, L0Error> {
    todo!("ships in Task 2")
}

/// Convenience: read + parse + seed.
///
/// Body shipped in Task 3.
pub async fn seed_l0_from_file(_pool: &PgPool, _path: &Path) -> Result<L0SeedReport, L0Error> {
    todo!("ships in Task 3")
}

/// Returns the currently-active L0 rule set — newest version per
/// `l0_rule_id` — newest-first, bounded by the two caps.
///
/// Body shipped in Task 3.
pub async fn load_l0_active(
    _pool: &PgPool,
    _cap_rows: usize,
    _cap_bytes: usize,
) -> Result<Vec<Memory>, DbError> {
    todo!("ships in Task 3")
}

/// Convenience wrapper pinning the two published defaults.
///
/// Body shipped in Task 3.
pub async fn load_l0_active_default(_pool: &PgPool) -> Result<Vec<Memory>, DbError> {
    todo!("ships in Task 3")
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
        // Build a 1024-byte body (passes) and a 1025-byte body (fails).
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
        // [rules] (singular) instead of [[rule]] — operator typo.
        let toml = "[rules]\nfoo = 1\n";
        let err = parse_l0_rules(p(), toml).expect_err("must fail");
        assert!(matches!(err, L0Error::TomlParse { .. }), "got {err:?}");
    }

    #[test]
    fn parse_rejects_unknown_rule_key() {
        // `tag` (singular) instead of `tags` — operator typo.
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
```

### Step 3: Verify the module compiles (with `todo!()` bodies)

- [ ] **Build check**

Run:

```bash
source "$HOME/.cargo/env"
cargo build --workspace
```

Expected: clean build, no warnings. The `todo!()` bodies don't fail compilation, only execution.

### Step 4: Run the unit tests

- [ ] **Test run**

Run:

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-core memory::l0_seed::tests -- --nocapture
```

Expected: all 15 unit tests pass.

### Step 5: Commit

- [ ] **Commit Task 1**

```bash
git add core/src/memory/l0_seed.rs core/src/memory/mod.rs
git commit -m "$(cat <<'EOF'
feat(core,memory): L0 seed loader scaffold + pure parser

New core::memory::l0_seed module: types (L0Rule, L0Error,
L0SeedReport), pure parse_l0_rules helper with full validation
(charset, length, dedup, unknown-field rejection), and pure
helpers (compute_body_sha256, compute_source_sha256,
build_l0_metadata, hex_encode_lower). Constants L0_DEFAULT_CAP_ROWS,
L0_DEFAULT_CAP_BYTES, L0_MAX_BODY_BYTES, L0_MAX_ID_LEN.

DB writer + reader bodies stub with todo!() — implemented in
Tasks 2 + 3. 15 unit tests pin the parse / helper contracts.

Spec: docs/superpowers/specs/2026-05-16-l0-seed-loader-design.md.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: `seed_l0_from_rules` + `db::memories::load_active_l0`

**Files:**
- Modify: `db/src/memories.rs` (add `load_active_l0` function)
- Modify: `core/src/memory/l0_seed.rs` (implement `seed_l0_from_rules`)
- Create: `core/tests/memory_l0_seed_e2e.rs` (3 DB integration tests for this task; the remaining 6 land in Task 3)

This task implements the per-rule INSERT path, the idempotency check, and the dedup SELECT. We need the DB-side `load_active_l0` first so the integration tests can read back what was seeded.

### Step 1: Add `db::memories::load_active_l0`

Edit `db/src/memories.rs`. Locate the `load_layer` function (around line 633) and add `load_active_l0` immediately after it.

- [ ] **Edit `db/src/memories.rs`**

Find the end of `load_layer` (just before `#[cfg(test)] mod tests` or the next `pub async fn` boundary; search for `Ok(out)` followed by a closing brace at the function level). Insert this new function:

```rust
/// Load the currently-active L0 rule set, deduplicated by
/// `metadata->>'l0_rule_id'`.
///
/// L0 rows are append-only by `seed_meta_memory`; an edited rule
/// produces a *new* row with the same `l0_rule_id` and a different
/// `body_sha256`. The active set is the newest row per
/// `l0_rule_id`. Rows missing the `l0_rule_id` metadata key (e.g.
/// hand-written test rows or future legacy fixtures) are excluded —
/// they're not part of the seed-loader's universe.
///
/// Returns up to `cap_rows` rows ordered by
/// `(l0_rule_id ASC, created_at DESC, id DESC)` for stable per-rule
/// dedup, but the *outer* return order is `created_at DESC, id DESC`
/// across the deduplicated set so the caller can drop oldest-first
/// when budgeting. The `id DESC` tiebreaker matches `load_layer` for
/// microsecond-clock collisions.
///
/// `cap_rows = 0` is a fast-path no-op (no SQL issued). Saturating
/// cast on `cap_rows` via `limit_as_i64` matches `load_layer`.
pub async fn load_active_l0<'e, E>(
    executor: E,
    cap_rows: usize,
) -> Result<Vec<Memory>, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    if cap_rows == 0 {
        return Ok(Vec::new());
    }
    let limit = limit_as_i64(cap_rows);

    // Two-step SELECT:
    //   1. DISTINCT ON (rule_id) ORDER BY rule_id, created_at DESC,
    //      id DESC — newest row per rule.
    //   2. Outer wrapper re-orders by created_at DESC across the
    //      deduplicated set so the caller's byte-budget drop logic
    //      cuts oldest-first (consistent with load_layer).
    //
    // The `metadata ? 'l0_rule_id'` predicate excludes any L0 rows
    // written without the rule_id metadata key. Such rows are not
    // part of the seed-loader's universe and would otherwise produce
    // a NULL group from the DISTINCT ON.
    let rows = sqlx::query(
        "SELECT id, body, metadata, embedding::text, layer, created_at \
         FROM ( \
             SELECT DISTINCT ON (metadata->>'l0_rule_id') \
                    id, body, metadata, embedding, layer, created_at \
               FROM memories \
              WHERE layer = 0 \
                AND metadata ? 'l0_rule_id' \
              ORDER BY metadata->>'l0_rule_id', created_at DESC, id DESC \
         ) AS dedup \
         ORDER BY created_at DESC, id DESC \
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(executor)
    .await
    .map_err(|e| DbError::Query(format!("load_active_l0: {e}")))?;

    let mut out: Vec<Memory> = Vec::with_capacity(rows.len());
    for row in rows {
        use sqlx::Row;
        let id: i64 = row
            .try_get("id")
            .map_err(|e| DbError::Query(format!("decode id: {e}")))?;
        let body: String = row
            .try_get("body")
            .map_err(|e| DbError::Query(format!("decode body: {e}")))?;
        let metadata: serde_json::Value = row
            .try_get("metadata")
            .map_err(|e| DbError::Query(format!("decode metadata: {e}")))?;
        let layer_raw: i16 = row
            .try_get("layer")
            .map_err(|e| DbError::Query(format!("decode layer: {e}")))?;
        let layer = MemoryLayer::from_db(layer_raw)?;
        let created_at: time::OffsetDateTime = row
            .try_get("created_at")
            .map_err(|e| DbError::Query(format!("decode created_at: {e}")))?;
        out.push(Memory {
            id,
            body,
            metadata,
            layer,
            created_at,
        });
    }
    Ok(out)
}
```

### Step 2: Implement `seed_l0_from_rules` in `core::memory::l0_seed`

Replace the `todo!()` body. The function performs the idempotency check + insert in a single SQL round-trip per rule via the existing `seed_meta_memory` plus a small `query_exists` helper.

- [ ] **Edit `core/src/memory/l0_seed.rs`**

Replace this block:

```rust
pub async fn seed_l0_from_rules(
    _pool: &PgPool,
    _source_path: &Path,
    _source_sha256: &str,
    _rules: &[L0Rule],
) -> Result<L0SeedReport, L0Error> {
    todo!("ships in Task 2")
}
```

with:

```rust
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
        kastellan_db::memories::seed_meta_memory(pool, &rule.body, &metadata, None).await?;
        report.new_rows_written += 1;
    }

    Ok(report)
}
```

### Step 3: Create the integration test scaffold

Create the new test file with the first 3 scenarios. Follow `memory_layers_e2e.rs` structure verbatim.

- [ ] **Create `core/tests/memory_l0_seed_e2e.rs`**

```rust
//! End-to-end smoke for [`kastellan_core::memory::l0_seed`] — the
//! L0 (meta-rule) seed loader and its paired read-side helper.
//!
//! Each scenario brings up its own per-test Postgres cluster (same
//! recipe as `memory_recall_e2e.rs` and `memory_layers_e2e.rs`) so
//! seeded rows cannot drift between scenarios.
//!
//! Skips silently with `[SKIP]` lines on hosts without Postgres or a
//! reachable supervisor; `cargo test -- --nocapture` to see them.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::Path;

use kastellan_core::memory::l0_seed::{
    seed_l0_from_rules, L0Rule,
};
use kastellan_db::memories::load_active_l0;
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime")
}

fn seed_path() -> &'static Path {
    Path::new("seeds/memory/l0_meta_rules.toml")
}

fn make_rule(id: &str, body: &str) -> L0Rule {
    L0Rule {
        id: id.to_string(),
        body: body.to_string(),
        tags: Vec::new(),
    }
}

#[test]
fn seed_from_rules_writes_new_rows() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l0n-d",
        "l0n-l",
        &format!("kastellan-supervisor-test-pg-l0new-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l0-seed-new"}),
        )
        .await
        .expect("probe");

        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let rules = vec![
            make_rule("rule_a", "first body"),
            make_rule("rule_b", "second body"),
        ];
        let report = seed_l0_from_rules(&pool, seed_path(), "src-sha-1", &rules)
            .await
            .expect("seed");

        assert_eq!(report.rules_loaded, 2);
        assert_eq!(report.new_rows_written, 2);
        assert_eq!(report.unchanged_skipped, 0);

        let active = load_active_l0(&pool, 64).await.expect("load");
        assert_eq!(active.len(), 2);
        // Both rules visible; bodies match.
        let bodies: std::collections::HashSet<&str> =
            active.iter().map(|m| m.body.as_str()).collect();
        assert!(bodies.contains("first body"));
        assert!(bodies.contains("second body"));
        // Layer is L0 / Meta.
        for m in &active {
            assert_eq!(
                m.layer,
                kastellan_db::memories::MemoryLayer::Meta,
                "all active L0 rows must report layer=Meta"
            );
        }
        // Metadata keys present.
        for m in &active {
            let meta = m.metadata.as_object().expect("metadata object");
            assert!(meta.contains_key("l0_rule_id"));
            assert!(meta.contains_key("body_sha256"));
            assert!(meta.contains_key("tags"));
            assert!(meta.contains_key("source_path"));
        }

        pool.close().await;
    });
}

#[test]
fn seed_from_rules_is_idempotent_on_unchanged_input() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l0i-d",
        "l0i-l",
        &format!("kastellan-supervisor-test-pg-l0idem-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l0-idempotent"}),
        )
        .await
        .expect("probe");

        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let rules = vec![
            make_rule("rule_a", "first body"),
            make_rule("rule_b", "second body"),
        ];

        let r1 = seed_l0_from_rules(&pool, seed_path(), "sha", &rules)
            .await
            .expect("seed-1");
        assert_eq!(r1.new_rows_written, 2);
        assert_eq!(r1.unchanged_skipped, 0);

        // Same input again → zero new rows.
        let r2 = seed_l0_from_rules(&pool, seed_path(), "sha", &rules)
            .await
            .expect("seed-2");
        assert_eq!(r2.new_rows_written, 0);
        assert_eq!(r2.unchanged_skipped, 2);

        let active = load_active_l0(&pool, 64).await.expect("load");
        assert_eq!(active.len(), 2);

        pool.close().await;
    });
}

#[test]
fn seed_from_rules_writes_new_row_on_edited_body() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l0e-d",
        "l0e-l",
        &format!("kastellan-supervisor-test-pg-l0edit-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l0-edit"}),
        )
        .await
        .expect("probe");

        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let rules_v1 = vec![
            make_rule("rule_a", "original body"),
            make_rule("rule_b", "untouched body"),
        ];
        let r1 = seed_l0_from_rules(&pool, seed_path(), "sha-1", &rules_v1)
            .await
            .expect("seed-v1");
        assert_eq!(r1.new_rows_written, 2);

        // Edit rule_a body, re-seed.
        let rules_v2 = vec![
            make_rule("rule_a", "edited body"),
            make_rule("rule_b", "untouched body"),
        ];
        let r2 = seed_l0_from_rules(&pool, seed_path(), "sha-2", &rules_v2)
            .await
            .expect("seed-v2");
        assert_eq!(r2.new_rows_written, 1); // rule_a got a new row
        assert_eq!(r2.unchanged_skipped, 1); // rule_b already there

        // Active set has 2 rows; rule_a body is the edited one.
        let active = load_active_l0(&pool, 64).await.expect("load");
        assert_eq!(active.len(), 2);
        let mut by_rule_id: std::collections::HashMap<String, String> = Default::default();
        for m in &active {
            let rid = m.metadata["l0_rule_id"].as_str().expect("rule_id").to_string();
            by_rule_id.insert(rid, m.body.clone());
        }
        assert_eq!(by_rule_id.get("rule_a").map(String::as_str), Some("edited body"));
        assert_eq!(by_rule_id.get("rule_b").map(String::as_str), Some("untouched body"));

        // Total memories at layer 0 is 3 (rule_a v1 + rule_a v2 + rule_b).
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM memories WHERE layer = 0",
        )
        .fetch_one(&pool)
        .await
        .expect("count");
        assert_eq!(count, 3, "edited rule must leave its old row behind for audit");

        pool.close().await;
    });
}
```

### Step 3: Verify everything builds

- [ ] **Build check**

Run:

```bash
source "$HOME/.cargo/env"
cargo build --workspace
```

Expected: clean build, no warnings.

### Step 4: Run the new tests

- [ ] **Test run**

Run:

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test memory_l0_seed_e2e -- --nocapture
```

Expected: 3 tests pass (or `[SKIP]` lines if PG isn't reachable — verify with `--nocapture` that they aren't `[SKIP]` on this host).

Also rerun the unit tests to confirm no regression:

```bash
cargo test -p kastellan-core memory::l0_seed::tests
```

Expected: 15 unit tests pass.

### Step 5: Commit

- [ ] **Commit Task 2**

```bash
git add db/src/memories.rs core/src/memory/l0_seed.rs core/tests/memory_l0_seed_e2e.rs
git commit -m "$(cat <<'EOF'
feat(db,core): L0 seed writer + active-set reader

db::memories::load_active_l0 implements the L0-specific
SELECT DISTINCT ON (metadata->>'l0_rule_id') WHERE layer = 0 path
(rows missing the rule_id metadata key are excluded from the active
set; outer ORDER BY created_at DESC matches load_layer).

core::memory::l0_seed::seed_l0_from_rules implements the idempotent
per-rule insert: SHA-256 of body keys the (l0_rule_id, body_sha256)
EXISTS check; on miss, seed_meta_memory inserts a new row carrying
the 4-key metadata blob. Returns L0SeedReport with the
new_rows_written / unchanged_skipped counters.

3 DB integration tests pin: new rows on fresh DB; idempotency on
unchanged input; edited body produces new row while old row stays
for audit (active set surfaces newest version per rule_id).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: `seed_l0_from_file` + `load_l0_active` + `load_l0_active_default` + remaining DB tests

**Files:**
- Modify: `core/src/memory/l0_seed.rs` (implement file/cap helpers)
- Modify: `core/tests/memory_l0_seed_e2e.rs` (add 6 more integration tests)

This task fills in the file-side I/O and the cap-wrapping readers, then adds the remaining 6 scenarios from the spec's test plan.

### Step 1: Implement `seed_l0_from_file`

- [ ] **Edit `core/src/memory/l0_seed.rs`**

Replace this block:

```rust
pub async fn seed_l0_from_file(_pool: &PgPool, _path: &Path) -> Result<L0SeedReport, L0Error> {
    todo!("ships in Task 3")
}
```

with:

```rust
pub async fn seed_l0_from_file(pool: &PgPool, path: &Path) -> Result<L0SeedReport, L0Error> {
    let content = tokio::fs::read_to_string(path).await.map_err(|e| L0Error::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let source_sha256 = compute_source_sha256(&content);
    let rules = parse_l0_rules(path, &content)?;
    seed_l0_from_rules(pool, path, &source_sha256, &rules).await
}
```

Also add the `tokio` `fs` feature usage — `tokio::fs::read_to_string` is gated behind the `fs` feature. Check `core/Cargo.toml`: the workspace `tokio` includes `["fs", "macros", "rt-multi-thread", "signal", "sync", "time", "io-util", "io-std", "net", "process"]`. So `fs` is already there; no Cargo edit needed. (Verify with `grep -A 3 "^tokio" Cargo.toml` if uncertain.)

### Step 2: Implement `load_l0_active` + `load_l0_active_default`

Replace this block:

```rust
pub async fn load_l0_active(
    _pool: &PgPool,
    _cap_rows: usize,
    _cap_bytes: usize,
) -> Result<Vec<Memory>, DbError> {
    todo!("ships in Task 3")
}

pub async fn load_l0_active_default(_pool: &PgPool) -> Result<Vec<Memory>, DbError> {
    todo!("ships in Task 3")
}
```

with:

```rust
pub async fn load_l0_active(
    pool: &PgPool,
    cap_rows: usize,
    cap_bytes: usize,
) -> Result<Vec<Memory>, DbError> {
    if cap_rows == 0 || cap_bytes == 0 {
        return Ok(Vec::new());
    }
    let candidates = kastellan_db::memories::load_active_l0(pool, cap_rows).await?;

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

pub async fn load_l0_active_default(pool: &PgPool) -> Result<Vec<Memory>, DbError> {
    load_l0_active(pool, L0_DEFAULT_CAP_ROWS, L0_DEFAULT_CAP_BYTES).await
}
```

### Step 3: Add the remaining 6 integration tests

Append to `core/tests/memory_l0_seed_e2e.rs`. Update the imports at the top to pull in `load_l0_active` and `load_l0_active_default`.

- [ ] **Edit `core/tests/memory_l0_seed_e2e.rs` — update import block**

Replace the existing `use kastellan_core::memory::l0_seed::{...}` line:

```rust
use kastellan_core::memory::l0_seed::{
    seed_l0_from_rules, L0Rule,
};
```

with:

```rust
use kastellan_core::memory::l0_seed::{
    load_l0_active, load_l0_active_default, seed_l0_from_file, seed_l0_from_rules,
    L0Error, L0Rule, L0_DEFAULT_CAP_BYTES, L0_DEFAULT_CAP_ROWS,
};
```

### Step 4: Append the 6 new tests

- [ ] **Edit `core/tests/memory_l0_seed_e2e.rs` — append**

Append at the end of the file:

```rust

#[test]
fn seed_from_file_reads_parses_and_seeds() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l0f-d",
        "l0f-l",
        &format!("kastellan-supervisor-test-pg-l0file-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l0-from-file"}),
        )
        .await
        .expect("probe");

        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // Write a small TOML to a temp dir.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("l0.toml");
        let toml = r#"
[[rule]]
id = "from_file_a"
body = "rule A body"

[[rule]]
id = "from_file_b"
body = "rule B body"
"#;
        tokio::fs::write(&path, toml).await.expect("write toml");

        let report = seed_l0_from_file(&pool, &path).await.expect("seed");
        assert_eq!(report.rules_loaded, 2);
        assert_eq!(report.new_rows_written, 2);
        assert_eq!(report.unchanged_skipped, 0);
        assert_eq!(report.source_path, path);
        assert_eq!(report.source_sha256.len(), 64, "SHA-256 hex");

        let active = load_l0_active(&pool, 64, 8192).await.expect("load");
        assert_eq!(active.len(), 2);

        pool.close().await;
    });
}

#[test]
fn seed_from_file_fails_closed_on_malformed_toml() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l0m-d",
        "l0m-l",
        &format!("kastellan-supervisor-test-pg-l0mal-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l0-malformed"}),
        )
        .await
        .expect("probe");

        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("bad.toml");
        // Missing body, unterminated string — toml crate must reject.
        tokio::fs::write(&path, "[[rule]]\nid = \"x\"\nbody = \"oops")
            .await
            .expect("write");

        let err = seed_l0_from_file(&pool, &path)
            .await
            .expect_err("malformed toml must fail closed");
        assert!(matches!(err, L0Error::TomlParse { .. }), "got {err:?}");

        // No rows written.
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM memories WHERE layer = 0",
        )
        .fetch_one(&pool)
        .await
        .expect("count");
        assert_eq!(count, 0, "fail-closed must write zero rows");

        pool.close().await;
    });
}

#[test]
fn load_l0_active_returns_newest_per_rule_id() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l0d-d",
        "l0d-l",
        &format!("kastellan-supervisor-test-pg-l0dedup-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l0-dedup"}),
        )
        .await
        .expect("probe");

        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // Seed v1, then v2 of the same rule_id.
        let v1 = vec![make_rule("ruleX", "version 1")];
        seed_l0_from_rules(&pool, seed_path(), "sha-1", &v1)
            .await
            .expect("v1");
        // Sleep 5 ms so created_at differs at microsecond resolution
        // (defense-in-depth — the `id DESC` tiebreaker would also
        // pick the newer row, but pinning on created_at is the
        // documented load_active_l0 contract).
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        let v2 = vec![make_rule("ruleX", "version 2")];
        seed_l0_from_rules(&pool, seed_path(), "sha-2", &v2)
            .await
            .expect("v2");

        let active = load_l0_active_default(&pool).await.expect("load");
        assert_eq!(active.len(), 1, "dedup must return one row per rule_id");
        assert_eq!(active[0].body, "version 2", "newest version wins");

        pool.close().await;
    });
}

#[test]
fn load_l0_active_respects_cap_rows() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l0r-d",
        "l0r-l",
        &format!("kastellan-supervisor-test-pg-l0caprows-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l0-cap-rows"}),
        )
        .await
        .expect("probe");

        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let rules = vec![
            make_rule("r1", "a"),
            make_rule("r2", "b"),
            make_rule("r3", "c"),
        ];
        seed_l0_from_rules(&pool, seed_path(), "sha", &rules)
            .await
            .expect("seed");

        let two = load_l0_active(&pool, 2, 8192).await.expect("cap 2");
        assert_eq!(two.len(), 2, "cap_rows must trim DB-side");

        // Defense-in-depth: cap_rows = 0 returns empty.
        let zero = load_l0_active(&pool, 0, 8192).await.expect("cap 0");
        assert!(zero.is_empty());

        pool.close().await;
    });
}

#[test]
fn load_l0_active_oversize_body_dropped_silently() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l0o-d",
        "l0o-l",
        &format!("kastellan-supervisor-test-pg-l0over-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l0-oversize"}),
        )
        .await
        .expect("probe");

        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // 600-byte body (over 500-byte cap) and 100-byte body (under).
        // Insert the small one *first* so it's older; the dedup query
        // returns newest-first, so the big one comes back first. The
        // byte-budget check trips on the big one and we break — the
        // smaller one is left in the candidates but not added to acc.
        //
        // To make the test deterministic we order so the small body
        // is the newest (returned first by created_at DESC):
        let big_body = "x".repeat(600);
        let small_body = "y".repeat(100);

        let rules1 = vec![L0Rule {
            id: "big".to_string(),
            body: big_body.clone(),
            tags: Vec::new(),
        }];
        seed_l0_from_rules(&pool, seed_path(), "sha-big", &rules1)
            .await
            .expect("seed big");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        let rules2 = vec![L0Rule {
            id: "small".to_string(),
            body: small_body.clone(),
            tags: Vec::new(),
        }];
        seed_l0_from_rules(&pool, seed_path(), "sha-small", &rules2)
            .await
            .expect("seed small");

        // cap_bytes = 500 < big body (600). The small body comes back
        // first (newest), fits; the big body comes back second and
        // pushes cumulative bytes past the cap → break.
        let active = load_l0_active(&pool, 64, 500).await.expect("load");
        assert_eq!(active.len(), 1, "only the small body fits");
        assert_eq!(active[0].body, small_body);

        pool.close().await;
    });
}

#[test]
fn load_l0_active_excludes_legacy_l0_rows_without_rule_id() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l0l-d",
        "l0l-l",
        &format!("kastellan-supervisor-test-pg-l0legacy-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l0-legacy"}),
        )
        .await
        .expect("probe");

        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // A "legacy" L0 row written directly via seed_meta_memory with
        // empty metadata (no l0_rule_id). load_active_l0 must skip it.
        kastellan_db::memories::seed_meta_memory(
            &pool,
            "legacy without rule_id",
            &serde_json::json!({}),
            None,
        )
        .await
        .expect("seed legacy");

        // A real L0 rule.
        let rules = vec![make_rule("real", "real rule body")];
        seed_l0_from_rules(&pool, seed_path(), "sha", &rules)
            .await
            .expect("seed real");

        let active = load_l0_active_default(&pool).await.expect("load");
        assert_eq!(active.len(), 1, "legacy row must be excluded");
        assert_eq!(active[0].body, "real rule body");

        // Sanity: layer-0 total is 2 (the legacy row is in the table,
        // just not in the active set).
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM memories WHERE layer = 0",
        )
        .fetch_one(&pool)
        .await
        .expect("count");
        assert_eq!(count, 2);

        pool.close().await;
    });
}
```

### Step 5: Verify the constants are used (silence unused-import)

The new import block pulls in `L0_DEFAULT_CAP_ROWS` and `L0_DEFAULT_CAP_BYTES`. They're not currently referenced in any of the 9 tests. Either:
(a) Drop them from the import (the tests use literal `64` / `8192` for clarity).
(b) Add `#[allow(unused_imports)]` to the import block.

Pick (a) — literal values in tests make the assertion-vs-default split visible.

- [ ] **Edit `core/tests/memory_l0_seed_e2e.rs` — trim unused imports**

Replace:

```rust
use kastellan_core::memory::l0_seed::{
    load_l0_active, load_l0_active_default, seed_l0_from_file, seed_l0_from_rules,
    L0Error, L0Rule, L0_DEFAULT_CAP_BYTES, L0_DEFAULT_CAP_ROWS,
};
```

with:

```rust
use kastellan_core::memory::l0_seed::{
    load_l0_active, load_l0_active_default, seed_l0_from_file, seed_l0_from_rules,
    L0Error, L0Rule,
};
```

### Step 6: Build + test

- [ ] **Build check**

```bash
source "$HOME/.cargo/env"
cargo build --workspace
```

Expected: clean build, no warnings.

- [ ] **Test run**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test memory_l0_seed_e2e -- --nocapture
cargo test -p kastellan-core memory::l0_seed::tests
```

Expected: 9 integration tests pass, 15 unit tests pass.

### Step 7: Commit

- [ ] **Commit Task 3**

```bash
git add core/src/memory/l0_seed.rs core/tests/memory_l0_seed_e2e.rs
git commit -m "$(cat <<'EOF'
feat(core,memory): L0 seed file loader + capped active-set reader

seed_l0_from_file reads + hashes + parses + seeds in one call.
Malformed TOML or validation failure surfaces as Err; no partial
state. load_l0_active wraps db::memories::load_active_l0 with the
in-Rust byte cap (mirrors load_l1's saturating_add idiom; oversize
single row dropped with tracing::warn). load_l0_active_default
pins the published 64 rows / 8192 bytes caps so callers cannot
silently empty the L0 block via cap_rows=0.

6 new integration tests pin: file round-trip; fail-closed on
malformed TOML; dedup returns newest version per rule_id;
cap_rows trims DB-side; cap_bytes drops oversize body silently
(over-budget single row → warn + skip); legacy L0 rows without
the rule_id metadata key are excluded from the active set.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Wire into `core/src/main.rs` + `l0.seeded` audit row

**Files:**
- Modify: `core/src/main.rs` (wire in the loader + write audit row)

### Step 1: Add the wire-in block

Edit `core/src/main.rs`. Find the existing prompts-loading block (around line 60-66):

```rust
// Load every prompts/*.md, hash, upsert into agent_prompts.
let prompts_dir = std::env::var("KASTELLAN_PROMPTS_DIR")
    .map(std::path::PathBuf::from)
    .unwrap_or_else(|_| std::path::PathBuf::from("prompts"));
let prompts = kastellan_core::scheduler::prompts::load_prompts_from_dir(&pool, &prompts_dir)
    .await
    .with_context(|| format!("loading prompts from {:?}", prompts_dir))?;
```

Insert the L0 loader block **immediately after** the prompts loader and **before** the LLM router setup:

- [ ] **Edit `core/src/main.rs`** — insert after prompts loader

```rust
    // Seed L0 (meta-rule) rows from the operator-edited TOML file.
    // Default: `seeds/memory/l0_meta_rules.toml` relative to CWD.
    // Override: `KASTELLAN_L0_RULES_FILE` env var. Missing file is
    // logged at info level and skipped (daemon comes up). Malformed
    // file is fatal (loader returns Err here, ? propagates).
    let l0_path = std::env::var("KASTELLAN_L0_RULES_FILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("seeds/memory/l0_meta_rules.toml"));
    if l0_path.exists() {
        let report = kastellan_core::memory::l0_seed::seed_l0_from_file(&pool, &l0_path)
            .await
            .with_context(|| format!("seeding L0 rules from {:?}", l0_path))?;
        write_l0_seeded_audit_row(&pool, &report).await?;
        info!(
            rules = report.rules_loaded,
            new = report.new_rows_written,
            unchanged = report.unchanged_skipped,
            "L0 seed loader completed"
        );
    } else {
        info!(path = ?l0_path, "no L0 rules file found, skipping seed");
    }
```

### Step 2: Add the `write_l0_seeded_audit_row` helper

Find the place near the bottom of `main.rs` where private helpers live (e.g. after `sha256_argv0_list` and `hex_encode`). Add this helper:

- [ ] **Edit `core/src/main.rs`** — add helper at module scope

```rust
async fn write_l0_seeded_audit_row(
    pool: &sqlx::PgPool,
    report: &kastellan_core::memory::l0_seed::L0SeedReport,
) -> anyhow::Result<()> {
    let payload = serde_json::json!({
        "rules_loaded": report.rules_loaded,
        "new_rows_written": report.new_rows_written,
        "unchanged_skipped": report.unchanged_skipped,
        "source_path": report.source_path.to_string_lossy(),
        "source_sha256": report.source_sha256,
    });
    kastellan_db::audit::insert(pool, "core", "l0.seeded", &payload)
        .await
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("write l0.seeded audit row: {e}"))
}
```

Verify the `audit::insert` signature matches what's in `db::audit`:

- [ ] **Verification step — check audit::insert signature**

Run:

```bash
grep -n "pub async fn insert" /home/hherb/src/kastellan/db/src/audit.rs
```

Expected output should be a function returning `Result<i64, DbError>` taking `(pool, actor, action, payload)`. If the actual signature differs (e.g. uses `&serde_json::Value` vs owned), adjust the helper body accordingly — the wire-in plus error propagation stays the same.

### Step 3: Confirm the `info!` macro is in scope

`core/src/main.rs` already uses `info!` for the prompts loader (line 56 + similar). No new import needed.

### Step 4: Build

- [ ] **Build check**

```bash
source "$HOME/.cargo/env"
cargo build --workspace
```

Expected: clean build, no warnings. If you see a warning about an unused import, drop it.

### Step 5: Run the full workspace test suite

- [ ] **Workspace test run**

```bash
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | tee /tmp/l0-task4-tests.log
```

Expected: previously-green tests still green. The unit + integration tests from Tasks 1-3 should already total 607 (baseline) + 15 (unit) + 9 (integration) = 631.

Quick verification:

```bash
grep -E "^test result.*passed" /tmp/l0-task4-tests.log | \
  sed -E 's/.*ok\. ([0-9]+) passed; ([0-9]+) failed; ([0-9]+) ignored.*/\1 \2 \3/' | \
  awk '{p+=$1; f+=$2; i+=$3} END {print "TOTAL passed="p, "failed="f, "ignored="i}'
```

Expected: `TOTAL passed=631 failed=0 ignored=4`.

### Step 6: Commit

- [ ] **Commit Task 4**

```bash
git add core/src/main.rs
git commit -m "$(cat <<'EOF'
feat(core,main): wire L0 seed loader into daemon startup

Reads KASTELLAN_L0_RULES_FILE (default: seeds/memory/l0_meta_rules.toml)
right after the prompts loader and before the LLM router. Missing
file is info-logged and skipped; malformed file is fatal (returns
Err, which ? propagates and refuses to start the daemon — matches
probe::run fail-closed posture).

On successful seed, writes one actor='core' action='l0.seeded'
audit row carrying the rules_loaded / new_rows_written /
unchanged_skipped counters plus source_path and source_sha256 for
cross-restart drift detection. Matches the registry.loaded
precedent from the tool-allowlist slice.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Ship the starter TOML

**Files:**
- Create: `seeds/memory/l0_meta_rules.toml`

### Step 1: Create the seeds directory

- [ ] **Create the directory layout**

Run:

```bash
mkdir -p seeds/memory
ls seeds/memory  # verify
```

### Step 2: Write the starter file

- [ ] **Create `seeds/memory/l0_meta_rules.toml`**

```toml
# L0 meta-rules / hard constraints loaded into every system prompt at
# the highest priority. Edit + commit + daemon restart to update.
#
# The loader is idempotent on (rule_id, body_sha256); old versions of
# edited rules stay in the database for audit. Removing a rule from
# this file does NOT delete it from the database — `kastellan-cli`
# tooling (future slice) will surface and prune stale rules.
#
# Each rule needs:
#   id     stable identifier, [a-z0-9_]+, unique within this file
#   body   the rule text the agent reads; <= 1024 bytes, one sentence
#   tags   optional array of strings; not used at load time today,
#          reserved for future filtering

[[rule]]
id = "never_rm_rf"
body = "Never invoke 'rm -rf' or any equivalent recursive destructive command without explicit operator confirmation. If a task plan calls for one, stop and ask."
tags = ["safety", "filesystem"]

[[rule]]
id = "refusal_is_terminal"
body = "If the constitutional reviewer or your own plan emits a refusal, do not re-enter the action loop on the same task. Surface the refusal and stop."
tags = ["safety", "constitutional"]
```

### Step 3: Build + run the full workspace test suite once more

The starter file lives at the daemon's default-CWD-relative path, so any test that brings up the full daemon (e.g. `supervisor_e2e`) will now exercise the loader against it. Verify no regression.

- [ ] **Workspace test run**

```bash
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | tee /tmp/l0-task5-tests.log
```

Expected: `TOTAL passed=631 failed=0 ignored=4`. The starter file does not change any test outcome because the integration tests bring up their own PG clusters with empty state.

If `supervisor_e2e` shows a new audit row in the count, that's expected (the daemon now writes one `actor='core' action='l0.seeded'` row at bring-up). If a hard-coded `audit_log` row-count assertion in `supervisor_e2e` regresses, update the assertion to reflect the new row.

- [ ] **Verify the seed file's location relative to test CWD**

The test harness sets the daemon's working directory via `KASTELLAN_DATA_DIR` / `KASTELLAN_STATE_DIR`, but the *working directory* for the daemon process itself in `supervisor_e2e` comes from the supervisor spec. Check `core_service_spec` to see what `WorkingDirectory` the daemon runs under:

```bash
grep -n "WorkingDirectory\|working_dir" /home/hherb/src/kastellan/supervisor/src/specs.rs | head -5
```

If `WorkingDirectory` is `/` or unset, the daemon will not find `seeds/memory/l0_meta_rules.toml` (cwd-relative). That's *fine* — the loader logs `info!("no L0 rules file found")` and continues. The supervisor smoke test should still pass without writing the `l0.seeded` audit row.

### Step 4: Commit

- [ ] **Commit Task 5**

```bash
git add seeds/memory/l0_meta_rules.toml
git commit -m "$(cat <<'EOF'
feat(seeds): ship starter L0 meta-rules TOML

Two defensible defaults (recursive-delete safety + refusal
stickiness) so a fresh install boots with a non-empty L0 set the
operator can edit. File lives at the daemon's default-CWD-relative
path; an absent file is non-fatal (logged at info).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Update HANDOVER.md + ROADMAP.md

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

### Step 1: Re-check the final test count

- [ ] **Final test verification**

```bash
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 > /tmp/l0-final-tests.log
grep -E "^test result.*passed" /tmp/l0-final-tests.log | \
  sed -E 's/.*ok\. ([0-9]+) passed; ([0-9]+) failed; ([0-9]+) ignored.*/\1 \2 \3/' | \
  awk '{p+=$1; f+=$2; i+=$3} END {print "TOTAL passed="p, "failed="f, "ignored="i}'
echo "SKIPS: $(grep -c '\[SKIP\]' /tmp/l0-final-tests.log)"
echo "WARNINGS: $(grep -c 'warning:' /tmp/l0-final-tests.log)"
```

Expected: `TOTAL passed=631 failed=0 ignored=4`, `SKIPS: 0`, `WARNINGS: 0`.

### Step 2: Add a new "Recently completed (this session)" entry to HANDOVER.md

The HANDOVER.md header field updates plus a new "Recently completed (this session, 2026-05-16 — L0 seed data loader, branch `feat/l0-seed-loader`)" section right after the existing top entry. Include:

- Spec link
- Plan link
- The 5 production commits + this commit (8 total once the docs commit lands)
- Test count 607 → 631
- File-touch list
- What ships / what does not

- [ ] **Edit `docs/devel/handovers/HANDOVER.md`**

(a) Update the header:

Find:

```markdown
**Last updated:** 2026-05-16 (issue #71 — runner rejects producer-supplied `agent_raised` provenance — **merged to `main` via PR #72 at `305941a`**, post-review fixup `5fabac5` on top; +9 unit tests; workspace 598 → **607**).
```

Replace with:

```markdown
**Last updated:** 2026-05-16 (L0 seed data loader — shipped on branch `feat/l0-seed-loader`; new `core::memory::l0_seed` module + `db::memories::load_active_l0` + starter TOML + daemon wire-in; +24 tests; workspace 607 → **631**).
```

Update the Last commit field similarly to point at the new HEAD of `feat/l0-seed-loader`.

(b) Add the new "Recently completed" section immediately after the header block, before the existing "Recently completed (this session, 2026-05-16 — issue #71 ..." section:

```markdown
## Recently completed (this session, 2026-05-16 — L0 seed data loader, branch `feat/l0-seed-loader`)

Branch: `feat/l0-seed-loader` (off `main` at `305941a`). Spec: [`docs/superpowers/specs/2026-05-16-l0-seed-loader-design.md`](../../superpowers/specs/2026-05-16-l0-seed-loader-design.md). Plan: [`docs/superpowers/plans/2026-05-16-l0-seed-loader.md`](../../superpowers/plans/2026-05-16-l0-seed-loader.md). Implements the HANDOVER's "Next concrete engineering pickup #2": startup-time loader that turns a hand-edited TOML of meta-rules into L0 (Meta) rows via the existing `seed_meta_memory` admin function, idempotent on `(l0_rule_id, body_sha256)`.

**Shape (1 NEW core module + 1 modified db module + 1 NEW core integration test file + 1 NEW starter TOML + main.rs wire-in):**

- **NEW `core/src/memory/l0_seed.rs`** — `L0Rule` / `L0Error` / `L0SeedReport` types; pure `parse_l0_rules` (TOML → validated Vec<L0Rule>) with full validation (charset, length, dedup, unknown-field rejection); pure helpers `compute_body_sha256`, `compute_source_sha256`, `build_l0_metadata`; async DB writer `seed_l0_from_rules` (per-rule EXISTS-check + `seed_meta_memory`); file convenience `seed_l0_from_file`; read-side `load_l0_active` / `load_l0_active_default` wrapping `db::memories::load_active_l0` with in-Rust byte caps (mirrors `load_l1`'s saturating_add idiom; oversize single row dropped with `tracing::warn!`). Constants `L0_DEFAULT_CAP_ROWS = 64`, `L0_DEFAULT_CAP_BYTES = 8192`, `L0_MAX_BODY_BYTES = 1024`, `L0_MAX_ID_LEN = 64`.
- **`db/src/memories.rs` — new `load_active_l0` function.** `SELECT DISTINCT ON (metadata->>'l0_rule_id') ... WHERE layer = 0 AND metadata ? 'l0_rule_id'`, outer `ORDER BY created_at DESC, id DESC LIMIT $1`. Rows missing the rule_id metadata key (e.g. legacy hand-fixed L0 rows) are excluded from the active set.
- **NEW `core/tests/memory_l0_seed_e2e.rs`** — 9 DB integration scenarios covering: fresh-DB seed; idempotency on unchanged input; edited body produces new row while old row stays for audit; file round-trip via `seed_l0_from_file`; fail-closed on malformed TOML (no partial state); dedup returns newest version per rule_id; `cap_rows` trims DB-side; `cap_bytes` drops oversize body silently with `tracing::warn!`; legacy L0 rows without `l0_rule_id` metadata excluded.
- **`core/src/main.rs` wire-in** — right after the prompts loader, before the LLM router. Default path `seeds/memory/l0_meta_rules.toml` cwd-relative; override via `KASTELLAN_L0_RULES_FILE`. Missing file → `info!` and skip (daemon comes up); malformed file → `Err`, daemon refuses to start. On success, one `actor='core' action='l0.seeded'` audit row carrying `{rules_loaded, new_rows_written, unchanged_skipped, source_path, source_sha256}`.
- **NEW `seeds/memory/l0_meta_rules.toml`** — starter file with 2 defensible-default rules (recursive-delete safety + refusal stickiness). Operator-owned thereafter.

**Test count delta:** **607 → 631** (+15 unit + +9 DB integration). Zero failures, zero warnings, zero `[SKIP]` lines on Linux.

**Audit-row contract (the new row):**

| When | actor | action | payload keys |
| ---- | ----- | ------ | ------------ |
| Daemon startup if L0 file present | core | `l0.seeded` | `rules_loaded`, `new_rows_written`, `unchanged_skipped`, `source_path`, `source_sha256` |

Five keys exactly; pinned implicitly via the L0SeedReport struct's field set + the wire-in helper's `serde_json::json!` literal. No schema migration.

**TDD ordering (per CLAUDE.md rule #2):** five RED → GREEN → commit cycles matching the plan tasks.
1. `feat(core,memory)`: scaffold + pure parser + 15 unit tests.
2. `feat(db,core)`: `load_active_l0` + `seed_l0_from_rules` + 3 integration tests.
3. `feat(core,memory)`: file loader + cap-wrapping reader + 6 integration tests.
4. `feat(core,main)`: wire-in + audit-row helper.
5. `feat(seeds)`: starter TOML.
6. `docs(handover,roadmap)`: this update.

**What this slice deliberately does NOT do** (matches the spec's non-goals):

- **No prompt-assembler wiring.** `load_l0_active_default` ships but nothing consumes it. Same posture as the L1 slice. Prompt assembler is the next slice.
- **No L0 admin CLI.** Future `kastellan-cli l0 list/diff/lint` is filed if observation surfaces a need.
- **No hot-reload.** Operator edits + restarts the daemon to pick up changes; matches `agent_prompts` cadence.
- **No tag-based filtering at load time.** Tags stored for future ops queries.
- **No embeddings on L0 rows.** They're pinned into every prompt unconditionally; no semantic recall path.
- **No dedicated audit-row shape pin test.** Covered indirectly by `db` audit round-trip tests + `supervisor_e2e` daemon bring-up.

**Open follow-up surfaces (not blocking):**

- **Prompt-assembler `llm_router::build_system_prompt`** is now unblocked — it has both `load_l0_active` and `load_l1` available.
- **`supervisor_e2e` audit-row count** may need a `+1` if the daemon's CWD points at the install root with the starter file present. The bring-up assertion already tolerates "at least one" rows, so this is a no-op unless the test's count became exact at some point.
- **Per-org overlay files** seam exists via `metadata.source_path`; not used today.

**Files touched (3 NEW + 3 modified + 2 docs):**

- NEW `core/src/memory/l0_seed.rs`.
- NEW `core/tests/memory_l0_seed_e2e.rs`.
- NEW `seeds/memory/l0_meta_rules.toml`.
- `core/src/memory/mod.rs` — module declaration.
- `db/src/memories.rs` — `load_active_l0` added.
- `core/src/main.rs` — wire-in + `write_l0_seeded_audit_row` helper.
- `docs/devel/handovers/HANDOVER.md` + `docs/devel/ROADMAP.md` — this update.

---
```

### Step 3: Update ROADMAP.md

Find the existing block of L0/L1 entries (around line 108-111 in the post-merge state). Add a new bullet after the issue #71 entry (line 111):

- [ ] **Edit `docs/devel/ROADMAP.md`** — add new entry after line 111

```markdown
- [x] **L0 seed data loader** — landed 2026-05-16 on branch `feat/l0-seed-loader`. New `core::memory::l0_seed` module: pure `parse_l0_rules` (TOML → validated Vec<L0Rule>); async `seed_l0_from_rules` (idempotent per-rule EXISTS-check on `(l0_rule_id, body_sha256)`); `seed_l0_from_file` (file convenience); `load_l0_active` / `load_l0_active_default` (cap-wrapped active-set reader, mirroring `load_l1`'s saturating_add idiom). New `db::memories::load_active_l0` carries the `SELECT DISTINCT ON (metadata->>'l0_rule_id') WHERE layer = 0` SQL; rows missing the rule_id metadata key are excluded from the active set. Wire-in in `core/src/main.rs` runs right after the prompts loader: env-overridable `KASTELLAN_L0_RULES_FILE`, default `seeds/memory/l0_meta_rules.toml`; missing file = `info!` and skip; malformed file = fatal. One `actor='core' action='l0.seeded'` audit row per daemon startup with `{rules_loaded, new_rows_written, unchanged_skipped, source_path, source_sha256}`. Starter TOML ships in-tree with two defensible-default rules (recursive-delete safety + refusal stickiness). +24 tests (607 → 631). Spec at `docs/superpowers/specs/2026-05-16-l0-seed-loader-design.md`; plan at `docs/superpowers/plans/2026-05-16-l0-seed-loader.md`. Unblocks the prompt-assembler `llm_router::build_system_prompt` slice.
```

### Step 4: Commit

- [ ] **Commit Task 6**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "$(cat <<'EOF'
docs(handover,roadmap): L0 seed data loader shipped

Workspace test count 607 → 631 (+24). Closes the HANDOVER "Next
concrete engineering pickup #2"; unblocks the prompt-assembler
build_system_prompt slice.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Step 5: Final verification

- [ ] **Final commit log**

```bash
git log --oneline main..HEAD
```

Expected: 7 commits (1 spec commit from before plan + 6 plan-task commits) — `7153b48` first, then the 6 task commits in order.

- [ ] **Push the branch when ready**

Don't push automatically; the user will create the PR.

```bash
# When user requests PR:
# git push -u origin feat/l0-seed-loader
# gh pr create --title "..." --body "..."
```

---

## Self-review (post-plan checklist)

**Spec coverage:** ✅ All sections of the spec map to tasks.
- §"Source-file shape" → Task 5
- §"Validation rules" → Task 1 unit tests
- §"Storage shape" → Task 2 `build_l0_metadata` + `seed_l0_from_rules`
- §"Public surface" → Tasks 1-3 (types in 1, db in 2, file/caps in 3)
- §"Dedup query" → Task 2 `db::memories::load_active_l0`
- §"Audit row on seed completion" → Task 4
- §"Wire-in" → Task 4
- §"Tests (TDD ordered)" → Tasks 1-3 cover all 15 unit + 9 integration
- §"Implementation order" → matches Tasks 1-6 (modulo dropping the Cargo.toml step since the dep is already present)

**Placeholder scan:** None remaining. Every step has complete code.

**Type consistency:** Cross-checked:
- `L0Rule { id, body, tags }` consistent across all tasks ✅
- `L0SeedReport` field set is identical between definition (Task 1), writer (Task 2), wire-in (Task 4), and audit-row JSON (Task 4) ✅
- `load_active_l0` is `db::memories::load_active_l0` and takes `(executor, cap_rows)` consistently ✅
- `load_l0_active` is `core::memory::l0_seed::load_l0_active` and takes `(pool, cap_rows, cap_bytes)` — distinct from `load_active_l0`, intentional (db vs core layer) ✅

The plan is ready to execute.

---

**Plan complete and saved to `docs/superpowers/plans/2026-05-16-l0-seed-loader.md`. Two execution options:**

**1. Subagent-Driven (recommended)** - I dispatch a fresh subagent per task, review between tasks, fast iteration

**2. Inline Execution** - Execute tasks in this session using executing-plans, batch execution with checkpoints

**Which approach?**
