# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20261108_996l3_pre-prune.md`](archive/handover_20261108_996l3_pre-prune.md)
> (#998's section and the 2026-11-07 "Previous" blocks in full),
> [`archive/handover_20261108_993s2_pre-prune.md`](archive/handover_20261108_993s2_pre-prune.md)
> (the 2026-10 "Previous" arcs in full),
> [`archive/handover_20261107_922s4_pre-prune.md`](archive/handover_20261107_922s4_pre-prune.md)
> (#987's and the README session in full) and
> [`archive/handover_20261107_986_pre-prune.md`](archive/handover_20261107_986_pre-prune.md)
> (#986's session and review round); each snapshot's header names the older ones.
> ⚠️ **Repoint this line in the same commit as the snapshot.** It has been stale twice.

**Last updated:** 2026-11-08 (#996 3 of 5: the notebook answer carries `python_skill`) ·
**Recent PRs, newest first:** [#1003](https://github.com/hherb/kastellan/pull/1003) (#996 `python_skill`), [#1002](https://github.com/hherb/kastellan/pull/1002) (#996 `l3_skill`, merged), [#998](https://github.com/hherb/kastellan/pull/998) (#996 `l1_insight`, merged), [#997](https://github.com/hherb/kastellan/pull/997) (#993 step 2, merged), [#995](https://github.com/hherb/kastellan/pull/995) (#993 step 1, merged), [#994](https://github.com/hherb/kastellan/pull/994) (#922 slice 4a, merged), [#992](https://github.com/hherb/kastellan/pull/992) (README, merged), [#990](https://github.com/hherb/kastellan/pull/990) (#987, merged), [#989](https://github.com/hherb/kastellan/pull/989) (#985, #988, merged), [#986](https://github.com/hherb/kastellan/pull/986) (#922 slice 2, `kastellan-cli session`, merged), [#984](https://github.com/hherb/kastellan/pull/984) (#965 items 4/6/7, merged), [#983](https://github.com/hherb/kastellan/pull/983) (#981, #970, merged), [#982](https://github.com/hherb/kastellan/pull/982) (#980, merged), [#975](https://github.com/hherb/kastellan/pull/975) (manuals, merged), [#974](https://github.com/hherb/kastellan/pull/974) (#973, #970 items, merged), [#972](https://github.com/hherb/kastellan/pull/972) (#965 items 3/8, prompt wording, merged), [#971](https://github.com/hherb/kastellan/pull/971) (#968, stashed sift read, merged), [#969](https://github.com/hherb/kastellan/pull/969) (#967, own table, #965 item 2, merged), [#966](https://github.com/hherb/kastellan/pull/966) (#960 forced table, #964, merged), [#963](https://github.com/hherb/kastellan/pull/963) (#960 by code, merged), [#962](https://github.com/hherb/kastellan/pull/962) (#922 sift `from`, merged), [#961](https://github.com/hherb/kastellan/pull/961) (#922 chain totals, merged), [#957](https://github.com/hherb/kastellan/pull/957) (#922 sift fixes, merged), [#955](https://github.com/hherb/kastellan/pull/955) (#922 `sift`, merged), [#951](https://github.com/hherb/kastellan/pull/951) (#941, #922 slice 1, merged), [#950](https://github.com/hherb/kastellan/pull/950) (#945, merged), [#949](https://github.com/hherb/kastellan/pull/949) (#947, merged); #946 … #795 are in the `960code` and older snapshots.
Older PRs are in the [`archive/`](archive/) snapshots; **`gh issue list --state open` is the live
answer** and the only one worth trusting. ·
**The DGX runs `main` with #995 (deployed with `upgrade_from_git.sh` 2026-11-08, both env files identical after install, `NRestarts=0`, Matrix up) and `KASTELLAN_PLANNER_MODE=notebook` in `.local`, now redundant since #994** (classic ran once for #993, tasks 387–393, experiment #18: 7/7, 2160 s; backups `~/kastellan.env*.bak-pre993`. ⚠️ Its `git pull` over SSH fails: run it with `GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=url.https://github.com/.insteadOf GIT_CONFIG_VALUE_0=git@github.com:`; migration 0029 since #955; localmail `62bdec6`, `api_minor` 6) **on kernel 7.0.0-1019, pinned by name in `GRUB_DEFAULT`** — 7.0.0-1024 boot-loops (below); **the Mac runs #917's** (its localmail serves `api_minor` 4: after #949 the mail worker refuses there until it is upgraded). **DGX:**
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

### This session (2026-11-08, latest): #996 3 of 5 — `answer.python_skill` on the notebook (PR #1003)

- `AnswerSpec.python_skill` (optional `PythonSkillCandidate`) → the terminal plan (`run.rs`, beside `l3_skill`); the same
  `execute` grounding gate. `reply.rs::skill_or_drop` is now **generic**: a mis-shaped `python_skill` drops with a WARN,
  not the answer. Prompt text in `agent_notebook.md`'s `answer` bullet (no worked example), pinned. ⚠️ **The classic
  prompt never documented `python_skill`** — on classic it only ran if the model guessed the key, so the notebook
  prompt is its first real exposure. ⚠️ `plan.formulate` has **no compact top-level `python_skill` key** (`l3_skill`
  has one): the row's whole `plan` object carries it.
- Tests: `reply/tests.rs::an_answer_may_carry_a_python_skill` (+ mis-shaped drops); `agent_raised_tests.rs` (reaches
  `terminal_python_skill` + the `plan.formulate` row; ungrounded and forced answers drop it);
  `completion_helpers_return_none_on_a_refused_plan` covers it; and **the first runner-level test of the Python-skill
  write** (`python_skill_crystallise_e2e::a_notebook_answers_python_skill_is_crystallised_by_the_runner`: memory row
  `kind: python` untrusted, `l3.crystallised` audit `inserted`). Mutant caught: carry dropped. ⚠️ **A `PgListener`
  from the pool must be dropped before `pool.close()`** — close waits on its connection forever (cost a 40-min hang).
  Finalcheck added the invalid twin (`a_notebook_answers_invalid_python_skill_writes_nothing`: a `secret://` in
  `code` → no row, no audit, the answer kept). Filed: issue 1004 (**nothing checks a `python_skill`'s code ever ran**
  — any dispatched call grounds it), 1005 (`bring_up_pg`'s `.ok()?` is a silent green even under PG REQUIRE; the
  crystallise suites are in no gate profile), 1006 (a resume resets `dispatch_count`: a post-resume skill drops).
- ⚠️ **Owed: ≥ 2 DGX runs** of `requery7.sh` on this tree (prompt changed; #998's and #1002's runs too, one pair covers
  all three). ⚠️ **Not prompt-only** (`deny_unknown_fields`): deploy the build.

### Previous (2026-11-08): #996 2 of 5 (PR #1002, merged) — `AnswerSpec.l3_skill` → the terminal plan; a mis-shaped skill drops with a WARN (`skill_or_drop`). `memory_l3_crystallise_e2e`'s four scheduler tests run the notebook (`ScriptedNotebook`) through the real runner write.

### Previous (2026-11-08): #996 1 of 5 (PR #998, merged) — `AnswerSpec.l1_insight` → the terminal plan; `Plan::completion_{insight,skill,python_skill}` return `None` on a **refused** terminal plan. Tests in **`notebook/run/agent_raised_tests.rs`** (the home for all five ports, called from `the_notebook_loop`, so **no libtest count**). Filed: issue 999 (runner-level L1 write untested), 1000 (300 characters vs 512 bytes), 1001 (L1 insights cross classification).

### Previous (2026-11-08): #993 step 2 (PR #997, merged) — `tests_common::scripted_notebook::ScriptedNotebook` (a closure picks each reply from `ctx`, usually `ctx.plans.len()`); ⚠️ a **first reply without `steps` is refused**, and every `call_reply` lists its own fresh step id. Classic comparison run #18: 7/7, 2160 s. ⚠️ **Five classic `Plan` features have no notebook path** → #996 (operator: port before deleting). The runner-level ask/expiry/lanes/conversation suites run the notebook; the classic loop lost their coverage.

### Previous (2026-11-07): #993 step 1 (PR #995), #922 slice 4a (PR #994), #987 (PR #990), merged — what still binds

- ⚠️ **The router default is `disable_thinking: true`**: a routine turn's answer is rewritten by a thinking turn, so a
  scripted daemon happy path is **3 turns** (`ScriptedNotebook` thinks by default: no rewrite). Unset / empty /
  `notebook` planner mode → notebook; **the Mac flips to notebook on its next install** unless `.local` says `classic`.
- One `PgListener` per process; `MAX_FOLLOWS` **64**; ⚠️ a follow's interval first ticks one period out.

### Previous (2026-11-06 → 07): #985 + #988 (PR #989), `kastellan-cli session` (PR #986), merged — what still binds

- `ask` prints one stderr line per entry via the session's `follow()`; SIGINT handler installed **before** the submit.
  ⚠️ **bottie matches the parking marker by substring** ([hherb/bottie#242](https://github.com/hherb/bottie/issues/242)).
  ⚠️ A test that `claim_one`s must not leave an older task pending.
- ⚠️ **A follow reads state BEFORE entries; its 3 s poll tick, not the NOTIFYs, makes it correct.** ⚠️ **`kastellan-cli`
  installs a warn-level stderr tracing subscriber** — PG e2es fail instead of skipping once PG is up.

### Previous (2026-11-06): #965 items 4, 6, 7 (PR #984, merged) — ⚠️ a sift read's allowlist key is `read_key(tool, method)` (**`web-fetch.web.fetch`**); the `chain_distinct` → `totals` pass-through (`sift/run.rs:230`) is untested.

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
- A forced answer with no table gets the newest sift's (`sift::answer::forced_table`); its own table is taken out
  under it (`without_own_tables`). A stashed (over 64 KiB) step is read back with `StepDispatcher::stashed`; an evicted
  stash is a **failed read**. ⚠️ The stash is purged whenever the loop returns, **a suspend too**. A call on a done
  step is refused with the fix (`reply::not_pending`). `task_id`/`plan_count` are in `db::audit::PRESERVED_KEYS`.
- Judge a change over **≥ 2 runs**. ⚠️ **Never show a full answer example with a table**
  [[notebook-model-mirrors-rendered-shape]]. A prompt-only change needs **no rebuild**: copy to
  `~/.local/share/kastellan/prompts/`, restart core, check `agent_prompts`. ⚠️ A table naming an **earlier turn's**
  chain is not expanded (#956). `~/q960.sh <from> <to>` and `~/deploy-branch.sh` are on the DGX.

### Previous (2026-11-02): #941 + the task notebook, slice 1 (PR #951, merged, deployed DGX 2026-11-03)
- `max_tokens` 16384 + a call timeout on every planner call (#941); `KASTELLAN_PLANNER_MODE=classic|notebook`; migration
  **0028** (⚠️ **applied on the DGX: never edit it**). ⚠️ Any key added to the render must be accepted in the reply.

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
### Previous (2026-10-08 → 17): thinning (#829, #858/#861, #837/#859/#864/#865), #810, and the Matrix inbound path (#835, #852, #853, #825, #832, #846, #827/#826) — what still binds (prose: the `867`/`825`/`815` snapshots)

- **Thinning:** pure `kastellan_core::audit_thinning` (⚠️ `#[doc(hidden)] pub`, binary-only); one
  `Thinning` per **channel × action × kind**; ⚠️ **one process-wide `LOST_ROWS`** via
  `pg_events::production_writer()`. #813's abandoned rows and #832's abandoned work are **not** thinned.
  ⚠️ A crash loses the thinned tail (#817). ⚠️ **`ChannelEvents::audit` takes `action: &'static str`**
  — never widen it (#868).
- **`InsertCause`** (⚠️ never the error text): one line per cause **per stretch**; `PANIC`/`FATAL`/`Io`/`Tls`/
  `Protocol` → "may not have been written". ⚠️ Signal tests await a probe stream first (#810).
- **Inbound:** ⚠️ **on an ack channel one undecodable event makes the whole batch undecodable**; matrix-sdk 0.18
  saves the sync token **before** handlers; **owed:** a live `LiveSdk::drain` leg (DGX). ⚠️ tokio ≥ 1.52.3 (#846);
  0027 is at-most-once, the terminal-state list lives in **four** places; no `.await` before `PendingInsert`.

### Previous (2026-09-30 → 10-03): the audit sink — #788–#802 (PRs #791, #795, #801, #806) — what still binds

`core/src/main/audit_sink*.rs` (prose: `792`/`796`/`802`/`813` snapshots). ⚠️ **`Ledger::snapshot` reads `starting`,
then live, then `pending`** (load-bearing); a row that must *stay pending* needs a **`connections: 0`** ledger;
`stalled_pool` is built **inside** a runtime [[stalled-postgres-test-fixture]].

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

1. **#922 slice 4b = [#993](https://github.com/hherb/kastellan/issues/993)** (retire classic). Steps 1–2 done
   (#995, #997). **Next: [#996](https://github.com/hherb/kastellan/issues/996)** — port the five
   agent-authored plan features to the notebook reply, one PR each (each prompt change ≥ 2 DGX runs).
   **Done: `l1_insight` (PR #998), `l3_skill` (PR #1002), `python_skill` (PR #1003); next: `floor_request` and
   `invoke_skill` (on `call`, not `answer`: the reply's `CallSpec` and `call_plan`).** Tests go in `notebook/run/agent_raised_tests.rs`. **Then the
   deletion**, with the re-points listed on #993 (≈19 per-step-view tests → `PlanRecord::last_step_view`, 7 timeout
   cases, the outcome-shapes prompt pin, `user_text_cannot_forge_a_key`); `router_agent_mock_e2e` (~720 lines) shrinks
   below the cap when its 5 classic-door tests go (2 already have notebook twins); #940 folds in. bottie's substring
   match on the parking line is [hherb/bottie#242](https://github.com/hherb/bottie/issues/242).
   #983/#986 are on the DGX (in `main`); one re-measure is optional. #970: operator to close or keep (known limits only). Re-measure with `bash ~/requery7.sh` (move `~/requery7.log`
   aside), one log entry per run, ≥ 2 runs per change; `~/q960.sh <from> <to>` dumps the tasks.
   **Still owed:** a revoked mail key once (ask first); #926, #939, #771, #538; the Mac's localmail upgrade to
   `api_minor` 6.

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
control** proving the checker can fail. Over cap today, biggest first: `core/tests/guard_tier_e2e.rs`
1558+ (#639), `core/src/workers/gliner_relex/tests.rs`, `db/src/asks.rs`,
`sandbox/src/linux_firecracker/plan.rs` (DGX-gated), `core/src/channel/ask_message.rs`, `db/graph.rs`,
`core/src/scheduler/asks.rs`, `core/src/tool_host.rs`,
`workers/mail/src/ids.rs`, `tests-common/src/require.rs`,
`llm-router/src/messages.rs`,
`core/src/main.rs` **880** (+7 in #810, +3 in #829, +6 in #862, +10 in #869's review), `tests-common/src/microvm/{mod,container}.rs`, `sandbox/tests/macos_smoke.rs`,
`core/src/channel/mod.rs` **569** (+10 in #849, +6 in #835, +16 in #836, +5 in #852, docs — the `actions` consts are the natural cut).
Also over (grew in #818 without a split): `db/src/tasks.rs` **819** (+11 in #825: `CRASHED_DETAIL`),
`db/src/graph.rs` **~935**, `core/src/entity_extraction/batch_upsert.rs` **~520**.
Also over: `core/src/memory/l3_surface.rs` 539 (+5 in #785, a doc paragraph), `core/src/scheduler/inner_loop.rs` 906,
`tool_dispatch.rs` 722, `attach/tests.rs` 727, `require.rs` 661,
`scripts/run-e2e-gate.sh` 866 (shell), `gate_script_tests/run.rs` 586, `gate_script_tests.rs` 538. ⚠️ **`core/tests/mail_live_shape_e2e.rs` 566** — #949 put
its new leg in `mail_live_shape_e2e/hit_fields.rs` (a `#[path]` module); move the next one there too, or split. ⚠️ `audit/truncate/tests.rs` is 833.
Recent splits (the pattern to keep; list in the `867` snapshot): #905 `supervisor/src/lib.rs` 624 → 431, #869 `pg_events/{test_support,
lost_rows/tests}.rs` and `audit_sink_thinning_tests.rs`, #849, #835, #806 — each proved byte-identical.
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
| **Mac** (#996 py) | `feat/996-python-skill` over `7d870a8b`, primary checkout | **5574 / 0 / 52**, **202** summaries, `TEST_EXIT=0`; `KASTELLAN_PG_BIN_DIR` set, `--test-threads=4`, `--no-fail-fast`, `--nocapture`; `[WARN]` 0; log `~/.local/state/kastellan/gate-logs/sweep-996py.log`. vs `sweep-996l3.log`, per suite: core lib 2815 → 2817 (+1 `an_answer_may_carry_a_python_skill`, +1 #1002's review-round `a_malformed_l3_skill_drops_the_skill_not_the_answer`, after that log), `python_skill_crystallise_e2e` 1 → 2; the loop tests run inside `the_notebook_loop` | cold, `CARGO_TARGET_DIR=$HOME/.cargo-clippy-996py`, clean, 27 `Checking kastellan` | **23** (as before) |

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

- **#998** (#996 1 of 5) — merged 2026-11-08: the notebook answer carries `l1_insight`.
- **#997** (#993 step 2) — merged 2026-11-08: `ScriptedNotebook`; the in-process scheduler suites run the notebook.
- **#995 / #994** (#993 step 1; #922 slice 4a) — merged 2026-11-07, deployed DGX 2026-11-08: the daemon e2es run the notebook; the notebook is the default planner mode.
- **#992 / #990** (README; #987) — merged 2026-11-07: the root README brought up to date (⚠️ it restates the user manual's chapter 4: change both together); one shared `LISTEN` connection for a process's follows, cap 64.
- **#989** (#985, #988) — merged 2026-11-07: `ask` prints notebook entries and resumes a lost follow; session reports lost audit rows. **bottie#236** (slice 3) merged 2026-11-07.
- **#986** (#922 slice 2) — merged 2026-11-06, deployed DGX 2026-11-07 (in #989's tree): `kastellan-cli session`, JSON-RPC over stdio/ssh.
- **#984 / #983** (#965 items 4/6/7; #981, #970) — merged 2026-11-06, deployed DGX 2026-11-07 (in #989's tree): tests only (#965 done); a table ref to no sift this conversation ran is shown back.
- **#982 … #950** (merged + deployed DGX 2026-11-02 → 06, measured 301–384): `sift` + the scrapbook (0029), the notebook behind `KASTELLAN_PLANNER_MODE`, bounded planner calls, and the sift/table/forced-answer fixes of #957–#982 (one line each in the ROADMAP's #922 entry); manuals (#975).
- **#949 … #920** (merged + deployed DGX 2026-10-29 → 11-01, re-measured 227/232–252): attachment names in search hits, localmail `api_minor` 6, the PG harness waits out `57P03` (#949); stable-first prompts (#943); the 15-minute fast-lane ceiling, caps 20/25 (#942); `plan_budget`, batched steps, coverage searches (#938); OpenShell survey (#937); an empty completion asked again once (#930); the even summary shrink (#927); a failed decode's row carries its cost (#924); worker-unforgeable view markers (#920).
- **#919 … #908** (merged 2026-10-24 → 29, deployed both hosts): deploy docs; dependabot rustls (⚠️ makes `python-exec:dev` stale) and uv; #915's paused-clock pin; `boot_report::record_refusal` + the late-Postgres boot e2e (#914); a guard boot refusal is a thinned row (#912); core waits ≤ 60 s for Postgres (#910); the supervised `kastellan-guard` unit (#908). **#905 / #903 / #901 / #899** (2026-10-23 → 24): health-wait timeouts name the crash-loop evidence; `ServiceStatus::Transitioning`; `uninstall` keeps the unit file / plist on a failed stop; headless launchd `uninstall`, `$HOME` uid check.
- **#897 … #766** (merged 2026-10-08 → 22, thinning family deployed both hosts 2026-10-18): launchd/systemd uninstall and stop fixes, sidecar drains, the gate's exact counts, the Apple-`container` tier, the audit-loss thinning family, Matrix reply claims (0027), `[audit-lost]`, the thinking switch, `UPSTREAM_AUTH_FAILED`, the live mail shape gate. One-liners in the `802`/`815`/`825`/`867`/`993s2` snapshots.

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
