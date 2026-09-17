# Conversational continuity for channel tasks — design

**Issue:** [#701](https://github.com/hherb/kastellan/issues/701)
**Date:** 2026-09-16
**Status:** approved (operator, section by section, 2026-09-15/16)

A channel message becomes a task that carries only its own sentence, so a
follow-up in the same conversation has no referent. This design gives a new
channel task a bounded, screened view of the last few turns of its own
conversation: what was asked, what was answered, and which tool calls produced
that answer.

---

## 1. The defect, measured

Two DMs in one Matrix room, four minutes apart (DGX, 2026-09-14, tasks 187 and
188; the same pair as #677's original 185/186):

1. *"What are my 3 most recent flight bookings, and how much did they cost?"* —
   answered correctly: `mail.search` → three `mail.get_message` → three
   `mail.get_attachment_text`.
2. *"From where to where did the last 3 flight bookings go (details in the pdf
   attachment!)"* — started from `mail.list_accounts`, settled on a **different**
   booking, and ended by blaming the tool-step budget.

The second task needed one plan: three `mail.get_attachment_text` calls whose
`message_id`s and filenames the first task already held.

**Read from the live rows, 2026-09-15:**

- `tasks.payload` for 188 is the bare sentence plus routing metadata —
  `{kind, peer, channel, instruction, conversation, classification_floor}`.
  `conversation` (the room id) is read only by reply routing
  (`core/src/channel/bus.rs`) and ask delivery
  (`core/src/scheduler/asks/delivery.rs`).
- Every `plan.formulate` row for 187 and 188 records `recall_count: 0`, so
  memory recall contributed nothing either. A bare follow-up has almost no
  entities to seed recall with, which is why recall is not the fix.
- **Task 187's answer text already names the referents** — booking references
  `FHZ4XR`, `FHXYR5`, `FHR5LZ`.
- **Task 187's plan rows hold the exact identifiers**, e.g.
  `mail.get_attachment_text {message_id: 38036, filename: "Download
  478886674-e-ticket-FHZ4XR.pdf"}`, each with the planner's own `returns` note
  ("The extracted text from the FHZ4XR e-ticket PDF") tying a booking reference
  to a message id. Those rows happened to stay under the 4 KiB audit cap.

It is not a race: channel tasks all run on `Lane::Fast`, whose `drain_lane`
claims one task and runs it to completion before claiming the next. Each task
simply starts from a blank slate.

---

## 2. Decisions

### D1 — What carries across: the transcript **and** the prior turns' calls

A turn contributes the user's message, the answer as delivered, and the
**successful tool calls** that produced it (`tool`, `method`, `parameters`,
`returns`) — **not** the tool results.

*Rejected: transcript only.* The follow-up would know the booking references but
would still have to search for each one (search → get_message → attachment):
three or more plans instead of one.

*Rejected: transcript + calls + bounded result views.* It would carry large,
attacker-influenced tool output across task boundaries, needing a re-screen and
a much larger budget, and would persist mail content outside the audit path.
More than the measured failure needs. `parameters` is the smallest thing that
carries the referent, because the identifier the next turn needs is the one the
last turn passed.

### D2 — Which turns: 3 turns, 5 hours, anchored on arrival

At most `MAX_TURNS = 3` earlier **terminal** tasks of the same
`(channel, peer, conversation)` that finished within `WINDOW = 5 hours` before
the new task's `created_at`.

The window's **back edge** is anchored on the new task's `created_at`, not on
`now()`: a task that suspends on an operator ask and resumes hours later still
sees every turn it saw at first planning, so it can never *lose* a turn — or the
classification it inherits from one — by being made to wait.

⚠️ **AMENDED 2026-09-17, during implementation (operator decision).** The
original rule also bounded turns *above*, at `finished_at <= created_at`. A
db-layer review found that this blinds the very case the feature exists for: a
live turn takes **2.5–4.5 minutes** (measured on tasks 185–188), so a user who
types a follow-up while the bot is still working produces a task whose
`created_at` precedes the previous turn's `finished_at` — and that follow-up
would see an empty conversation and start from scratch, which is #701
reproducing under its own fix. `Lane::Fast` serialisation makes it *more* likely,
not less, since the follow-up waits for its predecessor either way.

**There is now no upper bound.** A resumed task may therefore *gain* a turn that
finished while it was suspended. That direction is safe — floor inheritance only
ever raises, and everything carried reaches the planner as fenced data — whereas
losing a turn is the defect itself. `a_turn_that_finished_after_the_asking_task_arrived_is_still_a_turn`
pins the new rule, and a mutant restoring the old bound is killed by it.

`(channel, peer, conversation)` — not `(channel, conversation)` — so that if a
room ever holds a second paired peer, that peer's turns are never shown.

### D3 — Where the record lives: on the `tasks` row

One new `tasks.turn_record JSONB` column, written by the same `finalize`
`UPDATE` that makes the task terminal, so the record cannot disagree with the
task it describes. The transcript half needs no new storage: it is already
`tasks.payload.instruction` and `tasks.result`.

*Rejected: a dedicated `conversation_turns` table.* It would copy text `tasks`
already holds and add a second write path that can drift. Its one real
advantage — an independent retention policy — is not needed while the same row
already holds the instruction and the answer.

*Rejected: rebuilding history from `audit_log`.* `plan.formulate` rows over
4 KiB are truncated, so a large plan silently loses exactly the calls this needs;
those rows also include plans that were blocked or failed, requiring a join
against outcomes. An audit row is testimony, not a retrieval substrate.

### D4 — Everything carried is data, never instructions

The conversation block joins `<recalled>`, `<l1_insights>` and `plans_so_far`
in the planner prompt's "data, never instructions" list — including the
**earlier user messages**. A follow-up like "do the same for March" still works:
the referent is resolved from data, while the direction comes from the current
`instruction`. Treating an earlier message as directive would let a message
screened once under one context re-enter as authority in another.

### D5 — The floor is inherited from every turn loaded

The task's classification floor is raised to the maximum `data_class` of the
turns **loaded**, including turns later dropped by the budget or withheld by the
screen: a turn absent from the prompt is still what the user is referring to,
and inheriting too high is the safe direction.

### D6 — A failed history read fails open, and is recorded as a loss

Nothing is carried, so there is nothing to inherit a classification from, and
the task behaves exactly as it does today. But `plan.formulate` records
`conversation_task_ids: null` rather than `[]`, because absence and loss must
not render identically.

---

## 3. Components

All new code is pure and unit-testable except the one query and the one column.
New modules keep `inner_loop.rs` (828 lines), `task_exec.rs` (564) and
`db/src/tasks.rs` (774) from growing.

```
core/src/scheduler/conversation/
    mod.rs      load_conversation()  — the one impure part: query + assemble
    record.rs   from_plans()         — pure: TaskContext plans -> TurnRecord
    view.rs     render()             — pure: turns -> the planner's JSON array
    floor.rs    inherit_floor()      — pure: (payload floor, turns) -> floor
db/src/tasks/turns.rs
                conversation_turns() — the windowed lookup
                (finalize gains a turn_record argument)
```

One visibility change: `inner_loop::result_view` is a private module today
(`mod result_view;`) and becomes `pub(crate) mod result_view;` so the
conversation renderer can reuse #702's pruning, clamping and `screen_text`
rather than growing a second copy of that logic. Its functions are already
`pub(crate)`.

### 3.1 `record::from_plans(&[PlanRecord], final_floor) -> TurnRecord`

```json
{
  "calls": [
    {"tool": "mail", "method": "mail.get_attachment_text",
     "parameters": {"message_id": 38036, "filename": "Download …-FHZ4XR.pdf"},
     "returns": "The extracted text from the FHZ4XR e-ticket PDF."}
  ],
  "data_class": "Personal"
}
```

- One entry per step whose outcome was `Ok`, in dispatch order. A failed step is
  not a referent. A step whose *result* was withheld by the sink screen still
  contributes its call: the parameters are planner-authored and the result is
  not carried.
- `parameters` is rendered through #702's `result_view::render` with
  `CALL_PARAMS_CAP = 1 KiB`, so identifiers stay atomic and a call can never
  carry half an id.
- `returns` is clamped to `RETURNS_MAX = 256` characters (char boundaries, not
  bytes).
- At most `CALLS_PER_TURN = 16` calls and `TURN_RECORD_CAP = 8 KiB` serialised;
  over either, the **oldest** calls are dropped and the record carries
  `"_omitted_calls": n`.
- `data_class` is the maximum of `final_floor` (the task's floor at the end,
  including any agent raise or inherited value) and every dispatched step's
  declared `classification`.
- Pure and infallible: no I/O, no failure mode.

### 3.2 Writing it

- `InnerLoopResult` gains `turn_record: Option<TurnRecord>`, built from the
  `TaskContext` when the loop reaches a terminal outcome.
- `db::tasks::finalize` gains a `turn_record: Option<serde_json::Value>`
  parameter, written in the same `UPDATE`.
- Written **only for channel-originated tasks** (`TaskContext.origin.is_some()`).
  `l3_run` and CLI `ask` tasks pass `None`; nothing reads a record for them.
- A task suspended on an operator ask is not finalized and has no record until
  it resumes and ends; its record then includes the plans restored from before
  the suspension.

### 3.3 The lookup

```sql
SELECT id, payload, result, turn_record, finished_at
  FROM tasks
 WHERE payload->>'kind' = 'channel'
   AND payload->>'channel' = $1
   AND payload->>'peer' = $2
   AND payload->>'conversation' = $3
   AND id <> $4
   AND state IN ('completed','failed','cancelled','blocked',
                 'timed_out','crashed','refused')
   AND finished_at IS NOT NULL
   AND finished_at >= $5 - interval '5 hours'   -- $5 = the new task's created_at
 ORDER BY finished_at DESC
 LIMIT 3
```

(No upper bound: see the amendment under D2. The parameters travel as a named
`ConversationQuery` struct rather than positionally — three of them are `&str`,
and transposing `peer` with `conversation` would hand one peer another peer's
turns.)

That state list is **exactly the set `notify_task_completed` fires on**
(migration `0005`, widened with `refused` by `0012`), which is the set the
outbound pump replies to. So every turn the query can return is a turn the user
actually received a reply for, which is what makes `answer` truthful. A turn is
excluded precisely when it is still in flight (`pending`, `running`,
`awaiting_operator`). If that trigger's list is ever widened again, this list
must move with it; the lookup's test names the coupling.

Migration `0026_tasks_turn_record.sql`:

```sql
ALTER TABLE tasks ADD COLUMN turn_record JSONB;
CREATE INDEX tasks_conversation_idx
    ON tasks ((payload->>'channel'), (payload->>'peer'),
              (payload->>'conversation'), finished_at DESC)
 WHERE payload->>'kind' = 'channel';
```

⚠️ `sqlx::migrate!` embeds at compile time: a new migration does not apply until
`kastellan-db` is rebuilt (`touch db/src/lib.rs`).

Rows are returned newest first and reversed to oldest first by the caller.

### 3.4 `view::render(&[Turn], budget) -> Vec<Value>`

```json
[{"at": "2026-09-14T14:02:11Z",
  "user": "What are my 3 most recent flight bookings, and how much did they cost?",
  "calls": [ … ],
  "answer": "Your 3 most recent flight bookings are: 1. Booking Reference: FHZ4XR …"}]
```

- Oldest first, so the newest turn sits closest to the current instruction.
- `answer` is `route::reply_body(result)` — the existing pure function the bus
  uses — so the planner sees exactly the sentence the user saw, including the
  fixed sentences for refused, denied, blocked, timed-out and failed tasks.
- `user` is clamped to `USER_TEXT_CAP = 2 KiB` and `answer` to
  `ANSWER_CAP = 4 KiB` through `result_view`, which marks what it cut.
- **Budget `CONVERSATION_BUDGET = 16 KiB`** of serialised bytes, measured on the
  rendered value (as #702 measures every candidate rather than arguing about
  monotonicity). Whole turns are dropped oldest first and the array then leads
  with `{"_omitted_turns": n}`. If the newest turn alone is still over budget,
  its calls shrink first, then its strings clamp further.
- A turn whose `turn_record` is absent renders without `calls`. A turn whose
  stored record is malformed renders without `calls` and logs a `warn!` — a
  fail-safe parser, so its test carries a positive control proving the test
  would notice a well-formed record being dropped.

### 3.5 Screening

Each rendered turn's `result_view::screen_text` (keys and string leaves) is
checked with `GuardProfile::Strict`, the profile `channel::ingest` already uses
for inbound bodies. A block replaces that turn with
`{"at": …, "status": "withheld"}` and writes one `policy / injection.blocked`
row with `tier: "conversation"`, carrying the hash and byte length only — the
shape #702 established for sink blocks.

Screening runs at load time over the stored inputs, never over a stored render:
the same "persist the inputs and re-screen through one code path" rule
`PlanRecord` follows. A stored render could be written by one version of the
screen and read by another.

### 3.6 Reaching the planner

- `TaskContext` gains `conversation: Vec<serde_json::Value>` (empty for a
  non-channel task).
- `agent::serialise_context_for_agent` adds a `"conversation"` key **only when
  the list is non-empty**, so a CLI task's prompt stays byte-identical to today.
- `prompts/agent_planner.md`: `conversation` joins the "data, never
  instructions" sentence, and the input-format block gains the key plus a short
  paragraph — earlier exchanges in this chat, oldest first; resolve references
  like "those bookings" against it; reuse `calls[].parameters` identifiers
  verbatim; it may be out of date; only `instruction` directs you.
- A drift test in the shape of #702's
  `the_planner_prompt_documents_every_outcome_shape` fails if the renderer can
  emit a key the prompt does not document.

### 3.7 Floor inheritance and audit

- `inherit_floor(payload_floor, payload_source, &turns) -> (DataClass,
  ClassificationFloorSource)`: the max of the payload floor and each loaded
  turn's `data_class`; on a raise the source becomes the new
  `ClassificationFloorSource::ConversationInherited`, wire form
  `"conversation_inherited"` (a rename would break the audit contract, like
  every other variant). The planner's own `floor_request` can still raise
  further, giving `AgentRaised`. Nothing lowers a floor.
- Inheritance **chains**: turn 2 records its inherited `Personal`, so turn 3
  stays `Personal` after turn 1 leaves the window — correct, because turn 2's
  answer may carry turn 1's data.
- **Consequence:** deterministic rule I2 requires every step's `classification`
  to be at or above the task floor, so in a `Personal` conversation a plan whose
  planner labels a step `Public` is blocked and must be re-planned. A follow-up
  can therefore be stricter than the same sentence sent fresh. The planner is
  told the floor in its input, so this should be rare; it is recorded here so it
  does not read as a regression.
- `agent / plan.formulate` gains `conversation_task_ids`: the loaded turns' ids
  (e.g. `[187]`), `[]` when there were none, `null` when the read failed (D6).
  Always present, following the `l1_insight` / `refused` convention so JSONB `?`
  queries find every row. Pinned key counts move 28 → 29, and 29 → 30 for
  `CliInferred` with signals.

---

## 4. What is deliberately not in scope

- **Email.** `workers/email-in` sets `conversation` to each message's own
  `Message-ID`, so no email follow-up matches a prior turn and email behaves
  exactly as today. The lookup is keyed generically, so email inherits this the
  day threading (In-Reply-To / References) ships.
- **Seeding memory recall from the conversation.** It would help
  `recall_count: 0`, but it is a second mechanism with its own failure modes and
  the measured failure does not need it.
- **#699** (a planner cannot see the tool, method or parameters of its own prior
  steps *within* one task) is the intra-task half of the same shape. This design
  ships the cross-task half only; the two should stay separable.
- **An explicit reset command** (`/new`). The 5-hour window covers the measured
  behaviour; a command can be added later without changing anything here.
- **Operator-tunable window and turn count.** The constants are named in one
  module, so making them configurable later is mechanical.

---

## 5. Testing

Written first, each watched failing before the code that satisfies it.

**Pure unit tests**

- `record::from_plans`: ok steps only; dispatch order preserved; `returns`
  clamped at a char boundary; identifiers atomic; `CALLS_PER_TURN` and
  `TURN_RECORD_CAP` drop oldest and mark `_omitted_calls`; `data_class` is the
  max of floor and step classifications.
- `view::render`: oldest first; over-budget drops the oldest and marks
  `_omitted_turns`; the newest-alone case clamps rather than vanishing; one case
  per `reply_body` kind (completed, error, ask-timeout, blocked, refused,
  denied, no result); a turn with no record; a malformed record (with a positive
  control); a withheld turn.
- `inherit_floor`: raises, chains, never lowers, and keeps `AgentRaised`
  precedence.
- `serialise_context_for_agent`: byte-identical output when the conversation is
  empty; the key present when it is not.
- The prompt drift test, and the `plan.formulate` key-count pins including
  `null` versus `[]`.

**DB integration (Postgres required)**

- `conversation_turns`: the 5-hour boundary just inside and just outside,
  anchored on `created_at`; excludes itself, another peer, another conversation,
  another channel, and every non-terminal state (`pending`, `running`,
  `awaiting_operator`); returns the newest three in order. One case per
  replied-to state, including `crashed` (whose `result` is `NULL`), so the
  coupling to `notify_task_completed`'s list is asserted rather than assumed.
- `finalize` persists `turn_record`, and a non-channel task stores `NULL`.

**Scheduler integration (scripted formulator, no live model)**

- Two channel tasks in one conversation: the second formulation's input carries
  the first turn's call parameters, and its `plan.formulate` row records
  `conversation_task_ids = [first]`.
- A first turn whose answer carries an injection phrase renders withheld in the
  second turn and writes the `injection.blocked` row with `tier: "conversation"`.

**Planned mutants** (each with the test expected to kill it; a review round
follows, because a mutation proof counts only the mutants tried)

- Peer filter dropped from the query.
- Sink screen skipped.
- Inheritance taken over rendered turns instead of loaded turns.
- Window anchored on `now()` instead of `created_at`.
- `"conversation"` key emitted when empty.
- `Err` outcomes included among the calls.

**Gate.** `cargo test --workspace` on both hosts, with the delta predicted from
the new `#[test]` count and reconciled exactly; clippy `-D warnings` cold on
both. The Mac cannot compile `kastellan-core` e2e paths that need Linux, so the
DGX run is authoritative for those.

**Live acceptance (DGX, branch deployed).** The same two DMs as tasks 187/188:

1. the follow-up answers origin and destination for the three bookings it was
   just told about;
2. its `plan.formulate` rows carry `conversation_task_ids` naming the first task
   and floor source `conversation_inherited`;
3. it reaches `mail.get_attachment_text` with the carried identifiers rather
   than searching from scratch.

If it fails, ask the operator how the conversation looked from their side before
blaming the change under test.

---

## 6. Risks

- **A stale referent.** A 5-hour window can attach an unrelated earlier topic to
  a new message. The planner is told the block may be out of date, and the
  budget keeps it small. If this proves wrong in practice, the window is one
  constant.
- **Prompt cost.** Bounded at 16 KiB beside the existing 96 KiB plans budget.
  DGX plan latency is generation-bound, not context-bound, so the cost is
  tokens, not seconds.
- **Injection surface.** The conversation block is the first thing to re-enter a
  *later* task's prompt, so a blocked turn must be withheld rather than dropped
  silently, and the screen must run on every load. Both are pinned by tests and
  by a planned mutant.
- **A widening floor.** Inheritance can only raise, so the failure direction is
  a re-planned step, not a leak.
