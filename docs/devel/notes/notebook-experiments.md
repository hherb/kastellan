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
