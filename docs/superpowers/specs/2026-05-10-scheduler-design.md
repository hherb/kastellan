# Scheduler — Design Spec

**Date:** 2026-05-10
**Status:** Approved design, ready for implementation plan
**Scope:** The agent loop / scheduler for Phase 1 of [ROADMAP.md](../../devel/ROADMAP.md). Tasks-table-drain model, two concurrent lanes, iterative replanning per task, CASSANDRA review pipeline scaffolded in (with stub stages for the experimental observation phase that follows ship), prompt-traceability ledger end-to-end.

**Related:**
- [docs/architecture.md](../../architecture.md) — invariants this builds on (dispatcher chokepoint, audit-log append-only, process-per-worker)
- [docs/threat-model.md](../../threat-model.md) — threat boundaries
- [docs/cassandra_design_plan.md](../../cassandra_design_plan.md) — the upstream design this scheduler accommodates as a structural seam
- [docs/superpowers/plans/read-docs-cassandra-design-plan-md-caref-polymorphic-squirrel.md](../plans/read-docs-cassandra-design-plan-md-caref-polymorphic-squirrel.md) — the assessment that surfaced the sequencing question

---

## 1. Context

The roadmap line under [Phase 1 — Memory & Loop](../../devel/ROADMAP.md) reads: *"`scheduler` agent loop: tick → drain channel bus → next task → LLM call → tool calls → repeat."* That sentence is directionally right but underspecified, and several premises (channel bus, embedding worker, full CASSANDRA pipeline) are not yet in place. This spec fills it in.

The design objective is correctness, not throughput or latency. The user's framing: *"This is a deeply personal product which hopefully will be useful to others too, but emphasis is getting it right and not developing ourselves into a dead end for wishing to ship sooner."* No real-world use beyond test data is planned during the implementation horizon of this spec.

Workload character: a senior emergency physician's personal agent. Interrupt-driven, asymmetric task durations (sub-second status checks interleaved with multi-minute document summarisations), eventual handling of `ClinicalConfidential` pathology and radiology data. The scheduler has to accommodate that asymmetry without degrading short tasks behind long ones, and has to be born CASSANDRA-aware so the security model can mature in the same code path rather than being retrofitted.

## 2. Architecture overview

```
                                  ┌──────────── hhagent (daemon process) ───────────────┐
                                  │                                                       │
   producer (CLI / channel /      │                                                       │
   scheduled routine)             │  ┌──────── scheduler ───────────────────────────┐    │
       │                          │  │                                                │    │
       │ INSERT tasks(state=pending,                                                  │    │
       │              lane='fast'|'long',                                             │    │
       │              payload={...})                                                  │    │
       │                          │  │  ┌─ lane_fast runner ──┐  ┌─ lane_long runner ─┐ │  │
       ▼                          │  │  │ PgListener          │  │ PgListener         │ │  │
   ┌────────┐                     │  │  │  ├ tasks_inserted   │  │  ├ tasks_inserted  │ │  │
   │ tasks  │  ◄─── NOTIFY ────── │  │  │  └ tasks_cancelled  │  │  └ tasks_cancelled │ │  │
   │ table  │                     │  │  │ claim_one (FOR UPDATE SKIP LOCKED)         │ │  │
   └────────┘  ──── NOTIFY ────►  │  │  │ run_to_terminal:                            │ │  │
       ▲   tasks_completed        │  │  │   formulate_plan ─► review ─► dispatch[]   │ │  │
       │                          │  │  │   replan if not terminal                   │ │  │
       │                          │  │  │ finalize (UPDATE state, audit, NOTIFY)     │ │  │
       │ UPDATE  tasks SET state= │  │  └─────────────────────────────────────────────┘ │  │
       │ 'cancelled'              │  │                  │                                │  │
       │ (CLI cancel)             │  │                  ▼                                │  │
       │                          │  │   tool_host::dispatch (existing chokepoint, Option M)
   hhagent-cli ask                │  │                  │                                │  │
   hhagent-cli tasks ...          │  │                  ▼                                │  │
                                  │  │   bwrap / sandbox-exec → worker process            │  │
                                  │  └────────────────────────────────────────────────────┘  │
                                  │                                                       │
                                  │  ┌── audit_mirror (existing, Option I) ──────────────┐  │
                                  │  │ PgListener audit_log_inserted → JSONL on disk     │  │
                                  │  └────────────────────────────────────────────────────┘  │
                                  └────────────────────────────────────────────────────────┘
```

**Invariants this preserves**

- **Dispatcher chokepoint** ([core/src/tool_host.rs](../../../core/src/tool_host.rs), Option M). Every tool call goes through `tool_host::dispatch`; `WorkerCommand` is sealed by a compile-fail doctest. The scheduler never constructs a `WorkerCommand` directly.
- **Audit-log append-only at the DB-role layer** (Option L). The `hhagent_runtime` role has `SELECT, INSERT` on `audit_log`, never `UPDATE/DELETE`.
- **Process-per-worker, sandbox-per-worker.** Each task gets its own `Workspace` and dispatches to its own worker process; the two lanes do not share workers.
- **No in-process untrusted code.**

## 3. Task schema and lifecycle

### 3.1 Migration `db/migrations/0005_tasks_scheduler.sql`

```sql
ALTER TABLE tasks
    ADD COLUMN lane TEXT NOT NULL DEFAULT 'fast'
        CHECK (lane IN ('fast', 'long')),
    ADD COLUMN result JSONB,
    ADD COLUMN started_at      TIMESTAMPTZ,
    ADD COLUMN finished_at     TIMESTAMPTZ,
    ADD COLUMN lease_expires_at TIMESTAMPTZ,
    ADD COLUMN plan_count INT NOT NULL DEFAULT 0;

ALTER TABLE tasks DROP CONSTRAINT tasks_state_check;
ALTER TABLE tasks
    ADD CONSTRAINT tasks_state_check CHECK (state IN
        ('pending','running','completed','failed','cancelled',
         'blocked','timed_out','crashed'));

DROP INDEX tasks_state_created_at_idx;
CREATE INDEX tasks_lane_state_created_at_idx
    ON tasks (lane, state, created_at);

CREATE FUNCTION notify_task_inserted() RETURNS trigger AS $$
BEGIN
    PERFORM pg_notify('tasks_inserted', NEW.id::text);
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER tasks_notify_inserted
    AFTER INSERT ON tasks FOR EACH ROW
    EXECUTE FUNCTION notify_task_inserted();

CREATE FUNCTION notify_task_cancelled() RETURNS trigger AS $$
BEGIN
    IF NEW.state = 'cancelled' AND OLD.state <> 'cancelled' THEN
        PERFORM pg_notify('tasks_cancelled', NEW.id::text);
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER tasks_notify_cancelled
    AFTER UPDATE OF state ON tasks FOR EACH ROW
    EXECUTE FUNCTION notify_task_cancelled();

CREATE FUNCTION notify_task_completed() RETURNS trigger AS $$
BEGIN
    IF NEW.state IN ('completed','failed','cancelled','blocked','timed_out','crashed')
       AND OLD.state NOT IN ('completed','failed','cancelled','blocked','timed_out','crashed') THEN
        PERFORM pg_notify('tasks_completed', NEW.id::text);
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER tasks_notify_completed
    AFTER UPDATE OF state ON tasks FOR EACH ROW
    EXECUTE FUNCTION notify_task_completed();

GRANT SELECT, INSERT, UPDATE ON tasks TO hhagent_runtime;
GRANT USAGE, SELECT ON SEQUENCE tasks_id_seq TO hhagent_runtime;
```

### 3.2 `payload` shape

Opaque to the schema; contract enforced in Rust:

```json
{
    "kind": "ask",
    "instruction": "summarise this report",
    "deadline_seconds": 60,
    "max_plans": 12,
    "classification_floor": "Personal"
}
```

`kind` is one of `ask | channel_event | routine | ...` (extension point for future producers). All other fields optional; lane defaults apply when absent.

### 3.3 State machine

```
pending ──► running ──► { completed | failed | cancelled
                       | blocked | timed_out | crashed }
```

All terminal. Every transition writes one `audit_log` row (`actor='scheduler'`, `action='task.<state>'`, `payload={task_id, lane, plan_count}`).

### 3.4 Lease semantics — single clock

`lease_expires_at` is set at claim time to `now() + lane_deadline` and **never extended**. Wall-clock deadline and crash-liveness collapse into one column. Cost: a crashed long task sits in `state='running'` for up to `lane_deadline_long` (default 30 min) before a startup sweep reclaims it as `crashed`. Recovery latency is operational, not user-blocking; `hhagent-cli tasks fail <id>` is the manual override.

### 3.5 Atomic claim

```sql
-- Illustrative; actual sqlx call binds $lane and a Rust-computed timestamp.
UPDATE tasks
   SET state = 'running',
       started_at = now(),
       lease_expires_at = $now_plus_deadline
 WHERE id = (
     SELECT id FROM tasks
      WHERE lane = $lane AND state = 'pending'
      ORDER BY created_at ASC
      LIMIT 1
      FOR UPDATE SKIP LOCKED
 )
 RETURNING id, payload, lane, lease_expires_at;
```

`FOR UPDATE SKIP LOCKED` is the long-standing PG queue idiom; the per-lane filter means the two lane runners never race over the same row.

### 3.6 Crash sweep on startup

Before either lane runner enters its loop, the daemon runs once:

```sql
UPDATE tasks
   SET state = 'crashed', finished_at = now()
 WHERE state = 'running' AND lease_expires_at < now();
```

The sweep is idempotent and safe to re-run.

## 4. Lane runners

Two long-lived tokio tasks, one per lane, spawned by `core::scheduler::spawn_scheduler` from `main.rs` after the database pool is up.

```rust
pub struct SchedulerHandle {
    shutdown: tokio::sync::watch::Sender<bool>,
    fast: tokio::task::JoinHandle<()>,
    long: tokio::task::JoinHandle<()>,
}

pub fn spawn_scheduler(
    pool: PgPool,
    router: Arc<llm_router::Router>,
    sandbox: Arc<dyn SandboxBackend>,
    workspace_root: PathBuf,
    review: Arc<dyn ReviewStage>,
    prompts: Arc<PromptCache>,
) -> SchedulerHandle;
```

Per-lane loop:

```text
listener = PgListener::connect(pool)
listener.listen("tasks_inserted")
listener.listen("tasks_cancelled")
loop {
    select! {
        _ = shutdown.changed()    => break,
        _ = listener.recv()       => fall through,
        _ = sleep(30s)            => fall through,   // heartbeat for missed-NOTIFY
    }
    while let Some(task) = claim_one(pool, lane).await? {
        let outcome = run_to_terminal(task).await;
        finalize(pool, task.id, outcome).await;
    }
}
```

`finalize` writes the terminal `state` + `result` + `finished_at`, INSERTs the matching `audit_log` row, and the trigger from §3.1 emits `pg_notify('tasks_completed', ...)`.

**Daemon shutdown.** SIGTERM flips the watch channel. Each lane runner stops claiming new tasks and kills the in-flight worker (existing `SupervisedWorker` Drop kills the process and watchdog), then exits *without* writing a terminal state. The next daemon startup's sweep finds the row with `state='running' AND lease_expires_at < now()` and marks `crashed`. Same path as a hard crash.

**Cancellation NOTIFY.** Listening on `tasks_cancelled` lets a cancel request pre-empt the per-iteration poll for cases where a long step is mid-execution. The inner loop additionally polls `state` between plan iterations (§5.4).

## 5. Inner loop (`run_to_terminal`)

### 5.1 `TaskContext`

```rust
struct TaskContext {
    task_id: i64,
    lane: Lane,
    instruction: String,                   // payload.instruction; immutable
    classification_floor: DataClass,       // pinned by producer
    plans: Vec<(Plan, Vec<StepOutcome>)>,  // every iteration's plan + step results
    advisories: Vec<String>,               // accumulated CASSANDRA Advisory concerns
    blocks: Vec<String>,                   // accumulated Block reasons (replan inputs)
    plan_count: u32,                       // mirrored to tasks.plan_count
}
```

### 5.2 Loop structure

```text
loop {
    if cancelled?(pool, task_id)              → return Cancelled
    if plan_count >= cap_for(lane)            → return Failed("plan_iteration_cap_exceeded")

    ctx.ensure_budget(&router).await?         // §6.3 stub in v0
    plan = agent::formulate_plan(&router, &prompts, &ctx).await?
    ctx.plan_count += 1
    persist plan_count to tasks.plan_count
    audit "plan.formulate" with timing payload (§7)

    verdict = review.review(&plan, &ctx).await
    audit "cassandra:<stage>" with verdict + latency
    match verdict {
        ConstitutionalBlock(p)   → return Blocked { principle: p }
        Block(reason)            → ctx.blocks.push(reason); continue
        Escalate(_, _)           → /* v0: no channel bus; treat as Block(reason) */
        Advisory(concern)        → ctx.advisories.push(concern)
        Approve                  → /* proceed */
    }

    if plan.is_terminal()                     → return Completed(plan.result)

    let mut outcomes = vec![];
    for step in &plan.steps {
        if cancelled?(pool, task_id)          → return Cancelled
        let outcome = dispatch(pool, &mut worker, step.into()).await;
        outcomes.push(outcome.clone());
        if outcome.is_err() { break; }        // exit plan; agent replans next iter
    }
    audit "plan.outcome" { plan_count, terminal_kind }
    ctx.plans.push((plan, outcomes));
}
```

### 5.3 `Plan` shape

```rust
pub struct Plan {
    pub context:   String,        // §16.1: 1–3 sentence situation summary
    pub decision:  String,        // single sentence; "task_complete" is the terminal sentinel
    pub rationale: String,        // why this approach
    pub steps:     Vec<PlannedStep>,
    pub result:    Option<Value>, // present iff decision == "task_complete"
    pub data_ceiling: DataClass,  // max classification touched by any step
}

// Invariant (enforced by future Stage 0 — not by stubs in this work's scope):
//   plan.data_ceiling >= task.classification_floor
// i.e. outputs cannot be classified *below* the producer-pinned floor without
// passage through an anonymiser/declassifier (anonymiser is out of scope here).
// The floor is a producer-set minimum on outputs; the ceiling is the inferred
// maximum classification of any input the plan touches.

pub struct PlannedStep {
    pub tool:       String,           // e.g. "shell-exec", "document-reader"
    pub method:     String,           // JSON-RPC method on the worker
    pub parameters: Value,
    pub returns:    String,           // human-readable description of expected output
    pub done_when:  String,           // observable completion criterion
    pub classification: DataClass,    // classification of this step's output
}
```

`is_terminal()` returns `true` iff `decision == "task_complete"` AND `steps.is_empty()` AND `result.is_some()`. The reviewer trivially Approves terminal plans (no actions to evaluate); the loop returns `Completed(result)`. This is the only happy-path exit.

The agent emits `Plan` as JSON via the LLM router with strict JSON-mode. Output is validated against a schema; failure → `RouterError::DecodeResponse` → retry policy (§5.5).

### 5.4 Cancellation polling

Two checks per iteration: top of loop, and before each step. One cheap `SELECT state FROM tasks WHERE id=$1` per plan boundary (~seconds apart). The lane runner's `tasks_cancelled` LISTEN provides the wake-up; this poll provides the actual decision point inside the loop.

### 5.5 Retry policy

Inside `formulate_plan` (LLM call):

| Error category | Action |
|---|---|
| `RouterError::Transport` (network down) | Exponential backoff, max 3 attempts |
| `RouterError::HttpStatus(5xx)` | Exponential backoff, max 3 attempts |
| `RouterError::HttpStatus(4xx)` | Permanent, return `Outcome::Failed("llm_4xx: …")` |
| `RouterError::DecodeResponse` (JSON drift / schema mismatch) | Permanent, return `Outcome::Failed("llm_decode: …")` |
| `RouterError::PolicyDeniedFrontier` | Not reachable in this spec's scope (`DefaultLocalPolicy` always picks `Local`) |

No retry at the loop level — replanning *is* the retry shape.

### 5.6 Failure handling matrix

| Trigger | Outcome | Replan? |
|---|---|---|
| Tool call returns error / non-zero exit | Step result encoded as `{"error": "...", "detail": "..."}`, plan exits, agent sees error in next iteration | Yes, bounded by plan-iteration cap |
| Tool call sandbox-denies (`POLICY_DENIED`) | Same as above | Yes |
| Worker times out (existing `WorkerSpec.wall_clock_ms`) | Same as above | Yes |
| LLM transient error | Backoff + retry inside `formulate_plan` | (handled below loop level) |
| LLM permanent error (4xx, decode) | `Outcome::Failed("llm_…")` | No |
| `Verdict::Block(reason)` | `reason` appended to `ctx.blocks`, replan | Yes, bounded by plan-iteration cap |
| `Verdict::ConstitutionalBlock` | `Outcome::Blocked { principle }`; task terminal | Never |
| `Verdict::Escalate` | This spec's scope: same as `Block` (no channel bus yet). Future: pause + Tier-2 channel notification. | Future-yes after pause |
| `Verdict::Advisory` | `concern` appended to `ctx.advisories`, plan still executes | (no need) |
| `plan_count >= cap` | `Outcome::Failed("plan_iteration_cap_exceeded")` | No |
| `tasks.state = 'cancelled'` polled | Worker killed, `Outcome::Cancelled` | No |
| Wall-clock deadline (lease_expires_at) | Worker killed, `Outcome::TimedOut` | No |
| Daemon crash mid-task | Lease eventually expires, startup sweep marks `crashed`. **No auto-resume.** Producer/operator decides whether to re-submit. | Never |

### 5.7 Lane defaults

| Setting | `fast` | `long` |
|---|---|---|
| Plan-iteration cap | 3 | 12 |
| `lease_expires_at` (= deadline) | 60 s | 30 min |

Overridable per-task via `payload.max_plans` and `payload.deadline_seconds`.

## 6. Seams: CASSANDRA, prompts, context, recall

### 6.1 CASSANDRA review pipeline (scaffolded)

Module `core/src/cassandra/`. Trait + types + `ChainReviewStage` ship in production wiring; `ConstitutionalGuard` and `DeterministicPolicy` ship as stubs that always return `Approve`. They emit a real `cassandra:<name>` audit-log row with their `latency_ms`, so the observation phase (§7) sees the timing slot even though the verdict is trivial.

```rust
pub trait ReviewStage: Send + Sync {
    fn name(&self) -> &str;
    async fn review(&self, plan: &Plan, ctx: &TaskContext) -> Verdict;
}

pub enum Verdict {
    Approve,
    Advisory(String),
    Escalate(String, Severity),
    Block(String),
    ConstitutionalBlock { principle: u8, reason: String },
}

pub struct ChainReviewStage(Vec<Arc<dyn ReviewStage>>);
// Iterates; first non-Approve verdict wins; emits one cassandra:<name> row per stage call.

pub struct ConstitutionalGuard;   // stub — always Approve in this spec's scope
pub struct DeterministicPolicy;   // stub — always Approve in this spec's scope
pub struct NoopReviewStage;       // test seam only
```

**Why stubs and not real implementations** (deliberate, not a scope cut): the user wants to (a) measure agent-loop performance and turnaround under load *without* CASSANDRA in the path, so CASSANDRA's overhead becomes observable as a clean delta when real stages land; (b) evaluate multiple local models for performance/quality, isolated from reviewer variability; (c) observe what failure modes the agent actually exhibits empirically, so the real Stage -1 / Stage 0 rules are *informed* by observed failures rather than theorised ones.

The eventual real implementations replace the structs in place — no scheduler-side change.

### 6.2 Prompt-traceability ledger

#### Migration `db/migrations/0006_agent_prompts.sql`

```sql
CREATE TABLE agent_prompts (
    sha256          CHAR(64) PRIMARY KEY,
    name            TEXT NOT NULL,
    content         TEXT NOT NULL,
    first_loaded_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX agent_prompts_name_idx
    ON agent_prompts (name, first_loaded_at DESC);

GRANT SELECT, INSERT ON agent_prompts TO hhagent_runtime;
-- never UPDATE/DELETE; same append-only shape as audit_log
```

#### Source of truth: `prompts/*.md` in git

`prompts/agent_planner.md` ships in this work. It is a code-reviewed file (PR + commit). It includes the constitutional principles list inline (per [§16.1 of the upstream design](../../cassandra_design_plan.md)) so the agent emits plans aware of the rules — even though the stub stages don't yet enforce them.

#### Runtime ledger

At daemon startup, each prompt file is read, SHA-256'd, and upserted into `agent_prompts` (`INSERT … ON CONFLICT (sha256) DO NOTHING`). The runtime caches `(name → (sha256, content))` in `Arc<PromptCache>` shared by both lane runners.

#### Audit linkage

Every `plan.formulate` audit-log row's payload carries `{prompt_name, prompt_sha256}`. CASSANDRA's eventual reviewer code reads prompt content via `SELECT content FROM agent_prompts WHERE sha256 = $1` — no filesystem dependency, queryable for forensic correlation.

#### Change discipline

Editing a prompt file is a commit + daemon restart. Old `agent_prompts` rows persist forever (forensic record). When real Stage 4 longitudinal landing happens later, it can issue an Advisory if the agent's first plan under a fresh prompt hash diverges from prior baseline.

### 6.3 Context manager

`TaskContext::ensure_budget(&Router) -> Result<(), CtxOverflow>` is called at the top of each plan iteration, before `formulate_plan`. **Stub in this work** — always returns `Ok(())`. Real implementation later: count tokens via the router, if `serialised_context > model_max_tokens * 0.7`, run a summarisation pass — write the summary as a memory row (with the task's `classification_floor`), drop older `plans` entries from `TaskContext` and replace with a reference. This is the [Phase 1](../../devel/ROADMAP.md) `context_manager` and "reset snapshot writer" items.

If a task exceeds the local model's context window, the next `formulate_plan` returns a `RouterError` and the task ends `failed`. Acceptable: typical local context windows (32 k+) versus a 12-plan iteration cap means other limits trigger first.

### 6.4 Memory recall — deliberate non-default

`memory::recall` (Option N for lexical+semantic; Option O for the embedding worker, still ahead) is **not** auto-invoked by the scheduler. The agent's `instruction` is its entire input. If the agent needs prior context, it requests it as an explicit step (`tool: memory-recall`) when its plan needs it — costs one tool dispatch, shows up in the plan, CASSANDRA reviews what is being recalled.

Rationale: never bloat the context window by default. The user (via the producer payload) and the agent (via plan steps) are the only paths that introduce prior context.

## 7. Instrumentation

To support the post-implementation observation phase (§9), audit-log payload schemas are pinned now rather than retrofitted later.

| Audit row | `actor` | `action` | Payload (JSONB) |
|---|---|---|---|
| Task lifecycle transition | `scheduler` | `task.<state>` | `{task_id, lane, plan_count}` |
| Plan formulation | `agent` | `plan.formulate` | `{task_id, plan_count, prompt_name, prompt_sha256, llm_model, llm_backend, latency_ms, retry_count, plan_step_count, decision_kind: "act"\|"task_complete"}` |
| Review verdict (per stage) | `cassandra:<stage_name>` | `verdict` | `{task_id, plan_count, verdict_kind, latency_ms}` (`latency_ms ≈ 0` under stubs) |
| Tool dispatch (existing, Option I) | `tool:<name>` | `<method>` | `{req, result\|err, ms}` (already pinned) |
| Plan iteration outcome | `scheduler` | `plan.outcome` | `{task_id, plan_count, terminal_kind: ok\|err\|cancel\|timeout\|block, steps_executed, steps_total}` |
| Task finalize | `scheduler` | `task.finalize` | `{task_id, lane, state, plan_count, total_llm_calls, total_dispatch_calls, total_duration_ms, started_at, finished_at}` |

Payloads stay under the [Option I `PAYLOAD_MAX_BYTES = 4096` envelope](../../../db/src/audit.rs); oversized payloads (e.g., a plan with hundreds of steps) get the SHA-256 + length + truncation marker for free.

## 8. CLI / producer surface

Extends the existing `hhagent-cli` binary (which currently has only `audit tail`).

| Subcommand | Behaviour |
|---|---|
| `ask "<instruction>" [--long]` | Default lane `fast`. Insert `pending` row, LISTEN-before-INSERT for `tasks_completed` to avoid race, block until terminal, print `result.body` if `kind=text`, exit 0 on `completed` else non-zero. SIGINT → UPDATE state='cancelled' → exit non-zero. |
| `tasks list [--lane fast\|long] [--state running\|...] [-n 20]` | Recent tasks, FIFO, one per line: `id state lane created_at instruction_summary`. |
| `tasks status <id>` | Single task: state, lane, plan_count, started_at, finished_at, lease_expires_at, payload, result, last-N audit rows. |
| `tasks cancel <id>` | `UPDATE tasks SET state='cancelled' WHERE id=$1 AND state IN ('pending','running')`. Exit 0 if a row was updated. |
| `tasks fail <id>` | Manual escape hatch: `UPDATE tasks SET state='crashed' WHERE id=$1 AND state='running' AND lease_expires_at > now()`. Loud-fails an apparently-stuck task before its lease elapses. |
| `tasks tail <id>` | Filtered variant of `audit tail`: stream JSONL rows where `payload->>'task_id' = '<id>'`. |

`ask` blocking has no CLI-side timeout; wall-clock is enforced by the daemon via the lease. CLI dies → task continues running → terminal state is observable via `tasks status` later. The right asymmetry for tasks-table-drain.

**Output convention.** `tasks.result` is JSONB; `{kind: "text", body: "..."}` is the convention for ask-shaped tasks. `hhagent-cli ask` pretty-prints `kind=text`; falls back to JSON dump for unknown kinds.

**Future producers** (no scheduler-side change required):
- Phase 2 channel adapter writes `{kind: 'channel_event', source: 'telegram', message_id: …, instruction: …, classification_floor: 'Personal'}` with `lane='long'` default.
- Future scheduled routines materialise via a small `routines` table → spawned `tasks` rows on schedule.

## 9. Sequence and observation phase

Dependency-ordered commits. Each commit ships the *correct* thing for its scope; workspace stays green at every commit.

1. **Schema + types** — migrations `0005`, `0006`; `core/src/cassandra/{mod,types}.rs`; `db/src/tasks.rs`; `db/src/agent_prompts.rs`. Unit tests for the SQL builders, trait shape, ChainReviewStage short-circuit semantics, agent_prompts upsert idempotence. One integration test (`tasks_lifecycle_e2e`) drives the lifecycle SQL and NOTIFY triggers against a per-test PG cluster.

2. **Inner loop** — `core/src/scheduler/{inner_loop,agent,prompts}.rs` + `prompts/agent_planner.md`. `spawn_scheduler` is callable but lane runners are stubbed to a no-op pending commit 3. Integration test (`scheduler_inner_loop_e2e`) drives the inner loop with a scripted-router stub through: happy path; tool-fail-then-replan; plan-cap exhaustion; cancel mid-flight.

3. **Lane runners + crash recovery** — real lane runners with `PgListener` on `tasks_inserted` and `tasks_cancelled`, shutdown wiring, startup crash-sweep; `main.rs` integration. Two integration tests: `scheduler_lanes_e2e` (concurrent fast+long claim, timing-bounded); `scheduler_crash_recovery_e2e` (kill daemon mid-task, restart, assert `state='crashed'`).

4. **CLI surface** — `hhagent-cli` subcommands. `cli_ask_e2e` (subprocess CLI happy path + SIGINT cancellation).

5. **Prompt-traceability ledger end-to-end** — `agent_prompts_e2e` (hash recorded on startup, edited prompt → second row, `plan.formulate` row carries hash). HANDOVER.md + ROADMAP.md updates. ROADMAP gains: "next: Stage -1 + Stage 0 implementations replace stubs in `core/src/cassandra/`, informed by observation-phase findings."

**Synthetic load harness** — script under `scripts/observation/` that drives N concurrent `hhagent-cli ask` invocations against a configured local model. SQL queries against `audit_log` produce per-task / per-plan / per-step latency distributions. Ships in commit 5 unless deferred.

**Observation phase** (deliberate non-coding hold after commit 5):

- Local-model evaluation: drive a curated task corpus through different local models pinned via `HHAGENT_LLM_LOCAL_MODEL`. Compare success rate, plan quality, JSON-drift rate, latency distribution. Pin a chosen model.
- Failure-mode catalogue: drive synthetic adversarial / clumsy / ambiguous instructions through the scheduler with stubs in place. Catalogue what the agent actually does. This becomes the design input for real Stage -1 + Stage 0 rules.
- Latency baselining: p50 / p95 / p99 for plan formulation, dispatch, end-to-end. The numbers CASSANDRA's overhead will be measured against.

Output: `docs/observation/scheduler-baseline.md` (or wherever the existing `docs/devel/` tree convention dictates).

**Real Stage -1 + Stage 0 implementations** then ship as a follow-up sequence informed by observed failures. Trait + chain unchanged; structs replaced in place.

## 10. Testing strategy

All integration tests use the existing per-test PG cluster recipe (see `db/tests/postgres_e2e.rs`, `core/tests/audit_dispatch_e2e.rs`). Each test brings up its own cluster with a unique socket path, runs `db::probe::run` to apply migrations, exercises the scheduler against the real PG, tears down on Drop. Cross-platform: every new test runs on both Linux (bwrap) and macOS (sandbox-exec) without per-platform branching, since `tool_host::dispatch` is already platform-neutral.

| Test | Verifies |
|---|---|
| `db/tests/postgres_e2e.rs::tasks_lifecycle_e2e` | Migration `0005` shape; `claim_one` SQL; lifecycle transitions; NOTIFY triggers (insert, cancel, completed); crash sweep idempotence. |
| `core/tests/scheduler_inner_loop_e2e.rs` | `run_to_terminal` against scripted-router stub: happy path, tool-fail-then-replan, plan-cap exhausted, cancel mid-flight, decode-error → `Failed`. |
| `core/tests/scheduler_lanes_e2e.rs` | Two `pending` tasks (one per lane) run concurrently; `tasks_completed` NOTIFY fires for each. |
| `core/tests/scheduler_crash_recovery_e2e.rs` | Plant `pending`, claim, kill daemon mid-flight, restart, assert `state='crashed'`. |
| `core/tests/cli_ask_e2e.rs` | Subprocess `hhagent-cli ask` happy path; SIGINT during execution → `state='cancelled'`, exit non-zero, worker dead. |
| `core/tests/agent_prompts_e2e.rs` | Hash recorded on startup; edited content → second row; `plan.formulate` row carries the matching hash. |
| `core/src/cassandra/` unit tests | `Verdict` exhaustiveness; `ChainReviewStage` short-circuit (Approve → Approve → Block ⇒ Block, never bypass); audit-log row emission per stage. |

## 11. Decisions log

| # | Decision | Rationale |
|---|---|---|
| 1 | Tasks-table drain (not direct CLI ↔ daemon) | Single source of truth; future channels and routines plug in as additional row writers; forensic record. |
| 2 | Two lanes (`fast` + `long`), no work-stealing | Workload is interrupt-driven; long tasks must not starve short ones. |
| 3 | Iterative replanning per task (not one-shot, not free-form ReAct) | Matches CASSANDRA §13.2 "full re-review on amendment"; allows agent to learn from step results. |
| 4 | Plan-iteration cap (per lane) + wall-clock deadline (= lease) | Two cheap signals beat one expensive signal; lease is needed anyway for crash recovery. |
| 5 | Tool failure: error → step result, plan exits, replan allowed | Lets agent recover from format mismatches and transient errors; bounded by plan-iteration cap. |
| 6 | Block: replan allowed (not terminal first time) | Symmetric with tool failure; same bound. |
| 7 | Crash recovery: `state='crashed'`, **no auto-resume** | Medical workflow safety: never silently re-execute side-effecting tools after a crash. |
| 8 | Single-clock lease (no separate liveness clock) | Simpler; ≤30 min crash-recovery latency on long lane is operational, not user-blocking. |
| 9 | Plan terminal signal as `decision: "task_complete"` plan-shape | Bypasses CASSANDRA review on the answer — *not acceptable*. The plan-shape forces the reviewer to see every byte the agent intends to return. |
| 10 | Memory recall is *not* auto-invoked | Don't bloat context; agent requests recall as an explicit step when needed. |
| 11 | CASSANDRA: trait + scaffold + stub stages in this work, real impls follow-up | Deliberate experimental design — measure baseline + observe agent failure modes empirically, then design Stage -1/0 rules from observed failures. |
| 12 | Prompts in git, hashed on startup, hash in every `plan.formulate` row | CASSANDRA can correlate behavioural drift to prompt drift. Source-of-truth in git, queryable ledger in DB. |
| 13 | Default ask lane: `fast` | Matches interactive expectation. Long tasks opt in via `--long`. |
| 14 | `tasks fail` ships in this work | Manual escape hatch from a stuck-running task before lease elapses. |

## 12. Out of scope (genuinely deferred — has prerequisites)

- **Real Stage -1 / Stage 0 implementations** — by design, deferred until observation phase is complete.
- **Stage 3 LLM review** — needs egress proxy (Phase 3) for frontier; needs frozen adversarial test corpus + reviewer-prompt iteration. A local-only Stage 3 is implementable now but its value is bounded without observation-phase failure data to design against.
- **Stage 4 longitudinal pattern analysis** — needs months of plan history.
- **Tier-2 escalation** — needs channel bus (Phase 2).
- **Privacy gate (Stage 2)** — needs anonymiser worker (third-party).
- **Embedding worker (Option O)** — `memory::recall` invoked as a step requires this.
- **Real `context_manager`** — needs production observation of overflow patterns.
- **Auto-restart of crashed tasks** — the design is "fail loud"; auto-restart is a future feature with a different policy.

## 13. Verification overview

End-to-end happy path: `hhagent-cli ask "ping"` → CLI INSERTs row → fast lane runner wakes via `tasks_inserted` NOTIFY → claims atomically → `formulate_plan` produces a `task_complete` plan via the stub-scripted router → `ChainReviewStage` returns `Approve` → loop returns `Completed` → `finalize` UPDATEs `state='completed'` → `tasks_completed` NOTIFY fires → CLI subscribed listener wakes → CLI prints `result.body` and exits 0. All intermediate steps emit the audit-log payload schemas pinned in §7.

Cross-platform: same flow runs green on Linux (bwrap + Landlock + seccomp) and macOS (sandbox-exec) because all platform-specific code lives in `tool_host::dispatch`'s sandbox layer, which is already cross-platform green.

---

*This spec is the implementation contract for the scheduler. The follow-up work item is the writing-plans skill, which produces the step-by-step implementation plan against this spec.*
