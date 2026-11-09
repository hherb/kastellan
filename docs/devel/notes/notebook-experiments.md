# Notebook experiments — the running log (#922)

One entry per experiment, **appended, never rewritten**. Each entry records the date, host and commit, the knobs
as set, what changed and why, the questions, per-question results, what we learned, and how to reproduce. No
personal details: labels, task ids, counts and verdicts only. The DGX audit tables hold the rest.
Protocol: [the sift design §9](../../superpowers/specs/2026-11-02-sift-and-scrapbook-design.md).

**The seven questions** (`~/requery7.sh` on the DGX, asked one at a time with `kastellan-cli ask
--classification-floor Public`):

| # | label | the question asks for |
| --- | --- | --- |
| Q1 | flight table | a table of the 5 most recent domestic flight bookings (date, from, final destination, price, GST) |
| Q2 | invoices | every invoice received by email with a PDF since 1 July 2025 (sender, date, amount due) |
| Q3 | attachments | the 10 most recent emails with an attachment (sender, date, subject, file names) |
| Q4 | one PDF | one named fee-proposal PDF read in full (fee items, total, conditions) |
| Q5 | latest invoice | the most recent invoice (sender, date, amount) |
| Q6 | latest flight | the most recent flight booking (from, to) |
| Q7 | payments | the payments currently due or overdue |

**Reproduce any entry:** on the DGX, branch-deploy its commit (`scripts/upgrade_from_git.sh` with `git switch
<branch>`, see the `dgx-deploy` memory). Set its knobs in `~/.config/kastellan/kastellan.env.local`, restart with
`systemctl --user restart kastellan-core`, check `/proc/<MainPID>/environ`, then run `bash ~/requery7.sh`. Read
results with `SELECT id, state, plan_count, finished_at-started_at, result FROM tasks WHERE id BETWEEN …` and the
notebook with `SELECT seq, kind, payload FROM task_notebook WHERE task_id = …`.

Cell format: `turns / seconds`, then the outcome.

---

## 2026-11-01 — baseline: classic loop (tasks 246–252)

- **Host / build:** DGX, #949 deployed. Ollama 0.20. `gemma4:26b-a4b-it-q8_0-ctx64k`, thinking ON
  (`reasoning_effort`), `KASTELLAN_PLANNER_MODE` unset (classic).
- **Results:** Q1 6 / 570 · Q2 7 / 1053 · Q3 2 / 297 · Q4 4 / ~120 · Q5 3 / 122 · Q6 6 / 903 · Q7 **failed**
  (in every classic baseline).

## 2026-11-02 #1 — notebook, first rounds (tasks 259–271)

- **Build:** PR #951 at 46549d3a, then fcf74bc6, ab1ba8b4, 4239846e. Ollama 0.20, gemma ctx64k, thinking ON,
  `KASTELLAN_PLANNER_MODE=notebook`.
- **Hypothesis:** a bounded per-turn prompt finishes the seven where the classic loop runs out of context.
- **Results:** every task failed, all on model behaviour; none failed on the loop's logic:
  - the method put in `tool`;
  - the whole step list re-sent on a replan;
  - a tick plus a call for the same step;
  - render keys echoed back;
  - one turn thinking through the whole `max_tokens` (408 s).
- **Learned:** the model mirrors the shape it is shown. Read the obvious intent rather than refusing (memory
  `notebook-model-mirrors-rendered-shape`).
- **Changed:** six fixes, each with a test (see PR #951).

## 2026-11-02 #2 — notebook, thinking ON (tasks 272–279)

- **Build:** f47c3167 (Q2 re-asked on d847354c). Ollama 0.20, gemma ctx64k, thinking ON, notebook.
- **Results:**
  - Q1 13 / 718, 5 rows with price + GST
  - Q2 failed at the ceiling, 17 / 922; on d847354c completed, 22 / 958 (some candidates unchecked)
  - Q3 2 / 225
  - Q4 5 / 195
  - Q5 5 / 196
  - Q6 3 / 122
  - Q7 **completed**, 21 / 936
- **Prompt size:** 5.5k–14k tokens per turn; classic reached up to ~49k.
- **Operator verdict (eyeballed, 2026-11-02):** "they seem correct". The failures are context or plan exhaustion.

## 2026-11-02 #3 — bonsai-2 27B (tasks 280–281, stopped)

- **Build:** d847354c, Ollama **0.40**. Model `bonsai-2:27b-ctx64k`: a dense qwen35-architecture model, Q2_0,
  layered with `num_ctx 65536` to match gemma's tag. Thinking ON, notebook.
- **Results:**
  - Q1 11 / 781 (row 5 differs from #2)
  - Q2 5 / 948, honest but no amounts; one turn hit the per-call timeout
- **Learned:** decode ~25 tok/s against gemma's ~42, so 42–205 s a turn. Stopped: "if bonsai is slower than
  gemma, there is no point" (operator).

## 2026-11-02 #4 — gemma, thinking OFF (tasks 283–290)

- **Build:** d847354c; Q1 re-asked on 06113d26. Ollama 0.40, `KASTELLAN_LLM_DISABLE_THINKING=1`, notebook.
- **Results:**
  - Q1 failed at 10 / 160; on 06113d26, 17 / 230, but **only 4 rows, out of order**
  - Q2 11 / 154
  - Q3 2 / 52
  - Q4 5 / 63
  - Q5 4 / 36 (same answer as #2)
  - Q6 4 / 42 (**a different answer** from #2)
  - Q7 12 / 158 (a different list)
  - Total 735 s, against #2's 3350 s.
- **Learned:** 4–6× faster (≈14 s a turn), but quality drops on the hardest question.
- **Changed:** a full tick may close a failed step (06113d26).

## 2026-11-02 #5 — gemma, selective thinking (tasks 291–300)

- **Build:** cb1f59d0 (plan, replan and answer-rewrite turns think), then 19a0124b (repeat-call guard) and
  20469667 (a replan keeps the called step). Ollama 0.40, `KASTELLAN_LLM_DISABLE_THINKING=1`, notebook.
- **Runs:** task 291 (on cb1f59d0) ran out of budget: one call was repeated for 20 turns. Task 292 (on 19a0124b)
  failed: a replan dropped the step it called. The run below is on 20469667.
- **Results:**
  - Q1 28 / 923, 5 rows but 3 bookings that #2 found are missing
  - Q2 12 / 178
  - Q3 3 / 131
  - Q4 6 / 105
  - Q5 4 / 37, **an older invoice: wrong**
  - Q6 5 / 63 (same answer as #2)
  - Q7 14 / 660
  - Total 2097 s.
- **Learned:** no clear win. Both quality misses come from stopping the search too early, which thinking at
  answer time cannot fix.
- **Operator decision:** gemma MoE, thinking ON, notebook mode: "a correct answer is more important than speed".
  The DGX was set back to `KASTELLAN_LLM_DISABLE_THINKING=0`.
- **Next:** the sift-and-scrapbook design (one broad query, one sift step, a code-rendered table).

## 2026-11-03 #6 — notebook + `sift` (tasks 301–307)

- **Build:** `main` at 3c6f7a0f (PR #955, `sift` + the scrapbook), deployed with a plain `upgrade_from_git.sh`
  (migration 0029 applied at boot). Ollama 0.40.13, `gemma4:26b-a4b-it-q8_0-ctx64k`, thinking ON
  (`KASTELLAN_LLM_DISABLE_THINKING=0`, `reasoning_effort`), `KASTELLAN_PLANNER_MODE=notebook`,
  `KASTELLAN_LLM_TIMEOUT_MS=600000`, sift knobs unset (64 reads; mail get_message / get_attachment_text,
  handoff.fetch).
- **Hypothesis:** with one broad search and one `sift`, the find-all questions finish in a few turns, and their
  tables are at least as complete as #2's.
- **Results:**
  - Q1 3 / 291. One sift (50 items, 18 reads, 56 s) kept 18. The table had all 18 rows, not 5. The date column
    was sorted as text (`DD Mon YY`). The same booking appeared once per email about it (up to 3 times), and one
    international fare was kept.
  - Q2 8 / 830. Five sifts: one over a first 50-item search, then a chain of four over a 100-item search
    (`from` = `next_from`), keeping 25 + 28 + 24 + 9. **The answer tabled only the last link**, 9 rows ("9 kept of
    14 looked at"), where about 86 were kept. The 64-read cap and about 2 reads per item gave about 30 items per
    sift.
  - Q3 2 / 123, no sift, looks right.
  - Q4 4 / 201, no sift. The PDF was read in two pages. The stated total did not equal the lump sums: the operator
    should check it.
  - Q5 4 / 101, no sift. The answer is the newest invoice (it matches Q3's list).
  - Q6 40 / 928, **"cannot determine"**: a regression against #2's 3 / 122.
    - A `mail.get_message` result was stashed.
    - The model fetched it as tool `handoff`, method `handoff.fetch`. The dispatcher matched only `fetch`, so the
      call failed with `UNKNOWN_TOOL`.
    - 35 turns then followed with the get_message call discarded as `repeated_call`, plus 2 decode failures.
  - Q7 5 / 427. A chain of three sifts kept 13 + 23 + 16. **The answer tabled only the last link** (16 rows). Kept
    rows went back to 2017: the sorter has no "now" to judge "currently due" against.
  - Total 2901 s, against #2's 3350 s.
- **Learned:**
  - `sift` makes find-all questions short in turns.
  - The answer does not join a continuation chain: the prompt's "a list of names, one per sift" is not followed.
  - The table has no notion of dates, duplicates, a row limit, or "now".
  - The handoff fetch spelling was a latent bug: the prompt and the sift allowlist say `handoff.fetch`, but the
    dispatcher wanted `fetch`.
- **Changed** (branch `fix/922-sift-measure-fixes`, not yet re-measured): the dispatcher accepts `handoff.fetch` as
  well as `fetch`; a sift from the last one's `next_from` is linked to it (`continues`) and a table naming the last
  link shows the whole chain; rows alike but for their source are shown once; `answer.table` takes `limit`; the
  sorter is told `now` and writes dates `YYYY-MM-DD`.
- **Reproduce:** as above. Same build, then `bash ~/requery7.sh`.

## 2026-11-04 #7 — the sift fixes (#957) (tasks 308–314)

- **Build:** `main` at cc9574ed (PR #957), plain `upgrade_from_git.sh` (no migration; `kastellan.env` identical
  after install). Knobs as #6 (verified in `/proc/<MainPID>/environ`).
- **Hypothesis:** with the chain joined and `handoff.fetch` served, Q6 is short again and Q2/Q7 table every link.
- **Results:**
  - Q1 7 / 908. Three sifts: one over a 50-hit search (18 kept), then a chain of two over a 100-hit search
    (34 + 25). **The table joined the chain** ("59 kept of 100 looked at; 22 duplicates shown once", 37 rows,
    dates `YYYY-MM-DD`), but had **no `order_by` and no `limit`** for "the 5 most recent". The body said
    "25 bookings" (the last link) and called the table partial: 37 of 50 items were `unread` (the read was the
    attachment text; confirmations without one were decided on the hit, leaving price/GST empty).
  - Q2 7 / 592. A first sift (28 of 29, abandoned), then a chain of two with a new criterion (24 + 15). **The
    table joined the chain** ("39 kept of 50 looked at; 1 duplicate"), unordered; **the body said "15"**. A few
    rows predate the asked range (the search filters the email date, not the invoice's).
  - Q3 2 / 138, Q4 5 / 163, Q5 3 / 76 — as before.
  - Q6 **3 / 60**: fixed (#6: 40 / 928, "cannot determine").
  - Q7 4 / 1026. A sift over the search (50 read, 25 kept, 438 s), then a sift over that scrapbook (21 kept,
    no reads). **Answered in prose with no table** ("21 payments … would need a further step").
  - Total 2963 s (#6: 2901 s).
- **Learned:** chains and the handoff spelling work live. The body's counts come from the last sift's
  summary, which carried only its own counts. Using the table's `order_by`/`limit`, and naming a table at all,
  is still left to the model, and it does not always do it.
- **Changed** (branch `fix/922-sift-chain-totals`): a sift that continues a chain carries `chain: {sifts,
  looked_at, kept}` for the whole chain, and the prompt says to quote it. Filed #960 for the rest (no
  order/limit, no table, unread items with no attachment).
- **Reproduce:** as above, then `bash ~/requery7.sh`.

## 2026-11-04 #8 — the chain totals (#961) (tasks 315–321)

- **Build:** `main` at 5f9bd7a7 (PR #961), plain `upgrade_from_git.sh` (no migration; `kastellan.env` identical
  after install). Knobs as #6 (verified in `/proc/<MainPID>/environ`). Prompt `agent_notebook` sha256 `025fc4ba…`.
- **Hypothesis:** a body that gives a count after a chain gives the chain's, matching the table note.
- **Results:**
  - Q1 4 / 276. A sift over a 50-hit search (13 reads, **37 unread**, 18 kept), then a sift over that scrapbook
    (17 of 18). **This time the table had `order_by` date desc and `limit` 5** ("showing the first 5 of 10 rows").
    3 of the 5 rows have no from/price/GST (unread items, #960 item 3).
  - Q2 4 / 382. **A chain of two** (40 + 10 looked at, 38 + 7 kept). Table note "45 kept of 50 looked at; 15
    duplicates shown once", **body "45 invoices": the chain's count, as intended** — but the table has 30 rows.
    No `order_by` (rows in email order). Only 50 hits searched (#7: 100).
  - Q3 2 / 228, Q4 4 / 188, Q5 3 / 136 (Anthropic, 2026-11-02), Q6 3 / 135 — as before.
  - Q7 4 / 538. A sift over the search (50 read with `mail.get_message`, 45 kept), then a sift over that scrapbook
    (8 kept). **A table this time** (#7: prose only). Body "8 payments", table 6 rows (2 duplicates). Kept rows
    from 2021 and 2024 as "currently due".
  - Total 1883 s (#7: 2963 s).
- **Learned:** the chain count works live (Q2). The body's count is the sorter's `kept`, which still counts
  duplicates that the table later shows once (Q2 45 vs 30 rows, Q7 8 vs 6). Using `order_by`/`limit` and
  naming a table are **not deterministic**: Q1 used them in #6 and #8, not in #7; Q7 tabled in #8, not in #7.
  One run per change cannot tell a prompt fix from luck.
- **Next:** #960 item 1 as a prompt change, measured in #9.
- **Reproduce:** as above, then `bash ~/requery7.sh`.

## 2026-11-04 #9 — an `answer` example with `order_by`/`limit` (tasks 322–328) — **worse, reverted**

- **Build:** as #8 (5f9bd7a7), with only `prompts/agent_notebook.md` replaced (sha256 `2dcc2a6e…`, in
  `agent_prompts`) and core restarted. The change: the sift section showed a whole `answer` reply (`tick` +
  `answer` with `table: {scrapbook, order_by: {date, desc}, limit: 5}`, body "Your 5 most recent bookings (12
  found):") and bullets on when to use `order_by` and `limit`. Knobs as #6.
- **Hypothesis:** shown the shape, the model orders and limits "the N most recent" tables every time (#960 item 1).
- **Results:**
  - Q1 9 / 940, **no table**: "the filtering process failed". Six sifts. Twice the model sifted the 27-row
    scrapbook `from` 64 (the search's `next_from`): 0 items, reported as a success (`looked_at: 0`,
    `remaining: 0`), which it misread. Then a sift over step 2 (only sifts) was refused.
  - Q2 4 / 265, table, unordered (as #8).
  - Q3 2 / 132, **body "Here are your 10 most recent emails with attachments:" and nothing else**: no sift, no
    table, the list not written. The example's body is a lead-in to a rendered table; copied here, it drops the
    answer.
  - Q4 5 / 166, as before.
  - Q5 3 / 408, right, plus a 1-row table (`limit` 1) it did not need.
  - Q6 3 / 256, **the body ("Hong Kong to Sydney") contradicts its own 1-row table (Tokyo Haneda → Kumamoto)**.
  - Q7 4 / 911, **"cannot determine"** (#8 had a table).
  - Total 3078 s (#8: 1883 s).
- **Learned:** a worked example is copied **whole**: its lead-in body and its `limit` landed on questions with
  no sift (Q3) and on single answers (Q5, Q6). This is the #951 lesson again: what the model is shown, it
  mirrors. A prompt fix for item 1 needs an example that cannot be mistaken for a general answer shape, or a
  code-side default. The Q1 failure exposed a loop defect.
- **Changed:** prompt reverted on the DGX (sha `025fc4ba…`, core restarted) and on the branch. Fixed in code
  (branch `fix/960-sift-from-past-end`): a sift whose `from` is at or past the end of its list is refused
  with the list's length (`sift/run.rs::check_from`), not run as an empty success.
- **Reproduce:** the tried prompt is kept on the DGX as `~/agent_notebook.md.960` (sha256 `2dcc2a6e…`); copy it over
  `~/.local/share/kastellan/prompts/agent_notebook.md`, restart `kastellan-core`, `bash ~/requery7.sh`; restore
  `~/agent_notebook.md.main961` after.

## 2026-11-04 #10 — #960 by code (PR #963) (tasks 329–335)

- **Build:** PR #963 at 42c91057 (on `main` cbb89143, #962), branch-deployed by hand (`git switch`,
  `scripts/build-release.sh`, `./target/release/kastellan-cli install` with the Matrix flags; `kastellan.env`
  identical after install). Knobs as #6 (`KASTELLAN_PLANNER_MODE=notebook` checked in `/proc/<MainPID>/environ`).
  Prompt `agent_notebook` sha256 `6d42008e…` (the sift example's `read` is a two-read list; `distinct`; the
  default order).
- **Hypothesis:** four deterministic fixes close #960's gaps without a worked answer example: a table with no
  `order_by` is newest first; an answer without a table after a sift that kept rows is shown back once; the body
  can quote `distinct`; an item with no readable attachment takes the next read.
- **Results:**
  - Q1 3 / 348, table, 5 rows. The model used the read list (attachment, then message): **0 unread** of 50
    (#8: 37), and **all 5 rows have price and GST** (#8: 3 empty). 18 kept, 6 duplicates merged, `limit` 5,
    newest first. One re-ask (a top-level `data_ceiling` key, #945 path).
  - Q2 4 / 291, a chain of two (27 + 23 looked at, 21 + 15 kept). **Body "33 invoices" = the table's 33 rows**
    (`distinct`; #8: body 45, table 30). Ordered by date (no `order_by` named). The sorter still kept one
    invoice dated before the asked range (#960 item 4, not addressed). Single read, 5 unread.
  - Q3 2 / 233, the 10 emails in a model-written table, as before. One re-ask (an invalid JSON escape).
  - Q4 7 / 330 (#8: 4 / 188), right items; its stated total still needs the operator's check (as #957).
  - Q5 3 / 65, Q6 4 / 121 — right, as before.
  - Q7 4 / 911. Read list used (message, then attachment), 0 unread; 30 kept, `distinct` 28. **A planner call
    timed out, so the answer came on the forced-answer turn**, where the show-back does not apply: the model
    wrote its own 4-row prose table and a note "28 distinct items … only a subset shown". Kept a 2021 bill as
    "currently due" (sorter judgement, as #8).
  - Total 2299 s (#8: 1883, #9: 3078). The show-back fired **0** times.
- **Learned:** the read list and `distinct` work on first use. The default order is visible in Q1 and Q2. The
  show-back cannot help an answer forced by a timeout: a forced answer after a sift that kept rows still loses
  the scrapbook's table (Q7).
- **Reproduce:** deploy 42c91057 as above, then `bash ~/requery7.sh`; `bash ~/q960.sh 329 335` dumps the
  results, sift rows and re-asks.

## 2026-11-04 #11 — #960 by code, second run (tasks 336–342)

- **Build:** as #10 (42c91057), same knobs and prompt. Run straight after #10.
- **Hypothesis:** #10's gains hold on a second run (model behaviour varies between runs, #8).
- **Results:**
  - Q1 3 / 290, table, `limit` 5, newest first. Read list used again: **0 unread** of 50. But 4 of the 5
    newest rows are mails the sorter kept with only a date and a destination (no from, price or GST): sorter
    judgement on the criterion, not unread items. 21 kept, 12 rows.
  - Q2 3 / 292, **failed**. The search returned 68 KB, which was handoff-stashed, so the sift over the step
    found no `results` and was refused. The replan then called the done step 1 again; `validate` refused it
    ("not a pending step") and the re-ask repeated it. Two older defects, filed as #964.
  - Q3 2 / 305, Q4 5 / 251 — as before.
  - Q5 4 / 248, right (same as #10), plus a 5-row table it did not need (it sifted).
  - Q6 5 / 496, **a different booking from #10's** (one re-ask, an empty-at-EOF reply).
  - Q7 6 / 1047: three sifts and one refused; the third stopped at the deadline with 36 left. **Forced answer**,
    prose, 2 items, no table. The show-back fired 0 times again.
  - Total 2929 s (#10: 2299).
- **Learned:** over two runs the read list (Q1 0 unread both times) and the default order hold. `distinct` was
  quoted where the answer gave a count. The show-back never fired: every table-less sift answer came on the
  forced turn (Q7 in both runs), which it skips. Next for #960: attach the newest sift's table to a **forced**
  answer by code. A broad search large enough to be stashed breaks the sift over it (#964).
- **Reproduce:** as #10; `bash ~/q960.sh 336 342`.

## 2026-11-04 #12 — a forced answer gets its table by code; a sift reads a stashed search (PR #966) (tasks 343–349)

- **Build:** PR #966 at 6aeb89bf (on `main` b84f86f5, #963), branch-deployed with `~/deploy-branch.sh`
  (`kastellan.env` identical after install). The #965 commit a2b59dbc came after this measure and is **not** deployed.
  Knobs as #6. Prompt `agent_notebook`: system sha256 `946fc6d4…`, unchanged by #966.
- **Hypothesis:** a forced answer after a sift that kept rows gets the newest sift's table by code; a sift over a
  stashed search reads the stash back instead of being refused; a replan told "it is done … list it under a new id"
  stops re-calling a done step (#964).
- **Results:**
  - Q1 3 / 370, table, `limit` 5, newest first, 4 of 5 rows with price and GST. 17 kept, 2 duplicates merged.
  - Q2 4 / 389, a chain of two (25 + 25 looked at). Body "32 invoices" = the table's 32 rows (`distinct`). One
    invoice dated before the asked range again (#960 item 4). **The search was not stashed this time**, so the
    #964 stash path was not exercised.
  - Q3 2 / 288, Q4 4 / 159 (total 9943, as before), Q6 3 / 87 (FJGPFY), all as before.
  - Q5 4 / 600, right (Anthropic A$219.52). **A forced answer** (0 reasoning chars), and it **has a table**:
    5 rows, "5 kept of 20 looked at".
  - Q7 4 / 543, a written answer (not forced), table of 14 rows, chain 50 → 42 → 14.
  - Total 2436 s (#10: 2299, #11: 2929). No reply re-called a done step.
- **Learned:** the one forced answer in this run kept its table. Neither #964 path came up live: the stash path is
  covered by loop tests in `sift/tests/measure_findings.rs`, the done-step refusal by unit tests in
  `notebook/reply/tests.rs`.
- **Reproduce:** `~/deploy-branch.sh fix/960-forced-table-964` (6aeb89bf), then `bash ~/requery7.sh`;
  `bash ~/q960.sh 343 349`. A forced turn shows as `llm_usage.reasoning_chars` 0 on its `plan.formulate` row.

## 2026-11-04 #13 — PR #966, second run (tasks 350–356)

- **Build:** as #12, run straight after it.
- **Hypothesis:** as #12.
- **Results:**
  - Q1 6 / 717. The same sift ran twice (turns 2 and 5, 50 items each, 15 kept each time); one re-ask (`unknown
    field step`). Table newest first, but the dates differ from run A for the same bookings (2026-10-20 three
    times): the sorter read the booking mail's date, not the flight's, in this run.
  - Q2 8 / 865, **forced**. Two sifts were refused for a `from` past their list's end (`next_from` 57 on a list of
    21; 30 on 27): #962 working, though the model still miscounts. **The table was attached by code** (24 rows,
    "27 kept of 30 looked at; 20 not yet looked at"), **but the model also wrote its own 6-row table** in the body
    above it. Two tables, the first partial.
  - Q3 2 / 226 (a numbered list, not a table), Q4 4 / 283 (9943) — right.
  - Q5 3 / 173, right, **forced**, with a 3-row table.
  - Q6 6 / 321: a different booking again (5NQ5P9, Coffs Harbour → Learmonth, from a change notification), as in
    #11.
  - Q7 4 / 701, written answer: body "43 items" = the table's 43 rows (45 kept, 2 duplicates). The sorter
    kept about 30 Qantas Hotels booking confirmations as "due or overdue", a criterion judgement.
    ⚠️ This answer turn's `plan.formulate` row was over 4 KiB and was stored as the bare `_truncated` envelope,
    which has **no `task_id`**: filed as #967.
  - Total 3286 s. No reply re-called a done step.
- **Learned:** across the two runs, all 3 forced answers had a table (#10/#11: 0 of 2). Next for #960: a
  forced answer whose body holds its own markdown table above the attached one shows two tables. Q1's sift
  being repeated and Q6's moving answer are model variance, already seen in #11.
- **Reproduce:** as #12; `bash ~/q960.sh 350 356`.

## 2026-11-05 #14 — a stashed read reaches the sorter as its text (PR #971) (tasks 357–363)

- **Build:** `main` at acb187f4 (#971), deployed with `upgrade_from_git.sh` (`kastellan.env` identical after
  install). #972's prompt wording is **not** deployed: `agent_notebook` is still `ef4a9233…` (loaded 2026-11-04).
  Knobs as #6.
- **Hypothesis:** a sift read over 64 KiB is sorted from its stashed text, not its placeholder (#968); over-cap
  `plan.formulate` rows carry `task_id` (#967).
- **Results:**
  - Q1 4 / 414, table, `limit` 5 of 9 rows, newest first, price and GST on all 5. 15 kept of 50, 2 duplicates.
    Two itineraries show twice, once per leg's destination (same price).
  - Q2 4 / 329, a chain of two (42 + 8 looked at). Body "29 invoices" = the table's 29 rows (40 kept, 11
    duplicates). One finding withheld by the guard.
  - Q3 2 / 170, **no list at all**: no sift ran, and the answer's table named `sb:emails_with_attachments`, so
    the body was one line plus the refusal line. The #960 show-back could not fire (it needs an answer with no
    `table` after a sift that kept rows; this one named a table, and no sift ran).
    Filed as #973.
  - Q4 7 / 307, right (total 9943, conditions).
  - Q5 3 / 376: a table of 12 with a forwarded payment chase first (criterion judgement).
  - Q6 3 / 229: Coffs Harbour → Sydney, a 2027 booking (as #11/#13, the answer moves between bookings).
  - Q7 5 / 933: three sifts (64 + 36 read, then 35 over the kept rows), 19 rows, some from 2016–2022: the
    sorter judged "currently due" loosely.
  - Total 2758 s. No forced answer (every final `plan.formulate` row has `reasoning_chars` > 0); no
    `plan.formulate_failed` row.
- **Learned:** whether the #968 path came up is not known: the daemon's log holds no `notebook`/`sift` lines at its
  level, and the sift rows do not say which reads were stashed. A table
  can be named with no sift behind it: filed as #973, fixed in PR #974 by showing such an answer back once.
- **Reproduce:** as #12, on `main` acb187f4; `bash ~/q960.sh 357 363`. Log `~/requery7.log.971a`.

## 2026-11-05 #15 — PR #971, second run (tasks 364–370)

- **Build:** as #14, run straight after it.
- **Hypothesis:** as #14.
- **Results:**
  - Q1 3 / 347, table, `limit` 5 of 9, newest first; one row has no origin or price. 15 kept of 50, 3 duplicates.
  - Q2 6 / 771, three sifts (the first two over earlier rows, no reads). Body "44 distinct invoices" = the
    table's 44 rows (45 kept, 1 duplicate). Dates are written three ways (`2026-09-20T13:45:33+00:00`,
    `2026-10-06 23:01`, `2026-08-14`); the order still holds, as each starts `YYYY-MM-DD`.
  - Q3 2 / 152, a numbered list of 10 (as #12/#13). The #973 shape did not recur.
  - Q4 4 / 167: the fee items and conditions right, but **"a total is not explicitly stated"** (9943 in every
    run since #949). Model variance on one read.
  - Q5 3 / 710, **forced** (`reasoning_chars` 0 on its last row; one `plan.formulate_failed`), with its table:
    the same 11 rows as #14.
  - Q6 3 / 96: Coffs Harbour → Sydney, as #14.
  - Q7 5 / 914: 19 rows, mostly 2017–2025 bills and receipts. The last sift's 31 items were all `unread` and
    were decided on their hits. The sorter's "currently due" is loose in both runs.
  - Total 3157 s (#14: 2758).
- **Learned:** across the two runs every sift table's count matched its body, and the one forced answer had its
  table. What is still wrong is judgement, not code: Q7's "currently due" (both runs), Q4's total (once), Q6's
  booking. #973 (#14's Q3) is fixed in PR #974; to be measured.
- **Reproduce:** as #14; `bash ~/q960.sh 364 370`. Log `~/requery7.log.971b`.

## 2026-11-06 #16 — an answer table naming no scrapbook ref is shown back (PR #974) (tasks 371–377)

- **Build:** PR #974 at b707f8e4 (on `main` 9ea85fc4, so #972's prompt wording is in too), branch-deployed with
  `~/deploy-branch.sh` (`kastellan.env` identical after install, `NRestarts` 0, Matrix up on the first try).
  `agent_notebook` `1658ca9a…` (new: #972). Knobs as #6.
- **Hypothesis:** #973's shape (a table named with no sift behind it) is shown back and answered with a list; #972's
  wording changes nothing measurable.
- **Results:**
  - Q1 **failed**, 1 / 90. The first reply wrote its call without the `call` wrapper (`{"step": 2, "tool": "sift",
    …}`: `unknown field step`, as in #13). The #945 re-ask (thinking off) then wrote invalid JSON: it copied the
    prompt's `"filename": "{attachments.*}"` as a key with no value. Filed as #980.
  - Q2 10 / 980: six sifts and one refused (`from` 27 past a list of 23, #962). Body "64 distinct invoices" = the
    table's 64 rows (75 kept of 100, 11 duplicates); some rows are dated before the asked range (#960 item 4).
  - Q3 2 / 143, a numbered list of 10, right. #973's shape did not recur, so its show-back was not exercised.
  - Q4 4 / 221: fee items and conditions right, **no total** again (as #15).
  - Q5 5 / 135, prose with no sift: a different invoice from #14/#15's (the same day, another sender).
  - Q6 5 / 347: Coffs Harbour → Learmonth (5NQ5P9), the booking from #11/#13.
  - Q7 4 / 699: 37 rows, about 20 of them Qantas Hotels booking confirmations (as #13): "currently due" judged
    loosely again.
  - Total 2615 s.
- **Learned:** 6/7. The one failure is a pre-existing reply shape the re-ask could not rescue (#980), not #974. Q4's
  "no total" is now 2 of the last 3 runs (9943 in every run before #15). That came with the #972 wording only in
  this run, so it is model variance until shown otherwise. #973's path still needs a live hit; its loop wiring
  is the #960 show-back's.
- **Reproduce:** `~/deploy-branch.sh` on `fix/973-table-names-no-scrapbook` (b707f8e4), then `bash ~/requery7.sh`;
  `bash ~/q960.sh 371 377`. Log `~/requery7.log.974a`.

## 2026-11-06 #17 — a call written without its wrapper is read as `call` (#980) (tasks 378–384)

- **Build:** `fix/980-bare-call-shape` at 1b61fe10 (= `main` 9fe99923, #974 merged, + #980's fix), branch-deployed with
  `~/deploy-branch.sh` (`NRestarts` 0, Matrix up on the first try). Prompt unchanged from #16. Knobs as #6.
- **Hypothesis:** the #13/#16 reply shape (`{"step": 2, "tool": …}` with no `call`) no longer fails a turn; a refusal over
  a stray call field names the fix in the re-ask. Also the second #974 run owed (#16 was the first).
- **Results:**
  - Q1 3 / 324, table, `limit` 5 of 9, newest first. 2 of 5 rows have origin, price and GST; 3 are "prepare for
    your flight" / booking-update emails with no figures (sorter judgement). 16 kept of 50, 3 duplicates.
  - Q2 4 / 374, a chain of two (63 + 47 read). Body "38 distinct invoices" = the table's 38 rows, every one dated in
    the asked range (#16 had some before it); some are the operator's own sent invoices.
  - Q3 2 / 144, a numbered list of 10, right.
  - Q4 4 / 122: fee items, **total 9943** and the conditions, all right (no total in #15/#16).
  - Q5 8 / 374, prose: "Accounts Team, 2026-11-05, $12,540.00" — the newest is a DNA Recruit *remittance*
    (Q3's list), so "invoice" is judged loosely.
  - Q6 4 / 371: Coffs Harbour → Sydney (FJGPFY, a 2027 booking), with a table of the 16 flight emails.
  - Q7 3 / 429: 44 rows, again with ~15 Qantas Hotels confirmations and 2020–2025 items; "currently due" still loose.
  - Total 2138 s (#16: 2615). **No `plan.formulate_failed` row in any task.**
- **Learned:** 7/7, the first run since #14 with no failed formulation. Whether the bare-call lift fired is **not
  visible**: a successful `plan.formulate` row keeps the plan, not the raw reply. #973's show-back did not come up
  (no table named without a sift). Task 371 (#16) would **not** have been rescued by this fix alone: its first reply
  also wrote `"attachments.*"` as a key with no value, which the parse never reached because it stopped at `step`.
  What is wrong is judgement again (Q1's figure-less rows, Q5's remittance, Q7).
- **Reproduce:** `~/deploy-branch.sh` on `fix/980-bare-call-shape` (1b61fe10), then `bash ~/requery7.sh`;
  `bash ~/q960.sh 378 384`. Log `~/requery7.log` → renamed `~/requery7.log.980a`.

## 2026-11-08 #18 — the one classic comparison run #993 asks for (tasks 387–393)

- **Build:** `main` dd814896 (#994 + #995 merged), deployed with `scripts/upgrade_from_git.sh` (both env files
  identical after install, `NRestarts` 0, Matrix up). Ollama 0.40.13, `gemma4:26b-a4b-it-q8_0-ctx64k`, thinking ON
  (`reasoning_effort`). **`KASTELLAN_PLANNER_MODE=classic`** in `.local` for this run only, confirmed in
  `/proc/<pid>/environ`, and set back to `notebook` by the same script afterwards (confirmed the same way).
- **Hypothesis:** none to test — #993 keeps classic working for one run on the post-#994 tree before deletion, so the
  record shows what retiring it gives up. Compare with #17 (notebook, 378–384, same questions).
- **Results** (turns are `plan_count`):
  - Q1 6 / 534: a table of 5, **every row with price and GST** (#17: 2 of 5).
  - Q2 4 / 605: a list of 30 invoices (#17: a 38-row table, all in range).
  - Q3 2 / 123: a numbered list of 10, right.
  - Q4 4 / 156: fee items and conditions; **no total** ("not explicitly stated"; #17: 9943).
  - Q5 4 / 309: Mackay Marina Village, 30 Oct 2026, $1,124.15.
  - Q6 5 / 151: Coffs Harbour → Learmonth (the 5NQ5P9 booking of #11/#13/#16).
  - Q7 5 / 282: overdue and upcoming payments, prose lists.
  - Total 2160 s (#17: 2138). **No `plan.formulate_failed` row in any task.**
- **Learned:** on this tree classic is not worse by count: 7/7 and about the same wall time. It wins Q1's figures, and
  loses Q4's total and Q2's table. Single run, so per-question differences are within the variance seen across #14–#17.
  The case for retiring it is not quality but upkeep: one loop, one prompt, one set of tests. The five plan features only
  classic can produce (#996) are not exercised by these questions.
- **Reproduce:** `~/classic993.sh` on the DGX (it switches `.local`, runs `bash ~/requery7.sh`, switches back);
  `bash ~/q960.sh 387 393`; bodies in `~/classic993.bodies`. Log `~/requery7.log` → renamed `~/requery7.log.classic993`.

## 2026-11-09 #19 — #996's five ports on the live tree, first run (tasks 394–400)

- **Build:** `main` c209a8dd (#998, #1002, #1003, #1007, #1009 merged), deployed with `scripts/upgrade_from_git.sh`
  (`NRestarts` 0, Matrix up; installed `agent_notebook.md` sha256-equal to the repo's). Knobs as #18 but
  `KASTELLAN_PLANNER_MODE=notebook` (confirmed in `/proc/<pid>/environ`).
- **Hypothesis:** the prompt and reply changes of #996 (`l1_insight`, `l3_skill`, `python_skill`, `floor_request`,
  `invoke_skill`; `deny_unknown_fields` on the reply) do not cost the seven questions anything, and the planner does
  not raise those fields where nothing calls for them. Compare with #17 (notebook) and #18 (classic).
- **Results** (turns are `plan_count`):
  - Q1 3 / 340: table, 5 of 18 rows; **1 of 5** with price and GST (#17: 2, #18: 5).
  - Q2 9 / 787: four unchained sifts; a 36-row table, all in range; the body says "13" (the last sift's kept).
  - Q3 2 / 214: a table of 10, right.
  - Q4 4 / 216: fee items, **total 9943**, conditions — right.
  - Q5 9 / 939: **no answer.** Four near-identical sifts (kept 12, 11, 30, 0). The fourth hit the 15-minute ceiling
    after its reads (`stopped: "time"`, `sorter_calls: 0`). The forced answer said nothing matched, and got no table
    because the newest sift kept none → issue 1012.
  - Q6 4 / 136: Coffs Harbour → Learmonth (as #18).
  - Q7 4 / 529: a 6-row table, again with 2020/2021 items.
  - Total 3161 s (#17: 2138, #18: 2160). **No `plan.formulate_failed` row in any task.**
- **Learned:** 6/7. The miss is the old judgement problem (repeat the search rather than answer from what is
  kept), made visible by the ceiling, not the ports. **None of the 43 readable turns raised any of the five fields**
  (17 more turns were over-cap rows, stored truncated with `_truncated`). No `l1.*`/`l3.*` rows.
- **Reproduce:** `bash ~/requery7.sh`; `bash ~/q960.sh 394 400` (→ `~/q996a.txt`). Log → `~/requery7.log.996a`.

## 2026-11-09 #20 — #996's five ports, second run (tasks 401–407)

- **Build, knobs, hypothesis:** as #19, run straight after it.
- **Results:**
  - Q1 4 / 730: table, 5 of 13, **every row with price and GST** (first notebook run to match #18). The rows are the
    newest *flight dates* (2027), and two sifts read the same 50 items.
  - Q2 6 / 496: "35 distinct" = the 35-row table; one row dated 2024-12-05 (outside the asked range).
  - Q3 2 / 145: a numbered list of 10, right.
  - Q4 4 / 136: fee items, **total 9943**, conditions — right (both runs; #18 classic had no total).
  - Q5 3 / 197: Garmin, 2026-11-07, $20.00, with a 5-row table.
  - Q6 3 / 80: Coffs Harbour → Sydney (the FJGPFY 2027 booking, as #17).
  - Q7 5 / 468: one overdue invoice ($19,431.18) plus a 3-row table carrying 2020 Qantas Hotels items.
  - Total 2252 s. **No `plan.formulate_failed` row in any task.**
- **Learned:** 7/7. Over #19–#20: 13 of 14 answered, no failed formulation, no agent-raised field anywhere. The
  ports are neutral on these questions, which is what the hypothesis asked; they are still **unexercised live**
  (none of the seven questions calls for a skill, an insight or a floor raise). Per-question differences (Q1's
  figures, Q2's count, Q6's booking) are within the spread of #14–#18. On this evidence #996 can close.
- **Reproduce:** as #19; `bash ~/q960.sh 401 407` (→ `~/q996b.txt`). Log → `~/requery7.log.996b`.
