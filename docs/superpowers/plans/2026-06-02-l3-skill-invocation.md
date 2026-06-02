# L3 Skill Invocation (operator-triggered) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `hhagent-cli memory l3 run <id> [--arg k=v]… [--execute]` so an operator can execute an approved L3 skill — substitute params → live-registry re-validation → sandboxed dispatch of each step → audit — dry-run by default.

**Architecture:** A new pure engine module `core/src/memory/l3_invoke.rs` (arg parsing, `{{placeholder}}` substitution, a pure `prepare_invocation` decision reusing the approval gate against the *live* tool set, and an async `invoke_l3` orchestration that drives the existing `ToolHostStepDispatcher`). Registry construction is factored out of the daemon binary into a shared lib module `core/src/registry_build.rs` (no audit side effect) so the CLI can rebuild an identical registry in-process. New `l3.invoked` / `l3.invoke_outcome` / `l3.invoke_rejected` audit rows; per-step rows come from the unchanged `tool_host::dispatch` chokepoint. No CASSANDRA review on the operator path; planner prompt untouched.

**Tech Stack:** Rust (workspace crate `hhagent-core`), sqlx/PgPool, `serde_json`, the existing `StepDispatcher` / `ToolHostStepDispatcher` / `ToolRegistry` / `SkillTrust` / `evaluate_approval` machinery.

**Spec:** `docs/superpowers/specs/2026-06-02-l3-skill-invocation-design.md`

---

## File Structure

**New files:**
- `core/src/registry_build.rs` — shared, audit-free registry construction: `LoadedToolRecord`, `sha256_argv0_list`, `hex_encode`, `build_gliner_relex_entry`, `build_tool_registry`, `build_registry_loaded_payload`.
- `core/src/memory/l3_invoke.rs` — `InvokeError`, consts, `parse_args`, `substitute_template`, `is_runnable`, `prepare_invocation`, `InvokeRefusal`, `planned_step_from_l3`, `run_steps`, `InvokeReport`, `invoke_l3`.
- `core/src/memory/l3_invoke/tests.rs` — pure unit tests (created in Task 3, grown through Task 7) **only if** the parent approaches the 500-LOC cap; otherwise inline `#[cfg(test)] mod tests`. Default: inline until it nears the cap, then lift (the established L3 pattern).
- `core/tests/cli_memory_l3_run_e2e.rs` — live-PG e2e.

**Modified files:**
- `core/src/scheduler/audit.rs` — 3 new action constants + 3 pure payload builders.
- `core/src/memory/mod.rs` — `pub mod l3_invoke;`
- `core/src/lib.rs` — `pub mod registry_build;`
- `core/src/main.rs` — delete the moved registry helpers; call the lib versions; write the `registry.loaded` row at the call site.
- `core/src/bin/hhagent-cli/memory_l3.rs` — add the `run` subcommand + handler.

---

## Task 1: Factor registry construction into a shared lib module (behaviour-preserving refactor)

Moves the registry builder out of the daemon binary so the CLI can rebuild an identical registry, and removes the audit-write side effect from the builder (the daemon writes the `registry.loaded` row at the call site; the CLI must never write it).

**Files:**
- Create: `core/src/registry_build.rs`
- Modify: `core/src/lib.rs` (add `pub mod registry_build;`)
- Modify: `core/src/main.rs` (delete moved items; rewire call site)

- [ ] **Step 1: Create `core/src/registry_build.rs` with the moved, audit-free builder**

```rust
//! Shared construction of the scheduler's [`ToolRegistry`] — the host-side
//! allowlist of *which* tools the daemon may dispatch.
//!
//! Factored out of the daemon binary (`main.rs`) so the operator CLI can
//! rebuild an identical registry in-process (e.g. `memory l3 run`, which
//! re-validates an approved skill's tools against the registry *as it is
//! now* — the live TOCTOU close). The builder here has **no audit side
//! effect**: it returns the per-tool records and the caller decides whether
//! to write the `registry.loaded` row. The daemon writes it; the CLI must
//! NOT (writing a spurious row would corrupt the snapshot the approval gate
//! reads).

use crate::scheduler::tool_dispatch::ToolEntry;
use crate::scheduler::ToolRegistry;

/// One per-tool record carried in the `registry.loaded` audit-row payload.
#[derive(serde::Serialize)]
pub struct LoadedToolRecord {
    pub name: String,
    pub binary: String,
    pub allowlist_len: usize,
    /// SHA-256 of the canonical-form allowlist: `argv0_1 || '\n' || …`
    /// (lexicographically sorted, trailing newline after the last entry;
    /// empty list → SHA-256 of the empty string).
    pub allowlist_sha256: String,
}

/// SHA-256 of the canonical-form (sorted, newline-joined) argv0 allowlist.
pub fn sha256_argv0_list(argv0s: &[String]) -> String {
    use sha2::{Digest, Sha256};
    let mut sorted: Vec<&String> = argv0s.iter().collect();
    sorted.sort();
    let mut hasher = Sha256::new();
    for argv0 in sorted {
        hasher.update(argv0.as_bytes());
        hasher.update(b"\n");
    }
    hex_encode(&hasher.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

/// Build the GLiNER-Relex tool entry from environment variables. Returns
/// `None` on every skip path (worker disabled / weights missing / …),
/// logging the typed skip reason. Moved verbatim from `main.rs`.
pub fn build_gliner_relex_entry() -> Option<ToolEntry> {
    use crate::workers::gliner_relex::{gliner_relex_entry, resolve_env};

    match resolve_env(|k| std::env::var(k).ok(), |p| p.is_dir(), |p| p.exists()) {
        Ok(env) => Some(gliner_relex_entry(&env)),
        Err(reason) => {
            log_gliner_relex_skip(&reason);
            None
        }
    }
}

fn log_gliner_relex_skip(reason: &crate::workers::gliner_relex::ResolveSkipReason) {
    use crate::workers::gliner_relex::ResolveSkipReason as R;
    match reason {
        R::Disabled => tracing::info!(
            tool = crate::workers::gliner_relex::Client::TOOL_NAME,
            "gliner-relex disabled (HHAGENT_GLINER_RELEX_ENABLE != 1); not registered"
        ),
        other => tracing::error!(
            tool = crate::workers::gliner_relex::Client::TOOL_NAME,
            reason = ?other,
            "gliner-relex enabled but misconfigured; not registered"
        ),
    }
}

/// Build the registry of tools the scheduler may dispatch. Reads the
/// shell-exec argv allowlist from the `tool_allowlists` DB table and the
/// `HHAGENT_SHELL_EXEC_BIN` env var; folds in the optional gliner-relex
/// entry. **Writes no audit row** — returns the per-tool records so the
/// caller can write `registry.loaded` itself (daemon only).
pub async fn build_tool_registry(
    pool: &sqlx::PgPool,
    gliner_relex_entry: Option<ToolEntry>,
) -> Result<(ToolRegistry, Vec<LoadedToolRecord>), hhagent_db::DbError> {
    let mut reg = ToolRegistry::new();
    let mut loaded: Vec<LoadedToolRecord> = Vec::new();

    if let Some(bin_os) = std::env::var_os("HHAGENT_SHELL_EXEC_BIN") {
        let binary = std::path::PathBuf::from(&bin_os);
        if binary.is_file() {
            let allowlist = hhagent_db::tool_allowlists::list_for_tool(pool, "shell-exec")
                .await
                .map_err(|e| {
                    hhagent_db::DbError::Query(format!("loading shell-exec allowlist: {e}"))
                })?;
            let entry = crate::scheduler::shell_exec_entry(binary.clone(), &allowlist);
            tracing::info!(
                tool = "shell-exec",
                binary = %binary.display(),
                allowlist_len = allowlist.len(),
                "registering tool"
            );
            loaded.push(LoadedToolRecord {
                name: "shell-exec".to_string(),
                binary: binary.display().to_string(),
                allowlist_len: allowlist.len(),
                allowlist_sha256: sha256_argv0_list(&allowlist),
            });
            reg.insert("shell-exec", entry);
        } else {
            tracing::warn!(
                binary = %binary.display(),
                "HHAGENT_SHELL_EXEC_BIN does not point to an existing file; \
                 shell-exec NOT registered"
            );
        }
    }

    if std::env::var_os("HHAGENT_SHELL_EXEC_ALLOWLIST").is_some() {
        tracing::warn!(
            "HHAGENT_SHELL_EXEC_ALLOWLIST is no longer honored; \
             use 'hhagent-cli tools allowlist add <tool> <argv0>' to populate the DB"
        );
    }

    if let Some(entry) = gliner_relex_entry {
        tracing::info!(
            tool = crate::workers::gliner_relex::Client::TOOL_NAME,
            binary = %entry.binary.display(),
            "registering tool"
        );
        loaded.push(LoadedToolRecord {
            name: crate::workers::gliner_relex::Client::TOOL_NAME.to_string(),
            binary: entry.binary.display().to_string(),
            allowlist_len: 0,
            allowlist_sha256: sha256_argv0_list(&[]),
        });
        reg.insert(crate::workers::gliner_relex::Client::TOOL_NAME, entry);
    }

    Ok((reg, loaded))
}

/// Pure payload builder for the `registry.loaded` audit row. The daemon
/// calls this then `hhagent_db::audit::insert`; the CLI never does.
pub fn build_registry_loaded_payload(tools: &[LoadedToolRecord]) -> serde_json::Value {
    serde_json::json!({ "tools": tools })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_argv0_list_is_order_independent_and_empty_is_empty_string_sha() {
        let a = sha256_argv0_list(&["ls".into(), "cat".into()]);
        let b = sha256_argv0_list(&["cat".into(), "ls".into()]);
        assert_eq!(a, b, "canonical form sorts before hashing");
        // SHA-256 of "" (no entries → no bytes fed).
        assert_eq!(
            sha256_argv0_list(&[]),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn build_registry_loaded_payload_wraps_tools_array() {
        let recs = vec![LoadedToolRecord {
            name: "shell-exec".into(),
            binary: "/x".into(),
            allowlist_len: 1,
            allowlist_sha256: "deadbeef".into(),
        }];
        let v = build_registry_loaded_payload(&recs);
        assert_eq!(v["tools"][0]["name"], "shell-exec");
        assert_eq!(v["tools"][0]["allowlist_len"], 1);
    }
}
```

- [ ] **Step 2: Register the module in `core/src/lib.rs`**

Add (in the module list, alphabetical neighbourhood near other top-level modules — match existing ordering):

```rust
pub mod registry_build;
```

Run: `grep -n "pub mod registry_build;" core/src/lib.rs` — Expected: one match.

- [ ] **Step 3: Delete the moved items from `core/src/main.rs`**

Delete these now-duplicated definitions from `core/src/main.rs`:
- `async fn build_tool_registry(...)` (the whole fn)
- `fn build_gliner_relex_entry()` (the whole fn)
- `fn log_gliner_relex_skip(...)` (the whole fn)
- `struct LoadedToolRecord { ... }` (the whole struct)
- `fn sha256_argv0_list(...)` and `fn hex_encode(...)` (both)

Keep `async fn write_registry_loaded_row(...)` but rewrite its body to use the lib payload builder (Step 4).

- [ ] **Step 4: Rewire the `main.rs` call site + `write_registry_loaded_row`**

Replace the registry-build call (currently around `main.rs:120` and `main.rs:134`):

```rust
    let gliner_relex_entry = hhagent_core::registry_build::build_gliner_relex_entry();
```

and

```rust
    let (registry, loaded_tool_records) =
        hhagent_core::registry_build::build_tool_registry(&pool, gliner_relex_entry.clone())
            .await?;
    let tool_registry = Arc::new(registry);
    // Best-effort audit row (was previously written inside build_tool_registry;
    // moved here now that the builder is side-effect-free).
    if let Err(e) = write_registry_loaded_row(&pool, &loaded_tool_records).await {
        tracing::warn!(error = %e, "registry.loaded audit row insert failed");
    }
```

Rewrite `write_registry_loaded_row` in `main.rs` to take the lib record type and use the lib payload builder:

```rust
async fn write_registry_loaded_row(
    pool: &sqlx::PgPool,
    tools: &[hhagent_core::registry_build::LoadedToolRecord],
) -> Result<(), hhagent_db::DbError> {
    let payload = hhagent_core::registry_build::build_registry_loaded_payload(tools);
    hhagent_db::audit::insert(
        pool,
        "core",
        hhagent_core::scheduler::audit::ACTION_REGISTRY_LOADED,
        payload,
    )
    .await
    .map(|_| ())
}
```

- [ ] **Step 5: Build the workspace and run the registry-build unit tests**

Run: `source "$HOME/.cargo/env" && cargo build --workspace 2>&1 | tail -20`
Expected: clean build (the daemon binary + lib both compile; no `unused import` for the deleted `info!` if it's still used elsewhere — if `use tracing::info;` is now unused in main.rs, remove it).

Run: `cargo test -p hhagent-core registry_build 2>&1 | tail -20`
Expected: the 2 new unit tests PASS.

- [ ] **Step 6: Verify no behaviour change + clippy**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings 2>&1 | tail -20`
Expected: exit 0.

Run: `cargo test --workspace 2>&1 | tail -5`
Expected: same pass count as `main` baseline + 2 new (registry_build unit tests). No failures.

- [ ] **Step 7: Commit**

```bash
git add core/src/registry_build.rs core/src/lib.rs core/src/main.rs
git commit -m "refactor(registry): factor audit-free build_tool_registry into lib

Moves build_tool_registry + build_gliner_relex_entry + LoadedToolRecord +
sha256_argv0_list out of the daemon binary into core/src/registry_build.rs,
with NO audit side effect (returns the per-tool records; main.rs writes the
registry.loaded row at the call site). Lets the operator CLI rebuild an
identical registry in-process for live skill-invocation re-validation.
Behaviour-preserving; daemon registry.loaded row unchanged.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: L3 invocation audit constants + pure payload builders

**Files:**
- Modify: `core/src/scheduler/audit.rs` (constants near the existing `ACTION_L3_*`; payload builders near `build_l3_approved_payload` ~line 468)

- [ ] **Step 1: Write failing tests for the three payload builders**

Add to the `#[cfg(test)] mod tests` block in `core/src/scheduler/audit.rs` (or its `tests.rs` sibling if one exists — match where `build_l3_approved_payload` is tested):

```rust
#[test]
fn build_l3_invoked_payload_shape() {
    let p = build_l3_invoked_payload(7, "summarise_repo", "abc123", &["repo_path".into()], 2);
    assert_eq!(p["memory_id"], 7);
    assert_eq!(p["skill_name"], "summarise_repo");
    assert_eq!(p["body_sha256"], "abc123");
    assert_eq!(p["arg_names"][0], "repo_path");
    assert_eq!(p["step_count"], 2);
}

#[test]
fn build_l3_invoke_outcome_payload_shape() {
    let p = build_l3_invoke_outcome_payload(7, "summarise_repo", 1, 2, true);
    assert_eq!(p["memory_id"], 7);
    assert_eq!(p["skill_name"], "summarise_repo");
    assert_eq!(p["steps_executed"], 1);
    assert_eq!(p["steps_total"], 2);
    assert_eq!(p["any_err"], true);
}

#[test]
fn build_l3_invoke_rejected_payload_shape() {
    let p = build_l3_invoke_rejected_payload(7, Some("leaky"), Some("sha9"), &["bad tool".into()]);
    assert_eq!(p["memory_id"], 7);
    assert_eq!(p["skill_name"], "leaky");
    assert_eq!(p["body_sha256"], "sha9");
    assert_eq!(p["reasons"][0], "bad tool");
}

#[test]
fn build_l3_invoke_rejected_payload_omits_optional_when_none() {
    let p = build_l3_invoke_rejected_payload(7, None, None, &["r".into()]);
    assert!(p.get("skill_name").is_none());
    assert!(p.get("body_sha256").is_none());
    assert_eq!(p["memory_id"], 7);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-core build_l3_invoke 2>&1 | tail -20`
Expected: FAIL — `cannot find function build_l3_invoked_payload` etc.

- [ ] **Step 3: Add the constants**

Insert after `ACTION_L3_REVOKED` (~`core/src/scheduler/audit.rs:125`):

```rust
/// Action verb for the start-of-execution row written by `memory l3 run
/// --execute`. Payload built by [`build_l3_invoked_payload`].
pub const ACTION_L3_INVOKED: &str = "l3.invoked";
/// Action verb for the end-of-execution summary row. Payload built by
/// [`build_l3_invoke_outcome_payload`].
pub const ACTION_L3_INVOKE_OUTCOME: &str = "l3.invoke_outcome";
/// Action verb for a refused run attempt (trust gate or live re-validation
/// rejected), written before any dispatch. Audited because attempting to
/// run a non-runnable / now-invalid skill is a security-relevant event.
/// Payload built by [`build_l3_invoke_rejected_payload`].
pub const ACTION_L3_INVOKE_REJECTED: &str = "l3.invoke_rejected";
```

- [ ] **Step 4: Add the payload builders**

Insert near `build_l3_approved_payload` (~`core/src/scheduler/audit.rs:468`); use `serde_json::Value` (the file already imports `Value` — match the existing builders' `Value` usage):

```rust
/// Payload for the `l3.invoked` row. Carries arg *names* only (not
/// values); substituted values land in the per-step chokepoint rows where
/// secret-refs stay opaque.
pub fn build_l3_invoked_payload(
    memory_id: i64,
    skill_name: &str,
    body_sha256: &str,
    arg_names: &[String],
    step_count: usize,
) -> Value {
    serde_json::json!({
        "memory_id": memory_id,
        "skill_name": skill_name,
        "body_sha256": body_sha256,
        "arg_names": arg_names,
        "step_count": step_count,
    })
}

/// Payload for the `l3.invoke_outcome` row. Mirrors `plan.outcome`.
pub fn build_l3_invoke_outcome_payload(
    memory_id: i64,
    skill_name: &str,
    steps_executed: usize,
    steps_total: usize,
    any_err: bool,
) -> Value {
    serde_json::json!({
        "memory_id": memory_id,
        "skill_name": skill_name,
        "steps_executed": steps_executed,
        "steps_total": steps_total,
        "any_err": any_err,
    })
}

/// Payload for the `l3.invoke_rejected` row. `skill_name` / `body_sha256`
/// are optional (a row whose template would not parse has neither).
/// Mirrors `build_l3_approve_rejected_payload`.
pub fn build_l3_invoke_rejected_payload(
    memory_id: i64,
    skill_name: Option<&str>,
    body_sha256: Option<&str>,
    reasons: &[String],
) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("memory_id".into(), serde_json::json!(memory_id));
    if let Some(n) = skill_name {
        obj.insert("skill_name".into(), serde_json::json!(n));
    }
    if let Some(s) = body_sha256 {
        obj.insert("body_sha256".into(), serde_json::json!(s));
    }
    obj.insert("reasons".into(), serde_json::json!(reasons));
    Value::Object(obj)
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p hhagent-core build_l3_invoke 2>&1 | tail -20`
Expected: all 4 tests PASS.

- [ ] **Step 6: Commit**

```bash
git add core/src/scheduler/audit.rs
git commit -m "feat(audit): l3.invoked / l3.invoke_outcome / l3.invoke_rejected constants + builders

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: `l3_invoke` module skeleton — `InvokeError`, consts, `parse_args`

**Files:**
- Create: `core/src/memory/l3_invoke.rs`
- Modify: `core/src/memory/mod.rs` (add `pub mod l3_invoke;`)

- [ ] **Step 1: Register the module**

In `core/src/memory/mod.rs`, add alongside the other `pub mod l3_*;` lines:

```rust
pub mod l3_invoke;
```

- [ ] **Step 2: Create `l3_invoke.rs` with the error type, consts, and `parse_args` + failing tests**

```rust
//! Operator-triggered execution of an approved L3 skill (the invocation
//! "DOOR"). Pure parsing + substitution + a pure decision
//! ([`prepare_invocation`]) reusing the approval gate against the *live*
//! tool set, plus the async [`invoke_l3`] orchestration that drives the
//! existing [`crate::scheduler::tool_dispatch::ToolHostStepDispatcher`].
//!
//! Only `user_approved` / `pinned` skills run ([`is_runnable`]); dry-run is
//! the default (the CLI passes `execute = false`). There is NO agent-
//! autonomous invocation here and NO CASSANDRA review on the operator path
//! (the reviewer polices agent-formulated plans; an operator running their
//! own approved skill with explicit args is an authorised action).
//!
//! See `docs/superpowers/specs/2026-06-02-l3-skill-invocation-design.md`.

use std::collections::{BTreeMap, BTreeSet};

use crate::cassandra::types::L3SkillCandidate;

/// Max bytes for a single operator-supplied argument value. A value is
/// just a tool argument (shell-exec does no shell interpretation and
/// argv[0] stays operator-allowlisted), but keeping it bounded + clean
/// mirrors the template guards.
pub const L3_ARG_MAX_VALUE_BYTES: usize = 1024;

/// Errors from the pure invocation front-end (arg parse + substitution).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InvokeError {
    #[error("argument '{0}' is not of the form name=value")]
    MalformedArg(String),
    #[error("argument name '{0}' is not snake_case")]
    BadArgName(String),
    #[error("duplicate argument '{0}'")]
    DuplicateArg(String),
    #[error("missing value for declared parameter(s): {0}")]
    MissingArgs(String),
    #[error("unknown argument(s) not declared by the skill: {0}")]
    UnknownArgs(String),
    #[error("argument '{name}' value contains a newline or control character")]
    BadArgValue { name: String },
    #[error("argument '{name}' value exceeds {max} bytes ({got})")]
    ArgValueTooLong { name: String, max: usize, got: usize },
    #[error("placeholder '{{{{{0}}}}}' survived substitution (internal error)")]
    UnsubstitutedPlaceholder(String),
}

/// `true` iff `s` is a strict snake_case identifier (`[a-z][a-z0-9_]*`).
/// Mirrors `l3_crystallise::is_snake_ident` (kept local to avoid widening
/// that module's visibility).
fn is_snake_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Parse `name=value` tokens (the CLI strips the `--arg` flag) into a map.
/// Splits on the FIRST `=` so values may contain `=`. Rejects a token with
/// no `=`, a non-snake_case name, or a duplicate name.
pub fn parse_args(tokens: &[String]) -> Result<BTreeMap<String, String>, InvokeError> {
    let mut map = BTreeMap::new();
    for tok in tokens {
        let (name, value) = tok
            .split_once('=')
            .ok_or_else(|| InvokeError::MalformedArg(tok.clone()))?;
        if !is_snake_ident(name) {
            return Err(InvokeError::BadArgName(name.to_string()));
        }
        if map.insert(name.to_string(), value.to_string()).is_some() {
            return Err(InvokeError::DuplicateArg(name.to_string()));
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_happy_multi() {
        let got = parse_args(&["repo_path=/tmp/x".into(), "depth=2".into()]).unwrap();
        assert_eq!(got["repo_path"], "/tmp/x");
        assert_eq!(got["depth"], "2");
    }

    #[test]
    fn parse_args_value_may_contain_equals() {
        let got = parse_args(&["query=a=b=c".into()]).unwrap();
        assert_eq!(got["query"], "a=b=c");
    }

    #[test]
    fn parse_args_rejects_missing_equals() {
        assert_eq!(
            parse_args(&["noequals".into()]),
            Err(InvokeError::MalformedArg("noequals".into()))
        );
    }

    #[test]
    fn parse_args_rejects_non_snake_name() {
        assert_eq!(
            parse_args(&["Repo=/x".into()]),
            Err(InvokeError::BadArgName("Repo".into()))
        );
    }

    #[test]
    fn parse_args_rejects_duplicate() {
        assert_eq!(
            parse_args(&["a=1".into(), "a=2".into()]),
            Err(InvokeError::DuplicateArg("a".into()))
        );
    }
}
```

- [ ] **Step 3: Run tests to verify they pass (the module compiles and `parse_args` works)**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-core l3_invoke::tests::parse_args 2>&1 | tail -20`
Expected: 5 `parse_args` tests PASS. (`BTreeSet` import is unused until Task 4/5 — add `#[allow(unused_imports)]` is NOT needed yet; instead omit `BTreeSet` from the `use` in this task and add it in Task 5. Remove `BTreeSet` from the import line for now to keep clippy clean.)

> Implementer note: change the import in Step 2 to `use std::collections::BTreeMap;` for this task; widen to add `BTreeSet` in Task 5 when first used.

- [ ] **Step 4: Commit**

```bash
git add core/src/memory/l3_invoke.rs core/src/memory/mod.rs
git commit -m "feat(l3-invoke): module skeleton — InvokeError, consts, parse_args

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: `substitute_template` — closed-world arity + value guards + interpolation

**Files:**
- Modify: `core/src/memory/l3_invoke.rs`

- [ ] **Step 1: Write failing tests**

Add to the `mod tests` block:

```rust
use crate::cassandra::types::{L3Param, L3TemplateStep};

fn skill_one_param() -> L3SkillCandidate {
    L3SkillCandidate {
        name: "summarise_repo".into(),
        description: "Read a repo README".into(),
        parameters: vec![L3Param { name: "repo_path".into(), description: "abs path".into() }],
        steps: vec![L3TemplateStep {
            tool: "shell-exec".into(),
            method: "shell.exec".into(),
            parameters: serde_json::json!({ "argv": ["cat", "{{repo_path}}/README.md"] }),
        }],
    }
}

#[test]
fn substitute_happy_interpolates_embedded_placeholder() {
    let args = parse_args(&["repo_path=/tmp/r".into()]).unwrap();
    let steps = substitute_template(&skill_one_param(), &args).unwrap();
    assert_eq!(steps[0].parameters["argv"][1], "/tmp/r/README.md");
}

#[test]
fn substitute_zero_param_skill_with_no_args() {
    let s = L3SkillCandidate {
        name: "run_tests".into(),
        description: "run suite".into(),
        parameters: vec![],
        steps: vec![L3TemplateStep {
            tool: "shell-exec".into(),
            method: "shell.exec".into(),
            parameters: serde_json::json!({ "argv": ["make", "test"] }),
        }],
    };
    let steps = substitute_template(&s, &BTreeMap::new()).unwrap();
    assert_eq!(steps[0].parameters["argv"][0], "make");
}

#[test]
fn substitute_rejects_missing_arg() {
    let err = substitute_template(&skill_one_param(), &BTreeMap::new()).unwrap_err();
    assert!(matches!(err, InvokeError::MissingArgs(_)));
}

#[test]
fn substitute_rejects_unknown_arg() {
    let args = parse_args(&["repo_path=/x".into(), "extra=1".into()]).unwrap();
    let err = substitute_template(&skill_one_param(), &args).unwrap_err();
    assert!(matches!(err, InvokeError::UnknownArgs(_)));
}

#[test]
fn substitute_rejects_value_with_newline() {
    let args = parse_args(&["repo_path=/x".into()]).unwrap();
    let mut args = args;
    args.insert("repo_path".into(), "a\nb".into());
    let err = substitute_template(&skill_one_param(), &args).unwrap_err();
    assert_eq!(err, InvokeError::BadArgValue { name: "repo_path".into() });
}

#[test]
fn substitute_rejects_oversized_value() {
    let big = "x".repeat(L3_ARG_MAX_VALUE_BYTES + 1);
    let mut args = BTreeMap::new();
    args.insert("repo_path".into(), big);
    let err = substitute_template(&skill_one_param(), &args).unwrap_err();
    assert!(matches!(err, InvokeError::ArgValueTooLong { .. }));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p hhagent-core l3_invoke::tests::substitute 2>&1 | tail -20`
Expected: FAIL — `cannot find function substitute_template`.

- [ ] **Step 3: Implement `substitute_template` + the JSON walker**

Add to `l3_invoke.rs` (above the tests):

```rust
/// Replace every `{{name}}` occurrence inside a single string with the
/// supplied value. `args` is guaranteed complete by the caller's arity
/// check, so a `{{name}}` whose name is absent is left intact and caught
/// by the post-condition scan. Mirrors the writer's `scan_placeholders`
/// byte walk.
fn interpolate(s: &str, args: &BTreeMap<String, String>) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            // find closing }}
            let start = i + 2;
            let mut j = start;
            while j + 1 < bytes.len() && !(bytes[j] == b'}' && bytes[j + 1] == b'}') {
                j += 1;
            }
            if j + 1 < bytes.len() && bytes[j] == b'}' && bytes[j + 1] == b'}' {
                let name = &s[start..j];
                if let Some(v) = args.get(name) {
                    out.push_str(v);
                    i = j + 2;
                    continue;
                }
            }
        }
        // not a (resolvable) placeholder start — copy one char
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Recursively interpolate every string leaf of a JSON value.
fn interpolate_value(v: &serde_json::Value, args: &BTreeMap<String, String>) -> serde_json::Value {
    match v {
        serde_json::Value::String(s) => serde_json::Value::String(interpolate(s, args)),
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.iter().map(|e| interpolate_value(e, args)).collect())
        }
        serde_json::Value::Object(m) => {
            let mut out = serde_json::Map::new();
            for (k, val) in m {
                out.insert(k.clone(), interpolate_value(val, args));
            }
            serde_json::Value::Object(out)
        }
        other => other.clone(),
    }
}

/// `true` iff any `{{ident}}` placeholder remains in a string leaf.
fn has_placeholder(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => {
            let b = s.as_bytes();
            let mut i = 0;
            while i + 1 < b.len() {
                if b[i] == b'{' && b[i + 1] == b'{' {
                    let start = i + 2;
                    let mut j = start;
                    while j + 1 < b.len() && !(b[j] == b'}' && b[j + 1] == b'}') {
                        j += 1;
                    }
                    if j + 1 < b.len() {
                        return Some(s[start..j].to_string());
                    }
                }
                i += 1;
            }
            None
        }
        serde_json::Value::Array(a) => a.iter().find_map(has_placeholder),
        serde_json::Value::Object(m) => m.values().find_map(has_placeholder),
        _ => None,
    }
}

/// Substitute operator-supplied args into a stored skill template,
/// producing concrete (placeholder-free) steps.
///
/// Closed-world: the supplied arg names must EXACTLY equal the declared
/// parameter names. Each value must be free of newlines/control chars and
/// within [`L3_ARG_MAX_VALUE_BYTES`]. Asserts no `{{…}}` survives.
pub fn substitute_template(
    template: &L3SkillCandidate,
    args: &BTreeMap<String, String>,
) -> Result<Vec<L3TemplateStep>, InvokeError> {
    let declared: BTreeSet<&str> = template.parameters.iter().map(|p| p.name.as_str()).collect();
    let supplied: BTreeSet<&str> = args.keys().map(|s| s.as_str()).collect();

    let missing: Vec<&str> = declared.difference(&supplied).copied().collect();
    if !missing.is_empty() {
        return Err(InvokeError::MissingArgs(missing.join(", ")));
    }
    let unknown: Vec<&str> = supplied.difference(&declared).copied().collect();
    if !unknown.is_empty() {
        return Err(InvokeError::UnknownArgs(unknown.join(", ")));
    }

    for (name, value) in args {
        if value.len() > L3_ARG_MAX_VALUE_BYTES {
            return Err(InvokeError::ArgValueTooLong {
                name: name.clone(),
                max: L3_ARG_MAX_VALUE_BYTES,
                got: value.len(),
            });
        }
        if value.bytes().any(|b| b < 0x20) {
            return Err(InvokeError::BadArgValue { name: name.clone() });
        }
    }

    let mut out = Vec::with_capacity(template.steps.len());
    for step in &template.steps {
        let parameters = interpolate_value(&step.parameters, args);
        if let Some(name) = has_placeholder(&parameters) {
            return Err(InvokeError::UnsubstitutedPlaceholder(name));
        }
        out.push(L3TemplateStep {
            tool: step.tool.clone(),
            method: step.method.clone(),
            parameters,
        });
    }
    Ok(out)
}
```

Also widen the top-of-file import to `use std::collections::{BTreeMap, BTreeSet};`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p hhagent-core l3_invoke::tests::substitute 2>&1 | tail -20`
Expected: all 6 `substitute_*` tests PASS.

- [ ] **Step 5: Commit**

```bash
git add core/src/memory/l3_invoke.rs
git commit -m "feat(l3-invoke): substitute_template — closed-world arity, value guards, interpolation

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: `is_runnable` predicate + `prepare_invocation` (pure decision) + `planned_step_from_l3`

**Files:**
- Modify: `core/src/memory/l3_invoke.rs`

- [ ] **Step 1: Write failing tests**

Add to `mod tests`:

```rust
use crate::memory::l3_approval::SkillTrust;
use crate::memory::l3_surface::is_surfaceable;

#[test]
fn is_runnable_only_approved_and_pinned() {
    assert!(is_runnable(SkillTrust::UserApproved));
    assert!(is_runnable(SkillTrust::Pinned));
    assert!(!is_runnable(SkillTrust::Untrusted));
}

#[test]
fn is_runnable_matches_is_surfaceable() {
    // The two gates have identical membership; pin them in sync so a future
    // change to one is caught.
    for t in [SkillTrust::Untrusted, SkillTrust::UserApproved, SkillTrust::Pinned] {
        assert_eq!(is_runnable(t), is_surfaceable(t));
    }
}

fn tools(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

#[test]
fn prepare_rejects_untrusted_trust() {
    let args = parse_args(&["repo_path=/x".into()]).unwrap();
    let r = prepare_invocation(&skill_one_param(), SkillTrust::Untrusted, &args, &tools(&["shell-exec"]));
    match r {
        Err(InvokeRefusal { reasons }) => assert!(reasons.iter().any(|s| s.contains("trust"))),
        Ok(_) => panic!("untrusted must refuse"),
    }
}

#[test]
fn prepare_rejects_unknown_tool_via_live_gate() {
    let args = parse_args(&["repo_path=/x".into()]).unwrap();
    // approved trust, but the live registry lacks shell-exec
    let r = prepare_invocation(&skill_one_param(), SkillTrust::UserApproved, &args, &tools(&["gliner-relex"]));
    match r {
        Err(InvokeRefusal { reasons }) => assert!(reasons.iter().any(|s| s.contains("shell-exec"))),
        Ok(_) => panic!("unknown tool must refuse"),
    }
}

#[test]
fn prepare_happy_returns_concrete_steps() {
    let args = parse_args(&["repo_path=/tmp/r".into()]).unwrap();
    let steps = prepare_invocation(&skill_one_param(), SkillTrust::UserApproved, &args, &tools(&["shell-exec"]))
        .expect("clean approved skill with known tool");
    assert_eq!(steps[0].parameters["argv"][1], "/tmp/r/README.md");
}

#[test]
fn prepare_propagates_substitution_error_as_refusal() {
    // missing arg → refusal (not a panic)
    let r = prepare_invocation(&skill_one_param(), SkillTrust::UserApproved, &BTreeMap::new(), &tools(&["shell-exec"]));
    assert!(r.is_err());
}

#[test]
fn planned_step_from_l3_carries_tool_method_params() {
    let ts = L3TemplateStep {
        tool: "shell-exec".into(),
        method: "shell.exec".into(),
        parameters: serde_json::json!({ "argv": ["echo", "hi"] }),
    };
    let ps = planned_step_from_l3(&ts);
    assert_eq!(ps.tool, "shell-exec");
    assert_eq!(ps.method, "shell.exec");
    assert_eq!(ps.parameters["argv"][1], "hi");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p hhagent-core l3_invoke::tests 2>&1 | tail -20`
Expected: FAIL — `is_runnable` / `prepare_invocation` / `InvokeRefusal` / `planned_step_from_l3` not found.

- [ ] **Step 3: Implement**

Add to `l3_invoke.rs`:

```rust
use crate::cassandra::types::{DataClass, L3TemplateStep, PlannedStep};
use crate::memory::l3_approval::{evaluate_approval, ApprovalDecision, SkillTrust};

/// A refusal to invoke, carrying every human-readable reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvokeRefusal {
    pub reasons: Vec<String>,
}

/// PURE trust gate: only `user_approved` / `pinned` skills run. Identical
/// membership to [`crate::memory::l3_surface::is_surfaceable`] (pinned in
/// sync by a test) — a skill the planner may *see* is exactly a skill the
/// operator may *run*.
pub fn is_runnable(trust: SkillTrust) -> bool {
    matches!(trust, SkillTrust::UserApproved | SkillTrust::Pinned)
}

/// Synthesize a [`PlannedStep`] from a concrete (substituted) template
/// step. `returns` / `done_when` are empty and `classification` is set to
/// the most conservative class — all three are UNUSED on the operator-run
/// path: `ToolHostStepDispatcher::dispatch_step` reads only
/// `tool` / `method` / `parameters`. The conservative `classification`
/// is defensive in case a future reader inspects it.
pub fn planned_step_from_l3(step: &L3TemplateStep) -> PlannedStep {
    PlannedStep {
        tool: step.tool.clone(),
        method: step.method.clone(),
        parameters: step.parameters.clone(),
        returns: String::new(),
        done_when: String::new(),
        classification: DataClass::Secret,
    }
}

/// PURE decision: may this stored skill run with these args against this
/// live tool set, and if so, what are the concrete steps?
///
/// 1. trust must be runnable ([`is_runnable`]);
/// 2. substitute args into the template ([`substitute_template`]);
/// 3. re-run the approval gate ([`evaluate_approval`]) against `live_tools`
///    — the TOCTOU close (structural re-validation + `secret://` re-scan +
///    every tool must exist in the registry as it is now).
///
/// On any failure returns an [`InvokeRefusal`] collecting the reason(s).
pub fn prepare_invocation(
    template: &L3SkillCandidate,
    stored_trust: SkillTrust,
    args: &BTreeMap<String, String>,
    live_tools: &BTreeSet<String>,
) -> Result<Vec<L3TemplateStep>, InvokeRefusal> {
    if !is_runnable(stored_trust) {
        return Err(InvokeRefusal {
            reasons: vec![format!(
                "skill trust '{}' is not runnable (only user_approved / pinned)",
                stored_trust.as_str()
            )],
        });
    }

    // Re-validate the STORED template against the live registry first
    // (structural + secret-ref + tool existence). This guards against a
    // skill approved against a now-stale snapshot, and short-circuits on a
    // structurally broken template before substitution.
    match evaluate_approval(template, live_tools) {
        ApprovalDecision::Approve => {}
        ApprovalDecision::Reject { reasons } => {
            return Err(InvokeRefusal {
                reasons: reasons.iter().map(|r| r.to_string()).collect(),
            });
        }
    }

    // Substitution can still fail on operator-arg problems (missing /
    // unknown / bad value) — surface those as refusal reasons too.
    substitute_template(template, args).map_err(|e| InvokeRefusal { reasons: vec![e.to_string()] })
}
```

> Note: the `use` lines for `DataClass`/`PlannedStep`/`evaluate_approval`/`SkillTrust` go at the top of the file with the other imports; shown here inline for locality. Ensure no duplicate `use` of `L3TemplateStep` (already imported in Task 4 tests via `crate::cassandra::types`; import it once at module scope).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p hhagent-core l3_invoke::tests 2>&1 | tail -20`
Expected: all tests through Task 5 PASS.

- [ ] **Step 5: clippy + commit**

Run: `cargo clippy -p hhagent-core --all-targets --locked -- -D warnings 2>&1 | tail -10`
Expected: exit 0.

```bash
git add core/src/memory/l3_invoke.rs
git commit -m "feat(l3-invoke): is_runnable + prepare_invocation (live-gate TOCTOU close) + planned_step_from_l3

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: `run_steps` — drive a `StepDispatcher`, stop at first error (mock-tested)

**Files:**
- Modify: `core/src/memory/l3_invoke.rs`

- [ ] **Step 1: Write failing tests with an in-memory mock dispatcher**

Add to `mod tests`:

```rust
use crate::scheduler::inner_loop::{StepDispatcher, StepOutcome};
use crate::cassandra::types::PlannedStep as PS;

struct ScriptedDispatcher {
    // outcomes returned in order; calls record the tool seen
    outcomes: std::sync::Mutex<std::collections::VecDeque<StepOutcome>>,
    seen: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl StepDispatcher for ScriptedDispatcher {
    async fn dispatch_step(&self, step: &PS) -> StepOutcome {
        self.seen.lock().unwrap().push(step.tool.clone());
        self.outcomes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(StepOutcome::Ok(serde_json::json!(null)))
    }
}

fn two_steps() -> Vec<L3TemplateStep> {
    vec![
        L3TemplateStep { tool: "a".into(), method: "m".into(), parameters: serde_json::json!({}) },
        L3TemplateStep { tool: "b".into(), method: "m".into(), parameters: serde_json::json!({}) },
    ]
}

#[tokio::test]
async fn run_steps_executes_all_when_ok() {
    let d = ScriptedDispatcher {
        outcomes: std::sync::Mutex::new(
            vec![StepOutcome::Ok(serde_json::json!(1)), StepOutcome::Ok(serde_json::json!(2))].into(),
        ),
        seen: std::sync::Mutex::new(vec![]),
    };
    let outcomes = run_steps(&d, &two_steps()).await;
    assert_eq!(outcomes.len(), 2);
    assert_eq!(*d.seen.lock().unwrap(), vec!["a", "b"]);
}

#[tokio::test]
async fn run_steps_stops_at_first_error() {
    let d = ScriptedDispatcher {
        outcomes: std::sync::Mutex::new(
            vec![StepOutcome::Err { code: "X".into(), detail: "boom".into() }].into(),
        ),
        seen: std::sync::Mutex::new(vec![]),
    };
    let outcomes = run_steps(&d, &two_steps()).await;
    assert_eq!(outcomes.len(), 1, "must stop after the failing first step");
    assert_eq!(*d.seen.lock().unwrap(), vec!["a"], "second step never dispatched");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p hhagent-core l3_invoke::tests::run_steps 2>&1 | tail -20`
Expected: FAIL — `cannot find function run_steps`.

- [ ] **Step 3: Implement `run_steps`**

Add to `l3_invoke.rs`:

```rust
use crate::scheduler::inner_loop::{StepDispatcher, StepOutcome};

/// Dispatch each concrete step through the injected [`StepDispatcher`],
/// collecting outcomes and stopping at the first [`StepOutcome::Err`]
/// (mirrors `inner_loop::run_to_terminal`). No audit / DB here — the
/// per-step chokepoint rows are written inside `dispatch_step`; the
/// envelope rows are the caller's job.
pub async fn run_steps(
    dispatcher: &dyn StepDispatcher,
    steps: &[L3TemplateStep],
) -> Vec<StepOutcome> {
    let mut outcomes = Vec::with_capacity(steps.len());
    for step in steps {
        let ps = planned_step_from_l3(step);
        let outcome = dispatcher.dispatch_step(&ps).await;
        let is_err = outcome.is_err();
        outcomes.push(outcome);
        if is_err {
            break;
        }
    }
    outcomes
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p hhagent-core l3_invoke::tests::run_steps 2>&1 | tail -20`
Expected: both `run_steps_*` tests PASS.

- [ ] **Step 5: Commit**

```bash
git add core/src/memory/l3_invoke.rs
git commit -m "feat(l3-invoke): run_steps — drive StepDispatcher, stop at first error

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: `invoke_l3` async orchestration + `InvokeReport` (audit-writing)

**Files:**
- Modify: `core/src/memory/l3_invoke.rs`

This task wires `prepare_invocation` + audit + `run_steps`. Its DB-touching paths are covered by the live-PG e2e in Task 9; here we add the type + function and assert the module compiles and the existing pure tests still pass. (No new unit test — the orchestration's value is the audit/dispatch wiring, exercised end-to-end in Task 9.)

- [ ] **Step 1: Implement `InvokeReport` + `invoke_l3`**

Add to `l3_invoke.rs`:

```rust
use sqlx::PgPool;

use crate::scheduler::audit::{
    build_l3_invoke_outcome_payload, build_l3_invoke_rejected_payload, build_l3_invoked_payload,
    ACTION_L3_INVOKED, ACTION_L3_INVOKE_OUTCOME, ACTION_L3_INVOKE_REJECTED,
};
use crate::cli_audit::CLI_AUDIT_ACTOR;

/// Result of an [`invoke_l3`] call.
#[derive(Debug)]
pub enum InvokeReport {
    /// Trust gate or live re-validation refused; nothing dispatched.
    Refused { reasons: Vec<String> },
    /// Dry-run (default): the concrete steps that WOULD dispatch.
    DryRun { steps: Vec<L3TemplateStep> },
    /// `--execute`: the per-step outcomes (stops at first error).
    Executed { outcomes: Vec<StepOutcome>, steps_total: usize },
}

/// Orchestrate operator-triggered invocation of an approved skill.
///
/// `template` / `stored_trust` / `body_sha256` come from the stored L3
/// row's metadata; `live_tools` from the freshly-rebuilt registry's tool
/// names; `args` from `parse_args`. `execute == false` ⇒ dry-run (no audit,
/// no dispatch). Audit writes are best-effort (warn-on-failure), matching
/// the chokepoint posture.
pub async fn invoke_l3(
    pool: &PgPool,
    dispatcher: &dyn StepDispatcher,
    template: &L3SkillCandidate,
    stored_trust: SkillTrust,
    body_sha256: &str,
    args: &BTreeMap<String, String>,
    live_tools: &BTreeSet<String>,
    execute: bool,
) -> InvokeReport {
    let skill_name = template.name.clone();

    let steps = match prepare_invocation(template, stored_trust, args, live_tools) {
        Ok(steps) => steps,
        Err(InvokeRefusal { reasons }) => {
            let payload = build_l3_invoke_rejected_payload(
                0, // memory_id is filled by the CLI layer; see note below
                Some(&skill_name),
                Some(body_sha256),
                &reasons,
            );
            best_effort_audit(pool, ACTION_L3_INVOKE_REJECTED, payload).await;
            return InvokeReport::Refused { reasons };
        }
    };

    if !execute {
        return InvokeReport::DryRun { steps };
    }

    let arg_names: Vec<String> = args.keys().cloned().collect();
    let invoked = build_l3_invoked_payload(0, &skill_name, body_sha256, &arg_names, steps.len());
    best_effort_audit(pool, ACTION_L3_INVOKED, invoked).await;

    let steps_total = steps.len();
    let outcomes = run_steps(dispatcher, &steps).await;
    let any_err = outcomes.iter().any(|o| o.is_err());
    let outcome_payload = build_l3_invoke_outcome_payload(
        0, &skill_name, outcomes.len(), steps_total, any_err,
    );
    best_effort_audit(pool, ACTION_L3_INVOKE_OUTCOME, outcome_payload).await;

    InvokeReport::Executed { outcomes, steps_total }
}

async fn best_effort_audit(pool: &PgPool, action: &str, payload: serde_json::Value) {
    if let Err(e) = hhagent_db::audit::insert(pool, CLI_AUDIT_ACTOR, action, payload).await {
        tracing::warn!(error = %e, action, "l3 invoke audit insert failed (best-effort)");
    }
}
```

> **Implementer correction (apply before building):** the snippet above threads `memory_id` as `0` as a placeholder. Add a `memory_id: i64` parameter to `invoke_l3` (insert it right after `pool`) and pass it into all three payload builders so the audit rows carry the real id. The signature becomes `invoke_l3(pool, memory_id, dispatcher, template, stored_trust, body_sha256, args, live_tools, execute)` and each `0,` in the payload-builder calls becomes `memory_id,`. The Task 8 CLI handler passes the row id (it already does: `invoke_l3(&pool, id, …)`).

- [ ] **Step 2: Apply the `memory_id` parameter correction**

Edit the signature and the three `build_l3_*` calls per the note above so each audit payload carries the real `memory_id`.

- [ ] **Step 3: Build + run all l3_invoke unit tests**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-core l3_invoke 2>&1 | tail -20`
Expected: all prior l3_invoke unit tests still PASS; the module compiles with `invoke_l3` + `InvokeReport`.

- [ ] **Step 4: Check file size; lift tests to sibling if near cap**

Run: `wc -l core/src/memory/l3_invoke.rs`
If > ~470 lines, move the `#[cfg(test)] mod tests { … }` block into a sibling `core/src/memory/l3_invoke/tests.rs` and replace it with `#[cfg(test)] mod tests;` (the established L3 pattern — de-indent one level; production region must stay byte-identical). Re-run Step 3.

- [ ] **Step 5: clippy + commit**

Run: `cargo clippy -p hhagent-core --all-targets --locked -- -D warnings 2>&1 | tail -10`
Expected: exit 0.

```bash
git add core/src/memory/l3_invoke.rs
# include the sibling tests file if you lifted it:
# git add core/src/memory/l3_invoke/tests.rs
git commit -m "feat(l3-invoke): invoke_l3 orchestration + InvokeReport (dry-run / execute / refuse + audit)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: CLI `memory l3 run` subcommand + handler

**Files:**
- Modify: `core/src/bin/hhagent-cli/memory_l3.rs`

- [ ] **Step 1: Add `run` to the dispatch table + usage**

In `run_memory_l3` (`core/src/bin/hhagent-cli/memory_l3.rs`), update the usage string and add the arm:

```rust
    if args.is_empty() {
        eprintln!("usage: hhagent-cli memory l3 <list|approve|revoke|remove|run> ...");
        return ExitCode::from(2);
    }
    match args[0].as_str() {
        "list"    => with_runtime("memory l3", memory_l3_list(&args[1..])),
        "approve" => with_runtime("memory l3", memory_l3_approve(&args[1..])),
        "revoke"  => with_runtime("memory l3", memory_l3_revoke(&args[1..])),
        "remove"  => with_runtime("memory l3", memory_l3_remove(&args[1..])),
        "run"     => with_runtime("memory l3", memory_l3_run(&args[1..])),
        other     => {
            eprintln!("memory l3: unknown action '{other}'; expected: list | approve | revoke | remove | run");
            ExitCode::from(2)
        }
    }
```

- [ ] **Step 2: Implement the `memory_l3_run` handler**

Add to `memory_l3.rs`:

```rust
/// `memory l3 run <id> [--arg name=value]… [--execute]`
///
/// Default (no `--execute`): DRY-RUN — substitute + live-registry
/// re-validate, then print the concrete steps that WOULD dispatch. Spawns
/// nothing, writes no audit row. `--execute` runs the steps through the
/// sandbox, stopping at the first error.
async fn memory_l3_run(args: &[String]) -> ExitCode {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use hhagent_core::cassandra::types::L3SkillCandidate;
    use hhagent_core::memory::l3_approval::SkillTrust;
    use hhagent_core::memory::l3_invoke::{invoke_l3, parse_args, InvokeReport};
    use hhagent_core::scheduler::inner_loop::StepDispatcher;
    use hhagent_core::scheduler::tool_dispatch::ToolHostStepDispatcher;
    use hhagent_db::memories::{fetch_by_ids, MemoryLayer};
    use hhagent_db::pool::connect_runtime_pool;

    // --- parse argv: <id> then --arg k=v … and --execute ---------------
    let mut id_str: Option<&String> = None;
    let mut arg_tokens: Vec<String> = Vec::new();
    let mut execute = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--execute" | "--yes" => execute = true,
            "--arg" => {
                i += 1;
                match args.get(i) {
                    Some(kv) => arg_tokens.push(kv.clone()),
                    None => {
                        eprintln!("memory l3 run: --arg requires a name=value");
                        return ExitCode::from(2);
                    }
                }
            }
            s if id_str.is_none() && !s.starts_with("--") => id_str = Some(&args[i]),
            other => {
                eprintln!("memory l3 run: unexpected argument '{other}'");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }
    let id: i64 = match id_str.map(|s| s.parse()) {
        Some(Ok(n)) => n,
        _ => {
            eprintln!("usage: hhagent-cli memory l3 run <id> [--arg name=value]… [--execute]");
            return ExitCode::from(2);
        }
    };
    let args_map = match parse_args(&arg_tokens) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("memory l3 run: {e}");
            return ExitCode::from(2);
        }
    };

    // --- connect ------------------------------------------------------
    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    // --- load + layer-guard the row ----------------------------------
    let row = match fetch_by_ids(&pool, &[id]).await {
        Ok(mut v) => v.pop(),
        Err(e) => { eprintln!("memory l3 run: {e}"); return ExitCode::from(1); }
    };
    let row = match row {
        Some(r) if r.layer == MemoryLayer::Skill => r,
        _ => {
            eprintln!("memory l3 run: no layer-3 skill with id={id}");
            return ExitCode::from(1);
        }
    };
    let template: L3SkillCandidate = match row
        .metadata.get("template").cloned().and_then(|t| serde_json::from_value(t).ok())
    {
        Some(t) => t,
        None => {
            eprintln!("memory l3 run: id={id} has no parseable template");
            return ExitCode::from(1);
        }
    };
    let trust = SkillTrust::from_metadata_str(
        row.metadata.get("trust").and_then(|v| v.as_str()).unwrap_or(""),
    );
    let body_sha256 = row.metadata.get("body_sha256").and_then(|v| v.as_str()).unwrap_or("");

    // --- rebuild the live registry in-process (no registry.loaded write) ---
    let gliner = hhagent_core::registry_build::build_gliner_relex_entry();
    let (registry, _records) =
        match hhagent_core::registry_build::build_tool_registry(&pool, gliner).await {
            Ok(x) => x,
            Err(e) => { eprintln!("memory l3 run: building registry: {e}"); return ExitCode::from(1); }
        };
    let live_tools: BTreeSet<String> =
        registry.entries().map(|(name, _)| name.to_string()).collect();

    // --- build the dispatcher (same machinery as the daemon) ----------
    let sandboxes = Arc::new(hhagent_sandbox::SandboxBackends::default_for_current_os());
    let lifecycle: Arc<dyn hhagent_core::worker_lifecycle::WorkerLifecycleManager> =
        Arc::new(hhagent_core::worker_lifecycle::CompositeLifecycle::new(Arc::clone(&sandboxes)));
    let vault = Arc::new(hhagent_core::secrets::Vault::new());
    let dispatcher: Arc<dyn StepDispatcher> = Arc::new(ToolHostStepDispatcher::new(
        pool.clone(),
        vault,
        lifecycle,
        Arc::new(registry),
    ));

    // --- invoke -------------------------------------------------------
    let report = invoke_l3(
        &pool, id, dispatcher.as_ref(), &template, trust, body_sha256, &args_map, &live_tools, execute,
    )
    .await;

    match report {
        InvokeReport::Refused { reasons } => {
            eprintln!("REFUSED to run skill '{}' (#{id}):", template.name);
            for r in &reasons { eprintln!("  - {r}"); }
            ExitCode::from(1)
        }
        InvokeReport::DryRun { steps } => {
            println!("dry-run: skill '{}' (#{id}) would dispatch {} step(s):", template.name, steps.len());
            for (n, s) in steps.iter().enumerate() {
                println!("  [{n}] {}/{} {}", s.tool, s.method, s.parameters);
            }
            println!("(re-run with --execute to dispatch)");
            ExitCode::from(0)
        }
        InvokeReport::Executed { outcomes, steps_total } => {
            let any_err = outcomes.iter().any(|o| o.is_err());
            println!("executed skill '{}' (#{id}): {}/{} step(s)", template.name, outcomes.len(), steps_total);
            for (n, o) in outcomes.iter().enumerate() {
                match o {
                    hhagent_core::scheduler::inner_loop::StepOutcome::Ok(v) =>
                        println!("  [{n}] ok: {v}"),
                    hhagent_core::scheduler::inner_loop::StepOutcome::Err { code, detail } =>
                        println!("  [{n}] ERR {code}: {detail}"),
                }
            }
            if any_err { ExitCode::from(1) } else { ExitCode::from(0) }
        }
    }
}
```

> Note: confirm `hhagent_sandbox` is already a dependency of the `hhagent-cli` binary / `hhagent-core` crate (it is — `core` depends on `hhagent_sandbox`). The CLI binary is part of the `hhagent-core` crate, so `hhagent_sandbox::…` resolves. If a `use` is needed it is `hhagent_sandbox` (the crate).

- [ ] **Step 3: Build**

Run: `source "$HOME/.cargo/env" && cargo build --workspace 2>&1 | tail -20`
Expected: clean build.

- [ ] **Step 4: Smoke-check the usage path (no DB)**

Run: `./target/debug/hhagent-cli memory l3 run 2>&1 | head -3`
Expected: the usage line (exit 2) — `usage: hhagent-cli memory l3 run <id> …`.

- [ ] **Step 5: clippy + commit**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings 2>&1 | tail -10`
Expected: exit 0.

```bash
git add core/src/bin/hhagent-cli/memory_l3.rs
git commit -m "feat(cli): memory l3 run <id> [--arg k=v]… [--execute] — operator skill invocation

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: Live-PG end-to-end test

**Files:**
- Create: `core/tests/cli_memory_l3_run_e2e.rs`

This mirrors the existing `cli_memory_l3_e2e.rs` harness (skip-as-pass without `HHAGENT_PG_BIN_DIR`; the session-local-override pattern from the memory note — Postgres.app v18 at port 5532). The implementer should open `core/tests/cli_memory_l3_e2e.rs` first and reuse its bring-up + helper conventions (e.g. how it seeds an L3 row, sets trust, and reads audit rows).

- [ ] **Step 1: Read the existing harness for conventions**

Run: `sed -n '1,80p' core/tests/cli_memory_l3_e2e.rs`
Expected: note the `bring_up_pg_cluster` usage, the skip helper, how it inserts an L3 row + flips trust, and how it asserts audit rows. Reuse these.

- [ ] **Step 2: Write the e2e scenarios (TDD: write first, expect compile/skip)**

Create `core/tests/cli_memory_l3_run_e2e.rs` with these scenarios (reuse the harness helpers — the snippet below is the intent; wire it to the actual helper names from Step 1):

```rust
//! Live-PG e2e for `memory l3 run` (operator-triggered skill invocation).
//!
//! Skips-as-pass without HHAGENT_PG_BIN_DIR (no live cluster) — matches the
//! cross-platform posture of the sibling cli_memory_l3_e2e.

// (Reuse the bring-up + skip + seed helpers from cli_memory_l3_e2e.rs.)

// Scenario A — dry-run preview spawns nothing, writes no audit row:
//   1. Seed an approved (trust=user_approved) shell-exec skill with one
//      param; build a registry that includes shell-exec.
//   2. Call the run-engine (invoke_l3 with execute=false, OR exec the CLI
//      binary with no --execute), supplying the param.
//   3. Assert: report is DryRun with the substituted step; NO l3.invoked /
//      l3.invoke_outcome / tool:* rows were written for this run.

// Scenario B — --execute round-trips through the real sandbox:
//   1. Same approved shell-exec skill (e.g. argv ["true"] or a benign echo
//      that is in the test allowlist).
//   2. invoke_l3 with execute=true.
//   3. Assert: Executed report, all steps ok; exactly one l3.invoked row,
//      one l3.invoke_outcome row, and one tool:shell-exec/<method> chokepoint
//      row per step.

// Scenario C — untrusted skill refuses:
//   1. Seed a trust=untrusted skill.
//   2. invoke_l3 (execute=true).
//   3. Assert: Refused; one l3.invoke_rejected row; NO l3.invoked / tool:* rows.

// Scenario D — tool not in the live registry refuses (live re-validation):
//   1. Seed an approved skill whose step.tool is "ghost-tool".
//   2. Build a registry WITHOUT ghost-tool.
//   3. invoke_l3 (execute=true).
//   4. Assert: Refused with a reason mentioning ghost-tool; one
//      l3.invoke_rejected row; NO l3.invoked / tool:* rows.

// Scenario E — stop at first error:
//   1. Seed an approved two-step skill where step 1 fails (e.g. a
//      non-allowlisted argv0 → POLICY_DENIED) and step 2 would succeed.
//   2. invoke_l3 (execute=true).
//   3. Assert: Executed with exactly one outcome (the error); any_err=true;
//      the l3.invoke_outcome row shows steps_executed=1, steps_total=2.
```

> Implementer guidance: prefer calling the library `invoke_l3` directly (constructing the dispatcher exactly as the CLI handler does) over shelling out to the binary — it gives typed assertions on `InvokeReport` and avoids argv plumbing. Use the test allowlist seeding the sibling e2e already does (`tool_allowlists` add for shell-exec) so the rebuilt registry includes shell-exec. For the worker binary, set `HHAGENT_SHELL_EXEC_BIN` to the built `shell-exec` worker path the way `shell_exec_e2e.rs` discovers it (`worker_binary` helper).

- [ ] **Step 3: Run the e2e WITHOUT a live cluster (skip-as-pass)**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-core --test cli_memory_l3_run_e2e 2>&1 | tail -20`
Expected: compiles; tests SKIP-as-pass (no `HHAGENT_PG_BIN_DIR`).

- [ ] **Step 4: Run the e2e WITH the live cluster (Postgres.app v18)**

Run (session-local override per the memory note — preferred v18 at port 5532):
```bash
source "$HOME/.cargo/env"
HHAGENT_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin" \
  cargo test -p hhagent-core --test cli_memory_l3_run_e2e -- --nocapture 2>&1 | tail -40
```
Expected: all 5 scenarios PASS, zero `[SKIP]`.

- [ ] **Step 5: Commit**

```bash
git add core/tests/cli_memory_l3_run_e2e.rs
git commit -m "test(l3-invoke): live-PG e2e — dry-run / execute / untrusted / unknown-tool / stop-on-error

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 10: Final verification + handover/roadmap update

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md`

- [ ] **Step 1: Full workspace test + clippy + doc-links**

```bash
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | tail -5
cargo clippy --workspace --all-targets --locked -- -D warnings 2>&1 | tail -5
RUSTDOCFLAGS="-D rustdoc::broken_intra_doc_links" cargo doc -p hhagent-core --no-deps --document-private-items 2>&1 | tail -5
```
Expected: workspace tests pass (baseline + new unit tests); clippy exit 0; doc-links count == `main`'s 21 (zero new broken links).

- [ ] **Step 2: Live-PG regression (sibling L3 suites stay green)**

```bash
HHAGENT_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin" \
  cargo test -p hhagent-core --test cli_memory_l3_e2e --test memory_l3_crystallise_e2e --test cli_memory_l3_run_e2e 2>&1 | tail -20
```
Expected: all green, zero `[SKIP]`.

- [ ] **Step 3: Update HANDOVER.md + ROADMAP.md**

Per the checklist at the bottom of HANDOVER.md: bump header (Last-updated, Last-commit, Session-end verification counts), move the picked item into "Recently completed" with the why/how, write a fresh "Next TODO" (the L3 arc's remaining slice becomes agent-autonomous invocation + the `pin` command), and tick the ROADMAP L3 line with the new sub-entry. Commit both together with a `docs(handover,roadmap): …` message.

- [ ] **Step 4: Commit docs**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs(handover,roadmap): L3 skill invocation (operator-triggered) shipped

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review notes (resolved during planning)

- **Spec coverage:** §3 engine choice → Tasks 6–7; §4 in-process rebuild → Task 1 + Task 8; §5.1 parse_args → Task 3; §5.2 substitute → Task 4; §5.3 trust+live-gate → Task 5; §5.4 invoke_l3 → Task 7; §6 security (no CASSANDRA review; trust gate; live TOCTOU; dry-run) → Tasks 5/7/8 + e2e D; §7 audit contract → Task 2 + Task 7 + e2e B/C/D; §8 CLI → Task 8; §9 testing → Tasks 3–9. No gaps.
- **`memory_id` in audit rows:** caught in Task 7 — `invoke_l3` gains a `memory_id` parameter so the rows carry the real id (the inline `0` placeholder is corrected before building).
- **Type consistency:** `InvokeReport` / `InvokeRefusal` / `StepOutcome` / `L3TemplateStep` / `PlannedStep` / `SkillTrust` names are used identically across Tasks 5–9. `is_runnable` membership is pinned equal to `is_surfaceable` by a test (Task 5).
- **File-size cap:** Task 7 Step 4 lifts the test module to a sibling if `l3_invoke.rs` nears 500 LOC (established L3 pattern).
