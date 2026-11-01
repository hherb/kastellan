# The task notebook — design

**Status:** design, approved direction (operator, 2026-11-01). Issue: [#922](https://github.com/hherb/kastellan/issues/922).
**Measured against:** the seven mail questions (`~/requery7.sh` on the DGX); latest baseline tasks 246–252 (after #949).

## 1. Why

The planner today re-reads its whole history every turn. `plans_so_far` grows with every tool result until
#921's even shrink rewrites it, which also defeats the backend's prompt cache (#943). On the DGX a full
summary is ~49k tokens of a 65,536 context (#940). The result is slow turns (~100 s each, mostly thinking),
tasks running 15–20 minutes, and answers that lose track of what was already found.

The notebook replaces the growing prompt with **external working memory**. The model writes its own step
list at the start, works one step per turn, ticks each step off with a short note, and keeps a short
progress summary. Each turn it reads only what that turn needs.

## 2. Priorities (the operator's, in order)

1. **Confidentiality and security are never compromised.** Every tool call still goes through the review
   chain, the allowlists and the sandbox. Notebook text is screened like tool output. Classification carries
   through to the answer.
2. **The task is solved correctly, and the answer is communicated clearly.**
3. **In a time reasonable for the task**, about 5 minutes for a mail question.

**Success** = all seven #922 questions answered correctly, each within about 5 minutes, on the DGX.

## 3. Goals, non-goals, anti-goals

**Goals**

- A bounded per-turn prompt: its size does not grow with the number of tool results.
- The operator can **watch** a task's notebook fill in live, **stop** it, and **correct** it ("the invoice is in
  the other account") while it runs.
- The notebook is kept in Postgres as the **performance record** of every task, kept until a maintenance trim,
  and later usable for analysis and fine-tuning data.
- Bottie talks to Kastellan through a proper session instead of one `kastellan-cli ask` per message.

**Non-goals**

- No new network listener, no HTTP API. The link rides on ssh and the OS user (§8).
- The security audit stays `audit_log`; the notebook does not replace it and is not a security record.
- No change to the review chain, allowlists, sandbox or worker set.
- No streaming of the final answer token by token.
- Interrupting an LLM call already in flight (stop and correct take effect between turns).

**Anti-goals** (failure even if it "works")

- Any path where notebook text (notes, findings, summary, corrections) is treated as an instruction with more
  trust than the tool output it came from.
- A notebook view that silently drops facts the answer needs, so the answer is fluent but incomplete.
- Faster tasks bought by skipping review or screening.

## 4. Decisions the operator made

| # | Decision |
| --- | --- |
| D1 | The model writes its own step list at the start and works **one step per turn**. |
| D2 | Ticking a step off takes a **1–2 line note**. |
| D3 | The **model writes the short progress summary** itself (not code assembling it from notes). |
| D4 | When a step fails or its result breaks the plan, control **returns to the planner**, which writes a **new list of remaining steps** (done steps and notes stay) **or decides the task cannot be done** and says why. |
| D5 | Each turn reads only: the goal, the progress summary, the next step, that step's tool docs, that step's data. |
| D6 | **Findings:** besides the note, a step records the extracted facts the answer needs (e.g. one line per invoice). Bounded; each names the step and the source (message id) it came from. **The final answer is written from the findings**, never from raw tool output. |
| D7 | The operator can **watch** live, **stop**, and **correct** a running task. |
| D8 | The link is a **long-lived JSON-RPC session over stdio** (`kastellan-cli session`, run locally or over ssh), not an HTTP API. The current `ask`-over-ssh in bottie was a proof of concept and may be replaced. |
| D9 | Kept **in Postgres**, persistent until a maintenance trim, as the performance record. |
| D10 | A reply that is not a plan is shown back to the model once (#945, shipped ahead of this design in PR for #945). |
| D11 | **A correction adds to the instruction.** A correction from the CLI session (bottie or a terminal, the operator's OS user) is appended to the instruction the review chain sees (`ReviewStageContext.instruction`), so it can widen the task like a new instruction would. **Matrix corrections are not part of this version.** |

### Calls made in writing this up (not the operator's; check these)

| # | Call |
| --- | --- |
| C1 | **The answer's data ceiling is the highest classification of all step data the task touched**, not only of the findings the model says it used. A reply that leaves a Confidential finding out must not lower the ceiling. No weaker than today's rule. |
| C2 | **The live view is an operator surface.** `task.follow` is served only by the CLI session (operator's OS user). No channel (Matrix) gets a live notebook in this design; if one is added later, each entry passes the same classification gate as a reply on that channel. |
| C3 | **Escalation:** a notebook task that hits `Verdict::Escalate` parks in `awaiting_operator` as today. The notebook fold sits **alongside** `asks.resume_state` (0024) in this design and does not replace it; crash recovery resumes from the fold plus the existing resume rules, never re-dispatching a ticked step. The session adds `inbox.list` and `inbox.answer` so a bottie user is never stuck on an escalated task. |
| C4 | **Scope:** the planner-mode switch is **per daemon** (`KASTELLAN_PLANNER_MODE`, default `classic` until the seven pass). When on, it applies to every lane and to channel tasks; a channel task's `turn_record` (#701) is still written from the final answer as today. |
| C5 | **Caps:** every LLM turn counts against one turn cap, planning and replan turns included, so the injection-bounding role of today's cap (`inner_loop.rs`) is kept. Its value starts at today's caps scaled to one step per turn and is tuned by measurement; the fast lane's 15-minute ceiling binds first in practice. |
| C6 | **A finding withheld by the screen** is kept as a placeholder in the findings; the answer must say a fact was withheld ("one finding was withheld by the safety screen"), never answer as if it were complete. |
| C7 | **"Answered correctly"** for the seven is judged by the operator by hand, as for 239–252. |

## 5. Constraints

- AGPL-compatible dependencies only; Rust core; Linux and macOS first-class.
- Every tool call is a plan step reviewed by the existing `ChainReviewStage` before dispatch, exactly as today.
- The runtime Postgres role must not be able to rewrite or delete past notebook entries (same posture as
  `audit_log`'s REVOKE in `0002_runtime_role.sql`). Trimming is a separate, operator-run maintenance action.
- Per-turn prompt order stays **stable-first** for the prompt cache (#943): the system prompt, then the parts
  fixed for the task, then the append-only notebook view, then the current step's data, then `now` last.
- The fast lane's 15-minute ceiling and the plan caps stay. In the notebook they bound turns, including replans.
- Files under ~500 lines; pure functions for rendering and decisions; unit tests without Postgres where possible.

## 6. The design

### 6.1 Data model

One append-only table of notebook entries (migration `0028`), one row per event:

| Column | Meaning |
| --- | --- |
| `task_id`, `seq` | the task, and a per-task sequence number (gap-free, from 1) |
| `kind` | `steps` (a step list, the first or a replacement), `tick` (a step done: note + findings + summary), `failed_step`, `correction`, `answer`, `infeasible`, `cancelled` |
| `payload` | JSONB, shape per kind |
| `created_at` | timestamp |

The current state of a notebook (the live step list, done steps, findings, summary, pending corrections) is a
**pure fold** over its entries, so the loop, the CLI and bottie all read one shape. A `NOTIFY` on insert
(channel e.g. `task_notebook`, payload `task_id:seq`) lets watchers follow without polling.

**Findings** carry `{text, step, source}`, where `source` names where the fact came from (e.g. a mail message
id, an attachment, a URL). Each finding also carries the classification of the step data it came from; the
answer's data ceiling is computed from the findings it uses with the existing ceiling rules.

### 6.2 The turn

Turn 1 (**planning**): the model sees the goal, a one-line catalogue of the tools, and any conversation turns.
It replies with a step list (each step: what it is for, which tool it expects to use, what "done" means), plus
the first step's call.

Every later turn (**step turn**) sees:

- the goal (instruction, conversation, classification floor);
- the step list with each step's status and its tick note;
- the model's own progress summary (from the last tick);
- the findings so far;
- any operator corrections not yet acknowledged, marked as such;
- the current step, its tool's full docs, and **the result of the call made for it last turn**.

It replies with one of:

- **tick + next call**: tick the current step (note, findings, new summary) and give the call for the next
  step, in one reply, so a step costs one LLM call, not two;
- **another call for the same step** (e.g. the next page), with no tick;
- **replan**: a new list of remaining steps (D4);
- **answer**: the final answer, written from the findings (D6), as today's terminal plan;
- **infeasible**: why the task cannot be done, as a terminal answer.

Every reply that contains a tool call becomes a `Plan` with that one step and goes through the review chain and
dispatcher unchanged. A refused or failed call is a `failed_step` entry and the next turn is a replan turn.

### 6.3 What is screened, and how it is trusted

- The **notes, findings and summary** are written by the model from tool output, so they carry tool-output
  trust. They are screened by the same injection screen as tool output before they re-enter a prompt, and are
  rendered in a labelled block a worker cannot forge (the #704 markers).
- An **operator correction** from the CLI session has operator trust (the operator's OS user, as `ask` does
  today) and joins the instruction the review chain sees (D11). No channel can send one in this design.
- A screen block on a notebook entry withholds that entry (as `"[withheld: failed injection screen]"` does
  today) and is audited.

### 6.4 Stop and correct

- **Stop** reuses `tasks cancel`, which already accepts a running task; the loop already checks between plans
  (`inner_loop.rs`). The session's `task.cancel` calls the same helper and writes a `cancelled` entry.
- **Correct** appends a `correction` entry. The next turn shows it at the top of the notebook view; the model
  may replan. A turn in flight is not interrupted.

### 6.5 The session (`kastellan-cli session`)

JSON-RPC 2.0, line-delimited over stdin/stdout, built on `kastellan-protocol` (the same framing the workers
use). It runs as the operator's OS user and reaches Postgres over the local socket, exactly like `ask`.

| Method | Effect |
| --- | --- |
| `task.submit {instruction, classification_floor?}` | submits a task, returns `{task_id}` |
| `task.follow {task_id, after_seq?}` | streams `notebook.entry` notifications from `after_seq`, then `task.finished {state, answer?}`; resumable after a dropped connection |
| `task.correct {task_id, text}` | appends a correction |
| `task.cancel {task_id}` | cancels |
| `task.status {task_id}` | current state and notebook fold |
| `inbox.list`, `inbox.answer {ask_id, …}` | the operator questions of escalated tasks (C3), as `kastellan-cli inbox` |

Locally bottie spawns it directly; remotely `ssh <host> kastellan-cli session` with the same batch-mode and
argument rules bottie's `transport.rs` already enforces. No message text is ever on a command line.

### 6.6 Bottie

Replaces the per-message `ask` transport with one session per Kastellan conversation; renders the notebook
live in the waiting response (steps ticking off, notes, findings count); adds **Stop** and **Correct** controls
on a running task. Work in the bottie repo, after the Kastellan side is measured.

## 7. Related issues

- **#945** (non-JSON replies): shipped ahead of this design (D10).
- **#941** (no `max_tokens` on planner turns; the ceiling does not bound one LLM call): **ships ahead**, slice 0.
  It is orthogonal and the notebook needs it too.
- **#940** (the summary budget is in bytes, the context in tokens): **folded in**. The notebook view's bounds
  (findings, notes) are set in estimated tokens.
- **#678** (retire truncation): the notebook is the "map" half of it for the planner; the guard's
  `SCAN_BYTE_CAP` and `truncate_payload` are unaffected.

## 8. Order of work

0. #941: `max_tokens` on planner turns and a per-call deadline inside the fast-lane ceiling.
1. Notebook storage (migration, fold, NOTIFY) and the notebook loop with its prompt, behind a planner-mode
   switch so the classic loop stays available for comparison. Measured with the seven via `kastellan-cli ask`.
2. `kastellan-cli session` (submit, follow, correct, cancel, status), and `ask` printing notebook entries to
   stderr as they land.
3. Bottie: session transport, live view, Stop and Correct.
4. When the seven pass: the notebook becomes the default and the classic loop is retired.

## 9. Left to the builder

- Exact JSON shapes of the replies and entry payloads; the new prompt's wording (a new prompt file, not an edit
  of `agent_planner.md`, while both loops exist).
- The token bounds for findings, notes and summary, and what the model is told when findings are full.
- Whether a step turn sees the whole result of its last call or the existing result view (#677's pruned,
  labelled copy) of it.
- How the maintenance trim is exposed (a CLI command run as the operator is the expected shape).
- Notebook entries' indexes and the NOTIFY payload format.
