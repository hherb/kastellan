# L3 Skill Trust Enum + Approval Gate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a `SkillTrust` enum + a pure operator approval gate for crystallised L3 skills, with `hhagent-cli memory l3 {approve,revoke}` and typed audit rows — no execution.

**Architecture:** A new pure module `core/src/memory/l3_approval.rs` owns the `SkillTrust` enum, the `evaluate_approval` gate (secret-ref scan + structural re-validation + tool-existence against an injected `known_tools` set), and two pure helpers. A layer-guarded db helper `set_skill_trust` flips the stored `trust` JSONB field. The CLI sources `known_tools` from the latest `registry.loaded` audit snapshot (fail-closed when absent) and emits typed `l3.approved`/`l3.approve_rejected`/`l3.revoked` rows via two `cli_audit` composers.

**Tech Stack:** Rust (workspace), `sqlx`/Postgres, `serde_json`, `tokio` tests. No new crate, no new migration.

**Spec:** [`docs/superpowers/specs/2026-05-31-l3-skill-approval-gate-design.md`](../specs/2026-05-31-l3-skill-approval-gate-design.md)

**Build/test prelude (every task):** `source "$HOME/.cargo/env"` first; cargo is not on the non-interactive PATH.

---

## File Structure

| File | Responsibility | New/Mod |
|---|---|---|
| `core/src/memory/l3_approval.rs` | `SkillTrust`, `RejectReason`, `ApprovalDecision`, `evaluate_approval`, `scan_secret_refs`, `extract_tool_names` + unit tests | NEW |
| `core/src/memory/mod.rs` | `pub mod l3_approval;` | MOD |
| `db/src/memories/write.rs` | `set_skill_trust` (layer-guarded UPDATE) | MOD |
| `db/src/memories.rs` | re-export `set_skill_trust` | MOD |
| `db/tests/postgres_e2e.rs` | `set_skill_trust` flip + layer-guard cases | MOD |
| `core/src/scheduler/audit.rs` | 3 action consts + 3 pure payload builders + tests | MOD |
| `core/src/cli_audit.rs` | `l3_approve_and_audit`, `l3_approve_rejected_audit`, `l3_revoke_and_audit` | MOD |
| `core/src/bin/hhagent-cli/memory_l3.rs` | `approve` + `revoke` handlers + router + typed-trust list | MOD |
| `core/tests/cli_memory_l3_e2e.rs` | approve/revoke subprocess scenarios | MOD |

---

## Task 1: `SkillTrust` enum (new module skeleton)

**Files:**
- Create: `core/src/memory/l3_approval.rs`
- Modify: `core/src/memory/mod.rs` (add `pub mod l3_approval;`)

- [ ] **Step 1: Create the module with the enum + its tests (failing — module not yet wired)**

Create `core/src/memory/l3_approval.rs`:

```rust
//! Operator approval gate for crystallised L3 skills (the security
//! control that precedes any invocation path).
//!
//! Crystallised skills land `trust:"untrusted"` and non-executable (see
//! [`crate::memory::l3_crystallise`]). This module adds the typed
//! [`SkillTrust`] read boundary and the pure [`evaluate_approval`] gate
//! an operator runs (via `hhagent-cli memory l3 approve`) before a skill
//! is promoted to `user_approved`. **Nothing here executes a skill** —
//! `UserApproved`/`Pinned` are inert until the invocation slice lands.
//!
//! See `docs/superpowers/specs/2026-05-31-l3-skill-approval-gate-design.md`.

use std::collections::BTreeSet;

use crate::cassandra::types::L3SkillCandidate;
use crate::memory::l3_crystallise::validate_l3_skill;

/// Trust level of a crystallised L3 skill, stored as the metadata
/// `trust` string. Forward-compat: `Pinned` is defined but no command
/// produces it in the gate slice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkillTrust {
    Untrusted,
    UserApproved,
    Pinned,
}

impl SkillTrust {
    /// Metadata-string form. Single source of truth for the literals
    /// written to / read from `metadata->>'trust'`.
    pub fn as_str(self) -> &'static str {
        match self {
            SkillTrust::Untrusted => "untrusted",
            SkillTrust::UserApproved => "user_approved",
            SkillTrust::Pinned => "pinned",
        }
    }

    /// TOTAL, fail-safe parse from a metadata string: any unknown or
    /// absent value maps to [`SkillTrust::Untrusted`]. An unrecognised
    /// trust marker must never read as trusted.
    pub fn from_metadata_str(s: &str) -> SkillTrust {
        match s {
            "user_approved" => SkillTrust::UserApproved,
            "pinned" => SkillTrust::Pinned,
            _ => SkillTrust::Untrusted,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skilltrust_roundtrips_every_variant() {
        for t in [SkillTrust::Untrusted, SkillTrust::UserApproved, SkillTrust::Pinned] {
            assert_eq!(SkillTrust::from_metadata_str(t.as_str()), t);
        }
    }

    #[test]
    fn skilltrust_unknown_or_empty_is_untrusted() {
        assert_eq!(SkillTrust::from_metadata_str("bogus"), SkillTrust::Untrusted);
        assert_eq!(SkillTrust::from_metadata_str(""), SkillTrust::Untrusted);
        assert_eq!(SkillTrust::from_metadata_str("USER_APPROVED"), SkillTrust::Untrusted);
    }
}
```

Note: the `use` of `BTreeSet`, `L3SkillCandidate`, `validate_l3_skill` will be `unused` until Tasks 2–3; that's fine for this step but to avoid `-D warnings` in CI, **add them in Task 2/3 instead**. For Step 1, trim the `use` block to nothing (the enum needs no imports):

Replace the three `use` lines at the top with nothing for now (Task 2 re-adds them). The module top becomes just the doc comment then `#[derive...] pub enum SkillTrust`.

- [ ] **Step 2: Wire the module**

In `core/src/memory/mod.rs`, add alongside the existing `pub mod l3_crystallise;`:

```rust
pub mod l3_approval;
```

- [ ] **Step 3: Run the tests — expect PASS**

Run: `cargo test -p hhagent-core --lib l3_approval::tests`
Expected: 2 tests PASS.

- [ ] **Step 4: Clippy clean**

Run: `cargo clippy -p hhagent-core --all-targets --locked -- -D warnings`
Expected: exit 0 (no unused-import warnings — confirm the `use` block was trimmed).

- [ ] **Step 5: Commit**

```bash
git add core/src/memory/l3_approval.rs core/src/memory/mod.rs
git commit -m "feat(memory): add SkillTrust enum (fail-safe metadata parse)"
```

---

## Task 2: Pure helpers — `scan_secret_refs` + `extract_tool_names`

**Files:**
- Modify: `core/src/memory/l3_approval.rs`

- [ ] **Step 1: Add the failing tests**

Add to `mod tests` in `l3_approval.rs`:

```rust
    #[test]
    fn scan_secret_refs_finds_nested_in_object_and_array() {
        let v = serde_json::json!({
            "argv": ["cat", "secret://abc12345"],
            "nested": { "k": "secret://deadbeef" },
            "plain": "no ref here"
        });
        let mut out = Vec::new();
        scan_secret_refs(&v, &mut out);
        out.sort();
        assert_eq!(out, vec!["secret://abc12345".to_string(), "secret://deadbeef".to_string()]);
    }

    #[test]
    fn scan_secret_refs_ignores_plain_and_object_keys() {
        // A `secret://`-named KEY must NOT be flagged (only string leaves).
        let v = serde_json::json!({ "secret://notavalue": "ok", "x": 42, "y": true });
        let mut out = Vec::new();
        scan_secret_refs(&v, &mut out);
        assert!(out.is_empty(), "got {out:?}");
    }

    #[test]
    fn extract_tool_names_happy() {
        let payload = serde_json::json!({
            "tools": [{"name": "shell-exec", "binary": "/x"}, {"name": "gliner-relex"}]
        });
        let got = extract_tool_names(&payload);
        assert!(got.contains("shell-exec") && got.contains("gliner-relex"));
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn extract_tool_names_handles_missing_malformed() {
        assert!(extract_tool_names(&serde_json::json!({})).is_empty());
        assert!(extract_tool_names(&serde_json::json!({"tools": "notarray"})).is_empty());
        assert!(extract_tool_names(&serde_json::json!({"tools": [{"binary": "/x"}]})).is_empty());
    }
```

- [ ] **Step 2: Run — expect FAIL (functions not defined)**

Run: `cargo test -p hhagent-core --lib l3_approval::tests`
Expected: compile error — `scan_secret_refs` / `extract_tool_names` not found.

- [ ] **Step 3: Implement the helpers**

At the top of `l3_approval.rs`, restore the import block:

```rust
use std::collections::BTreeSet;
```

(Leave `L3SkillCandidate` / `validate_l3_skill` imports for Task 3.) Then add, after the `impl SkillTrust` block:

```rust
/// Recursively collect every string leaf that begins with the secret-ref
/// prefix (`secret://`). Walks objects + arrays but NOT object keys —
/// only *values* can carry a baked-in secret. Mirrors the writer's
/// `collect_placeholders` walker shape.
fn scan_secret_refs(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::String(s) => {
            if s.starts_with(crate::secrets::REF_PREFIX) {
                out.push(s.clone());
            }
        }
        serde_json::Value::Array(a) => {
            for e in a {
                scan_secret_refs(e, out);
            }
        }
        serde_json::Value::Object(m) => {
            for e in m.values() {
                scan_secret_refs(e, out);
            }
        }
        _ => {}
    }
}

/// Extract the set of tool names from a `registry.loaded` audit payload
/// `{ "tools": [ {"name": "..."}, ... ] }`. A missing/`!array` `tools`
/// key, or entries without a string `name`, yield an empty set (which
/// the CLI maps to `NoRegistrySnapshot`).
pub fn extract_tool_names(payload: &serde_json::Value) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    if let Some(arr) = payload.get("tools").and_then(|t| t.as_array()) {
        for entry in arr {
            if let Some(name) = entry.get("name").and_then(|n| n.as_str()) {
                set.insert(name.to_string());
            }
        }
    }
    set
}
```

- [ ] **Step 4: Run — expect PASS**

Run: `cargo test -p hhagent-core --lib l3_approval::tests`
Expected: 6 tests PASS.

- [ ] **Step 5: Clippy + commit**

```bash
cargo clippy -p hhagent-core --all-targets --locked -- -D warnings
git add core/src/memory/l3_approval.rs
git commit -m "feat(memory): scan_secret_refs + extract_tool_names pure helpers"
```

---

## Task 3: The pure approval gate — `evaluate_approval`

**Files:**
- Modify: `core/src/memory/l3_approval.rs`

- [ ] **Step 1: Add the failing tests**

Add to `mod tests`. First a fixture helper, then the cases:

```rust
    fn valid_template() -> L3SkillCandidate {
        use crate::cassandra::types::{L3Param, L3TemplateStep};
        L3SkillCandidate {
            name: "summarise_repo_readme".into(),
            description: "Read a repo README and summarise".into(),
            parameters: vec![L3Param { name: "repo_path".into(), description: "abs path".into() }],
            steps: vec![L3TemplateStep {
                tool: "shell-exec".into(),
                method: "shell.exec".into(),
                parameters: serde_json::json!({ "argv": ["cat", "{{repo_path}}/README.md"] }),
            }],
        }
    }

    fn tools(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn gate_approves_clean_skill_with_known_tool() {
        let d = evaluate_approval(&valid_template(), &tools(&["shell-exec"]));
        assert_eq!(d, ApprovalDecision::Approve);
    }

    #[test]
    fn gate_rejects_unknown_tool() {
        let d = evaluate_approval(&valid_template(), &tools(&["gliner-relex"]));
        assert_eq!(
            d,
            ApprovalDecision::Reject { reasons: vec![RejectReason::UnknownTool { tool: "shell-exec".into() }] }
        );
    }

    #[test]
    fn gate_empty_known_tools_rejects_every_tool() {
        let d = evaluate_approval(&valid_template(), &BTreeSet::new());
        assert!(matches!(d, ApprovalDecision::Reject { .. }));
    }

    #[test]
    fn gate_rejects_baked_in_secret_ref() {
        use crate::cassandra::types::{L3Param, L3TemplateStep};
        let t = L3SkillCandidate {
            name: "leaky".into(),
            description: "carries a secret".into(),
            parameters: vec![L3Param { name: "repo_path".into(), description: "p".into() }],
            steps: vec![L3TemplateStep {
                tool: "shell-exec".into(),
                method: "shell.exec".into(),
                parameters: serde_json::json!({ "argv": ["cat", "{{repo_path}}"], "tok": "secret://abc12345" }),
            }],
        };
        let d = evaluate_approval(&t, &tools(&["shell-exec"]));
        assert_eq!(
            d,
            ApprovalDecision::Reject {
                reasons: vec![RejectReason::SecretRefPresent { step: 0, found: "secret://abc12345".into() }]
            }
        );
    }

    #[test]
    fn gate_accumulates_secret_and_unknown_tool() {
        use crate::cassandra::types::{L3Param, L3TemplateStep};
        let t = L3SkillCandidate {
            name: "leaky_unknown".into(),
            description: "both problems".into(),
            parameters: vec![L3Param { name: "p".into(), description: "d".into() }],
            steps: vec![L3TemplateStep {
                tool: "ghost-tool".into(),
                method: "m.x".into(),
                parameters: serde_json::json!({ "a": "{{p}}", "tok": "secret://deadbeef" }),
            }],
        };
        let d = evaluate_approval(&t, &tools(&["shell-exec"]));
        match d {
            ApprovalDecision::Reject { reasons } => {
                assert!(reasons.contains(&RejectReason::SecretRefPresent { step: 0, found: "secret://deadbeef".into() }));
                assert!(reasons.contains(&RejectReason::UnknownTool { tool: "ghost-tool".into() }));
                assert_eq!(reasons.len(), 2);
            }
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[test]
    fn gate_structurally_invalid_short_circuits() {
        // A template the writer's validator rejects (empty name) → exactly
        // one StructuralInvalid reason, no secret/tool reasons appended.
        let mut t = valid_template();
        t.name = "".into();
        let d = evaluate_approval(&t, &BTreeSet::new());
        match d {
            ApprovalDecision::Reject { reasons } => {
                assert_eq!(reasons.len(), 1);
                assert!(matches!(reasons[0], RejectReason::StructuralInvalid(_)));
            }
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[test]
    fn reject_reason_renders_human_readable() {
        assert!(RejectReason::NoRegistrySnapshot.to_string().contains("registry"));
        assert!(RejectReason::UnknownTool { tool: "x".into() }.to_string().contains("not registered"));
    }
```

- [ ] **Step 2: Run — expect FAIL (types/fn not defined)**

Run: `cargo test -p hhagent-core --lib l3_approval::tests`
Expected: compile error — `ApprovalDecision` / `RejectReason` / `evaluate_approval` not found.

- [ ] **Step 3: Implement the gate**

At the top of `l3_approval.rs`, restore the remaining imports:

```rust
use crate::cassandra::types::L3SkillCandidate;
use crate::memory::l3_crystallise::validate_l3_skill;
```

Add (after `extract_tool_names`):

```rust
/// A single reason an approval was refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RejectReason {
    /// The stored template failed `validate_l3_skill` re-validation
    /// (e.g. hand-edited in SQL, or written by an older validator).
    StructuralInvalid(String),
    /// A step's parameters embed a baked-in `secret://` reference.
    SecretRefPresent { step: usize, found: String },
    /// A step names a tool the running daemon did not register.
    UnknownTool { tool: String },
    /// No `registry.loaded` snapshot exists, so tool existence could not
    /// be established. Constructed by the CLI orchestration, NOT by
    /// `evaluate_approval` (which only sees a `known_tools` set).
    NoRegistrySnapshot,
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RejectReason::StructuralInvalid(m) => {
                write!(f, "structural validation failed: {m}")
            }
            RejectReason::SecretRefPresent { step, found } => write!(
                f,
                "step {step} embeds a secret reference '{found}' \
                 (skills must not carry baked-in secrets)"
            ),
            RejectReason::UnknownTool { tool } => {
                write!(f, "tool '{tool}' is not registered by the running daemon")
            }
            RejectReason::NoRegistrySnapshot => write!(
                f,
                "no registry.loaded snapshot found; start the daemon once \
                 so the tool registry is recorded"
            ),
        }
    }
}

/// The gate's verdict.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApprovalDecision {
    Approve,
    Reject { reasons: Vec<RejectReason> },
}

impl ApprovalDecision {
    pub fn is_approve(&self) -> bool {
        matches!(self, ApprovalDecision::Approve)
    }
}

/// Decide whether a stored skill template may be promoted to
/// `UserApproved`. **PURE** — no I/O. `known_tools` is the set of tool
/// names the live daemon registered (from the latest `registry.loaded`
/// snapshot); an empty set is fail-closed (every step tool is unknown).
///
/// Checks, collecting ALL reasons so the operator sees every problem:
/// 1. structural re-validation (short-circuits — later checks assume a
///    well-formed template);
/// 2. baked-in `secret://` refs in step parameters (one reason per
///    occurrence, with the step index);
/// 3. tool existence (one reason per distinct unknown tool).
pub fn evaluate_approval(
    template: &L3SkillCandidate,
    known_tools: &BTreeSet<String>,
) -> ApprovalDecision {
    if let Err(e) = validate_l3_skill(template) {
        return ApprovalDecision::Reject {
            reasons: vec![RejectReason::StructuralInvalid(e.to_string())],
        };
    }

    let mut reasons = Vec::new();

    for (i, step) in template.steps.iter().enumerate() {
        let mut found = Vec::new();
        scan_secret_refs(&step.parameters, &mut found);
        for f in found {
            reasons.push(RejectReason::SecretRefPresent { step: i, found: f });
        }
    }

    let mut unknown_seen: BTreeSet<&str> = BTreeSet::new();
    for step in &template.steps {
        if !known_tools.contains(&step.tool) && unknown_seen.insert(step.tool.as_str()) {
            reasons.push(RejectReason::UnknownTool { tool: step.tool.clone() });
        }
    }

    if reasons.is_empty() {
        ApprovalDecision::Approve
    } else {
        ApprovalDecision::Reject { reasons }
    }
}
```

- [ ] **Step 4: Run — expect PASS**

Run: `cargo test -p hhagent-core --lib l3_approval::tests`
Expected: all (13) tests PASS.

- [ ] **Step 5: Clippy + commit**

```bash
cargo clippy -p hhagent-core --all-targets --locked -- -D warnings
git add core/src/memory/l3_approval.rs
git commit -m "feat(memory): evaluate_approval gate (secret-ref + structural + tool existence)"
```

---

## Task 4: db `set_skill_trust` (layer-guarded UPDATE)

**Files:**
- Modify: `db/src/memories/write.rs`
- Modify: `db/src/memories.rs` (re-export)
- Test: `db/tests/postgres_e2e.rs`

- [ ] **Step 1: Add the failing PG integration test**

In `db/tests/postgres_e2e.rs`, find an existing `#[tokio::test]` that brings up a cluster + inserts a memory (search for `insert_memory_at_layer` usage to copy the scaffold). Add this test, reusing that file's existing cluster-bringup helper (named `bring_up`/`pg_cluster_or_skip` — match the surrounding tests):

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_skill_trust_flips_and_is_layer_guarded() {
    // -- bring up cluster + runtime pool exactly like the sibling tests --
    let Some(ctx) = pg_e2e_or_skip("set_skill_trust").await else { return; };
    let pool = &ctx.pool;

    // Seed one L3 row (trust starts "untrusted") and one L1 row.
    let meta = serde_json::json!({ "trust": "untrusted", "template": {"name": "s"} });
    let l3_id = hhagent_db::memories::insert_memory_at_layer(
        pool, "body", &meta, None, hhagent_db::memories::MemoryLayer::Skill,
    ).await.expect("insert L3");
    let l1_id = hhagent_db::memories::insert_memory_at_layer(
        pool, "idxbody", &serde_json::json!({"trust": "untrusted"}), None,
        hhagent_db::memories::MemoryLayer::Index,
    ).await.expect("insert L1");

    // Flip the L3 row → returns true, metadata.trust becomes user_approved,
    // and the rest of metadata is preserved.
    let updated = hhagent_db::memories::set_skill_trust(pool, l3_id, "user_approved")
        .await.expect("set_skill_trust");
    assert!(updated, "existing L3 row must report updated=true");

    let row: serde_json::Value = sqlx::query_scalar("SELECT metadata FROM memories WHERE id = $1")
        .bind(l3_id).fetch_one(pool).await.expect("fetch metadata");
    assert_eq!(row.get("trust").and_then(|v| v.as_str()), Some("user_approved"));
    assert_eq!(row.get("template").and_then(|t| t.get("name")).and_then(|v| v.as_str()), Some("s"),
        "set_skill_trust must preserve other metadata keys");

    // Layer guard: the same id on the wrong layer (L1) is a no-op.
    let l1_updated = hhagent_db::memories::set_skill_trust(pool, l1_id, "user_approved")
        .await.expect("set_skill_trust L1");
    assert!(!l1_updated, "an L1 id must NOT be updated by the layer-3-guarded helper");

    // Non-existent id → false.
    let ghost = hhagent_db::memories::set_skill_trust(pool, 999_999, "user_approved")
        .await.expect("set_skill_trust ghost");
    assert!(!ghost);
}
```

> **Scaffold note:** match the exact cluster-bringup idiom used by the other `postgres_e2e.rs` tests (the helper name + skip pattern differ per file). If there is no single `pg_e2e_or_skip` helper, copy the `bring_up_pg_cluster(...) + probe_run + connect_runtime_pool + skip_if_no_supervisor/pg_bin_dir_or_skip` block from the nearest existing test verbatim and inline `pool`.

- [ ] **Step 2: Run — expect FAIL (fn not found)**

Run: `HHAGENT_PG_BIN_DIR=<pg bin> cargo test -p hhagent-db --test postgres_e2e set_skill_trust_flips_and_is_layer_guarded`
(Postgres.app v18 bin dir, e.g. `/Applications/Postgres 2.app/Contents/Versions/18/bin/`.)
Expected: compile error — `set_skill_trust` not found.

- [ ] **Step 3: Implement the helper**

In `db/src/memories/write.rs`, after `delete_memory_at_layer`, add:

```rust
/// Flip a layer-3 (`MemoryLayer::Skill`) row's metadata `trust` field via
/// `jsonb_set` (other metadata keys untouched). Layer-guarded so an
/// L0/L1/L2 id — or a non-existent id — is a no-op. Returns `true` iff a
/// row was updated. Takes a `&str` trust value: the `db` crate sits below
/// `core` and cannot depend on the `core`-owned `SkillTrust` enum.
pub async fn set_skill_trust<'e, E>(
    executor: E,
    id: i64,
    trust: &str,
) -> Result<bool, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows = sqlx::query(
        "UPDATE memories \
         SET metadata = jsonb_set(metadata, '{trust}', to_jsonb($2::text), true) \
         WHERE id = $1 AND layer = $3",
    )
    .bind(id)
    .bind(trust)
    .bind(MemoryLayer::Skill.as_db())
    .execute(executor)
    .await
    .map_err(|e| DbError::Query(format!("set_skill_trust id={id}: {e}")))?;
    Ok(rows.rows_affected() == 1)
}
```

(`MemoryLayer` + `DbError` are already in scope in `write.rs` — `delete_memory_at_layer` uses both.)

- [ ] **Step 4: Re-export**

In `db/src/memories.rs`, add `set_skill_trust` to the `pub use write::{ ... }` list (the line currently starting `delete_memory_at_layer, insert_memory, ...`):

```rust
pub use write::{
    delete_memory_at_layer, insert_memory, insert_memory_at_layer, link_memory_to_entities,
    seed_meta_memory, set_skill_trust,
};
```

(Keep whatever names are already there; just add `set_skill_trust`, alphabetically near `seed_meta_memory`.)

- [ ] **Step 5: Run — expect PASS**

Run: `HHAGENT_PG_BIN_DIR=<pg bin> cargo test -p hhagent-db --test postgres_e2e set_skill_trust_flips_and_is_layer_guarded`
Expected: 1 test PASS (or `[SKIP]` line if no PG — re-run with the bin dir set).

- [ ] **Step 6: Clippy + commit**

```bash
cargo clippy -p hhagent-db --all-targets --locked -- -D warnings
git add db/src/memories/write.rs db/src/memories.rs db/tests/postgres_e2e.rs
git commit -m "feat(db): set_skill_trust layer-guarded metadata UPDATE for L3 rows"
```

---

## Task 5: Audit constants + pure payload builders

**Files:**
- Modify: `core/src/scheduler/audit.rs`

- [ ] **Step 1: Add the failing tests**

In `core/src/scheduler/audit.rs`'s `mod tests`, add:

```rust
    #[test]
    fn l3_approved_payload_shape() {
        let p = build_l3_approved_payload(7, "summarise_repo_readme", "abcd", &["shell-exec".to_string()]);
        assert_eq!(p["memory_id"], 7);
        assert_eq!(p["skill_name"], "summarise_repo_readme");
        assert_eq!(p["body_sha256"], "abcd");
        assert_eq!(p["tools"][0], "shell-exec");
    }

    #[test]
    fn l3_approve_rejected_payload_includes_reasons_and_optionals() {
        let p = build_l3_approve_rejected_payload(
            9, Some("leaky"), Some("ff00"), &["tool 'x' is not registered".to_string()],
        );
        assert_eq!(p["memory_id"], 9);
        assert_eq!(p["skill_name"], "leaky");
        assert_eq!(p["body_sha256"], "ff00");
        assert_eq!(p["reasons"][0], "tool 'x' is not registered");

        // Optionals omitted when None.
        let p2 = build_l3_approve_rejected_payload(9, None, None, &["x".to_string()]);
        assert!(p2.get("skill_name").is_none());
        assert!(p2.get("body_sha256").is_none());
        assert_eq!(p2["reasons"][0], "x");
    }

    #[test]
    fn l3_revoked_payload_shape() {
        let p = build_l3_revoked_payload(3, true);
        assert_eq!(p["memory_id"], 3);
        assert_eq!(p["updated"], true);
    }
```

- [ ] **Step 2: Run — expect FAIL**

Run: `cargo test -p hhagent-core --lib scheduler::audit::tests::l3_`
Expected: compile error — builders not found.

- [ ] **Step 3: Add constants + builders**

Near the existing `ACTION_L3_REMOVED` constant, add:

```rust
/// Action verb for the operator `memory l3 approve` success row.
pub const ACTION_L3_APPROVED: &str = "l3.approved";
/// Action verb for the operator `memory l3 approve` rejection row (the
/// gate refused). Audited because an operator attempting to approve a
/// skill carrying a `secret://` ref is a security-relevant event.
pub const ACTION_L3_APPROVE_REJECTED: &str = "l3.approve_rejected";
/// Action verb for the operator `memory l3 revoke` row (trust → untrusted).
pub const ACTION_L3_REVOKED: &str = "l3.revoked";
```

Then, near `build_l3_write_payload`, add (the file already `use`s `serde_json::Value as Value` — match the existing alias; if it uses `serde_json::Value` fully-qualified, mirror that):

```rust
/// Payload for an `l3.approved` row. `tools` is the template's distinct
/// step tools the gate verified against the registry snapshot.
pub fn build_l3_approved_payload(
    memory_id: i64,
    skill_name: &str,
    body_sha256: &str,
    tools: &[String],
) -> Value {
    serde_json::json!({
        "memory_id": memory_id,
        "skill_name": skill_name,
        "body_sha256": body_sha256,
        "tools": tools,
    })
}

/// Payload for an `l3.approve_rejected` row. `skill_name`/`body_sha256`
/// are omitted when the row/template could not be parsed.
pub fn build_l3_approve_rejected_payload(
    memory_id: i64,
    skill_name: Option<&str>,
    body_sha256: Option<&str>,
    reasons: &[String],
) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("memory_id".into(), Value::Number(serde_json::Number::from(memory_id)));
    if let Some(n) = skill_name {
        obj.insert("skill_name".into(), Value::String(n.into()));
    }
    if let Some(s) = body_sha256 {
        obj.insert("body_sha256".into(), Value::String(s.into()));
    }
    obj.insert(
        "reasons".into(),
        Value::Array(reasons.iter().map(|r| Value::String(r.clone())).collect()),
    );
    Value::Object(obj)
}

/// Payload for an `l3.revoked` row.
pub fn build_l3_revoked_payload(memory_id: i64, updated: bool) -> Value {
    serde_json::json!({ "memory_id": memory_id, "updated": updated })
}
```

> If `audit.rs` does NOT alias `serde_json::Value as Value`, replace `Value` with `serde_json::Value` throughout the three builders (and `serde_json::Map`, `serde_json::Number`). Check the top of the file first.

- [ ] **Step 4: Run — expect PASS**

Run: `cargo test -p hhagent-core --lib scheduler::audit::tests::l3_`
Expected: 3 tests PASS.

- [ ] **Step 5: Clippy + commit**

```bash
cargo clippy -p hhagent-core --all-targets --locked -- -D warnings
git add core/src/scheduler/audit.rs
git commit -m "feat(scheduler): l3 approve/revoke audit constants + payload builders"
```

---

## Task 6: `cli_audit` composers (mutate + best-effort audit)

**Files:**
- Modify: `core/src/cli_audit.rs`

- [ ] **Step 1: Extend the constant import + add the three helpers**

In `core/src/cli_audit.rs`, extend the `use crate::scheduler::audit::{ ... }` block (around line 97–103) to also import the three new actions + the two builders that the helpers use directly:

```rust
use crate::scheduler::audit::{
    // ... existing imports ...
    ACTION_L3_APPROVED, ACTION_L3_APPROVE_REJECTED, ACTION_L3_REVOKED,
    build_l3_approved_payload, build_l3_approve_rejected_payload, build_l3_revoked_payload,
};
```

(Keep the existing names in that block; add the six new names. `build_*` may already be re-exported under `audit::`; import via the same path.)

Then, after `l3_remove_and_audit` (ends ~line 622), add:

```rust
/// Flip an L3 row to `user_approved` and emit one `actor='cli'
/// action='l3.approved'` row. The gate decision is made by the caller
/// ([`crate::memory::l3_approval::evaluate_approval`]); this helper only
/// composes the trust flip with its audit row. Best-effort audit.
pub async fn l3_approve_and_audit(
    pool: &PgPool,
    memory_id: i64,
    skill_name: &str,
    body_sha256: &str,
    tools: &[String],
) -> Result<i64, hhagent_db::DbError> {
    use crate::memory::l3_approval::SkillTrust;

    hhagent_db::memories::set_skill_trust(pool, memory_id, SkillTrust::UserApproved.as_str()).await?;
    let payload = build_l3_approved_payload(memory_id, skill_name, body_sha256, tools);
    let audit_id = match hhagent_db::audit::insert(
        pool, CLI_AUDIT_ACTOR, ACTION_L3_APPROVED, payload,
    ).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "l3.approved audit insert failed (best-effort)");
            0
        }
    };
    Ok(audit_id)
}

/// Emit one `actor='cli' action='l3.approve_rejected'` row. NO trust
/// change — the gate refused. Best-effort audit. Returns the audit id.
pub async fn l3_approve_rejected_audit(
    pool: &PgPool,
    memory_id: i64,
    skill_name: Option<&str>,
    body_sha256: Option<&str>,
    reasons: &[String],
) -> Result<i64, hhagent_db::DbError> {
    let payload = build_l3_approve_rejected_payload(memory_id, skill_name, body_sha256, reasons);
    let audit_id = match hhagent_db::audit::insert(
        pool, CLI_AUDIT_ACTOR, ACTION_L3_APPROVE_REJECTED, payload,
    ).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "l3.approve_rejected audit insert failed (best-effort)");
            0
        }
    };
    Ok(audit_id)
}

/// Flip an L3 row to `untrusted` (a downgrade — no gate) and emit one
/// `actor='cli' action='l3.revoked'` row. Returns `(updated, audit_id)`,
/// mirroring [`l3_remove_and_audit`]. Best-effort audit.
pub async fn l3_revoke_and_audit(
    pool: &PgPool,
    memory_id: i64,
) -> Result<(bool, i64), hhagent_db::DbError> {
    use crate::memory::l3_approval::SkillTrust;

    let updated = hhagent_db::memories::set_skill_trust(pool, memory_id, SkillTrust::Untrusted.as_str()).await?;
    let payload = build_l3_revoked_payload(memory_id, updated);
    let audit_id = match hhagent_db::audit::insert(
        pool, CLI_AUDIT_ACTOR, ACTION_L3_REVOKED, payload,
    ).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "l3.revoked audit insert failed (best-effort)");
            0
        }
    };
    Ok((updated, audit_id))
}
```

- [ ] **Step 2: Build — expect PASS (no new unit test; e2e covers behaviour in Task 8)**

Run: `cargo build -p hhagent-core`
Expected: compiles. (`PgPool` + `CLI_AUDIT_ACTOR` are already in scope in this file.)

- [ ] **Step 3: Clippy + commit**

```bash
cargo clippy -p hhagent-core --all-targets --locked -- -D warnings
git add core/src/cli_audit.rs
git commit -m "feat(cli_audit): l3 approve / approve_rejected / revoke composers"
```

---

## Task 7: CLI `approve` + `revoke` subcommands + typed-trust list

**Files:**
- Modify: `core/src/bin/hhagent-cli/memory_l3.rs`

- [ ] **Step 1: Update the router + usage**

In `core/src/bin/hhagent-cli/memory_l3.rs`, change the `run_memory_l3` match + the empty-args usage line:

```rust
pub(crate) fn run_memory_l3(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: hhagent-cli memory l3 <list|approve|revoke|remove> ...");
        return ExitCode::from(2);
    }
    match args[0].as_str() {
        "list"    => with_runtime("memory l3", memory_l3_list(&args[1..])),
        "approve" => with_runtime("memory l3", memory_l3_approve(&args[1..])),
        "revoke"  => with_runtime("memory l3", memory_l3_revoke(&args[1..])),
        "remove"  => with_runtime("memory l3", memory_l3_remove(&args[1..])),
        other     => {
            eprintln!("memory l3: unknown action '{other}'; expected: list | approve | revoke | remove");
            ExitCode::from(2)
        }
    }
}
```

- [ ] **Step 2: Type the `list` trust display**

In `memory_l3_list`, replace the `let trust = ...` line (currently `r.metadata.get("trust")...unwrap_or("?")`) with the fail-safe typed read:

```rust
        let trust = hhagent_core::memory::l3_approval::SkillTrust::from_metadata_str(
            r.metadata.get("trust").and_then(|v| v.as_str()).unwrap_or(""),
        )
        .as_str();
```

- [ ] **Step 3: Add the `approve` handler + the registry-snapshot helper**

Append to `memory_l3.rs`:

```rust
/// Fetch the latest `registry.loaded` snapshot's tool-name set, or `None`
/// when the daemon has never recorded one.
async fn latest_registry_tools(
    pool: &sqlx::PgPool,
) -> Result<Option<std::collections::BTreeSet<String>>, hhagent_db::DbError> {
    use hhagent_core::memory::l3_approval::extract_tool_names;
    use hhagent_core::scheduler::audit::ACTION_REGISTRY_LOADED;

    let payload: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT payload FROM audit_log \
         WHERE actor = 'core' AND action = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(ACTION_REGISTRY_LOADED)
    .fetch_optional(pool)
    .await
    .map_err(|e| hhagent_db::DbError::Query(format!("latest_registry_tools: {e}")))?;

    Ok(payload.map(|p| extract_tool_names(&p)))
}

async fn memory_l3_approve(args: &[String]) -> ExitCode {
    use std::collections::BTreeSet;

    use hhagent_core::cassandra::types::L3SkillCandidate;
    use hhagent_core::cli_audit::{l3_approve_and_audit, l3_approve_rejected_audit};
    use hhagent_core::memory::l3_approval::{evaluate_approval, ApprovalDecision, RejectReason};
    use hhagent_db::memories::{fetch_by_ids, MemoryLayer};
    use hhagent_db::pool::connect_runtime_pool;

    let id_str = match args {
        [s] => s,
        _ => {
            eprintln!("usage: hhagent-cli memory l3 approve <id>");
            return ExitCode::from(2);
        }
    };
    let id: i64 = match id_str.parse() {
        Ok(n) => n,
        Err(e) => {
            eprintln!("memory l3 approve: invalid id '{id_str}': {e}");
            return ExitCode::from(2);
        }
    };

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    // --- fetch + layer-guard the row -------------------------------------
    let row = match fetch_by_ids(&pool, &[id]).await {
        Ok(mut v) => v.pop(),
        Err(e) => { eprintln!("memory l3 approve: {e}"); return ExitCode::from(1); }
    };
    let row = match row {
        Some(r) if r.layer == MemoryLayer::Skill => r,
        _ => {
            eprintln!("memory l3 approve: no layer-3 skill with id={id}");
            return ExitCode::from(1);
        }
    };
    let body_sha256 = row.metadata.get("body_sha256").and_then(|v| v.as_str());

    // --- parse the stored template ---------------------------------------
    let template: L3SkillCandidate = match row
        .metadata
        .get("template")
        .cloned()
        .and_then(|t| serde_json::from_value(t).ok())
    {
        Some(t) => t,
        None => {
            let reasons = vec!["stored L3 row has no parseable 'template'".to_string()];
            let _ = l3_approve_rejected_audit(&pool, id, None, body_sha256, &reasons).await;
            eprintln!("memory l3 approve: id={id} has no parseable template; not approved");
            return ExitCode::from(1);
        }
    };
    let skill_name = template.name.clone();

    // --- registry snapshot → decision ------------------------------------
    let decision = match latest_registry_tools(&pool).await {
        Ok(Some(known)) => evaluate_approval(&template, &known),
        Ok(None) => ApprovalDecision::Reject { reasons: vec![RejectReason::NoRegistrySnapshot] },
        Err(e) => { eprintln!("memory l3 approve: {e}"); return ExitCode::from(1); }
    };

    match decision {
        ApprovalDecision::Approve => {
            let tools: Vec<String> = {
                let mut s = BTreeSet::new();
                for st in &template.steps { s.insert(st.tool.clone()); }
                s.into_iter().collect()
            };
            let sha = body_sha256.unwrap_or("");
            if let Err(e) = l3_approve_and_audit(&pool, id, &skill_name, sha, &tools).await {
                eprintln!("memory l3 approve: {e}");
                return ExitCode::from(1);
            }
            println!("approved skill '{skill_name}' (#{id}) → trust=user_approved");
            ExitCode::from(0)
        }
        ApprovalDecision::Reject { reasons } => {
            let rendered: Vec<String> = reasons.iter().map(|r| r.to_string()).collect();
            let _ = l3_approve_rejected_audit(&pool, id, Some(&skill_name), body_sha256, &rendered).await;
            eprintln!("approval REJECTED for skill '{skill_name}' (#{id}):");
            for r in &rendered { eprintln!("  - {r}"); }
            ExitCode::from(1)
        }
    }
}

async fn memory_l3_revoke(args: &[String]) -> ExitCode {
    use hhagent_core::cli_audit::l3_revoke_and_audit;
    use hhagent_db::pool::connect_runtime_pool;

    let id_str = match args {
        [s] => s,
        _ => {
            eprintln!("usage: hhagent-cli memory l3 revoke <id>");
            return ExitCode::from(2);
        }
    };
    let id: i64 = match id_str.parse() {
        Ok(n) => n,
        Err(e) => {
            eprintln!("memory l3 revoke: invalid id '{id_str}': {e}");
            return ExitCode::from(2);
        }
    };

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    match l3_revoke_and_audit(&pool, id).await {
        Ok((true, _))  => { println!("revoked id={id} → trust=untrusted"); ExitCode::from(0) }
        Ok((false, _)) => {
            println!("no row at layer 3 with id={id} (already gone or wrong layer)");
            ExitCode::from(0)
        }
        Err(e) => { eprintln!("memory l3 revoke: {e}"); ExitCode::from(1) }
    }
}
```

- [ ] **Step 4: Build — expect PASS**

Run: `cargo build -p hhagent-core --bin hhagent-cli`
Expected: compiles. If `fetch_by_ids` is not re-exported at `hhagent_db::memories::fetch_by_ids`, check `db/src/memories.rs`'s `pub use search::{...}` and use the exact path (it is re-exported there).

- [ ] **Step 5: Clippy + commit**

```bash
cargo clippy -p hhagent-core --all-targets --locked -- -D warnings
git add core/src/bin/hhagent-cli/memory_l3.rs
git commit -m "feat(cli): memory l3 approve/revoke + typed-trust list"
```

---

## Task 8: CLI subprocess e2e scenarios

**Files:**
- Modify: `core/tests/cli_memory_l3_e2e.rs`

Reuse the file's existing helpers: `valid_skill()`, `cli_env()`, `bring_up_pg_cluster`, `pg_bin_dir_or_skip`, `skip_if_no_supervisor`, `unique_suffix`, `probe_run`, `connect_runtime_pool`, `crystallise_l3`. Add a `secret-ref` fixture + a `registry.loaded` seeder near the top, then four scenarios.

- [ ] **Step 1: Add fixtures (after `valid_skill`)**

```rust
/// A structurally-valid skill that ALSO carries a baked-in secret ref —
/// the writer accepts it (no secret scan); the approval gate must reject.
fn skill_with_secret_ref() -> L3SkillCandidate {
    L3SkillCandidate {
        name: "leaky_skill".into(),
        description: "carries a secret ref".into(),
        parameters: vec![L3Param { name: "repo_path".into(), description: "abs path".into() }],
        steps: vec![L3TemplateStep {
            tool: "shell-exec".into(),
            method: "shell.exec".into(),
            parameters: serde_json::json!({
                "argv": ["cat", "{{repo_path}}"],
                "token": "secret://abc12345"
            }),
        }],
    }
}

/// Seed a `registry.loaded` audit row naming `tool_names` so the CLI's
/// approval gate can verify tool existence.
async fn seed_registry_loaded(pool: &sqlx::PgPool, tool_names: &[&str]) {
    let tools: Vec<serde_json::Value> =
        tool_names.iter().map(|n| serde_json::json!({ "name": n })).collect();
    hhagent_db::audit::insert(
        pool,
        "core",
        hhagent_core::scheduler::audit::ACTION_REGISTRY_LOADED,
        serde_json::json!({ "tools": tools }),
    )
    .await
    .expect("seed registry.loaded");
}
```

- [ ] **Step 2: Scenario — approve happy path**

```rust
/// Seed a valid skill + a registry.loaded row naming its tool; approve
/// exits 0 and a follow-up list shows `user_approved`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_memory_l3_approve_happy() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir, "cml3-app-d", "cml3-app-l",
        &format!("hhagent-postgres-cli-memory-l3-approve-{suffix}"),
    );
    probe_run(&cluster.conn_spec, "core", "startup",
        serde_json::json!({"test": "cli_memory_l3_approve_happy"})).await.expect("probe");
    let pool = connect_runtime_pool(&cluster.conn_spec).await.expect("pool");

    let outcome = crystallise_l3(&pool, &valid_skill(), L3Source::AgentRaised { task_id: 1 })
        .await.expect("crystallise_l3");
    let id = outcome.memory_id();
    seed_registry_loaded(&pool, &["shell-exec"]).await;

    let bin = cli_binary();
    let env = cli_env(&cluster.data_dir);

    let out = Command::new(&bin)
        .args(["memory", "l3", "approve", &id.to_string()])
        .env_clear().envs(env.clone()).output().expect("spawn approve");
    let so = String::from_utf8_lossy(&out.stdout).into_owned();
    let se = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "approve must exit 0; stdout={so}\nstderr={se}");
    assert!(so.contains("user_approved"), "approve stdout must confirm; got {so}");

    let list = Command::new(&bin).args(["memory", "l3", "list"])
        .env_clear().envs(env).output().expect("spawn list");
    let lo = String::from_utf8_lossy(&list.stdout).into_owned();
    assert!(lo.contains("user_approved"), "list must show user_approved; got {lo}");

    drop(pool); drop(cluster);
}
```

- [ ] **Step 3: Scenario — approve rejected on a baked-in secret ref**

```rust
/// A skill carrying a `secret://` ref is rejected: non-zero exit, trust
/// stays `untrusted`, an `l3.approve_rejected` audit row exists.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_memory_l3_approve_rejects_secret_ref() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir, "cml3-sec-d", "cml3-sec-l",
        &format!("hhagent-postgres-cli-memory-l3-secret-{suffix}"),
    );
    probe_run(&cluster.conn_spec, "core", "startup",
        serde_json::json!({"test": "cli_memory_l3_approve_rejects_secret_ref"})).await.expect("probe");
    let pool = connect_runtime_pool(&cluster.conn_spec).await.expect("pool");

    let outcome = crystallise_l3(&pool, &skill_with_secret_ref(), L3Source::AgentRaised { task_id: 1 })
        .await.expect("crystallise_l3");
    let id = outcome.memory_id();
    seed_registry_loaded(&pool, &["shell-exec"]).await; // tool IS known → only reason is the secret ref

    let bin = cli_binary();
    let env = cli_env(&cluster.data_dir);

    let out = Command::new(&bin)
        .args(["memory", "l3", "approve", &id.to_string()])
        .env_clear().envs(env).output().expect("spawn approve");
    let se = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!out.status.success(), "approve must exit non-zero on a secret ref");
    assert!(se.contains("secret"), "stderr must explain the secret-ref reason; got {se}");

    // trust unchanged
    let trust: String = sqlx::query_scalar("SELECT metadata->>'trust' FROM memories WHERE id = $1")
        .bind(id).fetch_one(&pool).await.expect("fetch trust");
    assert_eq!(trust, "untrusted", "trust must NOT change on a rejected approval");

    // a rejection audit row exists
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE actor='cli' AND action='l3.approve_rejected'")
        .fetch_one(&pool).await.expect("count rejected rows");
    assert!(n >= 1, "expected an l3.approve_rejected audit row");

    drop(pool); drop(cluster);
}
```

- [ ] **Step 4: Scenario — fail-closed when no registry snapshot**

```rust
/// With NO registry.loaded row, approve fails closed (NoRegistrySnapshot)
/// and trust stays untrusted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_memory_l3_approve_fail_closed_no_snapshot() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir, "cml3-noc-d", "cml3-noc-l",
        &format!("hhagent-postgres-cli-memory-l3-nosnap-{suffix}"),
    );
    probe_run(&cluster.conn_spec, "core", "startup",
        serde_json::json!({"test": "cli_memory_l3_approve_fail_closed_no_snapshot"})).await.expect("probe");
    let pool = connect_runtime_pool(&cluster.conn_spec).await.expect("pool");

    let outcome = crystallise_l3(&pool, &valid_skill(), L3Source::AgentRaised { task_id: 1 })
        .await.expect("crystallise_l3");
    let id = outcome.memory_id();
    // NOTE: deliberately NOT seeding registry.loaded.

    let out = Command::new(&cli_binary())
        .args(["memory", "l3", "approve", &id.to_string()])
        .env_clear().envs(cli_env(&cluster.data_dir)).output().expect("spawn approve");
    let se = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!out.status.success(), "approve must fail closed with no snapshot");
    assert!(se.contains("registry"), "stderr must mention the missing registry snapshot; got {se}");

    let trust: String = sqlx::query_scalar("SELECT metadata->>'trust' FROM memories WHERE id = $1")
        .bind(id).fetch_one(&pool).await.expect("fetch trust");
    assert_eq!(trust, "untrusted");

    drop(pool); drop(cluster);
}
```

- [ ] **Step 5: Scenario — revoke after approve**

```rust
/// Approve then revoke: trust goes untrusted → user_approved → untrusted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_memory_l3_revoke_after_approve() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir, "cml3-rev-d", "cml3-rev-l",
        &format!("hhagent-postgres-cli-memory-l3-revoke-{suffix}"),
    );
    probe_run(&cluster.conn_spec, "core", "startup",
        serde_json::json!({"test": "cli_memory_l3_revoke_after_approve"})).await.expect("probe");
    let pool = connect_runtime_pool(&cluster.conn_spec).await.expect("pool");

    let outcome = crystallise_l3(&pool, &valid_skill(), L3Source::AgentRaised { task_id: 1 })
        .await.expect("crystallise_l3");
    let id = outcome.memory_id();
    seed_registry_loaded(&pool, &["shell-exec"]).await;

    let env = cli_env(&cluster.data_dir);
    let approve = Command::new(&cli_binary())
        .args(["memory", "l3", "approve", &id.to_string()])
        .env_clear().envs(env.clone()).output().expect("spawn approve");
    assert!(approve.status.success(), "approve must succeed first");

    let revoke = Command::new(&cli_binary())
        .args(["memory", "l3", "revoke", &id.to_string()])
        .env_clear().envs(env).output().expect("spawn revoke");
    let so = String::from_utf8_lossy(&revoke.stdout).into_owned();
    assert!(revoke.status.success(), "revoke must exit 0");
    assert!(so.contains("untrusted"), "revoke stdout must confirm; got {so}");

    let trust: String = sqlx::query_scalar("SELECT metadata->>'trust' FROM memories WHERE id = $1")
        .bind(id).fetch_one(&pool).await.expect("fetch trust");
    assert_eq!(trust, "untrusted");

    drop(pool); drop(cluster);
}
```

- [ ] **Step 6: Run the e2e suite — expect PASS (PG live) or `[SKIP]`**

Run: `HHAGENT_PG_BIN_DIR=<pg bin> cargo test -p hhagent-core --test cli_memory_l3_e2e`
Expected: the 4 existing + 4 new scenarios PASS (or all `[SKIP]` without PG — then re-run with the bin dir).

- [ ] **Step 7: Clippy + commit**

```bash
cargo clippy -p hhagent-core --all-targets --locked -- -D warnings
git add core/tests/cli_memory_l3_e2e.rs
git commit -m "test(cli): l3 approve/revoke subprocess e2e (happy, secret-ref, fail-closed, revoke)"
```

---

## Task 9: Full verification + docs

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md`

- [ ] **Step 1: Full workspace test (skip-as-pass without PG)**

Run: `cargo test --workspace`
Expected: baseline 1177 + the new unit/db/e2e tests, 0 failed. Record the exact count.

- [ ] **Step 2: Full workspace test WITH live PG**

Run: `HHAGENT_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin/" cargo test --workspace`
Expected: the L3 approve/revoke e2e + `set_skill_trust` db e2e run green; only the known `embedding_recall_e2e`/`gliner_relex_e2e` initdb/pg_notify flake may appear (passes on retry; matches `main`).

- [ ] **Step 3: Clippy gate (whole workspace)**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: exit 0.

- [ ] **Step 4: Doc-link check**

Run: `RUSTDOCFLAGS=-D rustdoc::broken_intra_doc_links cargo doc -p hhagent-core --no-deps --document-private-items`
Expected: same unresolved-link count as `main` (21); zero new. Fix any new `l3_approval`/`set_skill_trust` links flagged.

- [ ] **Step 5: Update HANDOVER.md + ROADMAP.md**

Per the checklist at the bottom of `HANDOVER.md`: update the header (Currently-on = `feat/l3-skill-approval-gate`, what shipped, test-count delta), add a "Recently completed" entry, move the Next-TODO 10(b) sub-item to done with the follow-ups (invocation/execution + recall surfacing 10c still open), and mirror into `ROADMAP.md`. Stage only these two files (NOT `docs/essay-medium-draft.md`).

- [ ] **Step 6: Commit docs**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs(handover,roadmap): L3 skill trust enum + approval gate shipped"
```

- [ ] **Step 7: Push + open PR (only when the operator approves)**

```bash
git push -u origin feat/l3-skill-approval-gate
gh pr create --title "feat: L3 skill trust enum + approval gate (slice 1 of invocation arc)" --body "..."
```

---

## Self-Review

**Spec coverage:**
- `SkillTrust` enum (Untrusted|UserApproved|Pinned), fail-safe parse → Task 1. ✓
- Pure gate: secret-ref scan + structural re-validation + tool existence → Task 3 (helpers Task 2). ✓
- `NoRegistrySnapshot` fail-closed, CLI-constructed → Task 3 (variant) + Task 7 (CLI produces it). ✓
- db `set_skill_trust` layer-guarded, `&str` param → Task 4. ✓
- 3 audit actions + pure builders, rejected path audited → Task 5. ✓
- `cli_audit` composers → Task 6. ✓
- CLI approve + revoke + typed list, `Pinned` command-less → Task 7. ✓
- e2e: approve happy, secret-ref reject, fail-closed, revoke → Task 8. (Unknown-tool reject is covered at the unit tier in Task 3; not re-run as a separate e2e to keep the PG-bringup count down — noted intentionally.) ✓
- No execution / no recall surfacing → enforced by omission; documented in docs (Task 9). ✓

**Placeholder scan:** No TBD/TODO. The one "match the surrounding scaffold" note (Task 4 Step 1, Task 8) is a real instruction (the postgres_e2e/cli_e2e bringup idiom is file-local), not a content gap — the test-specific bodies are fully written.

**Type consistency:** `SkillTrust::{as_str,from_metadata_str}`, `evaluate_approval(&L3SkillCandidate, &BTreeSet<String>) -> ApprovalDecision`, `RejectReason::{StructuralInvalid,SecretRefPresent{step,found},UnknownTool{tool},NoRegistrySnapshot}`, `set_skill_trust(executor,id,&str)->bool`, `build_l3_approved_payload`/`build_l3_approve_rejected_payload`/`build_l3_revoked_payload`, `l3_approve_and_audit`/`l3_approve_rejected_audit`/`l3_revoke_and_audit` — names used identically across Tasks 1–8. ✓
