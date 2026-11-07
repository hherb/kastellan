# 15 — The planner: scheduler, inner loop and task notebook

The scheduler turns a queued task into an answer. It claims the task from
Postgres, asks the model for a plan, sends every step of that plan through
the CASSANDRA review chain and the `tool_host` dispatcher, shows the model
what came back, and repeats until the model answers, gives up, or a cap
stops it. Two loops do this today: the **classic** loop, which re-reads a
growing plan history every turn, and the **notebook** loop, which keeps
the planner's working memory in Postgres and works one step per turn.

> **Design sources.** `docs/superpowers/specs/2026-05-10-scheduler-design.md`
> (lanes, queue, audit rows) and
> [the task notebook design](../../superpowers/specs/2026-11-01-task-notebook-design.md)
> plus [`sift` and the scrapbook](../../superpowers/specs/2026-11-02-sift-and-scrapbook-design.md).
> The module doc comments under `core/src/scheduler/` are the live source of truth.

---

## Module layout

```
core/src/scheduler/
  mod.rs               spawn_scheduler, re-exports
  runner.rs            per-lane loop: LISTEN, claim, drain, finalize
  runner/task_exec.rs  run_one: build TaskContext, pick the loop, purge caches
  runner/audit_rows.rs task.<state>, task.finalize, l1/l3 crystallisation rows
  crash_recovery.rs    startup sweep -> task.crashed rows
  agent.rs, agent/     PlanFormulator trait + RouterAgent (the LLM adapter)
  plan_parser.rs       lenient JSON -> Plan (tolerates a ```json fence)
  prompts.rs           load prompts/*.md into PromptCache + agent_prompts ledger
  inner_loop.rs        TaskContext, Outcome, StepDispatcher, run_to_terminal (classic)
  inner_loop/
    execute.rs         review_and_dispatch: shared by BOTH loops
    llm_failure.rs     what to do when a planning call fails (retry/synthesis)
    ceiling.rs         plan cap + fast-lane wall-clock ceiling
    result_view.rs     the planner's pruned, labelled view of one tool result
    summary.rs, summary/  plans_so_far rendering, screening, size budget
  inner_loop_audit.rs  plan.formulate / plan.outcome / verdict audit rows
  tool_dispatch.rs     production StepDispatcher over tool_host::dispatch
  asks/                operator-ask (escalation) raise, deliver, resume
  conversation/        conversational continuity for channel tasks (turn_record)
  notebook/            the notebook loop (see below)
    sift/              the `sift` built-in and the scrapbook
core/src/handoff.rs    per-task cache for oversized tool results
prompts/               agent_planner.md, agent_notebook.md, sift_sorter.md
```

---

## The task queue

Tasks live in the `tasks` table. Two lanes, **fast** and **long**, each run
by their own runner task (`runner::lane_loop`):

- **Wake-up.** Each lane `LISTEN`s on `tasks_inserted`, `tasks_cancelled`
  and `tasks_resumed` (a task whose operator ask was answered), and runs a
  30-second heartbeat drain in case a NOTIFY was lost across a listener
  reconnect. It also drains once *after* subscribing, because a NOTIFY sent
  before the `LISTEN` is never delivered.
- **Claim.** `db::tasks::claim_one` uses `FOR UPDATE SKIP LOCKED` and stamps
  a lease (`lease_expires_at`; 60 s fast, 30 min long by default).
- **Crash recovery.** At daemon start, `crash_recovery::sweep_and_audit`
  marks every `running` row whose lease has elapsed as `crashed` and writes
  a `task.crashed` and a `task.finalize` audit row for each. The sweep's
  timestamp is *detection* time, not crash time.
- **Caps.** `DEFAULT_MAX_PLANS_FAST = 20`, `DEFAULT_MAX_PLANS_LONG = 25`
  (`db/src/tasks.rs`); a producer may lower or raise it with `max_plans` in
  the task payload. The fast lane also has a **15-minute wall-clock
  ceiling** (`DEFAULT_TIME_CEILING_FAST_S`), checked between plans — on the
  fast lane time is the real limit and the plan cap is a backstop against a
  planner looping on cheap plans.

Channel messages always land on the fast lane; `kastellan-cli ask` takes
`--fast` / `--long`.

---

## The classic loop (`inner_loop::run_to_terminal`)

Per iteration:

1. **Cancellation poll** — `kastellan-cli tasks cancel <id>` takes effect here,
   between plans.
2. **Cap / ceiling check.** If the cap or the fast-lane ceiling is spent *and*
   at least one tool call succeeded, spend **one forced-synthesis turn**:
   the model is told to stop gathering and answer from what it has. With
   nothing gathered the task fails with `plan_iteration_cap_exceeded`.
3. **Formulate** a `Plan` (`decision`, `rationale`, `steps`, `result`,
   `data_ceiling`, `refused`, `floor_request`, `l1_insight`, …).
   `plan_parser` tries strict JSON first, then the first complete JSON
   value after the first `{`. Failures are handled in `llm_failure.rs`,
   with **at most one retry per turn**:
   - an empty completion is asked again once;
   - a reply that is not a plan is shown back once with a request for the
     plan, thinking suppressed (`formulate_reformat`);
   - a request timeout (or a reply cut off at `max_tokens`) after something
     was gathered jumps to the forced-synthesis turn; a timed-out synthesis
     turn is retried once without thinking;
   - anything else fails the task as `llm: <error>`. There is no general
     retry: **replanning is the retry** — the model sees the failure next turn.
4. **Review and dispatch** (`execute::review_and_dispatch`). The plan goes
   through `ChainReviewStage` (chapter 11), then each step through the
   `StepDispatcher`. A plan may request at most `MAX_STEPS_PER_PLAN = 64`
   dispatches. Omitted method namespaces are completed against the
   registry *before* review, so review, audit and dispatch all see the name
   that goes on the wire. A `Verdict::Escalate` raises an operator ask and
   returns the non-terminal `Outcome::AwaitingOperator` (see
   [chapter 16](./16-channels.md#operator-asks-escalation)).
5. **Terminal plan** (`decision = "task_complete"`) → `Outcome::Completed`;
   a self-declared refusal → `Refused`; a reviewer block → `Blocked`.

### What the model sees of a result

`TaskContext::plans_so_far_summary` renders each earlier plan as
`{"decision", "step_outcomes"}`, each outcome carrying the `call` that made it
(tool, method, parameters) and one of four shapes: `ok` + `output`, `ok` +
`withheld`, `ok` + `elided`, or `err` + `code` + clamped `detail`.

- **`result_view.rs`** keeps the result's *structure*: identifier-shaped keys,
  numbers and booleans survive; strings are trimmed to one "water level"
  found by binary search; an identifier-like string (no whitespace, ≤ 1 KiB —
  ids, URLs, paths) is shown whole or not at all. One successful step's view
  is capped at 16 KiB (`STEP_OK_SUMMARY_MAX`).
- **`summary/budget.rs`** keeps the whole summary under 96 KiB by
  **water-filling**: every output gets an even share, small ones stay whole and
  their slack goes to the large ones, and only when a share would fall below
  2 KiB are the oldest outputs dropped whole (their `call` stays).
- Every view, decision and call is screened by the injection guard before it
  re-enters a prompt; a block becomes `"[withheld: failed injection screen]"`.
  View *keys* use markers a worker cannot spell.
- **The handoff cache** (`core/src/handoff.rs`): a result over 64 KiB is
  stashed per task, content-addressed, and replaced with a placeholder plus a
  1 KiB screened head; the planner pulls slices back with the `handoff`
  built-in (`handoff.fetch`, ≤ 256 KiB per fetch). The cache is in-memory and
  purged whenever the loop returns — **including a suspend for an operator
  ask**.

---

## The notebook loop (`notebook::run::run_notebook_to_terminal`)

On a busy mail question the classic summary filled most of a 64k-token
context and every turn re-read it. The notebook replaces the growing prompt
with external working memory:

- **Turn 1** writes a step list. Every later turn sees only the goal (and
  conversation), the step list with each step's status and tick note, the
  model's own progress summary, the findings so far, any operator
  corrections, the current step with its tool's full doc, and **only the
  last result of the call made for that step**. Its size does not grow with
  the number of tool results.
- **A reply** is one JSON object: optional `tick` (note, findings with their
  source, new summary) and `steps` (a replan), plus exactly one of `call`,
  `answer` or `infeasible`. A tick and the next call share one reply, so a
  step costs one LLM call.
- **Every `call` becomes a one-step `Plan`** and goes through the same
  `execute::review_and_dispatch` as a classic plan — no review is skipped. A
  refused or failed call becomes a `failed_step` entry and the next turn
  replans.
- **One cap for every turn**: `max_plans × NOTEBOOK_TURNS_PER_PLAN` (2),
  planning and replan turns included, plus the fast-lane ceiling and the
  same single forced-answer turn.
- **Repeats are not re-dispatched** (`repeat.rs`): a call identical to one
  that already succeeded (scalars compared as text) is set aside with a
  `plan.discarded` row and the model is told why.
- **Thinking** (`thinking.rs`): planning, replanning and writing the answer
  think; routine turns follow `KASTELLAN_LLM_DISABLE_THINKING`; the forced
  answer and the re-ask never think.
- **Trust.** Notes, findings, summary and replans are model-written from tool
  output, so they carry tool-output trust and are re-screened on every
  render. The answer's data ceiling is the highest class of *every* step the
  task dispatched, not only the findings the model cites.

The notebook is the `task_notebook` table (migration `0028`): one row per
event (`steps`, `tick`, `failed_step`, `correction`, `answer`, `infeasible`,
`cancelled`), a gap-free per-task `seq`, a `task_notebook` NOTIFY per insert,
and **append-only for the runtime role**. Its current state is a pure fold
(`notebook::state::fold`). An operator steers a running task with
`kastellan-cli tasks correct <id> <text>` (the correction is appended to the
instruction the reviewer sees) and stops it with `tasks cancel`. Both take
effect between turns.

`kastellan-cli session` (`core/src/bin/kastellan-cli/session.rs`) is the
long-lived link for a front end: JSON-RPC 2.0 over stdin/stdout, run locally or
over ssh (no listener; trust is ssh plus the operator's OS user, as for `ask`).
It serves `task.submit`, `task.follow`, `task.correct`, `task.cancel`,
`task.status` (the fold, rendered by `notebook::view::fold_view`), `inbox.list`
and `inbox.answer`, each through the same helper as the one-shot command. A
follow answers at once and streams `notebook.entry`, `task.awaiting_operator`
and `task.finished` notifications in the background; it reads the task's state
**before** its entries (so a state the planner sets means every entry is visible; for a
cancel, which lands first, the follow waits up to 120 s for the planner's
`cancelled` entry), and
its 3 s poll tick, not the NOTIFYs, is what makes it correct: `PgListener` drops
notifications on reconnect, and nothing notifies a task parking. All of a
session's follows (up to 64) share one `LISTEN` connection, `follow::Listener`,
which broadcasts the task id of each NOTIFY (#987). A dropped session cancels
nothing; a new one resumes with `after_seq`.

### `sift` and the scrapbook

`sift` (tool `sift`, method `sift.run`; the name is reserved in
`registry_build`) is a **core built-in available in notebook mode only**. One
call applies one criterion to every item of a candidate list: its reads are
**one reviewed plan** (read-only methods only, capped), each item goes to a
small "sorter" LLM call (`prompts/sift_sorter.md`, through `llm-router`), and
the kept items with their extracted fields land in `task_scrapbook`
(migration `0029`, append-only like the notebook). The planner sees only
counts and a sample. An answer can name a scrapbook (`sb:<task_id>:<sift>`)
and **code** renders it as a table. A follow-up may read an earlier turn's
scrapbook only if that turn is in its conversation window.

---

## Knobs

| Variable | Default | Effect |
|----------|---------|--------|
| `KASTELLAN_PLANNER_MODE` | `classic` | `classic` or `notebook`, per daemon (every lane and channel task). Any other value stops the daemon at boot. |
| `KASTELLAN_SIFT_MAX_READS` | `64` | Most reads (and items) one sift takes. Must be > 0. |
| `KASTELLAN_SIFT_READ_METHODS` | `mail.get_message,mail.get_attachment_text,handoff.fetch` | Methods a sift may read with. An unadvertised method, a sift or a skill stops the daemon at boot. |
| `KASTELLAN_PROMPTS_DIR` | `prompts` (relative to CWD) | Where prompt files are loaded from; the installer sets it to `~/.local/share/kastellan/prompts`. |
| `KASTELLAN_TIMEZONE` | host zone | IANA zone for the trusted `now` field in the prompt. |

The sift knobs are read only in notebook mode. Model-side knobs
(`KASTELLAN_LLM_*`) are in [chapter 13](./13-llm-router.md).

---

## Prompts

`prompts/agent_planner.md` (classic), `prompts/agent_notebook.md` (notebook)
and `prompts/sift_sorter.md` are loaded once at boot by
`prompts::load_prompts_from_dir`: each `.md` file is hashed and upserted into
`agent_prompts` (append-only, so every version ever used stays on record),
and every `plan.formulate` audit row carries the `prompt_sha256` it ran with.
The loop reads the in-memory cache, never the disk.

`kastellan-cli install` copies the repo's `prompts/` into
`~/.local/share/kastellan/prompts/`, **overwriting** what is there. To try a
prompt-only change on a deployed host, copy the file there and restart
`kastellan-core` — no rebuild — then check `agent_prompts` for the new
sha256. The next install puts the repo's version back, so commit what works.

The user message is ordered **stable-first** for the backend's prompt cache:
the system prompt, then what is fixed for the task, then the notebook (or
summary), then what changes every turn, and `now` last. Keep that order when
you add a field.

---

## Invariants and traps

- ⚠️ **The model mirrors whatever shape it is shown.** Render a key and the
  model will echo it back; show a full example answer with a table and it
  copies the example. `NotebookReply` uses `deny_unknown_fields`, so any key
  you add to the render must also be accepted (and ignored) in the reply —
  see the `echoed_*` fields in `notebook/state.rs`.
- **Both loops share `execute::review_and_dispatch`.** Never give the
  notebook (or a sift) its own dispatch path.
- **The plan cap bounds injection,** not just cost: an injected planner can
  loop on cheap plans. Don't remove it because the time ceiling binds first.
- **`Outcome::final_state` returns `Option`.** `AwaitingOperator` is not
  terminal; finalizing it would write a false `task.finalize` row.
- **Migrations `0028` and `0029` are applied on a live host — never edit
  them.** A new notebook `kind` is a new migration; the fold refuses kinds it
  does not know.
- Prompt-pinning tests fail if a prompt stops documenting what the code
  emits or accepts: `the_planner_prompt_documents_every_outcome_shape` and
  `the_notebook_prompt_documents_every_reply_key`. Change prompt and code
  together.

---

## Testing

```sh
cargo test -p kastellan-core scheduler::                     # unit tests (most need no Postgres)
cargo test -p kastellan-core --test scheduler_inner_loop_e2e # classic loop on a real cluster
cargo test -p kastellan-core --test scheduler_lanes_e2e
cargo test -p kastellan-core --test scheduler_crash_recovery_e2e
cargo test -p kastellan-db --test notebook_e2e --test scrapbook_e2e
bash scripts/run-e2e-gate.sh pg                              # PG tier as evidence
```

The notebook loop's own tests (`notebook/run/tests.rs`,
`notebook/sift/run/tests.rs`) use `notebook/test_harness.rs` — a scripted
model, a recording reviewer and scripted tools — against a real Postgres
cluster, and **skip-as-pass without one**; read `--nocapture` output or use
the `pg` gate profile.

A green suite says the loop's logic holds, not that the planner answers
well. **Planner behaviour is measured live**: the operator's fixed set of
questions, asked one at a time with
`kastellan-cli ask --classification-floor Public "<q>"`, logged in
[`docs/devel/notes/notebook-experiments.md`](../notes/notebook-experiments.md)
(append-only; date, build, knobs, per-question turns/seconds/outcome).
**Judge a change over at least two runs** — single runs vary too much.

---

## Where to start when changing it

- **Prompt wording** → `prompts/*.md`, then the pinning tests above, then a
  measured run.
- **What a result looks like to the planner** → `inner_loop/result_view.rs`
  and `inner_loop/summary/` (classic), `notebook/render.rs` (notebook).
- **Reply shape** → `notebook/reply.rs` + `notebook/state.rs`; mind the
  mirroring trap.
- **Retry/recovery after a failed model call** → `inner_loop/llm_failure.rs`
  (the notebook reuses its decisions via `notebook/ask.rs`).
- **Caps and ceilings** → `inner_loop/ceiling.rs`, `db/src/tasks.rs`
  defaults, `notebook/run.rs::turn_cap`.
- **A new built-in like `sift` or `handoff`** → reserve its name in
  `registry_build`, route it through `review_and_dispatch`, and never let it
  reach the model or the network from a worker.
