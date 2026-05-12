# `task.submitted` producer audit row — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire a producer-side `actor='cli' action='task.submitted'` audit row from `hhagent-cli ask`, mirroring the cancel slice (PR #43) so the audit-log lifecycle stream covers submit → claim → terminal in one query.

**Architecture:** New constant `ACTION_TASK_SUBMITTED` in `core::scheduler::audit`; new helper `cli_audit::submit_and_audit(pool, lane, payload) -> Result<i64, DbError>` mirroring `cancel_and_audit`'s chokepoint posture; one-line swap at `hhagent-cli ask`'s sole `insert_pending` call site.

**Tech Stack:** Rust + sqlx + tokio + per-test Postgres clusters via `hhagent-tests-common`.

**Spec:** `docs/superpowers/specs/2026-05-13-cli-task-submitted-audit-design.md`

**Branch (already created):** `feat/cli-task-submitted-audit` off `main` at `fdf1a52`. Spec committed at `d9f1920`.

**TDD discipline:** Per CLAUDE.md rule 2, tests are written first; per rule 6, all tests must be green before each commit. Per rule 4, no file should exceed the 500-LOC soft cap (existing `cli_audit.rs` is 147 LOC, will grow to ~210; `scheduler/audit.rs` is ~370 LOC, will grow by ~10 lines).

---

### Task 1: Add `ACTION_TASK_SUBMITTED` constant to `core::scheduler::audit`

**Files:**
- Modify: `core/src/scheduler/audit.rs` (between `ACTION_TASK_FINALIZE` and `ACTION_TASK_PREFIX`, lines 88–93)

- [ ] **Step 1: Read the existing constant block to confirm insertion point**

Run: `grep -n "ACTION_TASK_\|action_task_terminal" core/src/scheduler/audit.rs | head -20`

Expected: shows the const block at lines 78–93. `ACTION_TASK_SUBMITTED` will land between `ACTION_TASK_FINALIZE` (line 88) and `ACTION_TASK_PREFIX` (line 93) for alphabetical order (RUNNING → FINALIZE → SUBMITTED → PREFIX is the existing order; SUBMITTED slots in before PREFIX since PREFIX is the family separator and the most generic).

- [ ] **Step 2: Insert the constant**

Edit `core/src/scheduler/audit.rs`, after the `ACTION_TASK_FINALIZE` block (line 88), before the `ACTION_TASK_PREFIX` block (line 93):

```rust
/// `action` value for the producer-side row written by `hhagent-cli ask`
/// after `tasks::insert_pending` succeeds. Distinct from the scheduler's
/// own `task.running` row that fires later on claim — paired with
/// [`crate::cli_audit::CLI_AUDIT_ACTOR`] so observation queries grouping
/// by `(actor, action)` can separate submit-time intent from
/// scheduler-time observation. Carries the same lifecycle payload shape
/// (`{task_id, lane, plan_count}`) the rest of the `task.<state>` family
/// uses; `plan_count` is always 0 at submit by definition but is
/// included for shape parity so consumers don't need a special case.
pub const ACTION_TASK_SUBMITTED: &str = "task.submitted";
```

- [ ] **Step 3: Verify the crate builds**

Run: `source "$HOME/.cargo/env" && cargo build -p hhagent-core 2>&1 | tail -5`
Expected: clean build, no warnings.

- [ ] **Step 4: Commit**

```bash
git add core/src/scheduler/audit.rs
git commit -m "$(cat <<'EOF'
feat(core/scheduler): ACTION_TASK_SUBMITTED const for producer-side audit row

Pairs with CLI_AUDIT_ACTOR from core::cli_audit (cancel slice). Same wire
shape as the existing ACTION_TASK_RUNNING / ACTION_TASK_FINALIZE consts;
the dynamic action_task_terminal(state) builder is for the 5-variant
terminal family (completed/failed/cancelled/timed_out/blocked) — submit
is not in that family, so a fixed const is the right shape.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Write the failing integration test `cli_submit_audit_e2e.rs`

**Files:**
- Create: `core/tests/cli_submit_audit_e2e.rs`

- [ ] **Step 1: Create the test file**

Create `core/tests/cli_submit_audit_e2e.rs` with the following content. **Note:** the test pins both `Lane::Fast` and `Lane::Long` in a single test function to keep the PG bring-up cost down (matches the cancel slice's structure where lane round-trip is one of the assertions).

```rust
//! Producer-side submission audit row — end-to-end.
//!
//! What this test pins (against a per-test PG cluster):
//!
//! 1. [`hhagent_core::cli_audit::submit_and_audit`] on `Lane::Fast` and
//!    `Lane::Long`:
//!    * inserts a `pending` row in `tasks` with the input payload,
//!    * writes one `actor='cli' action='task.submitted'` row in
//!      `audit_log` per call with the canonical lifecycle payload
//!      `{task_id, lane, plan_count}` — same shape as the scheduler's
//!      `task.<state>` rows so observation SQL `WHERE action LIKE 'task.%'`
//!      captures the full lifecycle from submit through terminal,
//!    * returns the new task id (same shape as the underlying
//!      `tasks::insert_pending`).
//!
//! ## Why the test exists
//!
//! Before this slice, `hhagent-cli ask` called `tasks::insert_pending`
//! directly and emitted no producer-side audit row — the lifecycle
//! stream visible in `audit_log` started at the scheduler's
//! `task.running` observation on claim. "Submitted but never claimed"
//! gaps were invisible at the SQL layer (e.g. tasks submitted while the
//! scheduler is down), and submit-to-claim latency queries had to join
//! `audit_log.scheduler/task.running.ts` against `tasks.created_at`
//! across two clocks. This row closes that gap.
//!
//! ## Skip semantics
//!
//! Skips silently with `[SKIP]` lines on hosts without Postgres or a
//! reachable supervisor; run `cargo test -- --nocapture` to see them.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use hhagent_core::cli_audit::{submit_and_audit, CLI_AUDIT_ACTOR};
use hhagent_db::tasks::{get, Lane};
use hhagent_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};

/// Headline test for the slice: submitting on both lanes writes exactly
/// one canonical producer-side audit row per call and leaves matching
/// pending rows in the `tasks` table.
#[test]
fn submit_and_audit_emits_producer_task_submitted_row() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "csa-d",
        "csa-l",
        &format!("hhagent-supervisor-test-pg-csa-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-thread tokio runtime");

    rt.block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "purpose": "cli-submit-audit"}),
        )
        .await
        .expect("probe run");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("connect runtime pool");

        // Snapshot audit_log size before the test so we can assert the
        // exact delta (the probe step has already written 1 row).
        let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log")
            .fetch_one(&pool)
            .await
            .expect("count audit_log");

        // ── 1. Submit on Lane::Fast ────────────────────────────────────
        let fast_payload =
            serde_json::json!({"instruction": "fast lane task", "kind": "test"});
        let fast_id = submit_and_audit(&pool, Lane::Fast, fast_payload.clone())
            .await
            .expect("submit_and_audit fast");

        // ── 2. Submit on Lane::Long ────────────────────────────────────
        let long_payload =
            serde_json::json!({"instruction": "long lane task", "kind": "test"});
        let long_id = submit_and_audit(&pool, Lane::Long, long_payload.clone())
            .await
            .expect("submit_and_audit long");

        assert_ne!(fast_id, long_id, "two inserts must produce distinct ids");

        // ── 3. Confirm `tasks` table shape ─────────────────────────────
        let fast_task = get(&pool, fast_id).await.expect("get fast").expect("fast task exists");
        assert_eq!(fast_task.state, "pending");
        assert_eq!(fast_task.lane, Lane::Fast);
        assert_eq!(fast_task.plan_count, 0);
        assert_eq!(fast_task.payload, fast_payload, "fast payload round-trip");

        let long_task = get(&pool, long_id).await.expect("get long").expect("long task exists");
        assert_eq!(long_task.state, "pending");
        assert_eq!(long_task.lane, Lane::Long);
        assert_eq!(long_task.plan_count, 0);
        assert_eq!(long_task.payload, long_payload, "long payload round-trip");

        // ── 4. Confirm exactly two new audit rows, with the canonical shape ─
        let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log")
            .fetch_one(&pool)
            .await
            .expect("count audit_log after");
        assert_eq!(
            after - before,
            2,
            "exactly two new audit_log rows from two submit_and_audit calls"
        );

        // Fetch both producer rows ordered by id (= insertion order).
        let rows: Vec<(String, String, serde_json::Value)> = sqlx::query_as(
            "SELECT actor, action, payload \
             FROM audit_log \
             WHERE actor = $1 AND action = 'task.submitted' \
             ORDER BY id ASC",
        )
        .bind(CLI_AUDIT_ACTOR)
        .fetch_all(&pool)
        .await
        .expect("fetch cli_audit submit rows");

        assert_eq!(rows.len(), 2, "exactly two task.submitted rows");

        // First row pins fast-lane payload values; second row pins long-lane.
        for (i, (id, lane_str)) in [(fast_id, "fast"), (long_id, "long")].iter().enumerate() {
            let (actor, action, payload) = &rows[i];
            assert_eq!(actor, CLI_AUDIT_ACTOR);
            assert_eq!(action, "task.submitted");

            assert_eq!(
                payload.get("task_id").and_then(|v| v.as_i64()),
                Some(*id),
                "row {i}: payload.task_id must equal inserted id"
            );
            assert_eq!(
                payload.get("lane").and_then(|v| v.as_str()),
                Some(*lane_str),
                "row {i}: payload.lane must equal the SQL lane string"
            );
            assert_eq!(
                payload.get("plan_count").and_then(|v| v.as_i64()),
                Some(0),
                "row {i}: payload.plan_count must be 0 at submit time"
            );

            // Key-set pin — detects a future accidental extra field.
            let keys: std::collections::BTreeSet<_> = payload
                .as_object()
                .expect("payload is a JSON object")
                .keys()
                .cloned()
                .collect();
            let expected: std::collections::BTreeSet<String> =
                ["task_id", "lane", "plan_count"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
            assert_eq!(keys, expected, "row {i}: cli submit audit payload key set");
        }

        pool.close().await;
    });
}
```

- [ ] **Step 2: Run the test to verify it fails with a compile error**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-core --test cli_submit_audit_e2e 2>&1 | tail -10`

Expected: compile error on `use hhagent_core::cli_audit::{submit_and_audit, CLI_AUDIT_ACTOR};` — `submit_and_audit` is not yet defined. **Do not proceed to Task 3 until this red is confirmed.**

- [ ] **Step 3: Do NOT commit yet**

The test is red. Commit comes after the implementation lands and tests pass green.

---

### Task 3: Implement `submit_and_audit` in `core::cli_audit`

**Files:**
- Modify: `core/src/cli_audit.rs`

- [ ] **Step 1: Add imports for the new helper**

Edit `core/src/cli_audit.rs`. Locate the existing imports block at lines 62–67:

```rust
use hhagent_db::audit;
use hhagent_db::tasks::{mark_cancelled, Task};
use hhagent_db::DbError;
use sqlx::PgPool;

use crate::scheduler::audit::{action_task_terminal, build_lifecycle_payload};
```

Widen to also import `insert_pending` (for the helper body), `Lane` (helper's lane arg), and `ACTION_TASK_SUBMITTED` (the action const):

```rust
use hhagent_db::audit;
use hhagent_db::tasks::{insert_pending, mark_cancelled, Lane, Task};
use hhagent_db::DbError;
use sqlx::PgPool;

use crate::scheduler::audit::{
    action_task_terminal, build_lifecycle_payload, ACTION_TASK_SUBMITTED,
};
```

- [ ] **Step 2: Add the helper function**

Append `submit_and_audit` to `core/src/cli_audit.rs` after `cancel_and_audit` (line 123) but before the `#[cfg(test)] mod tests` block. Insert verbatim:

```rust
/// Producer-side task submission with audit-row emission.
///
/// Calls [`insert_pending`] and writes one `actor='cli'
/// action='task.submitted'` row to `audit_log` with the canonical
/// lifecycle payload `{task_id, lane, plan_count}` built via
/// [`build_lifecycle_payload`] (`plan_count` is `0` by definition at
/// submit time — included for shape parity with the rest of the
/// `task.<state>` family so consumers don't need a special case).
///
/// On success returns the new task id. The audit insert is best-effort:
/// a transient DB issue is logged at WARN but the id still propagates,
/// because the SQL INSERT already committed and the task is now a real
/// row in the `tasks` table — failing the call would be strictly worse
/// than a missing audit row, and would couple submit liveness to audit
/// availability the same way the cancel-slice trade-off documents.
///
/// # Two-rows-on-one-event note
///
/// `hhagent-cli ask` will produce two rows in `audit_log` for one
/// logical task entry: this producer row at submit time, and the
/// scheduler's later `task.running` observation row on claim. The split
/// is intentional — observation queries asking "who submitted" use
/// `actor='cli'`, queries asking "what did the scheduler observe" use
/// `actor='scheduler'`.
pub async fn submit_and_audit(
    pool: &PgPool,
    lane: Lane,
    payload: serde_json::Value,
) -> Result<i64, DbError> {
    let id = insert_pending(pool, lane, payload).await?;

    let row_payload = build_lifecycle_payload(id, lane, 0);
    if let Err(e) =
        audit::insert(pool, CLI_AUDIT_ACTOR, ACTION_TASK_SUBMITTED, row_payload).await
    {
        tracing::warn!(
            task_id = id,
            error = %e,
            "cli_audit::submit_and_audit: audit insert failed (task itself was submitted)",
        );
    }

    Ok(id)
}
```

- [ ] **Step 3: Build the crate to confirm compile success**

Run: `source "$HOME/.cargo/env" && cargo build -p hhagent-core 2>&1 | tail -5`
Expected: clean build, no warnings. If you see "unused import" on `Task`, that's expected (Task remains used by `cancel_and_audit`'s `CancelOutcome::Cancelled(Task)` variant) — no action needed.

- [ ] **Step 4: Run the new integration test (should now pass)**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-core --test cli_submit_audit_e2e -- --nocapture 2>&1 | tail -20`
Expected: `1 passed` (or 1 `[SKIP]` line on a host without PG, which is also acceptable).

- [ ] **Step 5: Commit (helper + test together since the test is the regression pin)**

```bash
git add core/src/cli_audit.rs core/tests/cli_submit_audit_e2e.rs
git commit -m "$(cat <<'EOF'
feat(core/cli_audit): submit_and_audit helper + producer-side task.submitted row

Mirrors cancel_and_audit's shape: chokepoint posture (audit insert
best-effort; SQL insert success is load-bearing), reuses the canonical
build_lifecycle_payload from scheduler::audit so producer and observer
rows share the same key-set ({task_id, lane, plan_count}) and differ
only in the actor column ('cli' vs 'scheduler').

Closes the audit gap where hhagent-cli ask emitted no audit row at
submit time — observation queries asking "which tasks were submitted
but never claimed" had to fall back to joining audit_log against
tasks.created_at across two clocks. After this slice the lifecycle
stream visible at WHERE action LIKE 'task.%' covers submit → claim →
terminal in one query.

Test pins both Lane::Fast and Lane::Long in a single test fn to keep
PG bring-up cost down (matches cancel slice's structure).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Rewire `hhagent-cli ask` to use `submit_and_audit`

**Files:**
- Modify: `core/src/bin/hhagent-cli.rs` (lines 242, 244, 267)

- [ ] **Step 1: Widen the `cli_audit` import to include `submit_and_audit`**

Edit `core/src/bin/hhagent-cli.rs` line 242. Current:

```rust
    use hhagent_core::cli_audit::cancel_and_audit;
```

Change to:

```rust
    use hhagent_core::cli_audit::{cancel_and_audit, submit_and_audit};
```

- [ ] **Step 2: Drop `insert_pending` from the `tasks` import**

Edit line 244. Current:

```rust
    use hhagent_db::tasks::{get, insert_pending};
```

Change to:

```rust
    use hhagent_db::tasks::get;
```

- [ ] **Step 3: Replace the direct `insert_pending` call**

Edit lines 267–276. Current:

```rust
    let id = match insert_pending(
        &pool,
        lane,
        serde_json::json!({"instruction": instruction, "kind": "ask"}),
    )
    .await
    {
        Ok(i) => i,
        Err(e) => { eprintln!("ask: insert failed: {e}"); return ExitCode::from(1); }
    };
```

Change to:

```rust
    let id = match submit_and_audit(
        &pool,
        lane,
        serde_json::json!({"instruction": instruction, "kind": "ask"}),
    )
    .await
    {
        Ok(i) => i,
        Err(e) => { eprintln!("ask: insert failed: {e}"); return ExitCode::from(1); }
    };
```

The function shape, the `Ok(i64)`/`Err(DbError)` return path, the error message, and the calling convention all match `insert_pending` exactly — a strict drop-in.

- [ ] **Step 4: Verify the binary compiles**

Run: `source "$HOME/.cargo/env" && cargo build -p hhagent-core --bin hhagent-cli 2>&1 | tail -5`
Expected: clean build, no warnings, no unused-import errors.

- [ ] **Step 5: Run the new submit-audit test plus the existing cancel-audit test together to confirm no regression**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-core --test cli_submit_audit_e2e --test cli_cancel_audit_e2e -- --nocapture 2>&1 | tail -20`
Expected: 3 passed total (1 from submit, 2 from cancel), or `[SKIP]` lines on hosts without PG.

- [ ] **Step 6: Do NOT commit yet**

`cli_ask_e2e.rs` still asserts the old multiset (no `cli/task.submitted` row); after this rewiring its assertions are wrong. Task 5 fixes them. Commit at the end of Task 5 to keep main green at every commit boundary.

---

### Task 5: Bump `cli_ask_e2e.rs` multiset assertions

**Files:**
- Modify: `core/tests/cli_ask_e2e.rs` (happy-path block ~lines 541–570; failure-path block ~lines 747–774)

- [ ] **Step 1: Add the new producer-row assertion to the happy path**

Edit `core/tests/cli_ask_e2e.rs` happy-path multiset block (line 541 onward). Find the line:

```rust
        assert_eq!(m.get(&("core".into(), "startup".into())), Some(&1),
                   "expected 1× core/startup; multiset = {m:?}");
```

Insert **immediately after** the line above (before the `agent/plan.formulate` assertion at line 544):

```rust
        assert_eq!(m.get(&("cli".into(), "task.submitted".into())), Some(&1),
                   "expected 1× cli/task.submitted (producer-side row from hhagent-cli ask); multiset = {m:?}");
```

- [ ] **Step 2: Bump the happy-path total row count**

Edit line 565. Current:

```rust
        let expected_total: i64 = 1 + 2 + 2 + 1 + 1 + 1 + 1 + 1; // = 10
```

Change to:

```rust
        let expected_total: i64 = 1 + 1 + 2 + 2 + 1 + 1 + 1 + 1 + 1; // = 11 (cli/task.submitted added)
```

The new leading `1 +` is the `cli/task.submitted` row.

- [ ] **Step 3: Add the new producer-row assertion to the failure path**

Edit `core/tests/cli_ask_e2e.rs` failure-path multiset block (line 747 onward). Find the line:

```rust
        assert_eq!(m.get(&("core".into(), "startup".into())), Some(&1),
                   "expected 1× core/startup; multiset = {m:?}");
```

Insert **immediately after** the line above (before the `agent/plan.formulate` assertion at line 750):

```rust
        assert_eq!(m.get(&("cli".into(), "task.submitted".into())), Some(&1),
                   "expected 1× cli/task.submitted (producer-side row from hhagent-cli ask); multiset = {m:?}");
```

- [ ] **Step 4: Bump the failure-path total row count**

Edit line 769. Current:

```rust
        let expected_total: i64 = 1 + 3 + 3 + 3 + 3 + 1 + 1 + 1; // = 16
```

Change to:

```rust
        let expected_total: i64 = 1 + 1 + 3 + 3 + 3 + 3 + 1 + 1 + 1; // = 17 (cli/task.submitted added)
```

The new leading `1 +` is the `cli/task.submitted` row.

- [ ] **Step 5: Run cli_ask_e2e to confirm it still passes with the bumps**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-core --test cli_ask_e2e -- --nocapture 2>&1 | tail -25`
Expected: 2 passed (happy + failure), or both `[SKIP]` on hosts without PG. **If the multiset still fails, double-check that the rewiring in Task 4 actually happened (the producer row only fires when `submit_and_audit` is called from `ask_async`, not from `insert_pending` directly).**

- [ ] **Step 6: Run the full workspace test suite to catch any other regression**

Run: `source "$HOME/.cargo/env" && cargo test --workspace 2>&1 | tail -25`
Expected: `354 passed; 0 failed`. The +1 vs the baseline of 353 comes from the new `cli_submit_audit_e2e.rs` test. **Do not proceed to Task 6 until full workspace is green.**

- [ ] **Step 7: Commit (CLI rewiring + multiset bumps together)**

```bash
git add core/src/bin/hhagent-cli.rs core/tests/cli_ask_e2e.rs
git commit -m "$(cat <<'EOF'
feat(cli): hhagent-cli ask emits producer-side task.submitted audit row

Single-line swap at ask_async line 267: insert_pending → submit_and_audit
(same Result<i64, DbError> shape, same error-display string). The import
block widens to pull submit_and_audit alongside cancel_and_audit, and
drops insert_pending which is no longer called directly.

cli_ask_e2e.rs multiset assertions bumped by 1 in both happy and failure
paths to expect the new cli/task.submitted row (totals 10→11 and 16→17).
Workspace test count 353 → 354 (one new test in cli_submit_audit_e2e).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: Update HANDOVER.md and ROADMAP.md

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

- [ ] **Step 1: Refresh HANDOVER.md header**

Edit `docs/devel/handovers/HANDOVER.md` lines 7–9.

Current header block:

```markdown
**Last updated:** 2026-05-13 (CLI cancel audit row — branch `feat/cli-cancel-audit`)
**Last commit (main):** `76fe940` (merge of PR #41 `feat/memory-graph-lane`).
**This session's working branch:** `feat/cli-cancel-audit` (off `main` at `830524b`, which is the doc-refresh commit on top of `76fe940`). Closes the HANDOVER "Immediate next pickups" item "`task.cancelled` row from CLI direct cancel of a `pending` task that was never claimed". Workspace test count: **349 → 353** (+4: 2 unit in `core::cli_audit::tests`, 2 integration in `core/tests/cli_cancel_audit_e2e.rs`).
```

Change to:

```markdown
**Last updated:** 2026-05-13 (CLI task.submitted producer audit row — branch `feat/cli-task-submitted-audit`)
**Last commit (main):** `fdf1a52` (merge of PR #43 `feat/cli-cancel-audit`).
**This session's working branch:** `feat/cli-task-submitted-audit` (off `main` at `fdf1a52`). Closes the HANDOVER "Immediate next pickups" item "`task.submitted` producer row from `hhagent-cli ask`" — symmetric to the just-merged cli-cancel-audit slice. Workspace test count: **353 → 354** (+1 integration test in new `core/tests/cli_submit_audit_e2e.rs`; `cli_ask_e2e.rs` multiset bumps don't add `#[test]` functions).
```

- [ ] **Step 2: Insert a new "Recently completed (this session)" section at the top**

Edit `docs/devel/handovers/HANDOVER.md` line 95 onward. Insert the following block **before** the existing `## Recently completed (this session, 2026-05-13 — CLI cancel audit row…` heading (the previous-session block stays as is — just demoted to "previous session" by virtue of being below):

```markdown
## Recently completed (this session, 2026-05-13 — CLI `task.submitted` producer audit row, branch `feat/cli-task-submitted-audit`)

Branch: `feat/cli-task-submitted-audit` (off `main` at `fdf1a52`, the merge of PR #43). Closes the HANDOVER "Immediate next pickups" item that was filed the same day PR #43 merged: "`task.submitted` producer row from `hhagent-cli ask`".

**Why this slice now.** PR #43 (cli-cancel-audit) just shipped the first producer-side audit row family with `actor='cli'`. It closed the gap for cancel of a never-claimed `pending` task. The symmetric gap was that `hhagent-cli ask` itself emitted no audit row at submit time — the lifecycle stream visible in `audit_log` started at the scheduler's `task.running` observation on claim. Submit-to-claim latency queries had to join `audit_log` against `tasks.created_at` across two clocks, and tasks submitted while the scheduler was down (no claim ever happens) left no row at all. This slice closes that gap.

**Shape (3 production files + 1 test file added + 1 test file bumped):**

- **`core/src/scheduler/audit.rs`** — one new constant `pub const ACTION_TASK_SUBMITTED: &str = "task.submitted"` inserted between `ACTION_TASK_FINALIZE` and `ACTION_TASK_PREFIX`. Const, not builder, because submit is a fixed-string action (not the dynamic 5-variant terminal family `action_task_terminal` covers).
- **`core/src/cli_audit.rs`** — new `pub async fn submit_and_audit(pool, lane, payload) -> Result<i64, DbError>`. Calls `tasks::insert_pending`; on Ok, best-effort emits one `actor='cli' action='task.submitted'` row with `build_lifecycle_payload(id, lane, 0)`. Audit failure → `tracing::warn!`, id still propagates (chokepoint posture). Same `Result<i64, _>` shape as the underlying `insert_pending`, so the call-site rewiring is a one-line swap.
- **`core/src/bin/hhagent-cli.rs::ask_async`** — line 267 `insert_pending(...)` → `submit_and_audit(...)`. Import line widened; `insert_pending` dropped from the `tasks` import.
- **NEW `core/tests/cli_submit_audit_e2e.rs`** — single integration test that pins both `Lane::Fast` and `Lane::Long` in one PG cluster bring-up. Asserts: (1) helper returns distinct ids for two calls, (2) `tasks` rows match expected state/lane/plan_count/payload, (3) `audit_log` gained exactly two `cli/task.submitted` rows, (4) both rows pin actor/action plus the 3-key payload `{task_id, lane, plan_count}` BTreeSet shape.
- **`core/tests/cli_ask_e2e.rs`** — happy + failure multiset assertions bumped by 1 `cli/task.submitted` row each (totals `1 + 1 + 2 + 2 + 1 + 1 + 1 + 1 + 1 = 11` and `1 + 1 + 3 + 3 + 3 + 3 + 1 + 1 + 1 = 17`).

**DB layer — no widening.** `tasks::insert_pending` stayed as `Result<i64, DbError>`. The cancel slice widened `mark_cancelled` to `Result<Option<Task>, _>` via `RETURNING *` because `plan_count` could have advanced between submit and cancel; at submit time `plan_count` is `0` by definition and the returned `id` plus the input `lane` give the helper everything `build_lifecycle_payload` needs. Smaller diff, no call-site churn.

**Audit-row contract (the headline):**

| When                                              | actor       | action            | payload keys                  |
| ------------------------------------------------- | ----------- | ----------------- | ----------------------------- |
| `hhagent-cli ask "..."` inserts a `pending` row   | `cli`       | `task.submitted`  | `{task_id, lane, plan_count}` (`plan_count` always 0 at submit) |

Same payload shape as the scheduler's existing lifecycle rows — observation queries grouping by `(actor, action)` see the full submit → claim → terminal stream under one `WHERE action LIKE 'task.%'` filter, with `actor` separating producer intent from scheduler observation.

**TDD ordering** (per CLAUDE.md rule #2):
1. `ACTION_TASK_SUBMITTED` const landed first — pure addition, no test (the integration test verifies the literal in the audit row downstream).
2. Wrote `core/tests/cli_submit_audit_e2e.rs` against the not-yet-existing `submit_and_audit` — compile-error red.
3. Implemented `submit_and_audit` in `cli_audit.rs`; test green.
4. Rewired `hhagent-cli.rs::ask_async`; `cli_ask_e2e.rs` red on multiset.
5. Bumped `cli_ask_e2e.rs` multiset; full workspace green at 354.

**What this slice deliberately does NOT do.**
- **No producer row from future channel adapters.** No channel adapter exists today; YAGNI. When one lands, the same helper can be promoted (take `actor: &str`) or a separate `CHANNEL_AUDIT_ACTOR` const added — wire shape is identical.
- **No producer `task.failed` row from `hhagent-cli tasks fail`.** Operator escape hatch; rare; scheduler's `task.crashed` lifecycle row already covers the running-after-restart path.
- **No DB transaction wrapping `insert_pending` + audit insert.** Best-effort matches the chokepoint and cancel-slice posture, documented at the helper doc-comment level (same trade-off `cli_audit.rs` already documents for `cancel_and_audit`).

**Test count delta:** 353 → **354** (+1 integration test).

**Files touched (5 modified, 1 added):**
- `core/src/scheduler/audit.rs` — `ACTION_TASK_SUBMITTED` const added.
- `core/src/cli_audit.rs` — `submit_and_audit` helper added.
- `core/src/bin/hhagent-cli.rs` — one-line swap + import widening at `ask_async`.
- NEW `core/tests/cli_submit_audit_e2e.rs` — single integration test (~140 LOC).
- `core/tests/cli_ask_e2e.rs` — happy + failure multiset bumps.
- `docs/devel/handovers/HANDOVER.md` + `docs/devel/ROADMAP.md` — this update.
- `docs/superpowers/specs/2026-05-13-cli-task-submitted-audit-design.md` + `docs/superpowers/plans/2026-05-13-cli-task-submitted-audit.md` — spec + plan committed earlier in the branch.

---
```

- [ ] **Step 3: Update HANDOVER.md "Immediate next pickups" list**

Locate the bullet near line 846 (the existing `task.submitted` follow-up entry). Current:

```markdown
- **`task.submitted` producer row from `hhagent-cli ask`** — symmetric gap to the just-shipped `task.cancelled`. Today `hhagent-cli ask` calls `tasks::insert_pending` and emits no producer-side audit row; the lifecycle stream starts at `scheduler/task.running` on claim. A producer `actor='cli' action='task.submitted'` with payload `{task_id, lane, plan_count: 0}` would let observation queries reconstruct submit-to-claim latency and detect "submitted but never claimed" gaps (e.g. scheduler down). One-session slice that reuses the now-existing `CLI_AUDIT_ACTOR` constant + `build_lifecycle_payload` from `scheduler::audit` (action would be a new `action_task_submitted()` builder or just a literal `"task.submitted"`). Independent of CASSANDRA / observation work.
```

Change to:

```markdown
- ~~**`task.submitted` producer row from `hhagent-cli ask`**~~ **Shipped this session 2026-05-13** as `actor='cli' action='task.submitted'` via the new `core::cli_audit::submit_and_audit` helper. Branch: `feat/cli-task-submitted-audit` (`ACTION_TASK_SUBMITTED` const, not a builder, slotted next to `ACTION_TASK_RUNNING` / `ACTION_TASK_FINALIZE`). See the "Recently completed (this session)" entry at the top.
```

- [ ] **Step 4: Tick the matching ROADMAP item**

Edit `docs/devel/ROADMAP.md`. Locate the existing line for the cli-cancel-audit producer row (line 97). Insert a new bullet **immediately after** it under Phase 1 — Memory & Loop:

```markdown
- [x] **[follow-up] Producer-side `task.submitted` audit row from `hhagent-cli ask`** — landed 2026-05-13 on branch `feat/cli-task-submitted-audit`. Closes the gap symmetric to the cli-cancel-audit slice: `hhagent-cli ask` was calling `tasks::insert_pending` directly with no producer-side audit row, so the lifecycle stream visible in `audit_log` only started at the scheduler's `task.running` observation on claim — submit-to-claim latency queries had to join `audit_log` against `tasks.created_at` across two clocks. New `core::scheduler::audit::ACTION_TASK_SUBMITTED = "task.submitted"` const + new `core::cli_audit::submit_and_audit(pool, lane, payload) -> Result<i64, DbError>` helper (mirror of `cancel_and_audit`: chokepoint posture, audit insert best-effort, id propagates even on audit failure). `tasks::insert_pending` left unchanged (no `RETURNING *` widening needed since all payload fields are known up-front at submit). `hhagent-cli ask` rewired in one line. **NOT in scope (filed as a follow-up):** producer row from future channel adapters (none exist today); producer `task.failed` row from `tasks fail` subcommand. Test count 353 → **354** (+1 integration test in `cli_submit_audit_e2e.rs`; `cli_ask_e2e` multiset bumps don't add `#[test]` functions).
```

- [ ] **Step 5: Verify both docs render reasonably (no broken anchors, no truncated tables)**

Run: `wc -l docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md`
Expected: HANDOVER grew by ~75 lines (from ~969 to ~1040); ROADMAP grew by 1 line (141 → 142). If either is shorter than the baseline, an Edit replaced more than intended — investigate before committing.

- [ ] **Step 6: Commit the docs together with the spec/plan as a documentation-only commit**

The spec was already committed at `d9f1920` on this branch. The plan file is still untracked. Stage it alongside the doc updates:

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md docs/superpowers/plans/2026-05-13-cli-task-submitted-audit.md
git commit -m "$(cat <<'EOF'
docs(handover,roadmap): cli task.submitted producer row shipped

Documents the slice that lands the actor='cli' action='task.submitted'
producer-side audit row symmetric to the cli-cancel-audit slice. Closes
the gap where hhagent-cli ask emitted no audit row at submit time and
the lifecycle stream only started at scheduler/task.running on claim.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 7: Final verification + push

**Files:** none (verification step only).

- [ ] **Step 1: Run the full workspace test suite one more time**

Run: `source "$HOME/.cargo/env" && cargo test --workspace 2>&1 | tail -10`
Expected: `354 passed; 0 failed; 0 ignored` (or higher `ignored` for the two pre-existing doctests in `hhagent-sandbox` and `hhagent-worker-prelude` that are explicitly marked).

- [ ] **Step 2: Verify the audit-tail JSONL on the test cluster shows the new row shape (optional smoke; skip if no PG handy)**

This is an optional sanity check, not a regression pin. Skip if PG isn't running locally.

```bash
# Only if you have a running test cluster and want to eyeball one line.
# Skip otherwise — the integration test is the load-bearing pin.
./target/debug/hhagent-cli audit tail --from-start --no-follow 2>&1 | grep '"task.submitted"' | head -3
```

Expected (if a cluster is up): one or more JSONL lines like `{"id":N,"ts":"...","actor":"cli","action":"task.submitted","payload":{"task_id":...,"lane":"fast","plan_count":0}}`.

- [ ] **Step 3: Confirm the branch is clean and ready to push**

Run: `git status && git log --oneline main..HEAD`

Expected:
- `git status`: clean working tree.
- `git log --oneline main..HEAD`: 4 commits — spec (Task 0, already done), `ACTION_TASK_SUBMITTED` const (Task 1), helper+test (Task 3), CLI rewiring+multiset bump (Task 5), docs (Task 6). **Five commits total** including the spec.

- [ ] **Step 4: Push the branch**

```bash
git push -u origin feat/cli-task-submitted-audit
```

Expected: the branch is published. Open a PR in the GitHub UI (or via `gh pr create` if available) targeting `main`.

---

## Self-Review

**Spec coverage:**
- Spec §1 (DB unchanged): Task 1 (const only) + Task 3 (helper) confirm `insert_pending` is not modified. ✓
- Spec §2 (new const): Task 1 adds `ACTION_TASK_SUBMITTED` with doc comment. ✓
- Spec §3 (helper): Task 3 adds `submit_and_audit` with chokepoint posture. ✓
- Spec §4 (CLI rewiring): Task 4 does the one-line swap + import widening. ✓
- Test plan — new file: Task 2 creates `cli_submit_audit_e2e.rs` with both lanes + payload key-set pin. ✓
- Test plan — multiset bumps: Task 5 does both. ✓
- Audit-row contract table: produced by Task 3 (helper) and pinned by Task 2 (test). ✓
- "What deliberately NOT in scope": documented in HANDOVER entry written in Task 6. ✓
- Test count delta 353 → 354: pinned in Task 5 Step 6 + Task 7 Step 1. ✓

**Placeholder scan:**
- No "TBD", "TODO", "implement later", "similar to Task N" found.
- Every step shows the actual edit content (use blocks, function bodies, exact lines to modify).
- Every test step has expected output described.

**Type / name consistency:**
- `submit_and_audit` (in spec) = `submit_and_audit` (in Task 3 + Task 4 + Task 6). ✓
- `ACTION_TASK_SUBMITTED` (in spec) = `ACTION_TASK_SUBMITTED` (in Task 1 + Task 3). ✓
- `CLI_AUDIT_ACTOR` (existing, from cancel slice) used unchanged in Tasks 2, 3.
- `build_lifecycle_payload(id, lane, 0)` (in spec) = exact arg shape used in Task 3.
- `Lane::Fast` / `Lane::Long` (in tests) = exact enum variants from `hhagent_db::tasks::Lane`.

No issues found.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-05-13-cli-task-submitted-audit.md`. Two execution options:

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.

**2. Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints.

Which approach?
