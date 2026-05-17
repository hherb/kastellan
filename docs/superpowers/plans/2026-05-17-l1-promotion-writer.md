# L1 Promotion Writer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the first writer for `MemoryLayer::Index` rows. Two paths: operator-explicit (`hhagent-cli memory l1 {add,list,remove}`) and agent-raised (`Plan.l1_insight` consumed by the inner loop on `Outcome::Completed`).

**Architecture:** A pure validator + idempotent writer module (`core::memory::l1_promote`) is shared by both paths. The operator path goes through `cli_audit::l1_*_and_audit` helpers (mirrors `tools_allowlist`). The agent-raised path is gated by `Plan::is_completion_with_insight()` and emitted in `runner::drain_lane` after the existing `task.finalize` row. Three new audit-row actions (`l1.added`, `l1.removed`, `l1.promoted`); one pure-additive payload key on `agent/plan.formulate` (`l1_insight`).

**Tech Stack:** Rust workspace (sqlx + tokio + serde). `hhagent-tests-common::bring_up_pg_cluster` for PG-backed unit tests. Mirror of existing `core::memory::l0_seed` shape from PR #77.

**Spec:** [docs/superpowers/specs/2026-05-17-l1-promotion-writer-design.md](../specs/2026-05-17-l1-promotion-writer-design.md)

**Branch:** `feat/l1-promotion-writer` (off `main` at `57e468a`).

**Workspace baseline:** 674 passed / 0 failed / 4 ignored / 0 warnings on `main` at `7553404` (post-PR-#79). Test count target after this slice: ~702-709.

---

## Test fixture pattern (READ BEFORE TASKS)

The codebase uses a specific shape for PG-backed integration tests. **Every per-task test snippet that bring-ups a `cluster` follows this exact pattern**; only the test body inside `rt().block_on(async { ... })` varies. The snippets in subsequent tasks show the *assertions*; substitute them into this pattern's body.

Canonical reference: [`core/tests/memory_l0_seed_e2e.rs`](../../../core/tests/memory_l0_seed_e2e.rs).

```rust
#![cfg(any(target_os = "linux", target_os = "macos"))]

use hhagent_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime")
}

#[test]
fn my_test_name() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "l1p-d",  // SHORT data-dir label (per-task: pick a 3-4 char prefix; keep socket-path budget)
        "l1p-l",  // SHORT log-dir label
        &format!("hhagent-supervisor-test-pg-l1p-{suffix}"),
    );

    rt().block_on(async {
        // 1. Apply migrations + write the bring-up audit row.
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "l1-promote-test"}),
        )
        .await
        .expect("probe");

        // 2. Get the runtime-role pool (auto SET ROLE hhagent_runtime).
        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // 3. TEST BODY GOES HERE.
        //    The per-task snippets below show what to write inside this block.
    });
}
```

Key invariants of the pattern:
- **Sync `#[test]`**, not `#[tokio::test]`. The `tokio::test` macro builds a single-thread runtime by default; the L1 path uses `sqlx::PgPool` which prefers multi-thread.
- **`skip_if_no_supervisor` + `pg_bin_dir_or_skip`** — early-return as `[SKIP]` when the host lacks supervisor (`systemctl --user` / `launchctl`) or Postgres binaries.
- **`probe::run` before `connect_runtime_pool`** — applies migrations + SET-ROLE.
- **`unique_suffix()`** — collision-proof labels across parallel-running tests.
- **3-4 char prefix** for data/log labels — Unix socket path budget is tight (108 bytes total).

For **inline unit tests inside the source module** (`core/src/memory/l1_promote.rs::tests`), the pattern is the same but lives in a `#[cfg(test)] mod tests` block; the `use hhagent_tests_common::*` imports go inside the `mod tests` (because dev-deps don't reach the parent crate's main namespace).

For **subprocess CLI tests** (Task 13), see the precedent at [`core/tests/cli_cancel_audit_e2e.rs`](../../../core/tests/cli_cancel_audit_e2e.rs) — same fixture shape but the test additionally `Command::new(workspace_target_binary("hhagent-cli"))` and `.envs(cluster.cli_env_vars_for_subprocess())` (or whatever the precedent calls it; copy the env-var injection block verbatim).

### Where PG-backed tests live (codebase convention)

The codebase **does not put PG-backed `#[tokio::test]` tests inline** in `core/src/` or `db/src/`. The convention is:

- Inline `#[cfg(test)] mod tests` blocks (in `core/src/foo.rs::tests`) hold **pure** unit tests: validators, SHA-256/serde shapes, payload builders, accessor logic. No `PgPool`. These run in tens of milliseconds.
- PG-backed scenarios live in `core/tests/*_e2e.rs` (or `db/tests/postgres_e2e.rs` for the db crate). Each scenario brings up its own per-test cluster via the canonical fixture pattern above.

This means some of the test snippets shown in **Tasks 1, 4, 5, 9, 10** below are written as if they were inline-able (with `#[tokio::test]` + `bring_up_pg_cluster`), but in the actual implementation:

- **Pure helpers** (`validate_l1_body`, `compute_body_sha256`, `build_l1_metadata`, `build_l1_write_payload`, `Plan::is_completion_with_insight`): ship as inline `#[test]` unit tests in their module's `mod tests`. These are Tasks 2, 3, 6 (small pure-test budgets).
- **PG-backed scenarios** (every test that needs `PgPool`): ship in the integration test files:
  - `db/tests/postgres_e2e.rs` — Task 1's `delete_memory_at_layer` PG coverage.
  - `core/tests/memory_l1_promote_e2e.rs` — Tasks 4, 5, 9, 10's PG coverage (one file, all scenarios — Task 12 is where they land).
  - `core/tests/cli_memory_l1_e2e.rs` — Task 13's CLI subprocess coverage.

**Practical implication for TDD ordering:** Tasks 1, 4, 5, 9, 10 ship their pure parts inline (typically minimal — module scaffold + signature compile-pins) and defer the heavy assertion work to the integration file in Task 12 or 13. The TDD "RED" for those tasks is the **compile error** ("function not defined") that the inline scaffold smoke-test catches; the production-grade assertion-RED happens in Task 12's integration scenarios. This is the codebase's existing rhythm — see how `l0_seed.rs::tests` is all-pure and `memory_l0_seed_e2e.rs` carries the 9 PG-backed scenarios.

If you find yourself writing `#[tokio::test]` + `bring_up_pg_cluster` in a `core/src/foo.rs::tests` block while reading these per-task snippets, **stop and move the scenario to its integration file**. The per-task snippets show *what to assert*; the canonical fixture above shows *where it lives*.

---

## Pre-flight (single step, not a task)

- [ ] **Step 0: Confirm green baseline + create branch**

```bash
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | tail -10
# Expected: "test result: ok. ... 0 failed; 4 ignored"
# Note: workspace-tier line aggregates per-crate counts; total ~= 674.

git checkout -b feat/l1-promotion-writer
git log --oneline -1
# Expected HEAD: 57e468a docs(spec): L1 promotion writer design ...
```

---

## Task 1: `db::memories::delete_memory_at_layer` async helper

**Files:**
- Modify: `db/src/memories.rs` (add async helper + inline unit tests)

**Context for the engineer:** Today there is no DELETE path through `db::memories`. We need a layer-guarded `DELETE FROM memories WHERE id = $1 AND layer = $2` to support `hhagent-cli memory l1 remove <id>` without ever touching L0/L3 rows. The existing AFTER DELETE trigger on `memories` (migration `0008`) journals the deletion into `deleted_memories`; we get audit-trail for free.

- [ ] **Step 1.1: Write the failing tests**

Append to `db/src/memories.rs` at the bottom of the existing `#[cfg(test)] mod tests`:

```rust
    #[tokio::test]
    async fn delete_memory_at_layer_happy_path() {
        let cluster = match hhagent_tests_common::bring_up_pg_cluster().await {
            Some(c) => c,
            None => { eprintln!("[SKIP] no PG"); return; }
        };
        let pool = cluster.runtime_pool().await.expect("runtime pool");

        // Seed an L1 row via the admin function used by promote_l1.
        let body_sha = "deadbeef".to_string();
        let metadata = serde_json::json!({"source": "operator", "body_sha256": body_sha});
        let id = crate::memories::insert_memory_at_layer(
            &pool, MemoryLayer::Index, "body-to-delete", metadata, None,
        ).await.expect("insert");

        let deleted = crate::memories::delete_memory_at_layer(
            &pool, id, MemoryLayer::Index,
        ).await.expect("delete");
        assert!(deleted, "row should have been deleted");

        // Second delete returns false (row already gone).
        let deleted_again = crate::memories::delete_memory_at_layer(
            &pool, id, MemoryLayer::Index,
        ).await.expect("delete-again");
        assert!(!deleted_again, "no row to delete on second call");
    }

    #[tokio::test]
    async fn delete_memory_at_layer_rejects_wrong_layer() {
        let cluster = match hhagent_tests_common::bring_up_pg_cluster().await {
            Some(c) => c,
            None => { eprintln!("[SKIP] no PG"); return; }
        };
        let pool = cluster.runtime_pool().await.expect("runtime pool");

        // Seed at L2 (the default Stable layer); the L1 DELETE must not touch it.
        let metadata = serde_json::json!({});
        let id = crate::memories::insert_memory(
            &pool, "stable-body", metadata, None,
        ).await.expect("insert-l2");

        let deleted = crate::memories::delete_memory_at_layer(
            &pool, id, MemoryLayer::Index,
        ).await.expect("delete-wrong-layer");
        assert!(!deleted, "wrong-layer guard must reject");

        // The L2 row is still there.
        let rows = crate::memories::fetch_by_ids(&pool, &[id]).await.expect("fetch");
        assert_eq!(rows.len(), 1, "L2 row must survive the wrong-layer guard");
    }
```

- [ ] **Step 1.2: Run tests to verify they fail**

```bash
cargo test -p hhagent-db delete_memory_at_layer 2>&1 | tail -20
# Expected: compile error (unresolved import) or "cannot find function `delete_memory_at_layer`"
```

- [ ] **Step 1.3: Implement `delete_memory_at_layer`**

Add to `db/src/memories.rs`, placed after `insert_memory_at_layer` (search for `fn insert_memory_at_layer` and add the new function below the closing `}`):

```rust
/// Delete one row from `memories` by id, but **only** if its layer
/// matches `layer`. Returns `true` if a row was deleted; `false` if
/// no row matched (id absent or layer mismatch).
///
/// The layer guard exists so that callers of the L1 CLI cannot
/// accidentally delete an L0 / L2 / L3 row through this path —
/// the `hhagent-cli memory l1 remove <id>` operator subcommand
/// passes `MemoryLayer::Index` here.
///
/// The existing AFTER DELETE trigger on `memories` (migration
/// `0008_deleted_memories_audit.sql`) journals the deleted row's
/// body, metadata, embedding, and `original_created_at` into the
/// `deleted_memories` table for the audit trail.
pub async fn delete_memory_at_layer<'e, E>(
    executor: E,
    id: i64,
    layer: MemoryLayer,
) -> Result<bool, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows = sqlx::query("DELETE FROM memories WHERE id = $1 AND layer = $2")
        .bind(id)
        .bind(layer.as_db())
        .execute(executor)
        .await
        .map_err(DbError::from)?;
    Ok(rows.rows_affected() == 1)
}
```

- [ ] **Step 1.4: Run tests to verify they pass**

```bash
cargo test -p hhagent-db delete_memory_at_layer 2>&1 | tail -10
# Expected: "2 passed; 0 failed"
```

- [ ] **Step 1.5: Commit**

```bash
git add db/src/memories.rs
git commit -m "$(cat <<'EOF'
feat(db,memories): delete_memory_at_layer async helper

Adds a layer-guarded DELETE so the upcoming `hhagent-cli memory l1
remove <id>` operator subcommand cannot reach into L0/L2/L3 rows
through the L1 CLI path. Returns true iff a row was deleted.

The existing AFTER DELETE trigger on `memories` (migration 0008)
journals the deletion into `deleted_memories` so the audit trail
is intact.

+2 inline unit tests pin the happy path + wrong-layer-guard.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: `Plan.l1_insight` field + `Plan::is_completion_with_insight()` accessor + cascade

**Files:**
- Modify: `core/src/cassandra/types.rs` (new field + accessor + new unit tests)
- Modify (cascade): 8 sites that construct `Plan { ... }` literals — `core/src/cassandra/deterministic.rs`, `core/src/cassandra/review.rs`, `core/src/scheduler/inner_loop.rs`, `core/src/observation/replay.rs`, `core/tests/observation_replay_cli_e2e.rs`, `core/tests/observation_replay_e2e.rs`, `core/tests/scheduler_inner_loop_e2e.rs`, `core/tests/scheduler_lanes_e2e.rs`

**Context for the engineer:** `Plan` has no `Default` impl, so adding a new field is a struct-literal cascade. Every test fixture has to gain `l1_insight: None`. The new accessor encapsulates the agent-raised emit gate so the inner-loop call site stays small.

- [ ] **Step 2.1: Write the failing accessor tests**

Append to `core/src/cassandra/types.rs` (find `mod tests` and add at the bottom):

```rust
    #[test]
    fn is_completion_with_insight_returns_some_when_terminal_and_insight_present() {
        let plan = Plan {
            context: "".into(),
            decision: DECISION_TERMINAL.into(),
            rationale: "".into(),
            steps: vec![],
            result: Some(serde_json::json!({"kind": "text", "body": "answer"})),
            data_ceiling: DataClass::Public,
            refused: None,
            floor_request: None,
            l1_insight: Some("shell-exec /bin/ls works for dirs".into()),
        };
        assert_eq!(plan.is_completion_with_insight(), Some("shell-exec /bin/ls works for dirs"));
    }

    #[test]
    fn is_completion_with_insight_returns_none_when_not_terminal() {
        let plan = Plan {
            context: "".into(),
            decision: "step_required".into(),  // not DECISION_TERMINAL
            rationale: "".into(),
            steps: vec![],
            result: Some(serde_json::json!({"kind": "text", "body": "x"})),
            data_ceiling: DataClass::Public,
            refused: None,
            floor_request: None,
            l1_insight: Some("foo".into()),
        };
        assert!(plan.is_completion_with_insight().is_none());
    }

    #[test]
    fn is_completion_with_insight_returns_none_when_insight_absent() {
        let plan = Plan {
            context: "".into(),
            decision: DECISION_TERMINAL.into(),
            rationale: "".into(),
            steps: vec![],
            result: Some(serde_json::json!({"kind": "text", "body": "x"})),
            data_ceiling: DataClass::Public,
            refused: None,
            floor_request: None,
            l1_insight: None,
        };
        assert!(plan.is_completion_with_insight().is_none());
    }

    #[test]
    fn plan_l1_insight_serde_round_trip_omits_none() {
        let plan = Plan {
            context: "c".into(),
            decision: DECISION_TERMINAL.into(),
            rationale: "r".into(),
            steps: vec![],
            result: Some(serde_json::json!({"kind": "text", "body": "x"})),
            data_ceiling: DataClass::Public,
            refused: None,
            floor_request: None,
            l1_insight: None,
        };
        let s = serde_json::to_string(&plan).expect("serialize");
        assert!(!s.contains("l1_insight"), "None should be omitted via skip_serializing_if; got: {s}");

        // And the round-trip survives deserialization with the field absent.
        let plan2: Plan = serde_json::from_str(&s).expect("deserialize");
        assert!(plan2.l1_insight.is_none());
    }
```

- [ ] **Step 2.2: Run tests to verify they fail at compile time**

```bash
cargo test -p hhagent-core cassandra::types 2>&1 | tail -30
# Expected: compile error "no field `l1_insight` on type `Plan`"
# AND: error[E0599]: no method named `is_completion_with_insight`
```

- [ ] **Step 2.3: Add the `Plan.l1_insight` field and accessor**

In `core/src/cassandra/types.rs`, find the `pub struct Plan { ... }` definition (line ~99) and add `l1_insight` as the final field before the closing `}`:

```rust
    /// Agent-raised L1 insight candidate. Only honoured on terminal
    /// plans that reach `Outcome::Completed` (i.e. reviewer didn't
    /// Block/Escalate/ConstitutionalBlock and the agent didn't refuse).
    /// The inner loop captures this into `InnerLoopResult.terminal_l1_insight`;
    /// `runner::drain_lane` writes it to `MemoryLayer::Index` with provenance
    /// `L1Source::AgentRaised { task_id }`.
    ///
    /// Validation rules + length cap live in [`crate::memory::l1_promote`];
    /// a payload that fails validation produces an `actor='scheduler'
    /// action='l1.promoted'` row with `action: "rejected_validation"` but
    /// does NOT abort task finalize.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub l1_insight: Option<String>,
```

Then in the `impl Plan { ... }` block (line ~141), add the accessor below `is_refused`:

```rust
    /// Returns `Some(insight)` iff this plan would produce
    /// `Outcome::Completed` AND carries an `l1_insight`. Encapsulates
    /// the agent-raised L1-promotion gate so the inner-loop call site
    /// stays small. `is_terminal()` is the existing check
    /// (`decision == DECISION_TERMINAL && steps.is_empty() && result.is_some()`).
    pub fn is_completion_with_insight(&self) -> Option<&str> {
        if self.is_terminal() {
            self.l1_insight.as_deref()
        } else {
            None
        }
    }
```

- [ ] **Step 2.4: Update all 8 cascade sites to add `l1_insight: None`**

Each site listed below has one or more `Plan { ... }` struct-literal constructions. Add `l1_insight: None,` as the last field in each literal (just before the closing `}`). Use the Edit tool's `replace_all` mode where the literal shape is repeated within a single file.

A precise grep shows the sites:

```bash
grep -rn "^\s*Plan\s*{\|^\s*let.*=\s*Plan\s*{" core/src core/tests | grep -v "PlannedStep\|target/"
```

Update each `Plan { ... }` block adding `l1_insight: None,` after the existing `floor_request:` field (or `refused:` if `floor_request` is absent). Files to touch:

1. `core/src/cassandra/deterministic.rs` — all `Plan { ... }` literals in `#[cfg(test)] mod tests`
2. `core/src/cassandra/review.rs` — all `Plan { ... }` literals in `#[cfg(test)] mod tests`
3. `core/src/scheduler/inner_loop.rs` — all `Plan { ... }` literals in test fixtures (8+ sites; grep first then bulk-edit)
4. `core/src/observation/replay.rs` — fixture `terminal_plan()` + others (4+ sites)
5. `core/tests/observation_replay_cli_e2e.rs` — 1 site
6. `core/tests/observation_replay_e2e.rs` — 1 site
7. `core/tests/scheduler_inner_loop_e2e.rs` — 1 site (in `ScriptedFormulator` fixture)
8. `core/tests/scheduler_lanes_e2e.rs` — `task_complete_plan()` + `one_step_plan()` helpers

- [ ] **Step 2.5: Run the full workspace to verify cascade + accessor tests**

```bash
cargo test --workspace 2>&1 | tail -10
# Expected: "test result: ok. ... 678 passed; 0 failed" (674 + 4 new accessor tests; some
# crates may report different sub-counts — what matters is 0 failed and 0 compile errors).
```

- [ ] **Step 2.6: Commit**

```bash
git add core/src/cassandra/types.rs \
        core/src/cassandra/deterministic.rs \
        core/src/cassandra/review.rs \
        core/src/scheduler/inner_loop.rs \
        core/src/observation/replay.rs \
        core/tests/observation_replay_cli_e2e.rs \
        core/tests/observation_replay_e2e.rs \
        core/tests/scheduler_inner_loop_e2e.rs \
        core/tests/scheduler_lanes_e2e.rs
git commit -m "$(cat <<'EOF'
feat(cassandra,types): Plan.l1_insight + is_completion_with_insight accessor

Adds the agent-raised L1 promotion channel: a new optional
`Plan.l1_insight: Option<String>` field with the same
`#[serde(default, skip_serializing_if = "Option::is_none")]`
shape as `refused` and `floor_request`. Existing `Plan` fixtures
in serialized form (audit-row payloads, observation captures)
remain byte-stable when the field is unset.

The new `Plan::is_completion_with_insight() -> Option<&str>`
accessor returns `Some(insight)` iff `is_terminal() && l1_insight.is_some()`.
Encapsulates the agent-raised emit gate so the upcoming inner-loop
hook stays small.

Cascade: 8 test-fixture sites updated to add `l1_insight: None`
to their `Plan { ... }` literals. Plan has no Default impl
(deliberately; the field set is too workflow-specific) so the
addition is a structural cascade.

+4 unit tests pin the accessor positive path, both negative gates,
and the serde-default round-trip.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: `core::memory::l1_promote` module scaffold (pure helpers + types)

**Files:**
- Create: `core/src/memory/l1_promote.rs`
- Modify: `core/src/memory/mod.rs` (add `pub mod l1_promote;` line)

**Context for the engineer:** This task creates the module scaffold with all pure helpers (validator, SHA-256, metadata builder) and the types (`L1Source`, `L1Error`, `L1WriteOutcome`). The async `promote_l1` writer is Task 4; the `list_l1` + `remove_l1` are Task 5. Splitting this way keeps each task's diff focused on one concern.

The L0 seed loader at `core/src/memory/l0_seed.rs` is the canonical precedent for the validator + helper shape — read it first.

- [ ] **Step 3.1: Write the failing tests for pure helpers**

Create `core/src/memory/l1_promote.rs` with this content (the tests + the bare type signatures; impls are filled in Step 3.3):

```rust
//! Writer for `MemoryLayer::Index` (L1) rows. Two callers:
//!
//! 1. **Operator** — via `hhagent-cli memory l1 add <body>` →
//!    [`crate::cli_audit::l1_add_and_audit`].
//! 2. **Agent-raised** — via `Plan.l1_insight` consumed by
//!    [`crate::scheduler::runner::drain_lane`] on `Outcome::Completed`.
//!
//! Both callers share the same validation + dedup discipline:
//! validate via [`validate_l1_body`], compute SHA-256, EXISTS-check
//! at `layer = 1` keyed on `metadata->>'body_sha256'`, insert on
//! miss via [`hhagent_db::memories::insert_memory_at_layer`].
//!
//! See `docs/superpowers/specs/2026-05-17-l1-promotion-writer-design.md`
//! for the full design.

use hhagent_db::DbError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Maximum body length in bytes for an L1 row. Half of L0's
/// `L0_MAX_BODY_BYTES = 1024`; the L1 read cap is 4 KiB across
/// all rows so 512 leaves room for ~8 typical-length rows.
pub const L1_MAX_BODY_BYTES: usize = 512;

/// Reserved substring that would close the `<l1_insights>` block
/// rendered by the prompt assembler. An agent-raised body cannot
/// embed this without prompt-injection risk (threat-model §6).
const RESERVED_TAG_CLOSE: &str = "</l1_insights>";

/// Reserved substring for the open tag; symmetric defence even
/// though a stray open tag is less directly exploitable.
const RESERVED_TAG_OPEN: &str = "<l1_insights>";

/// Provenance for an L1 row write. The audit-row `source` field
/// is **never** producer-supplied; only the writer constructs this
/// variant (mirrors `ClassificationFloorSource::AgentRaised`
/// from issue #71).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum L1Source {
    /// Operator-explicit write via `hhagent-cli memory l1 add`.
    Operator,
    /// Agent-raised write from `runner::drain_lane` after
    /// `Outcome::Completed`. The originating `task_id` is carried
    /// in the audit-row payload for cross-restart trace stitching.
    AgentRaised { task_id: i64 },
}

/// Error kinds the L1 writer can produce.
#[derive(Debug, thiserror::Error)]
pub enum L1Error {
    #[error("L1 body validation failed: {0}")]
    Validation(String),

    #[error("L1 db error: {0}")]
    Db(#[from] DbError),
}

/// Outcome of a single `promote_l1` call.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum L1WriteOutcome {
    /// New L1 row inserted at the carried `memory_id`.
    Inserted { memory_id: i64 },
    /// A row with the same `body_sha256` already exists at
    /// `layer = 1` (carrying the existing `memory_id`). No
    /// new row was written.
    SkippedDuplicate { memory_id: i64 },
}

impl L1WriteOutcome {
    pub fn memory_id(&self) -> i64 {
        match self {
            L1WriteOutcome::Inserted { memory_id }
            | L1WriteOutcome::SkippedDuplicate { memory_id } => *memory_id,
        }
    }
}

/// Validates an L1 body string. On success returns the trimmed slice
/// (so the writer never inserts leading/trailing whitespace). On
/// failure returns [`L1Error::Validation`] with a human-readable reason.
///
/// Rejections (in declared order, first hit wins):
/// 1. Empty after trim.
/// 2. Contains any newline (`\n` or `\r`).
/// 3. Contains any other ASCII control character (< 0x20, excluding
///    `\t`/`\n`/`\r` which are handled separately; `\t` is rejected
///    so bullet indentation stays uniform).
/// 4. Contains the literal substring `<l1_insights>` or `</l1_insights>`
///    (threat-model §6 defence — an agent-raised body cannot close
///    the trust-marked block early).
/// 5. Trimmed length exceeds [`L1_MAX_BODY_BYTES`].
pub fn validate_l1_body(body: &str) -> Result<&str, L1Error> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err(L1Error::Validation("body is empty after trim".into()));
    }
    if trimmed.contains('\n') || trimmed.contains('\r') {
        return Err(L1Error::Validation("body contains newline".into()));
    }
    if trimmed.bytes().any(|b| b < 0x20) {
        return Err(L1Error::Validation("body contains control character".into()));
    }
    if trimmed.contains(RESERVED_TAG_OPEN) || trimmed.contains(RESERVED_TAG_CLOSE) {
        return Err(L1Error::Validation("body contains reserved tag substring".into()));
    }
    if trimmed.len() > L1_MAX_BODY_BYTES {
        return Err(L1Error::Validation(format!(
            "body exceeds {L1_MAX_BODY_BYTES} bytes ({})",
            trimmed.len()
        )));
    }
    Ok(trimmed)
}

/// SHA-256 of the body, lowercase 64-char hex. Mirrors
/// [`crate::memory::l0_seed::compute_body_sha256`].
pub fn compute_body_sha256(body: &str) -> String {
    let mut h = Sha256::new();
    h.update(body.as_bytes());
    format!("{:x}", h.finalize())
}

/// Build the `metadata` JSONB blob for a new L1 row. Schema:
/// `{source, body_sha256, created_at, task_id?}`. `task_id` is
/// present iff `source` is `L1Source::AgentRaised`.
pub fn build_l1_metadata(
    source: &L1Source,
    body_sha256: &str,
    created_at_rfc3339: &str,
) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    match source {
        L1Source::Operator => {
            obj.insert("source".into(), serde_json::Value::String("operator".into()));
        }
        L1Source::AgentRaised { task_id } => {
            obj.insert("source".into(), serde_json::Value::String("agent_raised".into()));
            obj.insert(
                "task_id".into(),
                serde_json::Value::Number(serde_json::Number::from(*task_id)),
            );
        }
    }
    obj.insert(
        "body_sha256".into(),
        serde_json::Value::String(body_sha256.into()),
    );
    obj.insert(
        "created_at".into(),
        serde_json::Value::String(created_at_rfc3339.into()),
    );
    serde_json::Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_empty_after_trim() {
        let err = validate_l1_body("   \t  ").expect_err("empty");
        match err {
            L1Error::Validation(msg) => assert!(msg.contains("empty"), "{msg}"),
            _ => panic!("wrong error kind"),
        }
    }

    #[test]
    fn validate_rejects_newlines() {
        for s in &["foo\nbar", "foo\r\nbar", "trailing\n", "\nleading"] {
            let err = validate_l1_body(s).expect_err(s);
            match err {
                L1Error::Validation(msg) => assert!(msg.contains("newline"), "got: {msg}"),
                _ => panic!("wrong error kind for {s:?}"),
            }
        }
    }

    #[test]
    fn validate_rejects_control_chars() {
        for s in &["foo\tbar", "foo\x00bar", "foo\x07bar"] {
            let err = validate_l1_body(s).expect_err(s);
            match err {
                L1Error::Validation(msg) => {
                    assert!(msg.contains("control character") || msg.contains("newline"), "got: {msg}");
                }
                _ => panic!("wrong error kind for {s:?}"),
            }
        }
    }

    #[test]
    fn validate_rejects_reserved_tag_substring() {
        for s in &[
            "innocuous <l1_insights> not so innocuous",
            "before</l1_insights>after",
            "</l1_insights>",
        ] {
            let err = validate_l1_body(s).expect_err(s);
            match err {
                L1Error::Validation(msg) => assert!(msg.contains("reserved tag"), "got: {msg}"),
                _ => panic!("wrong error kind for {s:?}"),
            }
        }
    }

    #[test]
    fn validate_rejects_over_length() {
        let body = "a".repeat(L1_MAX_BODY_BYTES + 1);
        let err = validate_l1_body(&body).expect_err("over-length");
        match err {
            L1Error::Validation(msg) => {
                assert!(msg.contains("exceeds 512 bytes"), "got: {msg}");
                assert!(msg.contains(&format!("({})", L1_MAX_BODY_BYTES + 1)), "got: {msg}");
            }
            _ => panic!("wrong error kind"),
        }
    }

    #[test]
    fn validate_accepts_exact_cap() {
        let body = "a".repeat(L1_MAX_BODY_BYTES);
        let trimmed = validate_l1_body(&body).expect("at-cap");
        assert_eq!(trimmed.len(), L1_MAX_BODY_BYTES);
    }

    #[test]
    fn validate_returns_trimmed_slice() {
        let body = "   shell-exec /bin/ls works   ";
        let trimmed = validate_l1_body(body).expect("ok");
        assert_eq!(trimmed, "shell-exec /bin/ls works");
    }

    #[test]
    fn validate_accepts_typical_body() {
        let body = "shell-exec /usr/bin/ls reliably enumerates dir contents";
        let trimmed = validate_l1_body(body).expect("ok");
        assert_eq!(trimmed, body);
    }

    #[test]
    fn compute_body_sha256_is_deterministic_and_64_hex() {
        let s1 = compute_body_sha256("hello");
        let s2 = compute_body_sha256("hello");
        assert_eq!(s1, s2, "deterministic");
        assert_eq!(s1.len(), 64, "64-char hex");
        assert!(s1.chars().all(|c| c.is_ascii_hexdigit() && (!c.is_ascii_alphabetic() || c.is_ascii_lowercase())), "lowercase hex");
    }

    #[test]
    fn compute_body_sha256_distinct_for_distinct_inputs() {
        assert_ne!(compute_body_sha256("hello"), compute_body_sha256("hellp"));
    }

    #[test]
    fn build_l1_metadata_operator_has_no_task_id() {
        let m = build_l1_metadata(
            &L1Source::Operator,
            "abc123",
            "2026-05-17T12:00:00Z",
        );
        let obj = m.as_object().expect("object");
        assert_eq!(obj.get("source").unwrap(), "operator");
        assert_eq!(obj.get("body_sha256").unwrap(), "abc123");
        assert_eq!(obj.get("created_at").unwrap(), "2026-05-17T12:00:00Z");
        assert!(obj.get("task_id").is_none(), "Operator must NOT carry task_id");
        assert_eq!(obj.len(), 3, "exactly 3 keys for Operator");
    }

    #[test]
    fn build_l1_metadata_agent_raised_carries_task_id() {
        let m = build_l1_metadata(
            &L1Source::AgentRaised { task_id: 42 },
            "def456",
            "2026-05-17T12:00:01Z",
        );
        let obj = m.as_object().expect("object");
        assert_eq!(obj.get("source").unwrap(), "agent_raised");
        assert_eq!(obj.get("task_id").unwrap(), 42);
        assert_eq!(obj.get("body_sha256").unwrap(), "def456");
        assert_eq!(obj.get("created_at").unwrap(), "2026-05-17T12:00:01Z");
        assert_eq!(obj.len(), 4, "exactly 4 keys for AgentRaised");
    }

    #[test]
    fn l1_source_serializes_as_snake_case_internally_tagged() {
        let op = serde_json::to_value(&L1Source::Operator).expect("serialize");
        assert_eq!(op, serde_json::json!({"source": "operator"}));

        let ag = serde_json::to_value(&L1Source::AgentRaised { task_id: 7 }).expect("serialize");
        assert_eq!(ag, serde_json::json!({"source": "agent_raised", "task_id": 7}));
    }
}
```

- [ ] **Step 3.2: Wire the module + run failing tests**

In `core/src/memory/mod.rs`, add the line `pub mod l1_promote;` immediately after the existing `pub mod l0_seed;` (or wherever the existing layer-related re-exports live). Confirm the file with:

```bash
grep -n "pub mod" core/src/memory/mod.rs
```

Then check the `core/Cargo.toml` has `sha2` and `thiserror` as workspace deps (it should already — these are used by `l0_seed`). If they're missing from `core/Cargo.toml`, add them.

```bash
cargo build -p hhagent-core 2>&1 | tail -20
# If you see "unresolved import `sha2`" or "unresolved import `thiserror`",
# add to core/Cargo.toml under [dependencies]:
#   sha2 = { workspace = true }
#   thiserror = { workspace = true }
```

- [ ] **Step 3.3: Run tests**

```bash
cargo test -p hhagent-core memory::l1_promote 2>&1 | tail -15
# Expected: "12 passed; 0 failed"
```

- [ ] **Step 3.4: Commit**

```bash
git add core/src/memory/l1_promote.rs core/src/memory/mod.rs core/Cargo.toml
git commit -m "$(cat <<'EOF'
feat(core,memory): l1_promote module scaffold — types + validator + helpers

Adds the pure foundation for L1 row writes:

- `L1Source { Operator, AgentRaised { task_id } }` enum, snake-case
  internally-tagged serde shape so the audit-row payload renders as
  `{"source": "operator"}` / `{"source": "agent_raised", "task_id": N}`.

- `L1Error { Validation(String), Db(#[from] DbError) }`.

- `L1WriteOutcome { Inserted { memory_id }, SkippedDuplicate { memory_id } }`,
  snake-case internally-tagged so the audit-row payload renders as
  `{"action": "inserted", "memory_id": N}` / `{"action": "skipped_duplicate",
  "memory_id": N}`. `memory_id()` accessor returns the id regardless of variant.

- `L1_MAX_BODY_BYTES = 512` (half of L0's 1024; the L1 read cap is 4 KiB total).

- `validate_l1_body(&str) -> Result<&str, L1Error>` — declared-order rejections:
  empty-after-trim, newlines, other control chars, reserved-tag substrings
  (`<l1_insights>`, `</l1_insights>`, threat-model §6 defence), over-length.
  Returns the trimmed slice on success so the writer never inserts whitespace.

- `compute_body_sha256(&str) -> String` (lowercase 64-char hex, mirrors l0_seed).

- `build_l1_metadata(source, body_sha256, created_at) -> Value` —
  3-key JSON object for Operator, 4-key for AgentRaised (adds task_id).

+12 unit tests pin every rejection path, accepted-at-cap boundary,
serde shape for both `L1Source` variants, and metadata key-set for
both Operator (3 keys) and AgentRaised (4 keys).

Async writer + list/remove come in subsequent tasks (this task is
the pure scaffold; the writer Task 4 builds on these helpers).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: `core::memory::l1_promote::promote_l1` async writer

**Files:**
- Modify: `core/src/memory/l1_promote.rs` (add `promote_l1` + tests)

**Context for the engineer:** The async writer composes the pure helpers (validate → SHA-256 → metadata) with the database (EXISTS-check → INSERT). Idempotent: if a row with the same body SHA-256 exists at `layer = 1`, no new row is written; the existing `memory_id` is returned in `L1WriteOutcome::SkippedDuplicate`.

- [ ] **Step 4.1: Write the failing async tests**

Append inside `mod tests` in `core/src/memory/l1_promote.rs` (use a separate test module gate so the unit-tier tests still run cleanly):

```rust
    use hhagent_db::memories::{load_l1, MemoryLayer};

    fn now_rfc3339() -> String {
        time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .expect("rfc3339 format")
    }

    #[tokio::test]
    async fn promote_l1_inserts_new_row() {
        let cluster = match hhagent_tests_common::bring_up_pg_cluster().await {
            Some(c) => c,
            None => { eprintln!("[SKIP] no PG"); return; }
        };
        let pool = cluster.runtime_pool().await.expect("runtime pool");

        let outcome = promote_l1(
            &pool,
            "shell-exec /bin/ls works for dir listings",
            L1Source::Operator,
        ).await.expect("ok");

        match outcome {
            L1WriteOutcome::Inserted { memory_id } => assert!(memory_id > 0),
            other => panic!("expected Inserted, got {other:?}"),
        }

        // Confirm the row landed at layer=1.
        let rows = load_l1(&pool, 16, 4096).await.expect("load_l1");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].body, "shell-exec /bin/ls works for dir listings");
        assert_eq!(rows[0].layer, MemoryLayer::Index);
    }

    #[tokio::test]
    async fn promote_l1_is_idempotent_on_body_sha256() {
        let cluster = match hhagent_tests_common::bring_up_pg_cluster().await {
            Some(c) => c,
            None => { eprintln!("[SKIP] no PG"); return; }
        };
        let pool = cluster.runtime_pool().await.expect("runtime pool");

        let first = promote_l1(&pool, "X", L1Source::Operator).await.expect("first");
        let id1 = match first { L1WriteOutcome::Inserted { memory_id } => memory_id, _ => panic!() };

        let second = promote_l1(&pool, "X", L1Source::Operator).await.expect("second");
        match second {
            L1WriteOutcome::SkippedDuplicate { memory_id } => assert_eq!(memory_id, id1),
            other => panic!("expected SkippedDuplicate, got {other:?}"),
        }

        // And confirm only one row landed.
        let rows = load_l1(&pool, 16, 4096).await.expect("load");
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn promote_l1_dedup_across_sources() {
        // Same body, different source -> still deduped (body_sha256 is source-agnostic).
        let cluster = match hhagent_tests_common::bring_up_pg_cluster().await {
            Some(c) => c,
            None => { eprintln!("[SKIP] no PG"); return; }
        };
        let pool = cluster.runtime_pool().await.expect("runtime pool");

        let first = promote_l1(&pool, "Y", L1Source::Operator).await.expect("op");
        let id1 = match first { L1WriteOutcome::Inserted { memory_id } => memory_id, _ => panic!() };

        let second = promote_l1(
            &pool, "Y", L1Source::AgentRaised { task_id: 99 },
        ).await.expect("ag");
        match second {
            L1WriteOutcome::SkippedDuplicate { memory_id } => assert_eq!(memory_id, id1),
            other => panic!("expected SkippedDuplicate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn promote_l1_propagates_validation_error() {
        let cluster = match hhagent_tests_common::bring_up_pg_cluster().await {
            Some(c) => c,
            None => { eprintln!("[SKIP] no PG"); return; }
        };
        let pool = cluster.runtime_pool().await.expect("runtime pool");

        for bad in &["", "  ", "has\nnewline", "</l1_insights>", &"a".repeat(L1_MAX_BODY_BYTES + 1)] {
            let err = promote_l1(&pool, bad, L1Source::Operator).await.expect_err(bad);
            assert!(matches!(err, L1Error::Validation(_)), "expected Validation for {bad:?}");
        }

        // No rows landed for any rejection.
        let rows = load_l1(&pool, 16, 4096).await.expect("load");
        assert_eq!(rows.len(), 0);
    }

    #[tokio::test]
    async fn promote_l1_trims_body_before_storage() {
        let cluster = match hhagent_tests_common::bring_up_pg_cluster().await {
            Some(c) => c,
            None => { eprintln!("[SKIP] no PG"); return; }
        };
        let pool = cluster.runtime_pool().await.expect("runtime pool");

        let outcome = promote_l1(&pool, "   trimmed body   ", L1Source::Operator)
            .await.expect("ok");
        match outcome { L1WriteOutcome::Inserted { .. } => {}, _ => panic!() };

        let rows = load_l1(&pool, 16, 4096).await.expect("load");
        assert_eq!(rows[0].body, "trimmed body", "stored body must be trimmed");

        // Re-promoting with surrounding whitespace must still dedup
        // (the trimmed body produces the same SHA-256).
        let outcome2 = promote_l1(&pool, "  trimmed body  ", L1Source::Operator)
            .await.expect("ok2");
        assert!(matches!(outcome2, L1WriteOutcome::SkippedDuplicate { .. }));
    }
```

- [ ] **Step 4.2: Run tests to verify failure**

```bash
cargo test -p hhagent-core memory::l1_promote::tests::promote_l1 2>&1 | tail -15
# Expected: compile error "cannot find function `promote_l1` in this scope"
```

- [ ] **Step 4.3: Implement `promote_l1`**

Add to `core/src/memory/l1_promote.rs` above the `#[cfg(test)] mod tests` block:

```rust
use hhagent_db::memories::{insert_memory_at_layer, MemoryLayer};
use sqlx::PgPool;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Promote a single L1 row. Validates, computes SHA-256, EXISTS-checks
/// against `layer = 1` rows by `metadata->>'body_sha256'`, inserts on
/// miss. Idempotent on body SHA-256 across all source variants — the
/// dedup is source-agnostic (a body the operator added is not promoted
/// again when the agent later raises it, and vice versa).
///
/// The `metadata` blob carries `{source, body_sha256, created_at, task_id?}`
/// per [`build_l1_metadata`].
///
/// **Embedding:** not populated. L1 is loaded by sequential scan via
/// `load_l1` (newest-first, byte-capped); the embedding column would
/// only matter if L1 rows ever flowed through `recall(SEMANTIC_ONLY)`.
/// They don't today, and if they ever do (probably via a hybrid
/// always-in-context + semantically-retrieved hybrid) a follow-up
/// slice can backfill the column.
pub async fn promote_l1(
    pool: &PgPool,
    body: &str,
    source: L1Source,
) -> Result<L1WriteOutcome, L1Error> {
    let trimmed = validate_l1_body(body)?;
    let body_sha256 = compute_body_sha256(trimmed);

    // EXISTS-check keyed on metadata->>'body_sha256' at layer = 1.
    // `metadata` is JSONB, no index hit needed for v1 (operator load
    // is low-cardinality; if this gets hot a partial expression index
    // on `(metadata->>'body_sha256') WHERE layer = 1` is the
    // straightforward follow-up).
    let existing: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM memories \
         WHERE layer = $1 AND metadata->>'body_sha256' = $2 \
         ORDER BY id ASC LIMIT 1",
    )
    .bind(MemoryLayer::Index.as_db())
    .bind(&body_sha256)
    .fetch_optional(pool)
    .await
    .map_err(|e| L1Error::Db(hhagent_db::DbError::from(e)))?;

    if let Some(existing_id) = existing {
        return Ok(L1WriteOutcome::SkippedDuplicate { memory_id: existing_id });
    }

    let created_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("rfc3339 format");
    let metadata = build_l1_metadata(&source, &body_sha256, &created_at);

    let new_id = insert_memory_at_layer(
        pool,
        MemoryLayer::Index,
        trimmed,
        metadata,
        None, // embedding not populated for L1 v1
    )
    .await?;
    Ok(L1WriteOutcome::Inserted { memory_id: new_id })
}
```

- [ ] **Step 4.4: Run tests to verify pass**

```bash
cargo test -p hhagent-core memory::l1_promote 2>&1 | tail -15
# Expected: "17 passed; 0 failed" (12 from Task 3 + 5 new async)
```

- [ ] **Step 4.5: Commit**

```bash
git add core/src/memory/l1_promote.rs
git commit -m "$(cat <<'EOF'
feat(core,memory,l1_promote): promote_l1 writer with idempotent body_sha256 dedup

The shared writer for both L1 paths (operator + agent-raised).
Composes the Task-3 pure helpers (validate -> SHA-256 -> metadata)
with the database (EXISTS-check on `metadata->>'body_sha256'` at
layer=1 -> INSERT on miss via insert_memory_at_layer).

Dedup is source-agnostic: a body the operator added is not promoted
again when the agent later raises it, and vice versa. The audit row
distinguishes both cases via the `source` field; the L1 row itself
carries the FIRST-WRITER's source in its metadata (no metadata
overwrite on the duplicate path).

Embedding is intentionally not populated — L1 is loaded by sequential
scan in `load_l1` (newest-first, byte-capped); the column would only
matter if L1 ever flowed through recall's semantic lane. Follow-up
backfills if/when needed.

+5 inline async tests (PG-cluster gated, skip-as-pass on no-PG):
inserts new row at layer=1; idempotent on same body+source;
idempotent across Operator -> AgentRaised; propagates validation
errors with no row landing; trims body before storage and dedups
trimmed vs whitespace-padded.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: `core::memory::l1_promote::list_l1` + `remove_l1`

**Files:**
- Modify: `core/src/memory/l1_promote.rs` (add `list_l1`, `remove_l1`, tests)

**Context for the engineer:** Reader + remover for the operator CLI. `list_l1(pool, all)` flips between `load_l1_default` (in-prompt rows only) and `load_layer` (everything). `remove_l1(pool, id)` calls `db::memories::delete_memory_at_layer(MemoryLayer::Index)` so the wrong-layer guard is enforced at the DB level.

- [ ] **Step 5.1: Write the failing tests**

Append to `mod tests` in `core/src/memory/l1_promote.rs`:

```rust
    #[tokio::test]
    async fn list_l1_in_prompt_returns_default_cap_view() {
        let cluster = match hhagent_tests_common::bring_up_pg_cluster().await {
            Some(c) => c,
            None => { eprintln!("[SKIP] no PG"); return; }
        };
        let pool = cluster.runtime_pool().await.expect("runtime pool");

        for i in 0..3 {
            promote_l1(&pool, &format!("body-{i}"), L1Source::Operator).await.expect("seed");
        }

        let rows = list_l1(&pool, false).await.expect("in-prompt list");
        assert_eq!(rows.len(), 3, "should see all 3 (under both caps)");
    }

    #[tokio::test]
    async fn list_l1_all_returns_every_row() {
        let cluster = match hhagent_tests_common::bring_up_pg_cluster().await {
            Some(c) => c,
            None => { eprintln!("[SKIP] no PG"); return; }
        };
        let pool = cluster.runtime_pool().await.expect("runtime pool");

        // Seed more than the in-prompt cap (32 rows).
        for i in 0..40 {
            promote_l1(&pool, &format!("body-{i}"), L1Source::Operator).await.expect("seed");
        }

        let in_prompt = list_l1(&pool, false).await.expect("in-prompt");
        let everything = list_l1(&pool, true).await.expect("all");

        assert!(in_prompt.len() <= 32, "in-prompt respects 32-row cap, got {}", in_prompt.len());
        assert_eq!(everything.len(), 40, "list_l1(all=true) returns every row");
    }

    #[tokio::test]
    async fn remove_l1_deletes_at_layer_1() {
        let cluster = match hhagent_tests_common::bring_up_pg_cluster().await {
            Some(c) => c,
            None => { eprintln!("[SKIP] no PG"); return; }
        };
        let pool = cluster.runtime_pool().await.expect("runtime pool");

        let outcome = promote_l1(&pool, "to-remove", L1Source::Operator).await.expect("seed");
        let id = outcome.memory_id();

        let deleted = remove_l1(&pool, id).await.expect("remove");
        assert!(deleted);

        let rows = list_l1(&pool, true).await.expect("list");
        assert!(rows.iter().find(|r| r.id == id).is_none(), "row must be gone");
    }

    #[tokio::test]
    async fn remove_l1_refuses_to_touch_non_l1_row() {
        let cluster = match hhagent_tests_common::bring_up_pg_cluster().await {
            Some(c) => c,
            None => { eprintln!("[SKIP] no PG"); return; }
        };
        let pool = cluster.runtime_pool().await.expect("runtime pool");

        // Seed at the default Stable layer.
        let id = hhagent_db::memories::insert_memory(
            &pool, "stable-row", serde_json::json!({}), None,
        ).await.expect("insert");

        let deleted = remove_l1(&pool, id).await.expect("remove");
        assert!(!deleted, "wrong-layer guard must reject");

        // L2 row is untouched.
        let rows = hhagent_db::memories::fetch_by_ids(&pool, &[id]).await.expect("fetch");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].body, "stable-row");
    }
```

- [ ] **Step 5.2: Run tests to verify failure**

```bash
cargo test -p hhagent-core memory::l1_promote::tests::list_l1 \
                                memory::l1_promote::tests::remove_l1 2>&1 | tail -10
# Expected: compile error "cannot find function `list_l1`"
```

- [ ] **Step 5.3: Implement `list_l1` + `remove_l1`**

Add to `core/src/memory/l1_promote.rs` above the `#[cfg(test)] mod tests` block (just after `promote_l1`):

```rust
use hhagent_db::memories::{load_layer, Memory};
use crate::memory::layers::load_l1_default;

/// Operator-facing list view.
///
/// - `all = false` returns the **in-prompt** slice via `load_l1_default`
///   (newest-first, capped at 32 rows / 4 KiB). What the prompt
///   assembler will actually render.
/// - `all = true` returns every row at `layer = 1` (newest-first,
///   no byte cap, no row cap). For operator audit / cleanup.
pub async fn list_l1(pool: &PgPool, all: bool) -> Result<Vec<Memory>, DbError> {
    if all {
        load_layer(pool, MemoryLayer::Index, i64::MAX).await
    } else {
        load_l1_default(pool).await
    }
}

/// Operator-facing remove. Layer-guarded via
/// [`hhagent_db::memories::delete_memory_at_layer`]: cannot delete
/// an L0 / L2 / L3 row even if the operator typoed the id.
///
/// Returns `true` iff a row was deleted.
pub async fn remove_l1(pool: &PgPool, id: i64) -> Result<bool, DbError> {
    hhagent_db::memories::delete_memory_at_layer(pool, id, MemoryLayer::Index).await
}
```

- [ ] **Step 5.4: Run tests to verify pass**

```bash
cargo test -p hhagent-core memory::l1_promote 2>&1 | tail -10
# Expected: "21 passed; 0 failed" (12 + 5 + 4 new)
```

- [ ] **Step 5.5: Commit**

```bash
git add core/src/memory/l1_promote.rs
git commit -m "$(cat <<'EOF'
feat(core,memory,l1_promote): list_l1 + remove_l1 for operator CLI

Two read/delete entry points consumed by `hhagent-cli memory l1
{list,remove}`:

- `list_l1(pool, all)` flips between `load_l1_default` (in-prompt
  slice: 32 rows / 4 KiB) and `load_layer(Index, i64::MAX)`
  (everything at layer=1) so the operator can audit pruned rows.

- `remove_l1(pool, id)` delegates to `db::memories::delete_memory_at_layer`
  with `MemoryLayer::Index` — the layer guard is enforced at the DB
  level, so an operator who typoes an L0/L2/L3 id through this path
  gets a clean `false` return, not a silent cross-layer delete.

+4 inline async tests pin the in-prompt vs all distinction (40 rows
seeded; in-prompt <= 32, all = 40), happy-path remove + verify,
and the wrong-layer-guard against an L2 row.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Audit action constants + `build_l1_write_payload` helper

**Files:**
- Modify: `core/src/scheduler/audit.rs` (add 3 constants + helper + 4 tests)

**Context for the engineer:** The audit-row payload is shared between the operator path (Task 10 cli_audit helpers) and the agent-raised path (Task 9 drain_lane hook). One pure helper produces the wire shape so both paths land byte-identical rows on `(source, action, body_sha256, memory_id[, task_id])`.

- [ ] **Step 6.1: Write the failing tests**

Append to `core/src/scheduler/audit.rs` inside the existing `mod tests`:

```rust
    use crate::memory::l1_promote::{L1Source, L1WriteOutcome};
    use serde_json::json;

    #[test]
    fn build_l1_write_payload_operator_inserted_shape() {
        let payload = build_l1_write_payload(
            &L1WriteOutcome::Inserted { memory_id: 42 },
            &L1Source::Operator,
            "abc123",
        );
        assert_eq!(
            payload,
            json!({"source": "operator", "action": "inserted", "memory_id": 42, "body_sha256": "abc123"}),
        );
    }

    #[test]
    fn build_l1_write_payload_operator_skipped_duplicate_shape() {
        let payload = build_l1_write_payload(
            &L1WriteOutcome::SkippedDuplicate { memory_id: 7 },
            &L1Source::Operator,
            "def456",
        );
        assert_eq!(
            payload,
            json!({"source": "operator", "action": "skipped_duplicate", "memory_id": 7, "body_sha256": "def456"}),
        );
    }

    #[test]
    fn build_l1_write_payload_agent_raised_carries_task_id() {
        let payload = build_l1_write_payload(
            &L1WriteOutcome::Inserted { memory_id: 88 },
            &L1Source::AgentRaised { task_id: 123 },
            "abc123",
        );
        assert_eq!(
            payload,
            json!({"source": "agent_raised", "task_id": 123, "action": "inserted", "memory_id": 88, "body_sha256": "abc123"}),
        );
    }

    #[test]
    fn build_l1_write_payload_agent_raised_skipped_duplicate_shape() {
        let payload = build_l1_write_payload(
            &L1WriteOutcome::SkippedDuplicate { memory_id: 88 },
            &L1Source::AgentRaised { task_id: 99 },
            "ddd",
        );
        assert_eq!(
            payload,
            json!({"source": "agent_raised", "task_id": 99, "action": "skipped_duplicate", "memory_id": 88, "body_sha256": "ddd"}),
        );
    }

    #[test]
    fn l1_action_constants_are_distinct_and_stable() {
        // Stability check: these strings are wire contract. A future
        // rename would invalidate JSONB queries grouped on `action`.
        assert_eq!(ACTION_L1_ADDED, "l1.added");
        assert_eq!(ACTION_L1_REMOVED, "l1.removed");
        assert_eq!(ACTION_L1_PROMOTED, "l1.promoted");
    }
```

- [ ] **Step 6.2: Run to verify failure**

```bash
cargo test -p hhagent-core scheduler::audit::tests::build_l1_write_payload \
                                scheduler::audit::tests::l1_action_constants 2>&1 | tail -10
# Expected: compile error "cannot find function `build_l1_write_payload`"
# AND: "cannot find value `ACTION_L1_ADDED`"
```

- [ ] **Step 6.3: Implement the constants + helper**

Add to `core/src/scheduler/audit.rs`, placed adjacent to the existing `ACTION_L0_SEEDED` / `ACTION_REGISTRY_LOADED` constants:

```rust
/// Action string for `actor='cli' action='l1.added'` audit rows.
/// Emitted by `cli_audit::l1_add_and_audit` after a successful
/// `hhagent-cli memory l1 add` call. The payload is built by
/// [`build_l1_write_payload`].
pub const ACTION_L1_ADDED: &str = "l1.added";

/// Action string for `actor='cli' action='l1.removed'` audit rows.
/// Emitted by `cli_audit::l1_remove_and_audit` after a successful
/// `hhagent-cli memory l1 remove`. Payload: `{memory_id, deleted}`.
pub const ACTION_L1_REMOVED: &str = "l1.removed";

/// Action string for `actor='scheduler' action='l1.promoted'` audit
/// rows. Emitted by `runner::drain_lane` when the terminal plan
/// carried `l1_insight` and the inner loop reached `Outcome::Completed`.
/// The payload is built by [`build_l1_write_payload`].
pub const ACTION_L1_PROMOTED: &str = "l1.promoted";

/// Build the payload for `l1.added` (operator) and `l1.promoted`
/// (agent-raised) audit rows. Single helper so both paths land
/// byte-identical rows on the common keys.
///
/// Operator shape: `{source: "operator", action, memory_id, body_sha256}` (4 keys).
/// Agent-raised shape: `{source: "agent_raised", task_id, action, memory_id, body_sha256}` (5 keys).
pub fn build_l1_write_payload(
    outcome: &crate::memory::l1_promote::L1WriteOutcome,
    source: &crate::memory::l1_promote::L1Source,
    body_sha256: &str,
) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    match source {
        crate::memory::l1_promote::L1Source::Operator => {
            obj.insert("source".into(), serde_json::Value::String("operator".into()));
        }
        crate::memory::l1_promote::L1Source::AgentRaised { task_id } => {
            obj.insert("source".into(), serde_json::Value::String("agent_raised".into()));
            obj.insert(
                "task_id".into(),
                serde_json::Value::Number(serde_json::Number::from(*task_id)),
            );
        }
    }
    let (action_str, memory_id) = match outcome {
        crate::memory::l1_promote::L1WriteOutcome::Inserted { memory_id } => ("inserted", *memory_id),
        crate::memory::l1_promote::L1WriteOutcome::SkippedDuplicate { memory_id } => ("skipped_duplicate", *memory_id),
    };
    obj.insert("action".into(), serde_json::Value::String(action_str.into()));
    obj.insert(
        "memory_id".into(),
        serde_json::Value::Number(serde_json::Number::from(memory_id)),
    );
    obj.insert(
        "body_sha256".into(),
        serde_json::Value::String(body_sha256.into()),
    );
    serde_json::Value::Object(obj)
}
```

- [ ] **Step 6.4: Run to verify pass**

```bash
cargo test -p hhagent-core scheduler::audit 2>&1 | tail -15
# Expected: all prior audit tests still pass + 5 new (4 shape + 1 stability)
```

- [ ] **Step 6.5: Commit**

```bash
git add core/src/scheduler/audit.rs
git commit -m "$(cat <<'EOF'
feat(core,scheduler,audit): L1 action constants + build_l1_write_payload

Three new audit-row action strings (stable wire contract):
  ACTION_L1_ADDED    = "l1.added"     -- actor=cli, hhagent-cli memory l1 add
  ACTION_L1_REMOVED  = "l1.removed"   -- actor=cli, hhagent-cli memory l1 remove
  ACTION_L1_PROMOTED = "l1.promoted"  -- actor=scheduler, drain_lane after Completed

One pure helper build_l1_write_payload(outcome, source, body_sha256)
shared by the operator path (Task 10 cli_audit) and the agent-raised
path (Task 9 drain_lane). Operator payload has 4 keys
(source/action/memory_id/body_sha256); AgentRaised adds task_id for
5 keys total. Source serialisation maps L1Source -> snake_case
("operator" / "agent_raised") for SQL `WHERE payload->>'source' = ...`.

+5 unit tests pin every (Source, Outcome) combination's wire shape
plus a stability assertion on the three action constants.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: `agent_planner.md` prompt update

**Files:**
- Modify: `prompts/agent_planner.md`

**Context for the engineer:** The agent_prompts SHA-256 ledger (db migration 0006 + `db::agent_prompts::upsert_prompt`) records every prompt version automatically on daemon start. No code change is needed for the new SHA to land; the planner output starts including `l1_insight` once we teach the model what to fill.

The L1 distillation guidance lives next to the existing `refused` and `floor_request` guidance — it's another "optional field the agent fills in special circumstances".

- [ ] **Step 7.1: Locate the planner prompt's JSON-schema example**

```bash
cat prompts/agent_planner.md
# Find the JSON schema example block (it has `"refused": null,` somewhere).
```

- [ ] **Step 7.2: Update the prompt**

In the JSON-schema example block, add `"l1_insight": null` adjacent to `"refused": null` (likely on the next line, with matching indentation).

Then add one paragraph to the prose explaining the field. A reasonable place is immediately after the existing paragraph that introduces `floor_request`:

```markdown
**Optional: `l1_insight`.** On a *terminal* plan (`decision: "task_complete"` with `steps: []`) you may include `l1_insight` as a single short bullet (≤ 300 characters, no newlines) capturing a **generalizable lesson** learned across this task — something useful for *future* tasks, not a summary of *this* task. Examples: "shell-exec /usr/bin/ls reliably enumerates dir contents", "tasks needing /etc/shadow access always POLICY_DENIED — escalate via human approval first". Omit the field if no generalizable lesson exists; false positives bloat the always-in-context insight block and degrade later planning. The field is dropped if the reviewer Blocks, Escalates, or if you self-refuse.
```

- [ ] **Step 7.3: Smoke-test that the file is still valid markdown**

```bash
# No tests to run; just sanity-check that the file is still readable and the
# JSON-example block parses. A future hhagent daemon start will SHA-256 and
# ledger the new prompt automatically.
wc -l prompts/agent_planner.md
# Just confirm the count went up by a few lines.
```

- [ ] **Step 7.4: Commit**

```bash
git add prompts/agent_planner.md
git commit -m "$(cat <<'EOF'
docs(prompts,agent_planner): teach the model when to set l1_insight

Adds one paragraph + a `"l1_insight": null` line in the JSON
schema example. Field is optional on terminal plans only; the
guidance emphasises generalizable lessons (cross-task useful)
and explicitly tells the model that false positives are worse
than false negatives.

The agent_prompts SHA-256 ledger (migration 0006 + upsert_prompt)
captures the new prompt version automatically on next daemon start,
no code change needed here.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: `InnerLoopResult.terminal_l1_insight` + payload key

**Files:**
- Modify: `core/src/scheduler/inner_loop.rs` (add field; populate on Completed arm; bump payload builder; bump pin tests)
- Modify: `core/src/scheduler/runner.rs` (fix `failed_result` literal site)

**Context for the engineer:** `InnerLoopResult` is constructed in exactly two places: `core/src/scheduler/inner_loop.rs:207` (the happy / failed path inside `run_to_terminal`) and `core/src/scheduler/runner.rs:357` (the `failed_result` helper for pre-loop failures). Both need the new field. `build_plan_formulate_payload` is also in `inner_loop.rs`; bumps from 20/21 → 21/22 keys.

- [ ] **Step 8.1: Write the failing tests**

Append to `mod tests` in `core/src/scheduler/inner_loop.rs`:

```rust
    #[test]
    fn plan_formulate_payload_carries_l1_insight_when_set() {
        let plan = make_text_plan(); // existing helper that produces a terminal plan
        let plan_with_insight = Plan {
            l1_insight: Some("learned X".into()),
            ..plan
        };
        let meta = FormulationMeta {
            classification_floor: DataClass::Public,
            classification_floor_source: ClassificationFloorSource::Default,
            classification_floor_signals: None,
            assembled_prompt_sha256: "deadbeef".into(),
            l0_count: 0,
            l1_count: 0,
            recalled_memory_ids: vec![],
            recall_count: 0,
            recall_query_sha256: "cafe".into(),
        };
        let payload = build_plan_formulate_payload(&plan_with_insight, &meta, 1, &[]);
        assert_eq!(
            payload.get("l1_insight").expect("key present"),
            &serde_json::Value::String("learned X".into()),
        );
    }

    #[test]
    fn plan_formulate_payload_carries_explicit_null_l1_insight_when_unset() {
        // The key must be present-but-null when the agent does not set it,
        // so JSONB queries `WHERE payload ? 'l1_insight'` find the row.
        let plan = make_text_plan();
        let meta = FormulationMeta {
            classification_floor: DataClass::Public,
            classification_floor_source: ClassificationFloorSource::Default,
            classification_floor_signals: None,
            assembled_prompt_sha256: "deadbeef".into(),
            l0_count: 0,
            l1_count: 0,
            recalled_memory_ids: vec![],
            recall_count: 0,
            recall_query_sha256: "cafe".into(),
        };
        let payload = build_plan_formulate_payload(&plan, &meta, 1, &[]);
        assert_eq!(
            payload.get("l1_insight").expect("key present"),
            &serde_json::Value::Null,
        );
    }

    #[test]
    fn inner_loop_result_terminal_l1_insight_is_none_on_failed_outcome() {
        // failed_result helper must initialize terminal_l1_insight to None.
        // Imported from runner::failed_result; this is a structural pin.
        let r = crate::scheduler::runner::failed_result_for_test("forced");
        assert!(r.terminal_l1_insight.is_none());
    }
```

(Note: `failed_result_for_test` doesn't exist yet — we add it in Step 8.3 as a `#[cfg(test)] pub` shim if `failed_result` is private. If it's already `pub(crate)`, we can call it directly.)

- [ ] **Step 8.2: Update existing payload-key pin tests in inner_loop.rs**

Find the pin tests that assert the key count (search for `keys.len()` near the audit payload tests; they currently assert 20/21 keys for default vs cli_inferred source). Each one needs to bump expected key count by 1 (gain `l1_insight`):

```bash
grep -n "keys.len()\|assert.*key.*count\|17.*expected\|18.*expected\|20.*expected\|21.*expected" core/src/scheduler/inner_loop.rs | head -10
```

Update each assertion to the new count: 20 → 21 for default/operator/agent_raised, 21 → 22 for cli_inferred.

- [ ] **Step 8.3: Run tests to verify failure**

```bash
cargo test -p hhagent-core scheduler::inner_loop 2>&1 | tail -25
# Expected: compile error "no field `terminal_l1_insight` on type `InnerLoopResult`"
# AND: existing pin tests fail with off-by-one key counts
```

- [ ] **Step 8.4: Add the `InnerLoopResult` field + populate + bump payload**

In `core/src/scheduler/inner_loop.rs` find the `pub struct InnerLoopResult` definition and add the new field at the bottom:

```rust
    /// `l1_insight` from the terminal plan, captured only when the
    /// inner loop reaches `Outcome::Completed`. The lane runner reads
    /// this in `drain_lane` and writes one `actor='scheduler'
    /// action='l1.promoted'` audit row if `Some`. `None` on every
    /// other outcome (Failed / Cancelled — Refused / Blocked are
    /// also Failed-ish at the `final_state` level).
    pub terminal_l1_insight: Option<String>,
```

Find the construction site at the Completed arm (around line 207 / inside the `finish!` macro path) and capture the insight:

```rust
// At the top of run_to_terminal, before the loop, initialize:
let mut captured_l1_insight: Option<String> = None;

// Inside the loop, right after the reviewer's Approve/Advisory arm
// passes and before `if plan.is_terminal()` (around line 362), insert:
if let Some(insight) = plan.is_completion_with_insight() {
    captured_l1_insight = Some(insight.to_string());
}

// Then in the `finish!` macro / the existing InnerLoopResult { ... }
// construction site, add the field:
Ok(InnerLoopResult {
    outcome,
    plan_count,
    dispatch_count,
    terminal_l1_insight: captured_l1_insight.clone(),
})
```

(Look at the exact existing shape of `finish!` / the result-construction and slot the new field in. If the macro takes `outcome`, extend it to take an optional insight too — or restructure to a plain `Ok(...)` if cleaner.)

In `core/src/scheduler/runner.rs`, fix the `failed_result` helper:

```rust
fn failed_result(detail: String) -> InnerLoopResult {
    InnerLoopResult {
        outcome: Outcome::Failed(detail),
        plan_count: 0,
        dispatch_count: 0,
        terminal_l1_insight: None,
    }
}

#[cfg(test)]
pub(crate) fn failed_result_for_test(detail: &str) -> InnerLoopResult {
    failed_result(detail.to_string())
}
```

In `core/src/scheduler/inner_loop.rs`, update `build_plan_formulate_payload` to add the `l1_insight` key. Find the function (it's the one that builds the JSON object with `recalled_memory_ids` etc.) and add — adjacent to the existing `refused` key insertion (which already follows the "explicit null when absent" pattern):

```rust
// After the existing inserts for refused / recall keys, add:
obj.insert(
    "l1_insight".into(),
    match &plan.l1_insight {
        Some(s) => serde_json::Value::String(s.clone()),
        None => serde_json::Value::Null,
    },
);
```

- [ ] **Step 8.5: Run tests to verify pass**

```bash
cargo test -p hhagent-core scheduler::inner_loop 2>&1 | tail -20
# Expected: all prior tests pass + 3 new + bumped key-count tests now pass
```

```bash
cargo test --workspace 2>&1 | tail -10
# Expected: 0 failed across the workspace.
```

- [ ] **Step 8.6: Commit**

```bash
git add core/src/scheduler/inner_loop.rs core/src/scheduler/runner.rs
git commit -m "$(cat <<'EOF'
feat(core,scheduler,inner_loop): capture terminal_l1_insight + plan.formulate key

`InnerLoopResult` gains `terminal_l1_insight: Option<String>`. The
inner loop captures the value from `Plan::is_completion_with_insight()`
during the loop body (before the terminal check, so the value is
captured at exactly the iteration where the plan would produce
Outcome::Completed). The runner reads this in drain_lane (Task 9).

`build_plan_formulate_payload` adds an `l1_insight` key (Plan field
value, JSON-null when None) — pure-additive payload bump:
default/operator/agent_raised  20 -> 21 keys
cli_inferred                   21 -> 22 keys

The two `InnerLoopResult { ... }` literal sites are updated:
- inner_loop.rs::run_to_terminal — populates from captured value
- runner.rs::failed_result        — always None (pre-loop failures)

A new `#[cfg(test)] pub(crate) failed_result_for_test` helper in
runner.rs lets the inner_loop tests reach the failed_result shape
without dropping the encapsulation in production code.

+3 unit tests pin: payload carries the value when set; payload
carries explicit JSON-null when unset (so JSONB `?` queries find
the row); `InnerLoopResult.terminal_l1_insight` is None on
failed_result.

Bumped existing key-count pin tests by 1 in each affected source.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: `runner::drain_lane` agent-raised L1 promotion hook

**Files:**
- Modify: `core/src/scheduler/runner.rs` (hook after `write_finalize_row`)

**Context for the engineer:** Adds the production wire-in for agent-raised L1 promotion. Mirrors the `write_finalize_row` shape: a private `write_l1_promoted_row` helper that calls `promote_l1` + emits the audit row, best-effort posture (no error propagation; never aborts task finalize).

- [ ] **Step 9.1: Write a unit-level smoke test of `write_l1_promoted_row`**

Add to `core/src/scheduler/runner.rs` inside or below the existing test module (some files don't yet have `#[cfg(test)] mod tests`; create one if absent):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::l1_promote::{L1Source, L1WriteOutcome};

    // Pure / unit-tier smoke: the helper composes promote_l1 +
    // audit::insert; we only verify it doesn't panic with a
    // well-formed input. End-to-end DB-backed coverage lives in
    // memory_l1_promote_e2e.rs.

    #[tokio::test]
    async fn write_l1_promoted_row_handles_no_pool_gracefully() {
        // If no PG cluster is reachable, the helper logs and returns —
        // it does NOT panic and does NOT propagate the error to the
        // caller (it's best-effort, like write_finalize_row).
        let cluster = match hhagent_tests_common::bring_up_pg_cluster().await {
            Some(c) => c,
            None => { eprintln!("[SKIP] no PG"); return; }
        };
        let pool = cluster.runtime_pool().await.expect("pool");

        // Smoke: a single successful call should not panic.
        write_l1_promoted_row(&pool, 42, "learned X").await;

        // Confirm one L1 row + one audit row landed.
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM memories WHERE layer = 1",
        ).fetch_one(&pool).await.expect("count");
        assert_eq!(count, 1);
    }
}
```

- [ ] **Step 9.2: Run to verify failure**

```bash
cargo test -p hhagent-core scheduler::runner::tests 2>&1 | tail -10
# Expected: compile error "cannot find function `write_l1_promoted_row`"
```

- [ ] **Step 9.3: Implement the helper + wire into `drain_lane`**

Add to `core/src/scheduler/runner.rs` after `write_finalize_row` (around line 270):

```rust
/// Best-effort agent-raised L1 promotion writer. Called by
/// [`drain_lane`] after the `task.finalize` audit row is written.
///
/// Posture: errors are logged at WARN and swallowed. The task is
/// already finalized in the canonical `tasks` table; the L1 row +
/// audit row are observability aids, not correctness signals.
/// Validation errors from `promote_l1` are also swallowed (with
/// distinct WARN diagnostics so the operator can see which path failed).
async fn write_l1_promoted_row(pool: &PgPool, task_id: i64, insight: &str) {
    use crate::memory::l1_promote::{promote_l1, L1Error, L1Source};
    use crate::scheduler::audit::{build_l1_write_payload, ACTION_L1_PROMOTED};

    let source = L1Source::AgentRaised { task_id };
    let outcome = match promote_l1(pool, insight, source.clone()).await {
        Ok(o) => o,
        Err(L1Error::Validation(msg)) => {
            tracing::warn!(
                task_id, error = %msg,
                "agent-raised L1 promotion rejected on validation (skipping audit row)"
            );
            return;
        }
        Err(L1Error::Db(e)) => {
            tracing::warn!(
                task_id, error = %e,
                "agent-raised L1 promotion DB error (skipping audit row)"
            );
            return;
        }
    };

    let body_sha256 = crate::memory::l1_promote::compute_body_sha256(insight.trim());
    let payload = build_l1_write_payload(&outcome, &source, &body_sha256);

    if let Err(e) = hhagent_db::audit::insert(
        pool, SCHEDULER_AUDIT_ACTOR, ACTION_L1_PROMOTED, payload,
    ).await {
        tracing::warn!(
            task_id, error = %e,
            "audit insert for scheduler l1.promoted row failed (best-effort)"
        );
    }
}
```

Then in `drain_lane`, after the existing `write_finalize_row(pool, &claimed, final_state, &result, finished_at).await;` line (around line 211), append:

```rust
        // Agent-raised L1 promotion. Best-effort: a degraded write
        // never aborts task finalize. The terminal plan's `l1_insight`
        // is captured by the inner loop into `result.terminal_l1_insight`
        // only when Outcome::Completed; all other outcomes leave the
        // field None, so this branch is a no-op for them.
        if let Some(insight) = result.terminal_l1_insight.as_deref() {
            write_l1_promoted_row(pool, claimed.id, insight).await;
        }
```

- [ ] **Step 9.4: Run to verify pass**

```bash
cargo test -p hhagent-core scheduler::runner 2>&1 | tail -10
# Expected: smoke test passes (1 new test).

cargo test --workspace 2>&1 | tail -10
# Expected: 0 failed across the workspace.
```

- [ ] **Step 9.5: Commit**

```bash
git add core/src/scheduler/runner.rs
git commit -m "$(cat <<'EOF'
feat(core,scheduler,runner): agent-raised L1 promotion hook in drain_lane

Wires the agent-raised L1 path end-to-end. After write_finalize_row
emits its task.finalize summary, drain_lane checks
`result.terminal_l1_insight` and (when Some) calls a new
write_l1_promoted_row helper that:

  1. Constructs L1Source::AgentRaised { task_id: claimed.id }.
     Provenance is writer-decided; the inner-loop is the only legit
     constructor of this variant (mirrors issue #71 / PR #72 enum-
     binding discipline).
  2. Calls memory::l1_promote::promote_l1 which validates, SHA-256s,
     EXISTS-checks at layer=1, inserts on miss.
  3. Builds the payload via scheduler::audit::build_l1_write_payload
     (shared with the operator path).
  4. Inserts one `actor='scheduler' action='l1.promoted'` audit row.

Posture is best-effort throughout — same as write_finalize_row:
DB errors WARN and swallow. Validation errors from the agent's
l1_insight WARN with a distinct diagnostic and skip the audit row
(the row only fires on a successful or deduped write, not on a
rejected one).

The hook is a no-op when result.terminal_l1_insight is None, which
covers every non-Completed outcome AND every Completed plan where
the agent chose not to set l1_insight.

+1 inline smoke test confirms the helper composes promote_l1 +
audit::insert without panicking and that exactly one L1 row lands.
Full DB-backed coverage (operator + agent-raised happy paths, dedup
across sources, audit-row shape pinning) lives in
memory_l1_promote_e2e.rs (Task 12).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: `cli_audit` helpers `l1_add_and_audit` + `l1_remove_and_audit`

**Files:**
- Modify: `core/src/cli_audit.rs` (add 2 helpers + inline unit tests)

**Context for the engineer:** Mirrors the existing `tools_allowlist_add_and_audit` / `tools_allowlist_remove_and_audit` precedent. Each helper composes the underlying `memory::l1_promote::{promote_l1, remove_l1}` call with a best-effort audit-row insert.

- [ ] **Step 10.1: Write the failing tests**

Append to `core/src/cli_audit.rs` (find the existing `mod tests` and add):

```rust
    use crate::memory::l1_promote::{L1Source, L1WriteOutcome};
    use crate::scheduler::audit::{ACTION_L1_ADDED, ACTION_L1_REMOVED};

    #[tokio::test]
    async fn l1_add_and_audit_inserts_l1_row_and_writes_audit_row() {
        let cluster = match hhagent_tests_common::bring_up_pg_cluster().await {
            Some(c) => c,
            None => { eprintln!("[SKIP] no PG"); return; }
        };
        let pool = cluster.runtime_pool().await.expect("pool");

        let (outcome, audit_id) = l1_add_and_audit(&pool, "operator insight one")
            .await.expect("ok");
        assert!(matches!(outcome, L1WriteOutcome::Inserted { .. }));
        assert!(audit_id > 0);

        // Audit row landed at actor='cli' action='l1.added'.
        let rows = hhagent_db::audit::fetch_since(&pool, 0, 100).await.expect("fetch");
        let l1_added: Vec<_> = rows.iter()
            .filter(|r| r.actor == "cli" && r.action == ACTION_L1_ADDED)
            .collect();
        assert_eq!(l1_added.len(), 1, "exactly one l1.added row");
        let payload = l1_added[0].payload.as_object().expect("object");
        assert_eq!(payload.get("source").unwrap(), "operator");
        assert_eq!(payload.get("action").unwrap(), "inserted");
        assert!(payload.get("memory_id").is_some());
        assert!(payload.get("body_sha256").is_some());
        assert!(payload.get("task_id").is_none(), "operator must not carry task_id");
    }

    #[tokio::test]
    async fn l1_add_and_audit_audits_skipped_duplicate() {
        let cluster = match hhagent_tests_common::bring_up_pg_cluster().await {
            Some(c) => c,
            None => { eprintln!("[SKIP] no PG"); return; }
        };
        let pool = cluster.runtime_pool().await.expect("pool");

        l1_add_and_audit(&pool, "X").await.expect("first");
        let (outcome2, _) = l1_add_and_audit(&pool, "X").await.expect("second");
        assert!(matches!(outcome2, L1WriteOutcome::SkippedDuplicate { .. }));

        let rows = hhagent_db::audit::fetch_since(&pool, 0, 100).await.expect("fetch");
        let l1_added: Vec<_> = rows.iter()
            .filter(|r| r.actor == "cli" && r.action == ACTION_L1_ADDED)
            .collect();
        assert_eq!(l1_added.len(), 2, "two audit rows: one inserted, one skipped");
        let actions: Vec<_> = l1_added.iter()
            .map(|r| r.payload.get("action").unwrap().as_str().unwrap())
            .collect();
        assert!(actions.contains(&"inserted"));
        assert!(actions.contains(&"skipped_duplicate"));
    }

    #[tokio::test]
    async fn l1_add_and_audit_propagates_validation_error() {
        let cluster = match hhagent_tests_common::bring_up_pg_cluster().await {
            Some(c) => c,
            None => { eprintln!("[SKIP] no PG"); return; }
        };
        let pool = cluster.runtime_pool().await.expect("pool");

        let err = l1_add_and_audit(&pool, "has\nnewline").await.expect_err("invalid");
        assert!(matches!(err, crate::memory::l1_promote::L1Error::Validation(_)));

        // No L1 row written.
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM memories WHERE layer = 1",
        ).fetch_one(&pool).await.expect("count");
        assert_eq!(count, 0);

        // No audit row written (operator sees the validation error on stderr;
        // mirrors L0_seed's posture).
        let rows = hhagent_db::audit::fetch_since(&pool, 0, 100).await.expect("fetch");
        let l1_added: Vec<_> = rows.iter()
            .filter(|r| r.actor == "cli" && r.action == ACTION_L1_ADDED)
            .collect();
        assert_eq!(l1_added.len(), 0, "no audit row for validation rejection");
    }

    #[tokio::test]
    async fn l1_remove_and_audit_deletes_and_writes_audit() {
        let cluster = match hhagent_tests_common::bring_up_pg_cluster().await {
            Some(c) => c,
            None => { eprintln!("[SKIP] no PG"); return; }
        };
        let pool = cluster.runtime_pool().await.expect("pool");

        let (outcome, _) = l1_add_and_audit(&pool, "to-remove").await.expect("seed");
        let memory_id = outcome.memory_id();

        let (deleted, audit_id) = l1_remove_and_audit(&pool, memory_id).await.expect("remove");
        assert!(deleted);
        assert!(audit_id > 0);

        let rows = hhagent_db::audit::fetch_since(&pool, 0, 100).await.expect("fetch");
        let l1_removed: Vec<_> = rows.iter()
            .filter(|r| r.actor == "cli" && r.action == ACTION_L1_REMOVED)
            .collect();
        assert_eq!(l1_removed.len(), 1);
        let payload = l1_removed[0].payload.as_object().expect("object");
        assert_eq!(payload.get("memory_id").unwrap(), memory_id);
        assert_eq!(payload.get("deleted").unwrap(), true);
    }
```

- [ ] **Step 10.2: Run to verify failure**

```bash
cargo test -p hhagent-core cli_audit::tests::l1_ 2>&1 | tail -10
# Expected: "cannot find function `l1_add_and_audit`" / `l1_remove_and_audit`
```

- [ ] **Step 10.3: Implement the helpers**

Add to `core/src/cli_audit.rs` (after the existing `tools_allowlist_*_and_audit` helpers):

```rust
/// Compose `memory::l1_promote::promote_l1` with one `actor='cli'
/// action='l1.added'` audit row. The audit row IS written even on
/// `SkippedDuplicate` (records the operator intent); it is NOT
/// written on `L1Error::Validation` (operator sees the error on
/// stderr; mirrors `l0_seed`'s posture).
///
/// Returns the `L1WriteOutcome` and the audit row id (0 if the
/// audit insert failed; that's logged at WARN but doesn't propagate).
pub async fn l1_add_and_audit(
    pool: &sqlx::PgPool,
    body: &str,
) -> Result<(crate::memory::l1_promote::L1WriteOutcome, i64), crate::memory::l1_promote::L1Error> {
    use crate::memory::l1_promote::{compute_body_sha256, promote_l1, validate_l1_body, L1Source};
    use crate::scheduler::audit::{build_l1_write_payload, ACTION_L1_ADDED};

    // We need the trimmed body for body_sha256 in the audit payload,
    // but validate_l1_body returns a slice — re-running it here is
    // cheap and avoids capturing the borrow.
    let trimmed = validate_l1_body(body)?.to_string();
    let source = L1Source::Operator;
    let outcome = promote_l1(pool, &trimmed, source.clone()).await?;
    let body_sha256 = compute_body_sha256(&trimmed);

    let payload = build_l1_write_payload(&outcome, &source, &body_sha256);
    let audit_id = match hhagent_db::audit::insert(
        pool, CLI_AUDIT_ACTOR, ACTION_L1_ADDED, payload,
    ).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "l1.added audit insert failed (best-effort)");
            0
        }
    };

    Ok((outcome, audit_id))
}

/// Compose `memory::l1_promote::remove_l1` with one `actor='cli'
/// action='l1.removed'` audit row. Audit row is written even when
/// `deleted = false` (records the operator intent + the missing-id
/// outcome).
pub async fn l1_remove_and_audit(
    pool: &sqlx::PgPool,
    memory_id: i64,
) -> Result<(bool, i64), hhagent_db::DbError> {
    use crate::memory::l1_promote::remove_l1;
    use crate::scheduler::audit::ACTION_L1_REMOVED;

    let deleted = remove_l1(pool, memory_id).await?;
    let payload = serde_json::json!({"memory_id": memory_id, "deleted": deleted});

    let audit_id = match hhagent_db::audit::insert(
        pool, CLI_AUDIT_ACTOR, ACTION_L1_REMOVED, payload,
    ).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "l1.removed audit insert failed (best-effort)");
            0
        }
    };

    Ok((deleted, audit_id))
}
```

- [ ] **Step 10.4: Run to verify pass**

```bash
cargo test -p hhagent-core cli_audit 2>&1 | tail -15
# Expected: prior + 4 new
```

- [ ] **Step 10.5: Commit**

```bash
git add core/src/cli_audit.rs
git commit -m "$(cat <<'EOF'
feat(core,cli_audit): l1_add_and_audit + l1_remove_and_audit operator helpers

Two best-effort helpers wiring the operator CLI to the shared L1
writer. Each composes the underlying memory::l1_promote call with
one actor='cli' audit-row insert; both audit `Inserted` and
`SkippedDuplicate` outcomes from the underlying writer. Validation
errors propagate to the caller (the CLI prints the error on stderr;
no audit row — mirrors l0_seed's posture).

Wire shape:
- l1_add_and_audit: -> Result<(L1WriteOutcome, audit_id), L1Error>
- l1_remove_and_audit: -> Result<(bool deleted, audit_id), DbError>

Both call hhagent_db::audit::insert with actor=CLI_AUDIT_ACTOR and
action=ACTION_L1_ADDED/ACTION_L1_REMOVED. Audit insert failures are
logged at WARN and degrade to audit_id = 0 in the return — they do
NOT propagate, matching the chokepoint posture (the underlying
domain operation already succeeded; observability is a separate
concern).

+4 inline async tests pin: insert -> 1 row + 1 audit row;
duplicate insert -> 1 row + 2 audit rows (inserted + skipped_duplicate);
validation error -> 0 rows + 0 audit rows; remove path -> deletion
+ audit row carrying {memory_id, deleted=true}.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: `hhagent-cli memory l1 {add, list, remove}` subcommand tree

**Files:**
- Modify: `core/src/main.rs` (hand-rolled subcommand parsing)

**Context for the engineer:** The CLI uses hand-rolled subcommand parsing — search for the existing `tools allowlist` subcommand wiring and follow the same shape. The hierarchy is `hhagent-cli memory l1 {add, list, remove}` — three new leaf commands under a new `memory` group with a single `l1` subgroup for now (future `l0` / `l3` / `l4` subgroups would mirror this).

- [ ] **Step 11.1: Locate the existing subcommand wiring**

```bash
grep -n "fn cli_main\|\"tools\"\|\"tasks\"\|\"allowlist\"" core/src/main.rs | head -20
```

Read the function that dispatches subcommands (probably called `cli_main` or `dispatch_cli`) and find the closest precedent — `tools allowlist {add, remove, list}` is the right one.

- [ ] **Step 11.2: Add the `memory` subcommand branch**

In `core/src/main.rs`, add a new branch alongside the existing `"tools"` branch:

```rust
        "memory" => {
            // hhagent-cli memory l1 {add, list, remove}
            let (group, rest) = rest.split_first()
                .ok_or_else(|| anyhow::anyhow!("memory: missing subgroup (l1)"))?;
            match group.as_str() {
                "l1" => dispatch_memory_l1(rest).await,
                other => Err(anyhow::anyhow!("memory: unknown subgroup '{other}'; expected: l1")),
            }
        }
```

Then add the `dispatch_memory_l1` function (placed adjacent to `dispatch_tools_allowlist` or equivalent):

```rust
async fn dispatch_memory_l1(args: &[String]) -> anyhow::Result<()> {
    use hhagent_core::cli_audit::{l1_add_and_audit, l1_remove_and_audit};
    use hhagent_core::memory::l1_promote::list_l1;
    use hhagent_db::pool::connect_runtime_pool;

    let (action, rest) = args.split_first()
        .ok_or_else(|| anyhow::anyhow!("memory l1: missing action (add | list | remove)"))?;

    let spec = hhagent_db::conn::ConnectSpec::from_env()?;
    let pool = connect_runtime_pool(&spec).await?;

    match action.as_str() {
        "add" => {
            let body = rest.first()
                .ok_or_else(|| anyhow::anyhow!("memory l1 add: missing <body>"))?;
            match l1_add_and_audit(&pool, body).await {
                Ok((outcome, _audit_id)) => {
                    use hhagent_core::memory::l1_promote::L1WriteOutcome;
                    match outcome {
                        L1WriteOutcome::Inserted { memory_id } => {
                            println!("inserted id={memory_id}");
                        }
                        L1WriteOutcome::SkippedDuplicate { memory_id } => {
                            println!("skipped_duplicate id={memory_id} (body_sha256 already at layer 1)");
                        }
                    }
                    Ok(())
                }
                Err(e) => Err(anyhow::anyhow!("memory l1 add: {e}")),
            }
        }
        "list" => {
            let all = rest.iter().any(|s| s == "--all");
            let rows = list_l1(&pool, all).await?;
            // Tab-separated columns: id, created_at, body
            println!("id\tcreated_at\tbody");
            for r in rows {
                let created = r.created_at
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_else(|_| "?".into());
                println!("{}\t{}\t{}", r.id, created, r.body);
            }
            Ok(())
        }
        "remove" => {
            let id_str = rest.first()
                .ok_or_else(|| anyhow::anyhow!("memory l1 remove: missing <id>"))?;
            let id: i64 = id_str.parse()
                .map_err(|e| anyhow::anyhow!("memory l1 remove: invalid id '{id_str}': {e}"))?;
            let (deleted, _audit_id) = l1_remove_and_audit(&pool, id).await?;
            if deleted {
                println!("removed id={id}");
            } else {
                println!("no row at layer 1 with id={id} (already gone or wrong layer)");
            }
            Ok(())
        }
        other => Err(anyhow::anyhow!("memory l1: unknown action '{other}'; expected: add | list | remove")),
    }
}
```

(Adapt argument-parsing shape to match the existing `dispatch_tools_allowlist` style verbatim — flag conventions, error message phrasing, etc.)

- [ ] **Step 11.3: Verify build + manual smoke**

```bash
cargo build --workspace 2>&1 | tail -5
# Expected: no errors

# Manual smoke (skip if no PG available):
./target/debug/hhagent-cli memory l1 add 2>&1 | head -3
# Expected: "memory l1 add: missing <body>" error
./target/debug/hhagent-cli memory 2>&1 | head -3
# Expected: "memory: missing subgroup (l1)" error
./target/debug/hhagent-cli memory l1 bogus 2>&1 | head -3
# Expected: "memory l1: unknown action 'bogus'; expected: add | list | remove"
```

(Skip the live "add then list then remove" smoke here — Task 13's `cli_memory_l1_e2e.rs` covers it deterministically with a per-test PG cluster.)

- [ ] **Step 11.4: Commit**

```bash
git add core/src/main.rs
git commit -m "$(cat <<'EOF'
feat(core,cli): hhagent-cli memory l1 {add,list,remove} subcommand tree

Hand-rolled subcommand parsing (no clap dep) matching the existing
`tools allowlist` precedent. Three leaf commands under a new `memory
l1` group:

  hhagent-cli memory l1 add <body>      -> l1_add_and_audit
  hhagent-cli memory l1 list [--all]    -> list_l1 (in-prompt or all)
  hhagent-cli memory l1 remove <id>     -> l1_remove_and_audit

The `memory` group is scaffolded with a single `l1` subgroup for now;
future `l0` / `l3` / `l4` operator surfaces would slot in alongside
(L0 already has a startup-time seeding loader so the natural addition
there is a `list` subcommand against `memories WHERE layer = 0`).

CLI output is the existing tab-separated table shape (id\tcreated_at\tbody)
used by `tasks list` and `tools allowlist list`. No clap dep.

Live E2E test (subprocess spawning, real CLI binary, per-test PG
cluster) lands in Task 13's cli_memory_l1_e2e.rs.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 12: `core/tests/memory_l1_promote_e2e.rs` DB integration tests

**Files:**
- Create: `core/tests/memory_l1_promote_e2e.rs`

**Context for the engineer:** Cross-platform DB-backed integration coverage. Mirrors the shape of `core/tests/memory_l0_seed_e2e.rs` — per-test PG cluster from `tests-common`, skip-as-pass on no-PG, real probe + runtime-role pool. The agent-raised scenario uses a scripted `RouterAgent` mock (FIFO LLM responses) and verifies that the L1 row + `l1.promoted` audit row land in the right shape.

- [ ] **Step 12.1: Write the integration test file**

Create `core/tests/memory_l1_promote_e2e.rs`:

```rust
//! DB-integration coverage for the L1 promotion writer.
//! Operator path + agent-raised path (via scripted RouterAgent mock).
//! Per-test PG cluster from `hhagent-tests-common`; skips cleanly without PG.

use hhagent_core::cli_audit::{l1_add_and_audit, l1_remove_and_audit};
use hhagent_core::memory::l1_promote::{
    list_l1, promote_l1, L1Error, L1Source, L1WriteOutcome,
};
use hhagent_core::scheduler::audit::{
    ACTION_L1_ADDED, ACTION_L1_PROMOTED, ACTION_L1_REMOVED,
};
use hhagent_db::memories::MemoryLayer;
use hhagent_tests_common::bring_up_pg_cluster;

macro_rules! skip_if_no_pg {
    () => {
        match bring_up_pg_cluster().await {
            Some(c) => c,
            None => {
                eprintln!("[SKIP] no PG");
                return;
            }
        }
    };
}

#[tokio::test]
async fn operator_add_writes_l1_row_and_audit_row() {
    let cluster = skip_if_no_pg!();
    let pool = cluster.runtime_pool().await.expect("pool");

    let (outcome, audit_id) = l1_add_and_audit(&pool, "operator insight one")
        .await.expect("ok");
    assert!(matches!(outcome, L1WriteOutcome::Inserted { .. }));
    assert!(audit_id > 0);

    // 1 row at layer = 1.
    let rows = list_l1(&pool, true).await.expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].body, "operator insight one");
    assert_eq!(rows[0].layer, MemoryLayer::Index);

    // 1 audit row with the canonical operator payload.
    let audit_rows = hhagent_db::audit::fetch_since(&pool, 0, 100).await.expect("audit");
    let l1_added: Vec<_> = audit_rows.iter()
        .filter(|r| r.actor == "cli" && r.action == ACTION_L1_ADDED)
        .collect();
    assert_eq!(l1_added.len(), 1);
    let payload = l1_added[0].payload.as_object().expect("object");
    assert_eq!(payload.get("source").unwrap(), "operator");
    assert_eq!(payload.get("action").unwrap(), "inserted");
    assert!(payload.get("task_id").is_none());
    let key_set: std::collections::BTreeSet<&String> = payload.keys().collect();
    let expected: std::collections::BTreeSet<&String> = ["action", "body_sha256", "memory_id", "source"]
        .iter().map(|s| s.to_string()).collect::<Vec<_>>().iter().collect();
    // The above expected-set construction is awkward in Rust; simpler:
    let actual_keys: std::collections::BTreeSet<String> =
        payload.keys().cloned().collect();
    let expected_keys: std::collections::BTreeSet<String> =
        ["action", "body_sha256", "memory_id", "source"].iter().map(|s| s.to_string()).collect();
    assert_eq!(actual_keys, expected_keys, "operator payload key-set");
    let _ = (key_set, expected); // suppress unused
}

#[tokio::test]
async fn operator_add_is_idempotent_on_body_sha256() {
    let cluster = skip_if_no_pg!();
    let pool = cluster.runtime_pool().await.expect("pool");

    l1_add_and_audit(&pool, "X").await.expect("first");
    let (outcome2, _) = l1_add_and_audit(&pool, "X").await.expect("second");
    assert!(matches!(outcome2, L1WriteOutcome::SkippedDuplicate { .. }));

    let rows = list_l1(&pool, true).await.expect("list");
    assert_eq!(rows.len(), 1, "dedup must collapse to one row");

    // Two audit rows: one inserted + one skipped_duplicate.
    let audit_rows = hhagent_db::audit::fetch_since(&pool, 0, 100).await.expect("audit");
    let l1_added: Vec<_> = audit_rows.iter()
        .filter(|r| r.actor == "cli" && r.action == ACTION_L1_ADDED)
        .collect();
    assert_eq!(l1_added.len(), 2);
}

#[tokio::test]
async fn operator_add_rejects_invalid_body_with_no_audit_row() {
    let cluster = skip_if_no_pg!();
    let pool = cluster.runtime_pool().await.expect("pool");

    for bad in &["", "  ", "has\nnewline", "</l1_insights>"] {
        let err = l1_add_and_audit(&pool, bad).await.expect_err(bad);
        assert!(matches!(err, L1Error::Validation(_)));
    }

    // No L1 rows.
    let rows = list_l1(&pool, true).await.expect("list");
    assert_eq!(rows.len(), 0);

    // No audit rows.
    let audit_rows = hhagent_db::audit::fetch_since(&pool, 0, 100).await.expect("audit");
    let l1_added: Vec<_> = audit_rows.iter()
        .filter(|r| r.actor == "cli" && r.action == ACTION_L1_ADDED)
        .collect();
    assert_eq!(l1_added.len(), 0, "validation rejection writes no audit row");
}

#[tokio::test]
async fn operator_remove_deletes_and_audits() {
    let cluster = skip_if_no_pg!();
    let pool = cluster.runtime_pool().await.expect("pool");

    let (outcome, _) = l1_add_and_audit(&pool, "to-remove").await.expect("seed");
    let id = outcome.memory_id();

    let (deleted, audit_id) = l1_remove_and_audit(&pool, id).await.expect("remove");
    assert!(deleted);
    assert!(audit_id > 0);

    // L1 row is gone.
    let rows = list_l1(&pool, true).await.expect("list");
    assert!(rows.iter().find(|r| r.id == id).is_none());

    // Audit row carries memory_id + deleted=true.
    let audit_rows = hhagent_db::audit::fetch_since(&pool, 0, 100).await.expect("audit");
    let l1_removed: Vec<_> = audit_rows.iter()
        .filter(|r| r.actor == "cli" && r.action == ACTION_L1_REMOVED)
        .collect();
    assert_eq!(l1_removed.len(), 1);
    assert_eq!(l1_removed[0].payload.get("memory_id").unwrap(), id);
    assert_eq!(l1_removed[0].payload.get("deleted").unwrap(), true);
}

#[tokio::test]
async fn operator_remove_refuses_wrong_layer() {
    let cluster = skip_if_no_pg!();
    let pool = cluster.runtime_pool().await.expect("pool");

    let stable_id = hhagent_db::memories::insert_memory(
        &pool, "stable-row", serde_json::json!({}), None,
    ).await.expect("insert");

    let (deleted, _) = l1_remove_and_audit(&pool, stable_id).await.expect("remove");
    assert!(!deleted, "wrong-layer guard must reject");

    // L2 row survives.
    let surviving = hhagent_db::memories::fetch_by_ids(&pool, &[stable_id]).await.expect("fetch");
    assert_eq!(surviving.len(), 1);
    assert_eq!(surviving[0].body, "stable-row");

    // Audit row still written (records the operator intent + the false outcome).
    let audit_rows = hhagent_db::audit::fetch_since(&pool, 0, 100).await.expect("audit");
    let l1_removed: Vec<_> = audit_rows.iter()
        .filter(|r| r.actor == "cli" && r.action == ACTION_L1_REMOVED)
        .collect();
    assert_eq!(l1_removed.len(), 1);
    assert_eq!(l1_removed[0].payload.get("deleted").unwrap(), false);
}

#[tokio::test]
async fn agent_raised_promote_l1_writes_l1_row_with_task_id_metadata() {
    // Direct unit-style verification of the agent-raised path
    // (drain_lane's end-to-end integration with a scripted RouterAgent
    // mock lives in scheduler_inner_loop_e2e.rs — see Task 14).
    let cluster = skip_if_no_pg!();
    let pool = cluster.runtime_pool().await.expect("pool");

    let outcome = promote_l1(
        &pool,
        "shell-exec /bin/echo works for short string returns",
        L1Source::AgentRaised { task_id: 17 },
    ).await.expect("ok");
    assert!(matches!(outcome, L1WriteOutcome::Inserted { .. }));

    let rows = list_l1(&pool, true).await.expect("list");
    assert_eq!(rows.len(), 1);
    let metadata = rows[0].metadata.as_object().expect("metadata is object");
    assert_eq!(metadata.get("source").unwrap(), "agent_raised");
    assert_eq!(metadata.get("task_id").unwrap(), 17);
}

#[tokio::test]
async fn agent_raised_promote_dedups_against_operator_row() {
    let cluster = skip_if_no_pg!();
    let pool = cluster.runtime_pool().await.expect("pool");

    let (op_outcome, _) = l1_add_and_audit(&pool, "shared insight").await.expect("op");
    let op_id = op_outcome.memory_id();

    let ag_outcome = promote_l1(
        &pool,
        "shared insight",
        L1Source::AgentRaised { task_id: 99 },
    ).await.expect("ag");
    match ag_outcome {
        L1WriteOutcome::SkippedDuplicate { memory_id } => assert_eq!(memory_id, op_id),
        other => panic!("expected SkippedDuplicate, got {other:?}"),
    }

    // The L1 row's metadata still reflects the FIRST writer's source
    // (the operator); the agent-raised metadata is not stamped on the
    // existing row.
    let rows = list_l1(&pool, true).await.expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].metadata.get("source").unwrap(), "operator");
}

#[tokio::test]
async fn list_l1_in_prompt_vs_all_distinguishes_at_cap_boundary() {
    let cluster = skip_if_no_pg!();
    let pool = cluster.runtime_pool().await.expect("pool");

    for i in 0..40 {
        promote_l1(&pool, &format!("body-{i:02}"), L1Source::Operator)
            .await.expect("seed");
    }

    let in_prompt = list_l1(&pool, false).await.expect("in-prompt");
    let everything = list_l1(&pool, true).await.expect("all");

    assert!(in_prompt.len() <= 32, "in-prompt respects 32-row cap");
    assert_eq!(everything.len(), 40, "all returns every row");
}
```

- [ ] **Step 12.2: Run the file**

```bash
cargo test -p hhagent-core --test memory_l1_promote_e2e 2>&1 | tail -15
# Expected: "8 passed; 0 failed" (skips on no-PG with [SKIP] lines)
```

- [ ] **Step 12.3: Commit**

```bash
git add core/tests/memory_l1_promote_e2e.rs
git commit -m "$(cat <<'EOF'
test(core,memory,l1_promote): DB integration coverage for operator + agent paths

8 scenarios pinning the L1 writer end-to-end against a per-test PG
cluster (cross-platform skip-as-pass on no-PG):

  operator_add_writes_l1_row_and_audit_row
  operator_add_is_idempotent_on_body_sha256
  operator_add_rejects_invalid_body_with_no_audit_row
  operator_remove_deletes_and_audits
  operator_remove_refuses_wrong_layer
  agent_raised_promote_l1_writes_l1_row_with_task_id_metadata
  agent_raised_promote_dedups_against_operator_row
  list_l1_in_prompt_vs_all_distinguishes_at_cap_boundary

Verifies the L1WriteOutcome variants land correctly, the audit-row
payload key-set matches the contract (operator: 4 keys / AgentRaised: 5
keys), validation errors write no audit row (mirrors L0 posture),
the wrong-layer guard on remove rejects non-L1 ids without touching
them, and the cross-source dedup leaves the FIRST writer's source
intact in the row metadata.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 13: `core/tests/cli_memory_l1_e2e.rs` CLI subprocess integration

**Files:**
- Create: `core/tests/cli_memory_l1_e2e.rs`

**Context for the engineer:** Spawns the real `hhagent-cli` binary as a subprocess, points it at a per-test PG cluster, exercises the three subcommands end-to-end. Mirrors the shape of any existing `core/tests/cli_*_e2e.rs` file — find one (e.g. `cli_cancel_audit_e2e.rs`) and follow it.

- [ ] **Step 13.1: Find the closest CLI subprocess precedent**

```bash
ls core/tests/cli_*_e2e.rs
# Read one of them (probably cli_cancel_audit_e2e.rs or cli_submit_audit_e2e.rs)
# to see the subprocess-spawn idiom + env-var injection shape.
```

- [ ] **Step 13.2: Write the test file**

Create `core/tests/cli_memory_l1_e2e.rs` modeled on the precedent above. Three test functions:

```rust
//! Subprocess-level integration for `hhagent-cli memory l1 {add,list,remove}`.
//! Spawns the real CLI binary, points it at a per-test PG cluster,
//! verifies stdout shape + audit-row landing. Per-test PG cluster
//! from hhagent-tests-common; skip-as-pass on no-PG.

use hhagent_tests_common::{bring_up_pg_cluster, workspace_target_binary};
use std::process::Command;

macro_rules! skip_if_no_pg {
    () => {
        match bring_up_pg_cluster().await {
            Some(c) => c,
            None => {
                eprintln!("[SKIP] no PG");
                return;
            }
        }
    };
}

#[tokio::test]
async fn cli_memory_l1_add_writes_row_and_audit() {
    let cluster = skip_if_no_pg!();
    let pool = cluster.runtime_pool().await.expect("pool");

    let cli = workspace_target_binary("hhagent-cli");
    let env_vars = cluster.cli_env_vars();  // adapt to whatever the precedent uses

    let output = Command::new(&cli)
        .args(&["memory", "l1", "add", "shell-exec /bin/ls works"])
        .envs(env_vars.iter().cloned())
        .output()
        .expect("spawn");
    assert!(output.status.success(),
        "exit={:?}\nstderr={}", output.status.code(),
        String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("inserted id="), "stdout: {stdout}");

    // Audit row landed.
    let rows = hhagent_db::audit::fetch_since(&pool, 0, 100).await.expect("audit");
    let added: Vec<_> = rows.iter()
        .filter(|r| r.actor == "cli" && r.action == "l1.added")
        .collect();
    assert_eq!(added.len(), 1);
}

#[tokio::test]
async fn cli_memory_l1_list_shows_added_rows() {
    let cluster = skip_if_no_pg!();
    let _pool = cluster.runtime_pool().await.expect("pool");
    let cli = workspace_target_binary("hhagent-cli");
    let env_vars = cluster.cli_env_vars();

    // Add 3 rows via CLI.
    for body in &["alpha", "beta", "gamma"] {
        let st = Command::new(&cli)
            .args(&["memory", "l1", "add", body])
            .envs(env_vars.iter().cloned())
            .status()
            .expect("spawn");
        assert!(st.success());
    }

    let output = Command::new(&cli)
        .args(&["memory", "l1", "list"])
        .envs(env_vars.iter().cloned())
        .output()
        .expect("spawn");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("id\tcreated_at\tbody"));
    for body in &["alpha", "beta", "gamma"] {
        assert!(stdout.contains(body), "list output missing '{body}': {stdout}");
    }
}

#[tokio::test]
async fn cli_memory_l1_remove_deletes_specified_id() {
    let cluster = skip_if_no_pg!();
    let pool = cluster.runtime_pool().await.expect("pool");
    let cli = workspace_target_binary("hhagent-cli");
    let env_vars = cluster.cli_env_vars();

    // Add a row, parse its id from CLI stdout.
    let output = Command::new(&cli)
        .args(&["memory", "l1", "add", "to-remove"])
        .envs(env_vars.iter().cloned())
        .output()
        .expect("spawn");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Expected: "inserted id=N"
    let id: i64 = stdout.trim()
        .strip_prefix("inserted id=")
        .expect("inserted id=N format")
        .parse()
        .expect("parse id");

    // Now remove it.
    let out2 = Command::new(&cli)
        .args(&["memory", "l1", "remove", &id.to_string()])
        .envs(env_vars.iter().cloned())
        .output()
        .expect("spawn");
    assert!(out2.status.success());
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(stdout2.contains(&format!("removed id={id}")), "stdout: {stdout2}");

    // Verify row is gone.
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memories WHERE layer = 1",
    ).fetch_one(&pool).await.expect("count");
    assert_eq!(count, 0);
}
```

(Adapt to the exact env-var injection shape from `cli_cancel_audit_e2e.rs` — `cluster.cli_env_vars()` may not exist verbatim; use whatever the precedent does for setting `HHAGENT_DB_*` connection vars.)

- [ ] **Step 13.3: Run**

```bash
cargo test -p hhagent-core --test cli_memory_l1_e2e 2>&1 | tail -15
# Expected: "3 passed; 0 failed" or skip-as-pass on no-PG.
```

- [ ] **Step 13.4: Commit**

```bash
git add core/tests/cli_memory_l1_e2e.rs
git commit -m "$(cat <<'EOF'
test(core,cli,memory_l1): CLI subprocess integration for add/list/remove

3 scenarios spawning the real `hhagent-cli` binary against a per-test
PG cluster (cross-platform skip-as-pass on no-PG):

  cli_memory_l1_add_writes_row_and_audit
  cli_memory_l1_list_shows_added_rows
  cli_memory_l1_remove_deletes_specified_id

Verifies the hand-rolled subcommand parsing, the
hhagent-core::cli_audit wire-in, and the stdout format ("inserted id=N",
"removed id=N", "id\\tcreated_at\\tbody" table header).

The CLI process integration is intentionally lighter than the
DB-tier Task 12 — the heavy contract pinning lives there; this
file's job is to catch regressions in the subcommand routing and
stdout shape.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 14: `scheduler_inner_loop_e2e.rs` audit-payload pin update

**Files:**
- Modify: `core/tests/scheduler_inner_loop_e2e.rs` (extend the happy-path audit-gate to assert the `l1_insight` key)

**Context for the engineer:** The recall-lane wiring slice (PR #79) added mid-tier audit assertions on the happy-path completed-task scenario. This task extends those assertions to cover the new `l1_insight` key. No new `#[test]` functions; in-place expansion only.

- [ ] **Step 14.1: Find the happy-path audit-gate block**

```bash
grep -n "plan.formulate\|recall_count\|system_prompt_sha256" core/tests/scheduler_inner_loop_e2e.rs | head -20
```

The recall-lane slice added 4 assertions on `recalled_memory_ids`, `recall_count`, `recall_query_sha256`. Add 1-2 more for `l1_insight`.

- [ ] **Step 14.2: Add the new assertions**

In the happy-path scenario, after the existing recall-key assertions, append (adapt to the exact local variable / row reference):

```rust
        // l1_insight key: ScriptedFormulator produces a Plan without l1_insight,
        // so the payload key MUST be present-and-null (JSONB ? operator finds it).
        assert!(
            payload.get("l1_insight").is_some(),
            "plan.formulate payload must include l1_insight key (got keys: {:?})",
            payload.keys().collect::<Vec<_>>()
        );
        assert!(
            payload.get("l1_insight").unwrap().is_null(),
            "ScriptedFormulator emits no l1_insight; payload should be JSON null"
        );
```

If the scenario has a path where the agent DOES set `l1_insight`, also add a string-equality assertion against the value. (Otherwise, defer the value-equality pin to Task 12's DB integration tests where it's already covered.)

- [ ] **Step 14.3: Run**

```bash
cargo test -p hhagent-core --test scheduler_inner_loop_e2e 2>&1 | tail -10
# Expected: prior + new assertions pass
```

- [ ] **Step 14.4: Commit**

```bash
git add core/tests/scheduler_inner_loop_e2e.rs
git commit -m "$(cat <<'EOF'
test(core,scheduler,inner_loop_e2e): pin l1_insight key in plan.formulate payload

Extends the recall-lane-wiring slice's mid-tier audit gate (PR #79)
with assertions on the new l1_insight payload key. Verifies the key
is present-and-null when the ScriptedFormulator emits a Plan without
l1_insight, so a future regression that silently drops the key
(making `payload ? 'l1_insight'` SQL queries miss the row) is caught
at the full-stack tier.

In-place assertion expansion only; no new `#[test]` functions.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 15: HANDOVER.md + ROADMAP.md session-end update

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

**Context for the engineer:** Per CLAUDE.md rule #8, update HANDOVER.md + ROADMAP.md to reflect the completed slice. Move the "L1 promotion writer" entry from "Next TODO" / "Open follow-up surfaces" to "Recently completed (this session)".

- [ ] **Step 15.1: Update HANDOVER.md header + Recently completed entry**

In `docs/devel/handovers/HANDOVER.md`:
- Bump `Last updated:` and `Last commit (branch HEAD):` headers
- Bump the `cargo test --workspace` count line (target ~702-709)
- Add a new "Recently completed (this session, 2026-05-17 — L1 promotion writer, branch `feat/l1-promotion-writer`)" section with the headline shape used by prior session entries (the recall-lane-wiring section is the immediate precedent)
- Move the "L1 promotion writer" item in "Open follow-up surfaces" / "Next TODO" to a checked-off / done state

- [ ] **Step 15.2: Update ROADMAP.md**

In `docs/devel/ROADMAP.md`, find the existing `- [ ] **L1 promotion writer**` line in Phase 1 and flip it to `- [x] **L1 promotion writer**` with the full breakdown of what shipped + commit hash range + test-count delta.

The "L3 skill crystallisation" line above it should remove the "depends on L1 promotion writer landing first" pre-req note since L1 is now landed.

- [ ] **Step 15.3: Run the full test suite one final time as the verification step**

```bash
cargo test --workspace 2>&1 | tail -10
# Expected: 0 failed, 4 ignored (the same 4 doctests as today),
# total tests ~ 702-709.
```

- [ ] **Step 15.4: Commit**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "$(cat <<'EOF'
docs(handover,roadmap): L1 promotion writer shipped

Branch `feat/l1-promotion-writer` lands the first writer for
MemoryLayer::Index rows. Hybrid design (operator-explicit CLI
+ agent-raised channel via Plan.l1_insight), shared validator + dedup
helper, three new audit-row actions, one pure-additive payload bump
on agent/plan.formulate (20/21 -> 21/22 keys).

Test count: 674 -> ~702-709 (~+28-35: 12 validator/helper unit, 4
build_l1_write_payload unit, 4 cassandra Plan-shape unit, 5
promote_l1 async unit, 4 list_l1/remove_l1 async unit, 4 cli_audit
helper async unit, 1 runner smoke unit, 8 memory_l1_promote_e2e,
3 cli_memory_l1_e2e, 2 scheduler_inner_loop_e2e in-place assertions).

Spec: docs/superpowers/specs/2026-05-17-l1-promotion-writer-design.md
Plan: docs/superpowers/plans/2026-05-17-l1-promotion-writer.md

Closes the "L1 promotion writer" pickup from PR #79's "Open follow-up
surfaces" list. Unblocks L3 skill crystallisation as the next natural
slice (the L1 distillation pattern here sets the precedent).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-review (done before sharing the plan)

- **Spec coverage:** Every section of the spec maps to a task:
  - "Wire shape (5 NEW + 4 modified)" → Tasks 3, 4, 5, 12, 13 (NEW) + Tasks 1, 2, 6, 7, 8, 9, 10, 11 (MODIFIED). Covered.
  - "Validation rules for L1 body" → Task 3 (validator unit tests).
  - "Dedup behaviour" → Task 4 (`promote_l1` EXISTS-check) + Task 12 (cross-source dedup integration).
  - "Agent-raised provenance enforcement" → Task 9 (drain_lane is the only L1Source::AgentRaised constructor).
  - "Emit gate" → Task 8 (`Plan::is_completion_with_insight` + `terminal_l1_insight` capture).
  - "Data flow" → end-to-end coverage in Task 12.
  - "Audit-row contract" → Task 6 (`build_l1_write_payload`) + Tasks 8, 10 (consumers).
  - "Test budget +28 to +35" → ~+28-35 budgeted across Tasks 1-14.
- **Placeholder scan:** Every code step contains the actual code. No "TBD" / "TODO" / "fill in later" remain. The `cluster.cli_env_vars()` shape in Task 13 is flagged as "adapt to precedent" because the existing CLI E2E tests don't have a uniform helper method on `tests-common` — the engineer needs to look at the prior file. This is acceptable because (a) the precedent is exactly one grep away and (b) the env-var injection shape is non-trivial to bake into this plan without reading the prior file's exact env names. Reasonable trade.
- **Type consistency:**
  - `L1Source { Operator, AgentRaised { task_id } }` — used identically across Tasks 3, 4, 6, 9, 10, 12.
  - `L1WriteOutcome { Inserted { memory_id }, SkippedDuplicate { memory_id } }` — used identically across Tasks 3, 4, 6, 9, 10, 11, 12, 13.
  - `terminal_l1_insight: Option<String>` field on `InnerLoopResult` — used in Task 8 (defined), Task 9 (read).
  - `l1_insight: Option<String>` field on `Plan` — used in Task 2 (defined), Task 8 (read for payload + accessor).
  - Audit action constants `ACTION_L1_ADDED`, `ACTION_L1_REMOVED`, `ACTION_L1_PROMOTED` — defined Task 6, used Tasks 9, 10, 12, 13, 14.
  - Function names: `promote_l1`, `list_l1`, `remove_l1`, `l1_add_and_audit`, `l1_remove_and_audit`, `write_l1_promoted_row`, `build_l1_write_payload` — consistent across all task references.
- **Scope check:** 15 tasks fitting within one working session. Each task is bite-sized (1-5 minutes per step, 4-7 steps per task, one commit per task). The Plan-field cascade (Task 2) is the only "wide" task at 9 file edits, but each edit is mechanical (`l1_insight: None,` literal).
