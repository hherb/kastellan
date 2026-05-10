# Scheduler / CASSANDRA — Session Handover

> Resume point for the Phase 1 scheduler implementation. Read this
> first, then the spec, then the plan, then continue task-by-task
> under `superpowers:subagent-driven-development`.

**Last updated:** 2026-05-10 (end of session — Phase 1 complete)
**Branch:** `worktree-scheduler-phase1` (worktree at
`.claude/worktrees/scheduler-phase1`)
**Last commit on branch:** `b125e46` `test(db): tasks_lifecycle_e2e —
claim, finalize, mark_cancelled, sweep_crashed, NOTIFY round-trips`

---

## Read these first

1. **Spec** —
   [`docs/superpowers/specs/2026-05-10-scheduler-design.md`](../../superpowers/specs/2026-05-10-scheduler-design.md).
   The implementation contract.
2. **Plan** —
   [`docs/superpowers/plans/2026-05-10-scheduler.md`](../../superpowers/plans/2026-05-10-scheduler.md).
   Step-by-step tasks. **Phase 1 (Tasks 1.1–1.10) is done. Resume at Task 2.1.**
3. **Project memory** —
   [`HANDOVER.md`](HANDOVER.md) for general project state;
   [`docs/devel/ROADMAP.md`](../ROADMAP.md) for the phase view.

## How to resume

1. Enter the worktree (use the native `EnterWorktree` tool, pass
   `path: "/Users/hherb/src/hhagent/.claude/worktrees/scheduler-phase1"`).
   The branch is `worktree-scheduler-phase1`.
2. Re-read the spec + plan above.
3. Invoke `superpowers:subagent-driven-development` to continue the
   subagent-per-task + two-stage-review flow (this is what shipped
   Phase 1).
4. The next task is **Task 2.1 — `prompts/agent_planner.md`**.
5. Phase 1 ended on a green build with the integration test
   passing-by-skip on macOS. On the DGX (Linux + PG installed) the
   integration test must actually pass — verify with
   `cargo test -p hhagent-db --test postgres_e2e tasks_lifecycle_e2e -- --nocapture`
   before continuing.

## Phase 1 — what shipped (15 commits)

- Migrations `0005_tasks_scheduler.sql` (lanes, lease, expanded state
  CHECK, three NOTIFY triggers, GRANT shape with REVOKE DELETE on
  tasks) and `0006_agent_prompts.sql` (append-only-by-GRANT prompt
  ledger).
- `db::tasks` — `Lane` enum + lane-default constants, `Task` struct,
  `decode_task_row` helper (reuse for read fns), full CRUD:
  `insert_pending`, `claim_one` (FOR UPDATE SKIP LOCKED), `finalize`,
  `observe_state`, `mark_cancelled`, `mark_failed_running`,
  `sweep_crashed`, `increment_plan_count`, `get`, `list`.
- `db::agent_prompts` — `hash_content` (SHA-256 hex),
  `upsert_prompt`, `get_by_hash`.
- `core::cassandra::types` — `DataClass` (with Ord/PartialOrd),
  `Severity` (with Ord/PartialOrd), `PlannedStep`, `Plan` (with
  `is_terminal()` and `skip_serializing_if` on `result`), `Verdict`
  (5-variant), `DECISION_TERMINAL` constant, `Eq + Hash` where
  needed.
- `core::cassandra::review` — `ReviewStage` trait, `ReviewStageContext`,
  `ChainReviewStage` (first-non-Approve short-circuit), three stub
  stages (`ConstitutionalGuard`, `DeterministicPolicy`,
  `NoopReviewStage`).
- Integration test `tasks_lifecycle_e2e` (in
  `db/tests/postgres_e2e.rs`) — full lifecycle round-trip + NOTIFY
  assertions on both `tasks_inserted` and `tasks_completed`.
- Workspace deps added: `async-trait` (workspace + core).

## Conventions established in Phase 1 — keep using these

- **Error mapping:** every `.map_err` site uses a closure with named
  operation context, mirroring `db/src/audit.rs::insert`. Pattern:
  `format!("<module> <op>: {e}")` for query errors,
  `format!("decode <module>.<field>: {e}")` for `try_get` failures.
  No bare `.map_err(DbError::from)` in scheduler code.
- **`decode_task_row` helper** in `db/src/tasks.rs` is the canonical
  row → `Task` decoder. Reuse it; don't inline.
- **`DECISION_TERMINAL`** (re-exported from `core::cassandra`) is the
  protocol-level terminal sentinel. Inner loop, planner prompt, and
  audit-log payloads must all reference this constant — never the
  literal string `"task_complete"`.
- **Stage names are audit-log contract.** `"stage--1"`, `"stage-0"`,
  `"chain"`, `"noop"` are pinned by `stage_names_are_stable` test in
  `core/src/cassandra/review.rs`. Renaming any is a breaking change.
- **NOTIFY trigger functions** use `CREATE OR REPLACE FUNCTION ...
  LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$...$$;`
  + `DROP TRIGGER IF EXISTS ... ON tasks; CREATE TRIGGER ...` for
  idempotency. See `0003_audit_log_notify.sql` and the post-fix
  `0005_tasks_scheduler.sql`.
- **Append-only-by-GRANT** for `tasks`, `audit_log`, `agent_prompts`:
  runtime role gets `SELECT, INSERT` only on append-only tables, plus
  `UPDATE` where state machines need it. `REVOKE DELETE` is the
  explicit anti-pattern signal.
- **Migration numbering:** next free is **`0007`**. Use it for the
  next migration that lands.
- **Plan structure:** every CHANGE-REQUESTED review must produce a
  `fix(<area>): ...` commit, then a re-review. Don't skip re-reviews
  even for one-line fixes.

## Scope reminders for upcoming phases

- **CASSANDRA stages stay as stubs** through Phase 5 of this plan.
  This is **deliberate** — the user wants baseline performance
  measurements + observed agent failure modes before designing real
  Stage -1 / Stage 0 rules. See spec §6.1 ("Why stubs and not real
  implementations") and §9 (observation phase).
- **No auto-recall** in the scheduler. The agent requests
  `memory::recall` as an explicit tool step when it needs prior
  context. Spec §6.4.
- **Single-clock lease** — `tasks.lease_expires_at` is set at claim
  and never extended. Crashed-task recovery latency is bounded by
  the lane's deadline (30 min on long lane). Spec §3.4.
- **Crash recovery is fail-loud** — on daemon restart, sweep marks
  expired-lease running tasks as `crashed`. **Never auto-resume.**
  The user (or operator via `hhagent-cli tasks fail`) decides whether
  to resubmit. Spec §3.6, decisions log row 7.

## Notable judgment calls during the run

- Two implementer-level additions to `db::lib::DbError`:
  `Other(String)` variant and `From<sqlx::Error>` impl. Both minimal,
  both reviewed and accepted.
- `claim_one` was refactored to extract `decode_task_row`, used by
  `get` and `list`. Small DRY win that pays off in Phase 3+.
- `DataClass` and `Severity` got `Ord/PartialOrd` derives so the
  Stage 0 invariant (`data_ceiling >= classification_floor`) reads
  naturally. The `rank()` helper on `DataClass` was kept as a
  documented convenience.
- `Plan.result` ended up with both `#[serde(default)]` and
  `#[serde(skip_serializing_if = "Option::is_none")]` so `None` is
  fully absent from the wire — pinned by an updated test.

## What's still ahead (15 tasks)

| Phase | Tasks | What lands |
|---|---|---|
| 2 | 2.1 – 2.5 | `prompts/agent_planner.md`, `PromptCache`, `PlanFormulator`, inner loop, integration test |
| 3 | 3.1 – 3.4 | Lane runners + crash recovery, `main.rs` wiring, `ToolHostStepDispatcher`, two integration tests |
| 4 | 4.1 – 4.4 | `hhagent-cli` subcommands (ask, tasks list/status/cancel/fail/tail), CLI integration test |
| 5 | 5.1 – 5.3 | Prompt-ledger e2e, ROADMAP + main HANDOVER updates |
| – | Final review | One-shot review of the full scheduler implementation |

When the implementation is green and merged: hold for the
**observation phase** (spec §9) before swapping the stub stages for
real `ConstitutionalGuard` + `DeterministicPolicy`.
