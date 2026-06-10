# Issue #90 — `upsert_entities_and_relations` round-trip reduction (Layer A) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate the conditional follow-up `SELECT` in `upsert_entities_and_relations` by switching the per-entity `INSERT … ON CONFLICT DO NOTHING RETURNING id` (plus follow-up `SELECT` on conflict) to a single `INSERT … ON CONFLICT DO UPDATE SET name_norm = entities.name_norm RETURNING id, (xmax = 0) AS inserted`. Per-entity goes from 1–2 round-trips down to a guaranteed 1.

**Architecture:** Single-file change in [`core/src/entity_extraction/gliner_relex.rs`](../../../core/src/entity_extraction/gliner_relex.rs). The Rust loop keeps its per-entity shape (preserves error attribution per the operator's brainstorming choice — Layer A only, no full-batch `unnest`). Public types (`UpsertOutcome`'s three fields), function signature, and audit-row contract (`build_extract_entities_payload`'s 8-key shape) are unchanged. No migration, no schema change, no caller-side change.

**Tech Stack:** Rust (sqlx + tokio), PostgreSQL 18 (`xmax = 0` discriminator, `ON CONFLICT DO UPDATE` self-assignment idiom).

---

## File Structure

- **Modify:** [`core/src/entity_extraction/gliner_relex.rs`](../../../core/src/entity_extraction/gliner_relex.rs) (518 LOC → ~505 LOC). Only the per-entity loop body inside `upsert_entities_and_relations` (lines ~183-217). Function signature and surrounding code unchanged.
- **Modify:** [`core/tests/entity_extraction_e2e.rs`](../../../core/tests/entity_extraction_e2e.rs) — add 2 new integration tests at the end of the mock-tier block (after `upsert_dedup_works_with_case_variants` at line ~334, before `extractor_extract_writes_summary_audit_row` at line ~335). Existing tests stay byte-identical and now exercise the new SQL automatically.

No other files touched. No migration. No public-API change.

---

## Task 1: Failing test for quarantine-preservation regression pin

**Files:**
- Modify: `core/tests/entity_extraction_e2e.rs` (add new test after `upsert_dedup_works_with_case_variants`)

**Why this is Task 1:** This is the load-bearing bug-of-omission pin — if anyone changes the no-op `SET name_norm = entities.name_norm` to clobber `quarantine`, the operator quarantine-review CLI's approvals would be silently re-quarantined on the next extraction. The test must exist before the SQL change so we can verify it stays green across the refactor (i.e. the new code preserves the same invariant the old code did).

- [ ] **Step 1: Write the failing test**

Locate the end of `upsert_dedup_works_with_case_variants` in `core/tests/entity_extraction_e2e.rs` (around line ~334, just before `extractor_extract_writes_summary_audit_row` at line ~335) and insert the following:

```rust
/// Bug-of-omission regression pin: a future edit that replaces the no-op
/// `SET name_norm = entities.name_norm` with e.g. `SET quarantine = TRUE`
/// would silently re-quarantine operator-approved entities on next
/// re-extraction. This test catches that — Issue #90's load-bearing
/// invariant for the operator quarantine-review CLI (PR #93).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_preserves_operator_unquarantine_decision() {
    let Some((_cluster, pool)) = bring_up_pg("preserve-quar").await else {
        return;
    };

    // Seed one entity via the production path — it lands quarantined.
    let merged = ExtractResponse {
        entities: vec![Entity {
            text: "Dr Smith".into(),
            label: "person".into(),
            start: 0,
            end: 8,
            score: 0.99,
        }],
        triples: vec![],
    };
    let out1 = upsert_entities_and_relations(&pool, &merged)
        .await
        .expect("first upsert");
    assert_eq!(out1.entity_ids.len(), 1);
    let entity_id = out1.entity_ids[0];

    // Simulate `kastellan-cli entities approve <id>` — operator approves
    // the entity, flipping quarantine to FALSE.
    sqlx::query("UPDATE entities SET quarantine = FALSE WHERE id = $1")
        .bind(entity_id)
        .execute(&pool)
        .await
        .expect("operator approve simulation");

    // Re-extract the same entity. The upsert path hits ON CONFLICT.
    let out2 = upsert_entities_and_relations(&pool, &merged)
        .await
        .expect("second upsert");
    assert_eq!(out2.n_entities_upserted_new, 0, "no new row created");
    assert_eq!(out2.entity_ids, vec![entity_id], "same id returned");

    // The load-bearing assertion: the no-op SET must not have
    // clobbered the operator's approval.
    let quarantine_after: bool =
        sqlx::query_scalar("SELECT quarantine FROM entities WHERE id = $1")
            .bind(entity_id)
            .fetch_one(&pool)
            .await
            .expect("read back quarantine");
    assert!(
        !quarantine_after,
        "ON CONFLICT path must preserve operator approval (quarantine=FALSE)"
    );

    pool.close().await;
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run:
```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test entity_extraction_e2e upsert_preserves_operator_unquarantine_decision -- --nocapture
```

**Expected:** PASS (yes, PASS — not fail).

This test characterises the CURRENT behaviour and is expected to pass against the un-modified `upsert_entities_and_relations`. The current `INSERT … ON CONFLICT DO NOTHING` path leaves the existing row's columns untouched (DO NOTHING means literally nothing), so `quarantine` survives unchanged. The test is in TDD's "characterisation" role — it locks in the existing invariant so the Task 3 SQL change is provably non-regressive. If Task 3 broke the invariant (e.g. by switching the `SET` clause to clobber `quarantine`), this test would catch it.

If the test fails on the current code, stop and investigate — that means the migration/schema or some other change has already broken the invariant and the SQL change isn't safe to write.

- [ ] **Step 3: Commit**

```sh
git add core/tests/entity_extraction_e2e.rs
git commit -m "$(cat <<'COMMIT_EOF'
test(entity_extraction_e2e): characterise quarantine-preservation on re-upsert

Regression pin for Issue #90's load-bearing invariant: the
ON CONFLICT path of upsert_entities_and_relations must preserve
operator approval (quarantine=FALSE) across re-extraction.

Catches a future edit that changes the SET clause to clobber the
quarantine column. Today's INSERT ... ON CONFLICT DO NOTHING
satisfies this trivially (DO NOTHING leaves the row alone). The
upcoming Issue #90 SQL rewrite uses `SET name_norm =
entities.name_norm` (a no-op self-assignment to force RETURNING)
and must preserve the same invariant — this test pins that.

Test passes against current code (characterisation pin); will
also pass against the post-#90 SQL.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
COMMIT_EOF
)"
```

---

## Task 2: Failing test for mixed-batch counter pin

**Files:**
- Modify: `core/tests/entity_extraction_e2e.rs` (add new test after the Task 1 test)

**Why this is Task 2:** Existing tests cover all-new (`upsert_creates_quarantined_entities`, `upsert_dedup_works_with_case_variants`) and all-existing (`upsert_is_idempotent_on_rerun`). The mixed case — one new + one existing in the same call — is the gap. Issue #90's `xmax = 0` discriminator runs per-iteration; a mixed batch is the natural test for "the per-iteration accumulator is incrementing on the right rows."

- [ ] **Step 1: Write the failing test**

Add immediately after the Task 1 test:

```rust
/// Mixed-batch counter pin: existing tests cover all-new
/// (`upsert_creates_quarantined_entities`) and all-existing
/// (`upsert_is_idempotent_on_rerun`). This pins the only uncovered
/// case — one new + one pre-existing in the same upsert call. The
/// xmax=0 discriminator in Issue #90's SQL rewrite must increment
/// n_entities_upserted_new on exactly the new row, not both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_counts_new_inserts_correctly_in_mixed_batch() {
    let Some((_cluster, pool)) = bring_up_pg("mixed").await else {
        return;
    };

    // Seed one entity.
    let seeded = ExtractResponse {
        entities: vec![Entity {
            text: "Alpha".into(),
            label: "concept".into(),
            start: 0,
            end: 5,
            score: 0.9,
        }],
        triples: vec![],
    };
    let out_seed = upsert_entities_and_relations(&pool, &seeded)
        .await
        .expect("seed upsert");
    let alpha_id = out_seed.entity_ids[0];

    // Now upsert a mixed batch: same Alpha + fresh Beta.
    let mixed = ExtractResponse {
        entities: vec![
            Entity {
                text: "Alpha".into(),
                label: "concept".into(),
                start: 0,
                end: 5,
                score: 0.9,
            },
            Entity {
                text: "Beta".into(),
                label: "concept".into(),
                start: 10,
                end: 14,
                score: 0.9,
            },
        ],
        triples: vec![],
    };
    let out_mixed = upsert_entities_and_relations(&pool, &mixed)
        .await
        .expect("mixed upsert");

    assert_eq!(out_mixed.entity_ids.len(), 2, "both ids returned");
    assert_eq!(
        out_mixed.entity_ids[0], alpha_id,
        "Alpha keeps its original id (resolved via conflict arm)"
    );
    assert_ne!(
        out_mixed.entity_ids[1], alpha_id,
        "Beta gets a distinct id"
    );
    assert_eq!(
        out_mixed.n_entities_upserted_new, 1,
        "exactly one new row created (Beta); Alpha was pre-existing"
    );

    pool.close().await;
}
```

- [ ] **Step 2: Run the test to verify it passes**

Run:
```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test entity_extraction_e2e upsert_counts_new_inserts_correctly_in_mixed_batch -- --nocapture
```

**Expected:** PASS against current code (characterisation pin — current code already returns the right counters via the two-statement path; the Task 3 rewrite must preserve this).

- [ ] **Step 3: Commit**

```sh
git add core/tests/entity_extraction_e2e.rs
git commit -m "$(cat <<'COMMIT_EOF'
test(entity_extraction_e2e): pin mixed-batch counter in upsert

Existing upsert_* tests cover all-new and all-existing paths.
Mixed batches (one new + one pre-existing in the same call) are
the uncovered case — and the most natural test for Issue #90's
per-iteration xmax=0 discriminator accumulator: increment must
fire on exactly the new row, not both.

Test passes against current code (two-statement path already has
the correct accumulator); will also pass against the post-#90
single-statement path.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
COMMIT_EOF
)"
```

---

## Task 3: Rewrite the per-entity upsert SQL

**Files:**
- Modify: `core/src/entity_extraction/gliner_relex.rs` (lines ~183-217, the per-entity for-loop body inside `upsert_entities_and_relations`)

**Why this is Task 3:** With both characterisation tests in place (Tasks 1 + 2), the SQL change can be made with confidence that the post-change behaviour preserves both the operator-approval-preservation invariant and the per-iteration counter correctness.

- [ ] **Step 1: Apply the edit**

Open `core/src/entity_extraction/gliner_relex.rs`. The per-entity for-loop body inside `upsert_entities_and_relations` currently reads (lines ~183-217):

```rust
    // Per-entity upsert. Each entity gets one INSERT attempt; on
    // conflict, we follow up with a SELECT to resolve the existing id.
    // This is two round-trips for existing entities and one for new
    // ones — acceptable for v2's typical 5–20 entities per extract.
    for ent in &merged.entities {
        let name_norm = normalize_entity_name(&ent.text);
        // First try INSERT ... ON CONFLICT DO NOTHING RETURNING id.
        let inserted_id: Option<i64> = sqlx::query_scalar(
            "INSERT INTO entities (kind, name, name_norm, quarantine) \
             VALUES ($1, $2, $3, TRUE) \
             ON CONFLICT (kind, name_norm) DO NOTHING \
             RETURNING id",
        )
        .bind(&ent.label)
        .bind(&ent.text)
        .bind(&name_norm)
        .fetch_optional(pool)
        .await
        .map_err(|e| kastellan_db::DbError::Query(format!("upsert entity: {e}")))?;

        let id = match inserted_id {
            Some(id) => {
                n_new += 1;
                id
            }
            None => {
                // Pre-existing row — resolve via SELECT.
                sqlx::query_scalar(
                    "SELECT id FROM entities WHERE kind = $1 AND name_norm = $2",
                )
                .bind(&ent.label)
                .bind(&name_norm)
                .fetch_one(pool)
                .await
                .map_err(|e| kastellan_db::DbError::Query(format!("resolve entity id: {e}")))?
            }
        };
        entity_ids.push(id);
    }
```

Replace the entire for-loop body (from the `// Per-entity upsert.` comment through the closing `}` of the for-loop, before the `// Build a (label, name_norm) → id index ...` comment block) with:

```rust
    // Per-entity upsert. Each entity gets a single statement:
    // INSERT ... ON CONFLICT DO UPDATE SET name_norm = entities.name_norm
    // RETURNING id, (xmax = 0) AS inserted.
    //
    // The `SET name_norm = entities.name_norm` self-assignment is the
    // standard Postgres idiom for "force RETURNING to fire on conflict
    // without changing the row's logical state." It is load-bearing
    // that this clause does NOT touch `quarantine` — if the operator
    // has already approved an entity via the quarantine-review CLI
    // (PR #93), re-extraction must not silently re-quarantine it.
    // Pinned by upsert_preserves_operator_unquarantine_decision in
    // core/tests/entity_extraction_e2e.rs.
    //
    // `xmax = 0` is the canonical inserted-vs-existed discriminator:
    // a fresh row has xmax=0 (no future-deleting transaction); a
    // conflict-hit row carries the conflict txn's xid. This
    // eliminates the previous two-statement path (DO NOTHING +
    // follow-up SELECT) — every entity now costs exactly one
    // round-trip. (Issue #90; Layer A only — full-batch unnest is
    // deferred.)
    //
    // Side effect of the no-op UPDATE: Postgres advances xmin and
    // writes a new tuple version even though no column changed.
    // Acceptable at v2 volume; autovacuum absorbs it without
    // operator action.
    for ent in &merged.entities {
        let name_norm = normalize_entity_name(&ent.text);
        let row: (i64, bool) = sqlx::query_as(
            "INSERT INTO entities (kind, name, name_norm, quarantine) \
             VALUES ($1, $2, $3, TRUE) \
             ON CONFLICT (kind, name_norm) DO UPDATE \
               SET name_norm = entities.name_norm \
             RETURNING id, (xmax = 0) AS inserted",
        )
        .bind(&ent.label)
        .bind(&ent.text)
        .bind(&name_norm)
        .fetch_one(pool)
        .await
        .map_err(|e| kastellan_db::DbError::Query(format!("upsert entity: {e}")))?;
        if row.1 {
            n_new += 1;
        }
        entity_ids.push(row.0);
    }
```

The pre-loop variables (`entity_ids`, `n_new`) and the post-loop code (the `by_key` index, the triple loop, the `UpsertOutcome` construction) are unchanged.

- [ ] **Step 2: Build to verify it compiles**

Run:
```sh
source "$HOME/.cargo/env"
cargo build -p kastellan-core 2>&1 | tail -20
```

**Expected:** clean compile, no warnings on the touched file. If sqlx complains about `query_as` and the `(i64, bool)` tuple type, the most likely cause is a missing import — sqlx auto-derives `FromRow` for tuples up to 16 elements via the `sqlx::FromRow` blanket impls, but if a specific `use sqlx::FromRow;` was added at the top of the file at some point, leave it untouched. Otherwise no new imports are needed.

- [ ] **Step 3: Run both Task 1 and Task 2 tests against the new code**

Run:
```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test entity_extraction_e2e \
    upsert_preserves_operator_unquarantine_decision \
    upsert_counts_new_inserts_correctly_in_mixed_batch \
    -- --nocapture
```

**Expected:** both PASS. The Task 1 test proves the operator-approval invariant survives; the Task 2 test proves the per-iteration counter still distinguishes new vs. existing correctly.

If either fails, do NOT proceed to Step 4. Investigate. Most likely failure modes:

- `upsert_preserves_operator_unquarantine_decision` fails with `quarantine=TRUE`: the `SET` clause is clobbering `quarantine`. Check the SQL string — `SET name_norm = entities.name_norm` (NOT `SET quarantine = EXCLUDED.quarantine` or `SET quarantine = TRUE`).
- `upsert_counts_new_inserts_correctly_in_mixed_batch` fails with `n_entities_upserted_new == 2`: the `(xmax = 0)` boolean is being read as `TRUE` on the conflict arm too. Check the SQL — `RETURNING id, (xmax = 0) AS inserted` (the parens around `xmax = 0` are required, otherwise Postgres parses it as `id, xmax = 0 AS inserted`).
- Either fails with a column mismatch: the tuple type `(i64, bool)` must match the `RETURNING` column order (id first, then inserted). If you flip them in the SQL, flip them in the tuple destructure too.

- [ ] **Step 4: Run the touched test file in full to verify no regressions**

Run:
```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test entity_extraction_e2e -- --nocapture
```

**Expected:** all 12 tests in the file pass (10 pre-existing + 2 new from Tasks 1-2). The 2 real-model tests (`extractor_extract_against_real_worker_returns_seeds`, `extractor_chunking_path_against_real_worker`) will `[SKIP]` unless `KASTELLAN_GLINER_RELEX_ENABLE=1` is set and the venv + weights are staged — that's expected. Look for `test result: ok. N passed; 0 failed; M ignored` at the bottom.

If a previously-passing test now fails, the most likely cause is a parens / column-order typo in the new SQL. Revert the Task 3 edit and re-apply more carefully.

- [ ] **Step 5: Run the full workspace test suite**

Run:
```sh
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | grep -E "^test result" > /tmp/test_summary.txt
python3 -c "
import re
pass_total=fail_total=ign_total=0
with open('/tmp/test_summary.txt') as f:
    for line in f:
        m = re.search(r'(\d+) passed; (\d+) failed; (\d+) ignored', line)
        if m:
            pass_total += int(m.group(1))
            fail_total += int(m.group(2))
            ign_total += int(m.group(3))
print(f'passed={pass_total} failed={fail_total} ignored={ign_total}')
"
```

**Expected:** `passed=883 failed=0 ignored=4` (the +2 new tests over the 881 baseline on `main` at `028e541`).

- [ ] **Step 6: Commit**

```sh
git add core/src/entity_extraction/gliner_relex.rs
git commit -m "$(cat <<'COMMIT_EOF'
perf(entity_extraction): halve upsert round-trips via xmax=0 discriminator (Issue #90)

Replace the per-entity two-statement upsert (INSERT ... ON CONFLICT
DO NOTHING RETURNING id + follow-up SELECT on conflict) with a
single INSERT ... ON CONFLICT DO UPDATE SET name_norm =
entities.name_norm RETURNING id, (xmax = 0) AS inserted.

For pre-existing entities (the steady-state hot path once the
corpus warms up) this halves the per-entity round-trip cost.
For new entities, the cost is unchanged at one round-trip.

The `SET name_norm = entities.name_norm` self-assignment is the
standard Postgres idiom for forcing RETURNING on conflict without
changing the row's logical state. Critically, it does NOT touch
`quarantine` — operator approvals (per PR #93's quarantine-review
CLI) survive re-extraction. Pinned by
upsert_preserves_operator_unquarantine_decision.

xmax = 0 is the canonical inserted-vs-existed discriminator: a
fresh row has xmax=0 (no future-deleting transaction); a
conflict-hit row carries the conflict transaction's xid.

Scope (per Issue #90 brainstorming):
- Layer A only. Layer B (full-batch unnest) deferred — preserves
  per-entity error attribution at v2's 5-20-entity-per-call scale.
- Triple-loop unchanged.
- Public API unchanged (UpsertOutcome shape byte-identical).
- Audit-row contract unchanged
  (build_extract_entities_payload's 8-key shape preserved).
- No migration; schema untouched.

Workspace test count: 881 → 883 (+2 in #90's TDD-ordered tests).

Spec: docs/superpowers/specs/2026-05-20-issue-90-upsert-round-trip-reduction-design.md
Plan: docs/superpowers/plans/2026-05-20-issue-90-upsert-round-trip-reduction.md

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
COMMIT_EOF
)"
```

---

## Task 4: Session-end docs sync

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

**Why this is Task 4:** Per CLAUDE.md rule #8 and the handover convention, every working session ends with a HANDOVER.md + ROADMAP.md update.

- [ ] **Step 1: Update HANDOVER.md header**

Open `docs/devel/handovers/HANDOVER.md` and update the header. Replace the three header lines (lines ~7-11) to reflect the Issue #90 slice. Suggested header:

```markdown
**Last updated:** 2026-05-20 (Issue #90 — entity-upsert round-trip reduction (Layer A). `xmax = 0` discriminator + no-op SET clause; per-entity upsert goes from 1–2 round-trips down to a guaranteed 1; **workspace 881 → 883 (+2)** with 0 failures / 0 warnings).

**Last commit on `<branch>`:** `<HASH>` (`perf(entity_extraction): halve upsert round-trips via xmax=0 discriminator (Issue #90)`).

**Session-end verification:** **Rust workspace: 883 passed / 0 failed / 4 ignored / 0 warnings on Linux** (`cargo test --workspace` on the DGX, branch `<branch>`). 4 `[SKIP]` lines on `--nocapture` are all GLiNER-Relex real-model tests gated on `KASTELLAN_GLINER_RELEX_ENABLE=1` (operator-driven skip-as-pass).
```

Replace `<HASH>` with the actual commit hash from Task 3's commit (use `git log --oneline -1` to find it). Replace `<branch>` with the current branch name (use `git branch --show-current`).

- [ ] **Step 2: Add "Recently completed (this session)" entry**

Insert a new entry between the `## Recently completed (this session, 2026-05-20 — Operator quarantine-review CLI ...)` entry and its predecessor:

```markdown
## Recently completed (this session, 2026-05-20 — Issue #90 entity-upsert round-trip reduction (Layer A), branch `<branch>`)

Spec at [`docs/superpowers/specs/2026-05-20-issue-90-upsert-round-trip-reduction-design.md`](../../superpowers/specs/2026-05-20-issue-90-upsert-round-trip-reduction-design.md); plan at [`docs/superpowers/plans/2026-05-20-issue-90-upsert-round-trip-reduction.md`](../../superpowers/plans/2026-05-20-issue-90-upsert-round-trip-reduction.md).

**What shipped:**

- Replaced the per-entity two-statement upsert in `upsert_entities_and_relations` with a single `INSERT … ON CONFLICT DO UPDATE SET name_norm = entities.name_norm RETURNING id, (xmax = 0) AS inserted`. Halves the per-entity round-trip cost for pre-existing entities (the steady-state hot path); no change for new entities.
- The `SET name_norm = entities.name_norm` self-assignment is load-bearing: it forces `RETURNING` on conflict without clobbering `quarantine`. Pinned by `upsert_preserves_operator_unquarantine_decision`.
- `(xmax = 0)` is the canonical inserted-vs-existed discriminator; new rows return TRUE, conflict-hit rows return FALSE.
- Public API unchanged (`UpsertOutcome` shape byte-identical). Audit-row contract unchanged (`build_extract_entities_payload` 8-key shape preserved). No migration.

**What's deliberately NOT in this slice:**

- Layer B (full-batch `unnest` upsert) — deferred per operator decision during brainstorming to preserve per-entity error attribution at v2's 5–20-entity-per-call scale. Reopens if observation phase shows RT cost as material.
- Triple-loop batching — same reasoning.

**Test budget delta: 881 → 883 (+2).** Two new mock-tier integration tests in `core/tests/entity_extraction_e2e.rs`:

- `upsert_preserves_operator_unquarantine_decision` — bug-of-omission pin; manually flips `quarantine = FALSE` (simulating operator approval via the quarantine-review CLI), re-upserts, asserts `quarantine` survives.
- `upsert_counts_new_inserts_correctly_in_mixed_batch` — mixed-batch counter pin (one new + one pre-existing in the same call); asserts `n_entities_upserted_new == 1`.

**File-size watch:** `core/src/entity_extraction/gliner_relex.rs` shrinks from 518 → ~505 LOC. Still over the 500-LOC soft cap by a sliver; the natural split (lifting the `#[cfg(test)] mod tests` block to a sibling file) is the same candidate already flagged in v2's HANDOVER entry. No new urgency from this slice.

---
```

Replace `<branch>` with the current branch name.

- [ ] **Step 3: Update the "Working state" test-count narration line**

Find the line starting with `**\`cargo test --workspace\` on Linux:` (around line ~484) and edit it to reflect the new total:

Before:
```
**`cargo test --workspace` on Linux: 881 tests passed, 0 failed, 4 ignored, 0 `[SKIP]` lines, 0 warnings** on `main` at `028e541` ...
```

After:
```
**`cargo test --workspace` on Linux: 883 tests passed, 0 failed, 4 ignored, 0 warnings** on `<branch>` at `<HASH>` (Issue #90 entity-upsert round-trip reduction, Layer A). 4 `[SKIP]` lines on `--nocapture` are GLiNER-Relex real-model tests gated on `KASTELLAN_GLINER_RELEX_ENABLE=1`. The +2 jump from the 881 baseline is Issue #90's two TDD-ordered tests. Earlier checkpoints: **881 on `main` at `028e541`** (PR #93 merged + post-merge polish `6e6e85f`); ...
```

(Keep the rest of the existing "Earlier checkpoints" chain intact — only update the leading line and the immediate predecessor reference.)

- [ ] **Step 4: Update the "Next TODO" priority list**

In the "Next TODO (pick one)" section, find item #8 (Issue #90) under "Next concrete engineering pickups". Strike it out:

Before:
```markdown
    8. **[Issue #90](https://github.com/hherb/kastellan/issues/90) — `upsert_entities_and_relations` per-entity round-trip reduction** (engineering, filed in `2cf2a0a`) — current per-entity `INSERT … ON CONFLICT DO NOTHING RETURNING id` followed by `SELECT` on miss is 2× round-trips for existing entities. Needs `xmax = 0` discriminator pattern + audit-row contract update.
```

After:
```markdown
    8. ~~**[Issue #90](https://github.com/hherb/kastellan/issues/90) — `upsert_entities_and_relations` per-entity round-trip reduction**~~ **SHIPPED 2026-05-20** on branch `<branch>` — Layer A (`xmax = 0` discriminator + no-op `SET name_norm = entities.name_norm` to force RETURNING on conflict). Workspace 881 → **883** (+2). Audit-row contract unchanged (turned out to need no update). Layer B (full-batch unnest) deferred per brainstorming. See "Recently completed (this session)" entry above.
```

Renumber the remaining items if you prefer (not load-bearing).

- [ ] **Step 5: Update ROADMAP.md**

Open `docs/devel/ROADMAP.md`. Add a new bullet under the Phase 1 cont. section (the same section that contains the entity-extraction entries), placed after the operator quarantine-review CLI entry (line ~129):

```markdown
- [x] **Issue #90 — entity-upsert round-trip reduction (Layer A) (2026-05-20)** — branch `<branch>`. Replaced the per-entity two-statement upsert (`INSERT … ON CONFLICT DO NOTHING RETURNING id` + follow-up `SELECT`) with a single `INSERT … ON CONFLICT DO UPDATE SET name_norm = entities.name_norm RETURNING id, (xmax = 0) AS inserted`. Per-entity goes from 1–2 round-trips down to a guaranteed 1. The self-assignment `SET name_norm = entities.name_norm` is the standard Postgres idiom for forcing `RETURNING` on conflict without changing the row's logical state — critically, does NOT touch `quarantine`, so operator approvals (PR #93) survive re-extraction. `xmax = 0` is the canonical inserted-vs-existed discriminator. Public API unchanged (`UpsertOutcome` shape byte-identical). Audit-row contract unchanged (`build_extract_entities_payload` 8-key shape preserved). No migration. Workspace 881 → **883** (+2: `upsert_preserves_operator_unquarantine_decision` + `upsert_counts_new_inserts_correctly_in_mixed_batch` in `core/tests/entity_extraction_e2e.rs`). Layer B (full-batch unnest) and triple batching deferred per brainstorming. Spec at `docs/superpowers/specs/2026-05-20-issue-90-upsert-round-trip-reduction-design.md`; plan at `docs/superpowers/plans/2026-05-20-issue-90-upsert-round-trip-reduction.md`.
```

Replace `<branch>` with the current branch name.

- [ ] **Step 6: Commit the docs sync**

```sh
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "$(cat <<'COMMIT_EOF'
docs(handover,roadmap): Issue #90 entity-upsert round-trip reduction (Layer A) — session-end sync

- HANDOVER header bumps test count 881 → 883 (+2).
- New "Recently completed (this session)" entry documents the
  Layer A slice: xmax=0 discriminator, no-op SET clause to force
  RETURNING on conflict, quarantine-preservation invariant, and
  the two TDD-ordered tests that pin both the load-bearing
  invariant and the mixed-batch counter.
- "Next TODO" item #8 struck through and marked SHIPPED.
- ROADMAP Phase 1 cont. gains a new bullet mirroring the
  HANDOVER entry.

No code changes in this commit; documentation sync only.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
COMMIT_EOF
)"
```

---

## Self-Review Findings

Ran the self-review against the spec:

**1. Spec coverage:** Every section of the spec maps to a task:
- "The SQL change" → Task 3 (Step 1 contains the verbatim new SQL).
- "Side-effects of the no-op UPDATE" → documented inline in Task 3's code-block comment.
- "Rust changes" → Task 3 (the verbatim Rust block).
- "Tests" — both new tests → Tasks 1 + 2 (each contains the verbatim test code).
- "Preserved unchanged" tests → Task 3 Step 4 verifies the touched file as a whole stays green.
- "Verification" → Task 3 Step 5 (full workspace `cargo test --workspace`).
- "What this slice deliberately does NOT do" → Task 3 commit message explicitly enumerates the non-goals.
- "File-size watch" → Task 4 Step 2 surfaces the LOC change in the HANDOVER entry.

**2. Placeholder scan:** No TBD / TODO / "implement later" / "handle edge cases" / "similar to Task N" / "write tests for the above" — every test body, SQL block, comment text, and commit message is verbatim.

**3. Type consistency:** All call sites use `(i64, bool)` for the new RETURNING tuple. `n_new` stays `u32`. `entity_ids` stays `Vec<i64>`. `query_as` (not `query_scalar`) is the correct sqlx call for the multi-column return. No drift across tasks.

**4. Test names:** Both new tests use distinct `bring_up_pg("<label>")` labels (`"preserve-quar"`, `"mixed"`) so they get distinct Postgres cluster directory names and can run in parallel under `cargo test`. Existing tests use 4-letter labels (`"quar"`, `"idem"`, `"dedup"`) so longer labels are also fine; checked against the cluster-name length (`kastellan-supervisor-test-pg-extract-{label}-{suffix}` — generous limit).
