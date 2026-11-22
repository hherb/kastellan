# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20261117_1044_pre-prune.md`](archive/handover_20261117_1044_pre-prune.md)
> (#1045's session section and the #1036–#1043 Previous lines in full),
> [`archive/handover_20261115_959_pre-prune.md`](archive/handover_20261115_959_pre-prune.md)
> (#959's session section and the #1022 block in full),
> [`archive/handover_20261114_1027_pre-prune.md`](archive/handover_20261114_1027_pre-prune.md)
> (#1024's session section in full),
> [`archive/handover_20261114_1025_pre-prune.md`](archive/handover_20261114_1025_pre-prune.md)
> (#1025's session section in full),
> [`archive/handover_20261114_1023_pre-prune.md`](archive/handover_20261114_1023_pre-prune.md)
> (#1021's session section in full),
> [`archive/handover_20261114_1019_pre-prune.md`](archive/handover_20261114_1019_pre-prune.md)
> (#1020's session section in full),
> [`archive/handover_20261110_993d_pre-prune.md`](archive/handover_20261110_993d_pre-prune.md)
> (#1017's section and the full #993 deletion map),
> [`archive/handover_20261109_996dgx_pre-prune.md`](archive/handover_20261109_996dgx_pre-prune.md)
> (#1009's section and the #996 1–4 "Previous" blocks in full); each snapshot's header names the older ones.
> ⚠️ **Repoint this line in the same commit as the snapshot.** It has been stale twice.

**Last updated:** 2026-11-22 (#1051 deployed; two more #956 items: a mid-sift error writes `sift.finished` `stopped: "error"`, a sift over a step whose newest call failed says `over_older` — PR open at writing, not yet deployed) ·
**Recent PRs, newest first:** this session's #956 PR (mid-sift error row + `over_older`, open at writing — check `gh pr list`), [#1051](https://github.com/hherb/kastellan/pull/1051) (#956 sift number on resume + `elements_unread`, merged, deployed DGX 2026-11-22), [#1049](https://github.com/hherb/kastellan/pull/1049) (#1047 runaway re-ask, merged, deployed DGX 2026-11-21), [#1048](https://github.com/hherb/kastellan/pull/1048) (#956 scrapbook-row reads, merged, deployed DGX 2026-11-20), [#1046](https://github.com/hherb/kastellan/pull/1046) (#1044 partial answer, merged, deployed DGX 2026-11-18), [#1045](https://github.com/hherb/kastellan/pull/1045) (#1024 re-ask quotes the line, merged, deployed DGX 2026-11-17), [#1043](https://github.com/hherb/kastellan/pull/1043) (#956 zero-read sift, merged, deployed DGX 2026-11-16), [#1042](https://github.com/hherb/kastellan/pull/1042) (Kiri survey, docs, merged), [#1041](https://github.com/hherb/kastellan/pull/1041) (tick refusal, merged, deployed DGX 2026-11-16), [#1040](https://github.com/hherb/kastellan/pull/1040) (#956 refused sift + repeat stash, merged, deployed DGX 2026-11-16), [#1038](https://github.com/hherb/kastellan/pull/1038) (#956 read allowlist, merged, deployed DGX 2026-11-16), [#1036](https://github.com/hherb/kastellan/pull/1036) (#956 sorter error + boot prompts, merged, deployed DGX 2026-11-15), [#1034](https://github.com/hherb/kastellan/pull/1034) (#956 failed reads, merged, deployed DGX 2026-11-15), [#1033](https://github.com/hherb/kastellan/pull/1033) (#956 tick class + late reads, merged, deployed DGX 2026-11-15), [#1032](https://github.com/hherb/kastellan/pull/1032) (#959, merged, deployed DGX 2026-11-15), [#1031](https://github.com/hherb/kastellan/pull/1031) (#958, merged, deployed DGX 2026-11-15), [#1030](https://github.com/hherb/kastellan/pull/1030) (issue 1029, merged, deployed DGX 2026-11-14), [#1028](https://github.com/hherb/kastellan/pull/1028) (#1022, issue 1026, merged, deployed DGX 2026-11-14), [#1027](https://github.com/hherb/kastellan/pull/1027) (#1024, merged, deployed DGX 2026-11-14), [#1025](https://github.com/hherb/kastellan/pull/1025) (#1023, #1015, #1016, merged, deployed DGX 2026-11-14), [#1021](https://github.com/hherb/kastellan/pull/1021) (#1019, #1018, merged, deployed DGX 2026-11-14), [#1020](https://github.com/hherb/kastellan/pull/1020) (#993 classic deletion, merged, deployed DGX 2026-11-14), [#1017](https://github.com/hherb/kastellan/pull/1017) (#993 step 4 ports, merged), [#1014](https://github.com/hherb/kastellan/pull/1014) (#1012, merged, deployed DGX 2026-11-09), [#1013](https://github.com/hherb/kastellan/pull/1013) (experiments #19–#20, merged), [#1009](https://github.com/hherb/kastellan/pull/1009) (#996 `invoke_skill`, merged, deployed DGX 2026-11-09), [#1007](https://github.com/hherb/kastellan/pull/1007) (#996 `floor_request`, merged), [#1003](https://github.com/hherb/kastellan/pull/1003) (#996 `python_skill`, merged), [#1002](https://github.com/hherb/kastellan/pull/1002) (#996 `l3_skill`, merged), [#998](https://github.com/hherb/kastellan/pull/998) (#996 `l1_insight`, merged), [#997](https://github.com/hherb/kastellan/pull/997) (#993 step 2, merged), [#995](https://github.com/hherb/kastellan/pull/995) (#993 step 1, merged), [#994](https://github.com/hherb/kastellan/pull/994) (#922 slice 4a, merged), [#992](https://github.com/hherb/kastellan/pull/992) (README, merged), [#990](https://github.com/hherb/kastellan/pull/990) (#987, merged), [#989](https://github.com/hherb/kastellan/pull/989) (#985, #988, merged), [#986](https://github.com/hherb/kastellan/pull/986) (#922 slice 2, `kastellan-cli session`, merged), [#984](https://github.com/hherb/kastellan/pull/984) (#965 items 4/6/7, merged), [#983](https://github.com/hherb/kastellan/pull/983) (#981, #970, merged), [#982](https://github.com/hherb/kastellan/pull/982) (#980, merged), [#975](https://github.com/hherb/kastellan/pull/975) (manuals, merged), [#974](https://github.com/hherb/kastellan/pull/974) (#973, #970 items, merged), [#972](https://github.com/hherb/kastellan/pull/972) (#965 items 3/8, prompt wording, merged), [#971](https://github.com/hherb/kastellan/pull/971) (#968, stashed sift read, merged), [#969](https://github.com/hherb/kastellan/pull/969) (#967, own table, #965 item 2, merged), [#966](https://github.com/hherb/kastellan/pull/966) (#960 forced table, #964, merged), [#963](https://github.com/hherb/kastellan/pull/963) (#960 by code, merged), [#962](https://github.com/hherb/kastellan/pull/962) (#922 sift `from`, merged), [#961](https://github.com/hherb/kastellan/pull/961) (#922 chain totals, merged), [#957](https://github.com/hherb/kastellan/pull/957) (#922 sift fixes, merged), [#955](https://github.com/hherb/kastellan/pull/955) (#922 `sift`, merged), [#951](https://github.com/hherb/kastellan/pull/951) (#941, #922 slice 1, merged), [#950](https://github.com/hherb/kastellan/pull/950) (#945, merged), [#949](https://github.com/hherb/kastellan/pull/949) (#947, merged); #946 … #795 are in the `960code` and older snapshots.
Older PRs are in the [`archive/`](archive/) snapshots; **`gh issue list --state open` is the live
answer** and the only one worth trusting. ·
**The DGX runs `main` with #1051 (deployed with `upgrade_from_git.sh` 2026-11-22 12:00, `NRestarts=0`, Matrix up, installed `agent_notebook.md` + `sift_sorter.md` sha256-equal to `main`'s, both env files identical to their backups `~/.config/kastellan/kastellan.env*.bak-pre1051`) and `KASTELLAN_PLANNER_MODE=notebook` in `.local`, redundant but accepted (⚠️ `classic` there stops the daemon at boot)** (classic ran once for #993, tasks 387–393, experiment #18: 7/7, 2160 s; backups `~/kastellan.env*.bak-pre993`. ⚠️ Its `git pull` over SSH fails: run it with `GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=url.https://github.com/.insteadOf GIT_CONFIG_VALUE_0=git@github.com:`; migration 0029 since #955; localmail `62bdec6`, `api_minor` 6) **on kernel 7.0.0-1019, pinned by name in `GRUB_DEFAULT`** — 7.0.0-1024 boot-loops (below); **the Mac runs #917's** (its localmail serves `api_minor` 4: after #949 the mail worker refuses there until it is upgraded). **DGX:**
`kastellan-guard` active on GPU, core `After=` it; **reboot-proven 2026-10-25** on #908's tree (the
one restart it cost was the Postgres race #909 fixes) and **again on #917's tree 2026-10-29** —
core `NRestarts=0`, Postgres waited out in 1017 ms, guard after four `/props` waits, Matrix first try).
`kastellan-guard` reads `disabled` in `is-enabled` — harmless, the target's `Wants=` names it. Guard
keys `KASTELLAN_GUARD_SERVER_{BIN,MODEL}` are in `.local` (backups `~/kastellan.env*.bak-pre909`).
**Mac:** [[mac-kastellan-install-recipe]], no guard configured (backups `~/.config/kastellan/*.bak-pre909`).
The DGX boot ERROR `install dir is writable … (mode 0775)` is **pre-existing** (since 2026-07-31).
Live: thinking **ON** (`reasoning_effort`, `TIMEOUT_MS=600000`). DGX rootfs images last rebuilt
2026-10-19. Deploy detail: this session's section and the `909` snapshot (#609).

> **Header convention (since 2026-09-11, after three recurrences).** This header names **PRs and
> issues only — never a branch name, a HEAD sha, or the word OPEN.** A merge falsifies those with no
> actor in between; a PR number it cannot. **The tip and the open set are one command each:**
> `git log --oneline -1 origin/main` and `gh pr list --state open`. Run them before trusting a word
> of this file.

> ⚠️ **Standing review hazards (full prose in the archive snapshots).** An issue's census and
> diagnosis can both be wrong — read the rows, not the issue [[issue-as-filed-can-carry-a-regression]].
> A fix for a reviewer's finding can carry the next defect, and a passing mutation proof is not a
> review [[mutation-proof-counts-only-mutants-you-tried]] [[plan-text-is-a-defect-source]] (#677,
> #720, #750 ×2). A forward reference auto-closed #718 — run the closing-keyword regex over every
> PR body and commit message [[pr-body-not-fixed-autocloses-issue]]. A fixture nobody rebuilds is a
> gate nobody runs [[stale-fixture-turns-a-gate-into-a-formality]], and a guard built from a census
> shares its blind spot [[guard-shares-the-census-blind-spot]].

---

## Current state

### This session (2026-11-22, latest): #1051 deployed; two more #956 items (PR open at writing)

- **Deploy:** #1051 to the DGX 2026-11-22 12:00 (`NRestarts=0`, Matrix up, env files identical to `bak-pre1051`,
  prompt sha256s equal to `main`'s). No experiment run.
- **A mid-sift error writes `sift.finished`.** `sift/run.rs` `error_stop` writes `stopped: "error"`, `error` (capped),
  `looked_at`, `kept`, `rows_unwritten`, then the task fails as before; covers `review_and_dispatch`, the cancel check
  and `append_rows`. A failed cancel check still writes the rows kept so far. ⚠️ That path is untested (no fixture
  fails `observe_state` on demand); the append path is (a squatter row at the sift's number).
- **`over: {"step": n}` after a newer failed call says so.** `chain::failed_since` (pure) → summary + `sift.finished`
  `over_older` naming the code. Prompt sentence added (**deploy: the install copies `prompts/agent_notebook.md`**).
- PG legs in `sift/tests/stop_findings.rs`; `test_harness::try_run` returns the loop's `Result`. Both mutants caught.
- **Review round (`/finalcheck`):** a newer call review *blocked* or the loop rejected (no `ctx.plans` record) is
  named too: the notebook loop keeps `chain::RefusedCall`s and passes them in `SiftRun.refused` (⚠️ a resumed run
  starts them empty); every early `sift.finished` (`reads_not_run`, `error`) carries `over_older`; an `append_rows`
  error after a failed cancel check is logged, not dropped. New PG leg (blocked call) and a failed-sift assertion
  pin `!is_sift_plan`; the error leg's counts differ (3/1/2). Untested arms + a cancel during reads labelled
  `reads_not_run`: [issue 1053](https://github.com/hherb/kastellan/issues/1053). `sift/run.rs` **485**,
  `notebook/run.rs` **492**.

### Previous (2026-11-22): two #956 items (PR #1051, merged, deployed DGX 2026-11-22) — a resumed run seeds its turn past the scrapbook's highest sift (`scrapbook::last_sift` → `task_exec::seeded_budget`; ⚠️ a sift that wrote **no** rows and a failed mirror write can still reuse a number); `template::elements_unread` counts fan-out elements skipped for want of the path. `task_exec.rs` **736**. Full section: this file at #1051 in git history.

### Previous (2026-11-21): #1049 deployed, experiments #35–#36 (tasks 507–520), #1047 closed — all three runaways (515, 519, 520; ~400 s thinking) recovered by the suppressed re-ask in 12–17 s; task 512's `request_timeout` was the task deadline clamping the call; wrong answers model-side (Q5 self-sent invoice, Q6 "cannot determine", Q7 stale "due"). `~/q1049.sh <from> <to>` on the DGX. Full section: this file at #1050 in git history.

### Previous (2026-11-20 → 21): #1048 deployed, experiments #33–#34; a thinking runaway is asked again without thinking (PR #1049, #1047, merged, deployed DGX 2026-11-21) — `notebook::ask` re-asks a blank `length` reply (`AgentError::is_blank_cut_off`) once, the same turn, thinking suppressed, only when that call let the model think (`llm_failure::CallFacts.thinking`; never the forced answer); a failed re-ask takes the old out-of-room path. Every `plan.formulate_failed` row carries `elapsed_ms` and `thinking` (⚠️ a timeout still has no token count — that needs streaming). Legs in `notebook/run/runaway_tests.rs`. Full section: this file at #1049 in git history.

### Previous (2026-11-18): #1046 deployed, a follow-up sift reads its rows' source (#956 item, PR #1048, merged, deployed DGX 2026-11-20) — `sift::template::item_at` (pure) resolves a placeholder against the item, then a scrapbook row's `source`, then its `fields` (exact key first); `unread_why` lists a row's source and field names. Experiments #31–#32: 14 of 14, ~9 right. Full section: this file at #1048 in git history.

### Previous (2026-11-17): a failed forced-answer turn is answered by code (#1044, PR #1046, merged, deployed DGX 2026-11-18) — `notebook::partial::partial_answer` (pure) writes `PARTIAL_HEAD` + one line per ticked finding plus every sift's kept rows, reviewed like any answer; `plan.discarded` reads `forced_answer_failed_partial_used`; nothing found → fails as before. ⚠️ Only ticks the notebook **took** count; the answer's `plan.formulate` row carries `partial::code_meta()`. PG legs in `run/partial_tests.rs`. Full section: this file at #1046 in git history.

### Previous (2026-11-16): a syntax error's re-ask quotes its line (#1024, PR #1045, merged, deployed DGX 2026-11-17) — `llm_failure::line_at_error` (pure) quotes the line a decode error points into; `notebook::ask` appends `That line reads: …` to the re-ask. ⚠️ serde's column 0 of a later line maps to the `\n` before it. Experiments #27–#28: 12 of 14 (task 457 syntax error, task 464 → #1044). Full section: the `1044` snapshot.

### Previous (2026-11-15 → 16): #956 items, Kiri survey (PRs #1043, #1042, #1041, #1040, #1038, #1036, merged, deployed DGX) — a zero-read sift's summary carries `unread_why`, `sift.started` `read_parameters` (`sift/run.rs` split to 406); Kiri not adopted, use **rootless podman** ([notes](../notes/2026-11-16-kiri-survey.md)); a refused tick names the step's state (`reply::tick_refused`); a call not run records `SIFT_REFUSED`/`INVOKE_REJECTED`/`BLOCKED` (⚠️ the planner never sees the code), a repeat names a held stash; unset `KASTELLAN_SIFT_READ_METHODS` keeps the advertised methods; `sift.finished` carries `sorter_error`, the daemon stops at boot on a missing/blank prompt file (issue 1037 filed). ⚠️ `inner_loop/execute.rs` **512**. Full sections: the `1044` snapshot.

### Previous (2026-11-15): #956 failed reads, tick class + late reads, #959 (PRs #1034, #1033, #1032, merged, deployed DGX 2026-11-15) — `read_errors` in `sift.finished` and the summary; ⚠️ `failed_reads` counts **items**; every item's read failed → `reads_failed` (⚠️ a one-item list too; a mostly-failed batch is [#1035](https://github.com/hherb/kastellan/issues/1035)). A sift's tick carries the class it ran at; a late reply makes no reads, `reads` counts reads **made**. `over: {"step": n}` continues an earlier sift only if step n's list is still the one it went over (⚠️ a re-run giving the **same** list also ends the chain). Full sections: this file at each PR in git history, the `959` snapshot.

### Previous (2026-11-14): #958 + issue 1029, #1022 + issue 1026, #1024, #1023/#1015/#1016, #1019/#1018 (PRs #1031 … #1021, merged, deployed DGX) — **#958/issue 1029:** a date column sorts by `table::dates::iso_key` (unread cells after the dates, counted in the note; ⚠️ the shown cell is unchanged); `render_table` groups in **stored** order, then sorts groups by `table::first_cell` (⚠️ a merged group sorts by its earliest stored row's cell). Full sections: this file at #1031/#1030 in git history. **#1022/issue 1026:** a bare-`id` source carries its tool (`SOURCE_TOOL_KEY`; ⚠️ older rows have no tag); `kept`/`withheld` count **items**; each stopped question named (`answer::stop_lines`). ⚠️ An id-less row is one item each. #1024 stays open for its second half (quote the line in the re-ask). Full block: the `959` snapshot. **#1024, #1023/#1015/#1016, #1019/#1018:** full sections: the `1027`/`1025`/`1023` snapshots. ⚠️ `raw_at_error` is counted from the reply's **first `{`**, column in **bytes**; a failed rewrite answers with the draft; `table::alike` merges one source's rows across sifts (an empty source joins nothing).

### Previous (2026-11-09 → 10): #993 step 4 ports + the classic loop deleted (PRs #1017, #1020, merged, deployed DGX 2026-11-14) — ⚠️ **`KASTELLAN_PLANNER_MODE=classic` stops the daemon at boot**; #940 item 3 (a trace near `n_ctx`): **operator to keep or close**. Timeout/deadline cases in `notebook/run/timeout_tests.rs` (the harness `Script` has `TIMEOUT` + `turn_delay`). ⚠️ The notebook prompt omits the 255-char non-ASCII atomic limit. ⚠️ Turn cap = `max_plans × NOTEBOOK_TURNS_PER_PLAN` (#1012). Full sections: the `1019` snapshot.

### Previous (2026-11-08 → 09): #996 1–5 of 5 (PRs #998 … #1009, merged) — answer `l1_insight`/`l3_skill`/`python_skill` (`None` on a **refused** plan); call `floor_request` (refused on a `sift`; ⚠️ lost on suspend/resume, #713) or `invoke_skill`; a multi-step call's `last_result`/`last_failure` list every step; tests in `notebook/run/agent_raised_tests.rs` (**no libtest count**). ⚠️ **Drop a pool `PgListener` before `pool.close()`** (40-min hang). ⚠️ `l3.invoke*` rows carry no `task_id`. Filed: 999–1001, 1004–1006, 1010, 1011. Prose: the `996dgx` snapshot.

### Previous (2026-11-06 → 08): #993 steps 1–2, #922 slices 2/4a, #987, #985/#988, #965 items 4/6/7 (PRs #997 … #984, merged) — what still binds

- `ScriptedNotebook`: ⚠️ a **first reply without `steps` is refused**; each `call_reply` lists a fresh step id; the
  router default `disable_thinking: true` makes a scripted daemon happy path **3 turns**. One `PgListener` per process,
  `MAX_FOLLOWS` **64**; ⚠️ a follow reads state before entries, and its 3 s poll tick, not the NOTIFYs, makes it
  correct. ⚠️ **`kastellan-cli` installs a warn-level stderr subscriber** (PG e2es fail, not skip, once PG is up).
  ⚠️ **bottie matches the parking marker by substring** ([hherb/bottie#242](https://github.com/hherb/bottie/issues/242)).
  ⚠️ A test that `claim_one`s must not leave an older task pending. ⚠️ A sift read's allowlist key is
  `read_key(tool, method)` (**`web-fetch.web.fetch`**); the `chain_distinct` → `totals` pass-through is untested.

### Previous (2026-11-05 → 06): #981 (PR #983), #980 (PR #982, deployed DGX, measured 378–384: 7/7), manuals (PR #975), #974 — what still binds (prose: the `983`/`981`/`980` snapshots)

- A table **none of whose refs can show rows** is shown back once (`sift::answer::table_left_out`). ⚠️ A resume whose
  history would not decode restores `ctx.plans` empty: a real own ref then costs one re-ask (accepted, pinned).
  **#970 keeps only its "known limits"** (operator to close or keep).
- A top-level `tool`+`method` is lifted under `call` (`notebook::reply::parse_reply`). ⚠️ A lifted call is invisible
  in audit; a value-less `"attachments.*"` key is not rescued.
- ⚠️ **A new manual chapter goes in both `scripts/site/build_manual.py` `MANIFEST` and `check-manual.sh`.** ⚠️
  **`Option::is_none_or` is Rust 1.82 > MSRV 1.78.** ⚠️ The DGX daemon log has **no notebook/sift INFO lines**; the
  audit rows are the only record. Filed: #976–#979.

### Previous (2026-11-03 → 05): `sift` + the scrapbook and its fixes (PRs #955 … #971, merged, deployed) — what still binds (prose: the `966`/`980` snapshots)

- Code in `core/src/scheduler/notebook/sift/`; reads are **one reviewed plan** (`StepPolicy::SiftReads`), rows in
  **`task_scrapbook` (migration 0029, append-only — applied on the DGX: never edit it)**. Knobs
  `KASTELLAN_SIFT_MAX_READS` (64), `KASTELLAN_SIFT_READ_METHODS`. ⚠️ **The sift number is the turn (`plan_count`)**;
  a `returns` beginning `sift reads:` is refused (`SIFT_READS_DECISION`). Open minors: #956, #958, #959.
- A forced answer with no table gets every sift's that kept rows (`sift::answer::forced_table`, #1012); its own table is taken out
  under it (`without_own_tables`). A stashed (over 64 KiB) step is read back with `StepDispatcher::stashed`; an evicted
  stash is a **failed read**. ⚠️ The stash is purged whenever the loop returns, **a suspend too**. A call on a done
  step is refused with the fix (`reply::not_pending`). `task_id`/`plan_count` are in `db::audit::PRESERVED_KEYS`.
- Judge a change over **≥ 2 runs**. ⚠️ **Never show a full answer example with a table**
  [[notebook-model-mirrors-rendered-shape]]. A prompt-only change needs **no rebuild**: copy to
  `~/.local/share/kastellan/prompts/`, restart core, check `agent_prompts`. ⚠️ A table naming an **earlier turn's**
  chain is not expanded (#956). `~/q960.sh <from> <to>` and `~/deploy-branch.sh` are on the DGX.

### Previous (2026-11-02): #941 + notebook slice 1 (PR #951) — `max_tokens` 16384 + a call timeout on every planner call; migration **0028** (⚠️ **applied on the DGX: never edit it**). ⚠️ Any key added to the render must be accepted in the reply.

### Previous (2026-10-31 → 11-01): #945, #947, #923, #922 parts C/A, the prompt cache, OpenShell (PRs #950, #949, #930, #938, #942, #943, #937, merged) — what still binds (prose: the `922nb`/`945`/`922a` snapshots)

- `plan.formulate_failed` keeps `raw_head`; a blank completion is re-asked once, a non-JSON one via `formulate_reformat`,
  never both (#939). ⚠️ Scripted fakes' "out of script" `Decode` needs a non-blank `raw`. Mail **requires localmail
  `api_minor` 6**. ⚠️ **The context binds, not the plan count** (#940). ⚠️ **DGX kernel 7.0.0-1024 boot-loops**
  [[dgx-kernel-7-0-1024-boot-loop]]; DGX **AEDT**, Mac **AEST**. ⚠️ `tool_allowlists` is read **once** at registry build.

### Previous (2026-10-30): #921 (PR #927), the #773 re-measure (PR #924) — over budget, outputs are **water-filled to an even share** (`inner_loop/summary/budget.rs`); **thinking stays ON**; measure with `kastellan-cli ask --classification-floor Public "<q>"`, one question at a time. ⚠️ localmail sync can stall with the unit `active` (#925). ⚠️ A new `AgentError::Decode` field breaks 7 fakes in `core/tests/`.

### Previous (2026-10-26 → 29): the boot arc — #909, #906, #913/#911, #915 (PRs #910, #912, #914, #916, merged) — what still binds

- Boot waits ≤ **60 s** for Postgres on `ECONNREFUSED`/`ENOENT`/`57P03` (`db::boot_wait`; since #949 the PG test
  harness uses it too). ⚠️ A paused-clock loop test needs an outer `timeout` **and** a call cap in the fake
  [[async-fakes-must-yield-like-io]]. A guard boot refusal is a thinned `guard_tier.boot_refused` row; ⚠️ a new guard
  key must name itself in its error. ⚠️ The daemon's JSON log is **stdout**, a boot refusal **stderr**.

### Earlier (2026-10-17 → 29): #920, #908, the supervisor arc (#905 … #899), sidecar/drain + gate counts (#893 … #879), #875 … #860 — one line each (prose: the `993s2`/`906` snapshots)

- Never prune a rendered view twice (#920). Guard port/alias are **derived** from `KASTELLAN_LLM_GUARD_URL`/`_MODEL` at
  install; `scripted_llm`'s default **503s `/props`** (#908). systemd `show`: `LoadState=not-found` is the tell for an
  unknown unit; unknown state words read `Transitioning` [[guard-shares-the-census-blind-spot]].
- ⚠️ **After `ECHILD` never kill** a sidecar; a torn-down pty is `EIO` on Linux, **EOF on macOS** (#891 open). Gate
  `IGNORED`/`FILTERED_OUT` are exact (`worker-report` **13**, `microvm` **4**). ⚠️ **Any `Cargo.lock` mtime change makes
  `python-exec:dev` stale** (#997 changes it). A squash commit can auto-close an issue
  [[pr-body-not-fixed-autocloses-issue]].
### Previous (2026-10-08 → 17): thinning and the Matrix inbound path — what still binds (prose: the `867`/`825`/`815` snapshots)

- **Thinning:** `kastellan_core::audit_thinning` (⚠️ `#[doc(hidden)] pub`); one `Thinning` per channel × action × kind;
  ⚠️ one process-wide `LOST_ROWS`; a crash loses the thinned tail (#817); ⚠️ **`ChannelEvents::audit` takes `action:
  &'static str`** — never widen it (#868). `InsertCause` (never the error text), one line per cause per stretch.
- **Inbound:** ⚠️ on an ack channel one undecodable event makes the whole batch undecodable; matrix-sdk 0.18 saves the
  sync token **before** handlers; owed: a live `LiveSdk::drain` leg (DGX); ⚠️ no `.await` before `PendingInsert`.

### Previous (2026-09-30 → 10-03): the audit sink (PRs #791 … #806) — ⚠️ **`Ledger::snapshot` reads `starting`, then live, then `pending`** (load-bearing); a row that must *stay pending* needs a **`connections: 0`** ledger; `stalled_pool` is built **inside** a runtime [[stalled-postgres-test-fixture]].

### Previous (2026-09-20 → 27): thinking (#773/#774), the mail worker (#760, #698, #763, #765), the worker-report arc (#725 → #750, #748, #755) — prose: the `773`/`673`/`748`/`755`/`698`/`905` snapshots

- **`KASTELLAN_LLM_THINKING_SWITCH`**: `chat_template_kwargs` (default) or `reasoning_effort` (**the only
  one Ollama honours**). ⚠️ **Never send both** (vLLM 0.15.1 400s). `llm_usage` on every
  `plan.formulate` row. ⚠️ **Thinking on vs off is not decided** — that is the live re-measure.
- **Mail:** below localmail API **1.6** (#947) search/attachments/header reads refuse — **no fallback**. Wire
  facts in `workers/mail/src/localmail_contract.rs` — ⚠️ **`include!`d three times: `pub const`s only,
  no `use`, no `//!`.** Live gate: `bash scripts/mail/live-shape-gate.sh`. ⚠️ A new `*_or_skip` helper
  must be classified in `microvm/guard.rs`.
- **Markers:** call `panic_hook::install_once()` first in every profiled binary. ⚠️ **`warn_and_fall_back!`
  must stay a MACRO**; **`eprintln!` is load-bearing** [[libtest-capture-only-print-macros]]; no marker
  may begin with `[SKIP]`/`[WARN]`/`[E2E]`. The gate scan reads bytes — `grep -a`, `LC_ALL=C`
  [[gnu-grep-binary-file-prints-nothing-exit-0]]; ⚠️ `grep -c` counts lines [[grep-c-counts-lines-not-matches]].

### Merged arcs — only what still binds

**#782/#783 (PR #787), #785 (PR #786):** **Replies queue per conversation** (`ReplyQueues`, cap 256); given up after ≥ 1 h **and** ≥ 3 refusals.
⚠️ A channel-wide failure RESTARTS every give-up clock. ⚠️ **The Matrix worker's 401 is
`UPSTREAM_UNAVAILABLE`, NOT `UPSTREAM_AUTH_FAILED`.** ⚠️ Never test a driver line through a scoped
`tracing` subscriber [[tracing-scoped-subscriber-interest-cache-flake]]. **#785:** recall excludes L0/L3.

**#769 (PR #781) — a refusal is not a death;** #673/#674 (PR #770): `UPSTREAM_AUTH_FAILED = -32004`
for localmail 401/403 (⚠️ only `ClientError::Rpc` keeps its type through `client_error_to_anyhow`). One classifier, `persistent/call_failure.rs`
(`Gone` / `Refused` / `CredentialRefused` / `Unavailable`), read by the supervisor **and** the polled
driver. ⚠️ A worker reads its credential **once, at spawn**. ⚠️ **A refused ack holds the next poll.**
⚠️ **Workspace MSRV is 1.78.** The cognee survey (#784, [notes](../notes/2026-09-28-cognee-survey.md))
is **not a dependency**. **#767/#768:** route spellings are pure `pub fn`s in `localmail_contract.rs`;
the live gate pages from offset **2** [[probe-value-equal-to-default-hides-dropped-param]].

**#692 / #688 / #685 / #683 / #680 — micro-VM budgets and freshness.** `kastellan_sandbox::bounded_command`
for any host probe; ⚠️ **a bounded runner must not join its drains** [[bounded-subprocess-must-not-join-drains]].
Both guest kernels lack Landlock (seccomp-only by design). One producer for `target/release/`:
`bash scripts/build-release.sh`, run LAST [[cargo-package-selection-changes-binary-bytes]]; the
freshness reference is the **sha256 of the baked copy** [[cargo-relinks-identical-mtime-not-content]].

**#660 — the second pre-release security audit.** Three **fail-closed** lockdown rules bite careless
fixtures (missing `KASTELLAN_SECCOMP_PROFILE`, an unenforceable Landlock ruleset, a corrupt guest env
token). Per-spawn dirs via `create_private_dir` — **do not "fix" back to `create_dir_all`**.
**Before release: flip force-routing on.** Deferred list in `docs/security-audit-2026-09-02.md`.

**One-liners.** #694: `truncate_payload` derives `req_summary = {head, sha256, len}` centrally, over the
**whole** request — ⚠️ no sink double can test it [[audit-sink-doubles-hide-storage-transforms]];
⚠️ review subagents mutate the working tree — give each its own `git worktree`. #681: a lean tail plus recovery beat a fat verbatim tail (68.3 % vs 45.8 %). #675: a
failed micro-VM boot leaves `console.log` in the kept run dir [[microvm-guest-failures-are-invisible]];
the launcher has no env [[microvm-launcher-knobs-must-be-argv]]; release is `panic = "abort"`
[[release-profile-panic-abort-kills-raii]].

### The guard tier — what still binds

- **D10 — ADVISORY defence-in-depth, NOT a gate.** 65 % recall (36/55) at FP-0; **5/8 missed** on
  narrative framing. **Nothing downstream may relax on it.**
- **τ = 0.79552656 is a REQUIRED operator input with no default**; **five misconfigurations STOP THE
  DAEMON** (D6), incl. a context below `SCAN_BYTE_CAP + 512 = 66 048` (D8). **`best_tau` returns
  NONE** on real captured content, so **corpus growth from production is the cheap path**.
- ⚠️ **#612 stays OPEN despite #624/#626** [[metal-prompt-processing-is-nonlinear]].

### Standing hazards that have each cost a session

> ⚠️ **Clippy: parity is a `rustup update`, and a cached run lies** [[local-clippy-not-ci-parity-rust-version]].
> Count the `Checking kastellan` lines (27). Force cold with `CARGO_TARGET_DIR=$HOME/.cargo-clippy-<topic>`,
> **never** by `touch`ing sources (falsifies the #687 image gate, #691). Sweep first, lint after.

> ⚠️ **A private `CARGO_TARGET_DIR` breaks daemon e2es** [[custom-cargo-target-dir-breaks-daemon-e2e]];
> **rust-analyzer's `cargo check` holds `target/debug/.cargo-lock`** — ~10 min per save (all ~190
> test targets). Iterate lib/bin unit tests in a private `CARGO_TARGET_DIR=$HOME/.cargo-target-<topic>`
> (never for daemon e2es or the gate sweep), or kill the IDE's cargo child — it re-spawns on **every
> save**, so `pkill -f "cargo check --workspace --message-format"` before each run (2026-10-07).

> ⚠️ **Do NOT edit the tree while a sweep compiles in it — and give every reviewer its own worktree**
> [[never-edit-tree-during-a-sweep]] [[worktree-cwd-lands-on-main]]. **A fresh worktree has no worker
> binaries** — `cargo build --workspace` first [[worktree-has-no-worker-binaries]]. ⚠️ **Copying files
> into a DGX worktree with macOS `tar` creates `._*` AppleDouble files** — `sandbox/tests/._x.rs` is a
> cargo test target; delete them before building.

> ⚠️ **Mac `syspolicyd` saturation** (`ps -o time` stuck at `0:00.00`): `sudo killall syspolicyd`; but a
> slow build is usually CONTENTION [[mac-fresh-large-binaries-hang-in-dyld]].

> ⚠️ **egress-proxy leaks on the Mac**; **a `pgrep -f` wait loop matches itself** [[pgrep-wait-loops-match-themselves]].

## Read these first

[`docs/architecture.md`](../../architecture.md) (process model), [`docs/threat-model.md`](../../threat-model.md)
(the invariant), [`docs/devel/ROADMAP.md`](../ROADMAP.md), then the memory notes and [`archive/`](archive/).

---

## Next TODO

> Only *open* work is listed. Shipped items move to [Recently merged](#recently-merged) or the ROADMAP.

1. **Deploy this session's #956 PR** (`upgrade_from_git.sh`), then **#956's remaining items** — unnamed
   `columns`/`order_by`, a zero-row sift's reusable number, a withheld summary's blank scrapbook entry, an earlier
   turn's table column order (`first_row_fields`), `as_number` on `1.200,00`, an earlier turn's chain not expanded, … — and #940 (item 3, or close:
   operator). Still unseen live: `forced_answer_failed_partial_used`; owed from #1028: a forced multi-sift answer's one
   `kept` per item and stop lines. Model-side, not code: Q1's empty price/GST cells, Q5's self-sent invoice, Q7's paid or
   stale "due" items. If the ~400 s runaway cost matters, #1047's option 1 (cap or budget the reasoning) is the open
   lever. **Owed:** a revoked mail key once (ask first); #926, #939, #771, #538; the Mac's localmail upgrade to
   `api_minor` 6; bottie's substring match on the parking line ([hherb/bottie#242](https://github.com/hherb/bottie/issues/242)).

2. **#702 follow-up #703** (#704/#705 done in PR #920). ⚠️ **The guard model never sees object keys** — any
   new worker passing a third-party JSON object through reopens it silently; needs a DGX guard
   calibration run. The localmail changes the operator offered on 2026-09-14 have **shipped
   upstream** (slices A–E); the worker's side is #760.

3. **The #750 residue: #751 and #757** (the `[panic]` cap's blind spot before the first knob
   read — needs a *measured* default-hook count per profile, DGX too).

4. **Test-harness honesty, now that the gate itself is tested.**
   [#718](https://github.com/hherb/kastellan/issues/718)
   (92 hand-rolled `[SKIP]`s — adding the `sandbox` profile is its acceptance test),
   [#721](https://github.com/hherb/kastellan/issues/721), [#723](https://github.com/hherb/kastellan/issues/723),
   [#724](https://github.com/hherb/kastellan/issues/724), [#691](https://github.com/hherb/kastellan/issues/691)
   (a decision), and #237's absent macOS CI leg. ⚠️ **A fresh worktree's checkout mtimes make
   `kastellan/python-exec:dev` read stale** — 7 extra Mac `[SKIP]`s in #609's sweep, not a regression.
   [#891](https://github.com/hherb/kastellan/issues/891): the vsock relays' error arms (`eprintln!` the errno; back off on `EMFILE`).
   [#878](https://github.com/hherb/kastellan/issues/878): the `microvm` profile's intermittent red
   test (Wikipedia heavy-page render times out in the VM; passed 2026-10-20, undiagnosed).

5. **The thinning family, now deployed (2026-10-18) — its loss lines are still unseen live.** A quiet
   restart says nothing (verified on both hosts); a **restart under traffic or a wedged Postgres** is what
   shows them. Look for: `[audit-lost]` with `; row N of this burst` (#829/#866), `N <action> audit rows
   … had no line …` before the channels stop and `N more … while the channels stopped` after (#869),
   `may not have been written` = check `audit_log` before calling a row lost (#863), INFO `channel pump
   ended with replies queued` (#832), `channel.inbound_dropped` (`stage`) and
   `channel.inbound_undecodable` (`part`) rows (#846/#855). ⚠️ A stale `matrix.ext4` answers
   `matrix.drain` with METHOD_NOT_FOUND (rebuild with `bash scripts/build-release.sh` + the rootfs
   script). Open: [#805](https://github.com/hherb/kastellan/issues/805), #809–#811, #868
   (compiler-checked bounds), #817 (a crash loses the thinned tail, both writers). Security items:
   #833 (stderr panic in a `Drop`; a design call, now also `bus_guards` and `InboundReceiver`), #831.
   **#870** (PR #871): verified live 2026-10-29 — the Mac install needed no hand `bootstrap`.

**On the micro-VM path — one issue left, and it needs a kernel build.**
[#668](https://github.com/hherb/kastellan/issues/668) — repin a guest kernel with
`CONFIG_SECURITY_LANDLOCK`. Its macOS twin has a detector (`macos_container_smoke` fails the day the
Apple `container` kernel enforces Landlock); revisit the container backend's injection in the same breath.

**A standing architecture item:** [#678](https://github.com/hherb/kastellan/issues/678) — **retire
truncation as the answer to "bigger than the budget".** Truncation does three jobs and only one
becomes map-reduce: a *control that stops seeing its evidence* (the guard's 64 KiB `SCAN_BYTE_CAP`;
reduce `p = max(p_i)`), a *record that must be faithful* (`truncate_payload` — **spill, never
summarise**), and a *resource guard* (`MAX_RECORD_BYTES` — **these stay**). `core/src/handoff.rs`
already stashes oversized results whole; slice (e) is #681's anchor index. Likely subsumes #604 and
#612. ⚠️ **The polarity inverts to fail-closed**, which needs its own test.

**THEN the guard arc:** [#612](https://github.com/hherb/kastellan/issues/612) (a design call; #616
unblocked its favoured option), with [#639](https://github.com/hherb/kastellan/issues/639) (split
`guard_tier_e2e.rs`) and [#638](https://github.com/hherb/kastellan/issues/638) (214 rustdoc warnings,
67 broken intra-doc links) beside it.

**Next up — operator's choice, each roughly one session.** Only the gotchas *not* in the issues:

- **[#550](https://github.com/hherb/kastellan/issues/550)** — the naive fix is wrong: compare the
  *folded* environment (`fold_env_files`), since the overlay legitimately overrides keys.
- **[#548](https://github.com/hherb/kastellan/issues/548)** (test units in the real user manager) / [#676](https://github.com/hherb/kastellan/issues/676)
  (a socket that vanishes mid-test; the `57P03` variant is fixed in #949). Restore the shared suffix with a
  `.suffix()` setter, **not** by reverting #641 [[issue-as-filed-can-carry-a-regression]].
- **Web workers — [#706](https://github.com/hherb/kastellan/issues/706) before any release** (no rate
  limiting, backoff, conditional requests or `robots.txt`); #707 blocked upstream — ⚠️ **do not adopt
  Obscura before its V8 bump lands**; our jail would be its only layer.
- **Email channel — slices 2 and 3.** Spec `docs/superpowers/specs/2026-07-28-email-fallback-channel-design.md`.
  Slice 2 = SMTP outbound (`lettre`, MIT) + round trip; slice 3 = DGX deploy + live tier —
  **restart `localmail-serve` (+ `localmail-daemon`) on the DGX first**.
- **Also open, no gotcha beyond the issue text:** #551, #519, #554 (needs a live DGX gate), #534.
- **Live guard-host facts:** `/props` reports `default_generation_settings.n_ctx` (no top-level
  `n_ctx`); the D8 floor is `-c 66048`. Guard keys live in `kastellan.env.local`. Launch line: #609's unit.
- **A Mac daemon deployment is a deliberate decision, not a task** (#612 means the guard fails open
  on large documents there). ⚠️ **Host-mode gliner on macOS now works** (#726).
- **Deferred with a reason:** macOS Seatbelt-loopback verification of mail tier 1a; Telegram inbound;
  MITM-of-browser via an NSS trust-store import, **not** `--ignore-certificate-errors-*`.

**File-split backlog (Item 9b)** — ⚠️ **`wc -l` before picking; every number below rots.** Split
**before** the change that grows a file, in a movement-only commit, and **prove the movement rather
than asserting it** — #730's split checked every moved region was **byte-identical** to its range on
`main` and that the `fn`-name set matched (which a diff cannot show), and #750's added a **negative
control** proving the checker can fail. ⚠️ `inner_loop/execute.rs` **512** (#956 items) — split before the next change (`sift/run.rs` was split to 406; **485** now; `notebook/run.rs` 492). Over cap today, biggest first: `core/tests/guard_tier_e2e.rs`
1558+ (#639), `core/src/workers/gliner_relex/tests.rs`, `db/src/asks.rs`,
`sandbox/src/linux_firecracker/plan.rs` (DGX-gated), `core/src/channel/ask_message.rs`, `db/graph.rs`,
`core/src/scheduler/asks.rs`, `core/src/tool_host.rs`,
`workers/mail/src/ids.rs`, `tests-common/src/require.rs`,
`llm-router/src/messages.rs`,
`core/src/scheduler/runner/task_exec.rs` **736** (+25 in #1051), `core/src/main.rs` **935** (+3 in #956's allowlist PR; +7 in #810, +3 in #829, +6 in #862, +10 in #869's review), `tests-common/src/microvm/{mod,container}.rs`, `sandbox/tests/macos_smoke.rs`,
`core/src/channel/mod.rs` **569** (+10 in #849, +6 in #835, +16 in #836, +5 in #852, docs — the `actions` consts are the natural cut).
Also over (grew in #818 without a split): `db/src/tasks.rs` **819** (+11 in #825: `CRASHED_DETAIL`),
`db/src/graph.rs` **~935**, `core/src/entity_extraction/batch_upsert.rs` **~520**.
Also over: `core/src/memory/l3_surface.rs` 539 (+5 in #785, a doc paragraph), `core/src/scheduler/inner_loop.rs` 906,
`tool_dispatch.rs` 722, `attach/tests.rs` 727, `require.rs` 661,
`scripts/run-e2e-gate.sh` 866 (shell), `gate_script_tests/run.rs` 586, `gate_script_tests.rs` 538. ⚠️ **`core/tests/mail_live_shape_e2e.rs` 566** — #949 put
its new leg in `mail_live_shape_e2e/hit_fields.rs` (a `#[path]` module); move the next one there too, or split. ⚠️ `audit/truncate/tests.rs` is 833.
Recent splits (the pattern to keep; list in the `867` snapshot): #905 `supervisor/src/lib.rs` 624 → 431, #869 `pg_events/{test_support,
lost_rows/tests}.rs` and `audit_sink_thinning_tests.rs`, #849, #835, #806 — each proved byte-identical.
`core/src/scheduler/notebook/run/tests.rs` **560** (#1049 replaced the runaway leg with `run/runaway_tests.rs`, 159; the next leg goes in its own file): #1023's review moved the selective-thinking case to `run/rewrite_tests.rs` (91). ⚠️ `sift/table/tests.rs` **494**, near the cap (+73 in #958 with its review; its date reader `table/dates.rs` 262, tests inline; split before the next change grows it) after PR #1030 split its distinct/grouping cases into `table/tests/distinct.rs` (125).
⚠️ **At or near the cap:** `main/audit_sink.rs` **500**, `worker_stderr/report/delivery/tests.rs` **500**,
`polled_driver.rs` **498**, `polled_driver/replies.rs` **478** — split before the next change grows them.
Per-PR growth history: the [`785` snapshot](archive/handover_20260929_785_pre-prune.md).

**Standing deferrals (no owner):** egress #242, #251, #304, #260; micro-VM #381 and **true `jailer`**
(seam in `confine.rs`); python-exec Phase 4 curated wheels; web-research polish; an ANN index on
`entities.embedding`. Net-worker-in-VM needs no new work — 5c's `NetClientTransport` is the mechanism.

---

## Load-bearing findings that still bind

- **The four faults (2026-08-02).** One Matrix message, four independent faults, each masking the
  next. **A green stack with a silent output means look at every layer, one fix at a time.**
- **Egress / MITM traps.** The MITM upstream trusts **webpki roots only**
  [[egress-proxy-upstream-trusts-webpki-only]]; a force-routed loopback needs an **IP SAN**
  [[macos-force-routed-loopback-needs-ip-san]]; a bare-host `Net::Allowlist` entry is an **all-port
  grant** [[bare-host-net-allowlist-is-all-port-grant]].
- **Process lessons that have each cost a re-run.** A truncated gate log is not a gate
  [[truncated-gate-log-is-not-a-gate]], and ⚠️ **`cargo test --workspace` is FAIL-FAST** — without
  `--no-fail-fast` a sweep stops at the first red binary and reports a plausible partial total
  (#750: 59 suites, 2552 passed). Mutation testing contaminates the git **index**
  [[mutation-testing-contaminates-the-index]]; revert by copying, never `git checkout`
  [[mutation-revert-never-git-checkout]]. `sqlx::migrate!` embeds at compile time
  [[sqlx-migrate-embeds-at-compile-time]]. ⚠️ **A mutation harness must tell "did not compile" from
  "compiled and failed"** — `cargo test` prints `error: test failed` (#745).

---

## Working state

### Test baseline (authoritative)

| Host | Commit | Result | clippy `-D warnings` | `[SKIP]` |
| --- | --- | --- | --- | --- |
| **DGX** (#909) | #909's PR on `f59e6cb2` | **5366 / 0 / 84**, **196** summaries, `TEST_EXIT=0`, worktree `~/src/kastellan-wt-896` (detached at `origin/main` + the 4 changed files, sha256-checked), `KASTELLAN_PG_BIN_DIR` set, `--test-threads=4`, `--no-fail-fast`, `--nocapture`; `[WARN]` 0; log `~/sweep-909.log`. Per-suite vs `~/sweep-609.log`: `kastellan_db` lib 251 → 258 (+7 `boot_wait`), new `boot_waits_for_postgres_e2e` 1, core lib 2576 → 2584 (+8: #908's review round, which the #609 sweep predated — the same +8 on the Mac) | not re-run (Mac cold clippy covers the change; nothing Linux-only) | **7** (as before) |
| **Mac** (#956 error row + `over_older`) | `fix/956-db-error-older-over`, primary checkout | **5559 / 0 / 51**, **201** summaries, `BUILD_EXIT=0` `TEST_EXIT=0`; `KASTELLAN_PG_BIN_DIR` set, `--no-fail-fast`, `--nocapture`, `--test-threads=4`; `[WARN]` 0; log `~/sweep-956b.log`. vs `~/sweep-956.log` (5558): +1 = core lib `chain::tests::a_failed_call_after_the_listed_success_is_named` (the two PG legs run inside `the_sift_loop`, one libtest count) | cold `cargo clippy --workspace --all-targets -D warnings` (`~/.cargo-clippy-956`, 27 `Checking`) | **23** (as before) |

The DGX row predates #910's review round (+4 db lib), #906 (+4 core lib, +2 binaries),
#913/#911 (+6 core lib, +1 e2e), #915 (+1 db lib), #920 (+13 core lib), #924 … #949, #945, #951, #955, #957, #961, #962, #963 and #966; all are platform-neutral and were not swept on the DGX. Older Mac rows (incl. #945, #922 nb, sift, sift-fix, chain totals) are in the `909deploy`/`921`/`922a`/`945`/`922nb`/`sift`/`960code` snapshots.

**Both hosts are load-bearing, in opposite directions — always check both.** A `launchd_agents.rs`
change is invisible to the DGX; the Mac compiles **zero** `systemd_user` tests
[[mac-compiles-zero-systemd-tests]]; Mac clippy compiles `cfg(target_os = "linux")` items out
(`cargo clippy --target aarch64-unknown-linux-gnu` catches pure-Rust crates in seconds; `core` won't
cross-compile) [[cross-clippy-pure-rust-crates]]. ⚠️ **A whole file can be `#![cfg(target_os = …)]`**.

**Predict the count, then reconcile the delta exactly — by PER-SUITE counts, not test names**
(`--nocapture` interleaves; `#[should_panic]` prints `- should panic ... ok`). ⚠️ **An `ignored` delta
with no new `#[ignore]` is usually a doc-test** [[ignore-fenced-doc-example-moves-ignored-count]].

⚠️ **A `[SKIP]` can hide a dead fixture for months** (the DGX gliner `.venv` was a copy of the Mac's).
`readlink` before believing a skip; prefer a REQUIRE knob and the gate script. Every `[SKIP]` **should**
render through [`tests_common::skip::skip_line`](../../../tests-common/src/skip.rs) — **92 do not**
(#718). And **an opt-in tier is invisible to a sweep**: `entity_extraction_e2e` and
`memory_entity_link_e2e` were broken by #719 for two weeks and never showed, because without
`KASTELLAN_GLINER_RELEX_ENABLE=1` they skip. **Run `KASTELLAN_GLINER_RELEX_ENABLE=1` for any change
touching the gliner path.**

⚠️ **Known sweep flakes, not regressions** — re-read the failure text each time, because a flake
attributed once gets re-attributed forever: `scheduler_ask_expiry_e2e`, `asks_e2e`,
`conversation_turns_e2e`, `secret_vault_e2e` under a full sweep. **`the database system is starting up` is fixed
in #949** (the harness now waits out `57P03`); a vanished per-test socket is #676 and still open. A dropped
ephemeral port is not a reserved one (fixed by confirming).

⚠️ **On this Mac the DEFAULT full sweep is red and `-- --test-threads=4` is green** — #548/#676
per-test-cluster contention, not a regression. The tell is `the database system is starting up`, the
failing suites **move between runs**, and each passes in isolation. Reduce threads; never reduce suites.

⚠️ **Every Mac sweep: `export KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"`
in the same command line**, then `grep -c '^\[SKIP\]'` against the baseline (**5** with the Apple `container` service running, 23 without — `container system start`) — a three-digit
count means Postgres was not found and the run is not evidence (recurred in #849's session).

**Mac verification runs from the repo's own `target/`**, logs under `$HOME` and **whole**
[[dgx-run-logs-tmp-scrubbed]] [[truncated-gate-log-is-not-a-gate]].

### Build & test

The cargo commands and the one-time Linux host setup are in [`CLAUDE.md`](../../../CLAUDE.md)
§ Build, test, run and § Linux host setup. **FC e2e gotchas (DGX):** build release binaries **only**
with `bash scripts/build-release.sh` (#682); `export PATH=$HOME/.local/bin:$PATH` (firecracker is off
the non-interactive ssh PATH); use `KASTELLAN_MICROVM_REQUIRE_E2E=1` **and `-- --ignored`** — or
simply `bash scripts/run-e2e-gate.sh microvm` — whenever a Firecracker run is meant to be evidence.
⚠️ The stale release launcher (`kastellan-microvm-run`) is baked into no image, so no freshness gate
sees it (#362). A VM worker's `program` is the **in-rootfs** path [[vm-worker-in-rootfs-binary-path]].
**gliner Python tests:** `cd workers/gliner-relex && uv run --frozen pytest -q` — **81** tests (70/71
was recorded here, in issue #736 and in #726's archived row, and was wrong in all three).
⚠️ **A fresh worktree has neither worker binaries nor the gliner `.venv`** — `cargo build
--workspace` and `scripts/workers/gliner-relex/install.sh`, or 3 suites silently `[SKIP]` (#750).

**The tree — 27 crates:** full layout in the root [`README.md`](../../../README.md) § Layout; load-bearing crates in [`CLAUDE.md`](../../../CLAUDE.md) § Project shape.

### Integration-suite map

Only the rows that tell you *where to look when something goes red*.

| Suite | Tests | What's verified |
| ----- | ----- | --------------- |
| `sandbox` (`linux_smoke` / `macos_smoke`) | 10 / 13 | **real** jails: fs invisibility, net deny, relative-path reject, OOM-kill, per-spawn `/tmp`, fresh session leader, **worker starts in `/` on both backends** (#719), the Firecracker VMM jail launching (#671), the #689 Landlock drift detector |
| `core` Firecracker (15 suites, `#[ignore]`, DGX) | 30 | **real KVM** round-trips, mem cap, net deny, warm idle, VMM confinement, egress + broker channels, persistent store. `bash scripts/run-e2e-gate.sh microvm` |
| `core` gliner (`gliner_relex_e2e`, `entity_extraction_e2e`, `memory_entity_link_e2e`) | 5 / 16 / 6 | the real model under the real sandbox; the latter two only with `KASTELLAN_GLINER_RELEX_ENABLE=1`. `bash scripts/run-e2e-gate.sh gliner` |
| `core` (`shell_exec_e2e`, `python_exec_e2e`) | 4 / 4 | **real** core→sandbox→worker round-trips; per-spawn scratch; secret-scrub |
| `core` (`egress_proxy_e2e`, `egress_force_routing_e2e`, `email_mitm_e2e`) | 3 / 4 / 2 | real sidecar + CONNECT; Linux no-direct-route; hermetic MITM |
| `core` (`injection_guard_e2e`, `secret_vault_e2e`, `guard_boot_row_e2e`) | 10 / 9 / 1 | **PG-required** policy rows, privacy invariant, fail-closed redemption |
| worker-report (5 suites, 2 crates) | 13 | a dying worker's last words reach a failing test; a real broken fd 2, re-exec'd. `bash scripts/run-e2e-gate.sh worker-report` (both hosts) |
| `core` `mail_live_shape_e2e` (`#[ignore]`) | 1 | our reading of localmail against the **live** service. `bash scripts/mail/live-shape-gate.sh` (both hosts) |
| `tests-common` `gate_script_tests` | 69 | the gate script's table **and** its verdict (floors, caps, exactly 1 whole summary and ≥ 1 test per binary, exact `ignored` (recounted from the suites' attributes) and `filtered out` counts, per-binary hook check), run against a fake `cargo` |
| container (`core`: `python_exec_container_e2e`, `python_exec_warm_idle_e2e`, `lifecycle_container_routing_e2e`; `sandbox`: `macos_container_smoke`; macOS) | 4 / 3 / 1 / 10 | **real** Apple `container` micro-VM round-trips, mem cap, warm reuse. `bash scripts/run-e2e-gate.sh container` |

## Key design decisions locked in

**Not restated here — they drift.** Hard constraints: [`CLAUDE.md`](../../../CLAUDE.md) § Hard
constraints; the rest: [`docs/architecture.md`](../../architecture.md) and the ROADMAP.
**The one worth repeating:** worst-case compromise reaches *at most* the agent's own OS user, its own
Postgres role, its own scratch FS, and the allowlisted endpoints for the *one* compromised tool
([`docs/threat-model.md`](../../threat-model.md)).

## Recently merged

Newest first; full prose in the [`archive/`](archive/) snapshots and git history.

- **#1049 … #1020** (merged 2026-11-10 → 21, deployed DGX by 2026-11-21; one-liners in the `959`/`1044` snapshots and this file's Previous lines): #1049 a thinking runaway is asked again without thinking; #1048 a follow-up sift reads its rows' source; #1046 a failed forced-answer turn is answered by code; #1045 a syntax error's re-ask quotes its line; #1043 a zero-read sift says why; #1041 a refused tick says what to send instead; #1040 a refused sift is not a reviewer block, a repeat names its stash; #1038 the sift's default reads are the advertised ones; #1036/#1034/#1033 five #956 items; #959 a chain does not link across a re-run of the listed step; #958 a date column sorts by date; issue 1029 a sorted table groups in stored order; #1022/issue 1026 table counts by item, a bare id carries its tool; #1024 `raw_at_error`; #1023/#1015/#1016 a failed rewrite answers with the draft; #1019/#1018; #993 the classic loop deleted (`classic` stops the daemon at boot).
- **#1017 … #984** (merged 2026-11-06 → 09, deployed DGX by 2026-11-09; one-liners in the `1023` snapshot): #993 steps 1, 2 and 4 ports (`ScriptedNotebook`; the daemon e2es and shared-code tests on the notebook); #1012 kept rows accumulate over a run's sifts; #996 1–5 (`invoke_skill`, `floor_request`, `python_skill`, `l3_skill`, `l1_insight`); #922 slices 2 and 4a (`kastellan-cli session`; notebook default); #987 one shared `LISTEN`, cap 64; README (⚠️ restates the manual's chapter 4: change both together); #985/#988; #965 items 4/6/7; #981/#970.
- **#982 … #950** (merged + deployed DGX 2026-11-02 → 06, measured 301–384): `sift` + the scrapbook (0029), the notebook behind `KASTELLAN_PLANNER_MODE`, bounded planner calls, and the sift/table/forced-answer fixes of #957–#982 (one line each in the ROADMAP's #922 entry); manuals (#975).
- **#949 … #908** (merged 2026-10-24 → 11-01; #949 … #920 one-liners in the `1019` snapshot, deployed both hosts): deploy docs; dependabot rustls (⚠️ makes `python-exec:dev` stale) and uv; #915's paused-clock pin; `boot_report::record_refusal` + the late-Postgres boot e2e (#914); a guard boot refusal is a thinned row (#912); core waits ≤ 60 s for Postgres (#910); the supervised `kastellan-guard` unit (#908). **#905 / #903 / #901 / #899** (2026-10-23 → 24): health-wait timeouts name the crash-loop evidence; `ServiceStatus::Transitioning`; `uninstall` keeps the unit file / plist on a failed stop; headless launchd `uninstall`, `$HOME` uid check.

---

## How to update this document at session end

1. Move anything shipped from [Next TODO](#next-todo) into [Recently merged](#recently-merged) and
   add the ROADMAP line, in the **same commit**.
2. Update the [Test baseline](#test-baseline-authoritative) with the gate that actually ran, on the
   host it ran on, and **reconcile the delta against the row above it**. An unexplained delta is a
   finding, not a rounding error.
3. Record what still binds — the finding, not the narrative. The archive snapshots are the
   long-form record; keep **what would change a decision** here, except where the *way* it was
   found is itself the lesson (which is most of the ⚠️ blocks above).
4. Keep this file under ~500 lines. Past that, snapshot to
   `archive/handover_<date>_<topic>_pre-prune.md`, compress in place, and **repoint the link in the
   header in the same commit**.
5. **Keep the header free of VCS state** — PRs and issues only, no branch names, HEAD shas or "OPEN".
6. **Grep the PR body and every commit message for closing keywords before merging:**
   `(close[sd]?|fix(e[sd])?|resolve[sd]?)[[:space:]:,]+#[0-9]+` — including forward references.
