# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20261031_923_pre-prune.md`](archive/handover_20261031_923_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here (#921's review round in full; the
> #773 re-measure is in the `921` snapshot;
> #915's mutants and #913's detail are in the `773remeasure` snapshot; #909's review round is in the `906` snapshot, #609's deploy notes in `909`).
> ⚠️ **Repoint this line in the same commit as the snapshot.** It has been stale twice.

**Last updated:** 2026-10-31 (the NVIDIA OpenShell survey: follow-ups filed as #931–#936, docs-only) ·
**Recent PRs, newest first:** [#937](https://github.com/hherb/kastellan/pull/937) (OpenShell survey, draft), [#927](https://github.com/hherb/kastellan/pull/927) (#921), [#924](https://github.com/hherb/kastellan/pull/924) (#773 re-measure, #923 decode row, merged), [#920](https://github.com/hherb/kastellan/pull/920) (#704, #705, merged), [#919](https://github.com/hherb/kastellan/pull/919) (deploy docs, merged), [#918](https://github.com/hherb/kastellan/pull/918) (dependabot rustls, merged), [#917](https://github.com/hherb/kastellan/pull/917) (dependabot uv, merged), [#916](https://github.com/hherb/kastellan/pull/916) (#915, merged), [#914](https://github.com/hherb/kastellan/pull/914) (#913, #911, merged), [#912](https://github.com/hherb/kastellan/pull/912) (#906, merged), [#910](https://github.com/hherb/kastellan/pull/910) (#909, merged), [#908](https://github.com/hherb/kastellan/pull/908) (#609, merged), [#905](https://github.com/hherb/kastellan/pull/905) (#904, merged), [#903](https://github.com/hherb/kastellan/pull/903) (#900, #902, merged), [#901](https://github.com/hherb/kastellan/pull/901) (#896, merged), [#899](https://github.com/hherb/kastellan/pull/899) (#898, merged), [#897](https://github.com/hherb/kastellan/pull/897) (#894, #895, merged), [#893](https://github.com/hherb/kastellan/pull/893) (#890, #892, merged), [#889](https://github.com/hherb/kastellan/pull/889) (#888, #754, merged), [#887](https://github.com/hherb/kastellan/pull/887) (#752, #753, merged), [#886](https://github.com/hherb/kastellan/pull/886) (#885, merged), [#884](https://github.com/hherb/kastellan/pull/884) (#823, merged), [#883](https://github.com/hherb/kastellan/pull/883) (#882, merged); #881 … #795 are in the 896/905/906 snapshots.
**Last updated:** 2026-10-31 (#923: an empty planner completion is asked again once; a decode failure is told in words — PR #930) ·
**Recent PRs, newest first:** [#938](https://github.com/hherb/kastellan/pull/938) (#922 option 3 + prompt), [#930](https://github.com/hherb/kastellan/pull/930) (#923), [#927](https://github.com/hherb/kastellan/pull/927) (#921, merged), [#924](https://github.com/hherb/kastellan/pull/924) (#773 re-measure, #923 decode row, merged), [#920](https://github.com/hherb/kastellan/pull/920) (#704, #705, merged), [#919](https://github.com/hherb/kastellan/pull/919) (deploy docs, merged), [#918](https://github.com/hherb/kastellan/pull/918) (dependabot rustls, merged), [#917](https://github.com/hherb/kastellan/pull/917) (dependabot uv, merged), [#916](https://github.com/hherb/kastellan/pull/916) (#915, merged), [#914](https://github.com/hherb/kastellan/pull/914) (#913, #911, merged), [#912](https://github.com/hherb/kastellan/pull/912) (#906, merged), [#910](https://github.com/hherb/kastellan/pull/910) (#909, merged), [#908](https://github.com/hherb/kastellan/pull/908) (#609, merged), [#905](https://github.com/hherb/kastellan/pull/905) (#904, merged), [#903](https://github.com/hherb/kastellan/pull/903) (#900, #902, merged), [#901](https://github.com/hherb/kastellan/pull/901) (#896, merged), [#899](https://github.com/hherb/kastellan/pull/899) (#898, merged), [#897](https://github.com/hherb/kastellan/pull/897) (#894, #895, merged), [#893](https://github.com/hherb/kastellan/pull/893) (#890, #892, merged), [#889](https://github.com/hherb/kastellan/pull/889) (#888, #754, merged), [#887](https://github.com/hherb/kastellan/pull/887) (#752, #753, merged), [#886](https://github.com/hherb/kastellan/pull/886) (#885, merged), [#884](https://github.com/hherb/kastellan/pull/884) (#823, merged), [#883](https://github.com/hherb/kastellan/pull/883) (#882, merged); #881 … #795 are in the 896/905/906 snapshots.
Older PRs are in the [`archive/`](archive/) snapshots; **`gh issue list --state open` is the live
answer** and the only one worth trusting. ·
**The DGX runs #927's merged tree** (plain `upgrade_from_git.sh` 2026-10-31: exit 0, 15 binaries, both env files IDENTICAL to `~/kastellan.env*.bak-pre927`, `NRestarts=0`, Matrix up) **on kernel 7.0.0-1019, pinned by name in `GRUB_DEFAULT`** — 7.0.0-1024 boot-loops (below); **the Mac runs #917's**. **DGX:**
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

### This session (2026-10-31, latest): the NVIDIA OpenShell survey (docs only, no code; PR #937)

- **Note:** [`docs/devel/notes/2026-10-31-openshell-survey.md`](../notes/2026-10-31-openshell-survey.md) (OpenShell
  `42671c2`, Apache-2.0). Six follow-ups are filed and added to ROADMAP Phase 5 in dependency order. Independent:
  [#936](https://github.com/hherb/kastellan/issues/936) (one conformance table across all four backends) and
  [#935](https://github.com/hherb/kastellan/issues/935) (install-time worker-binary digests). Then
  [#931](https://github.com/hherb/kastellan/issues/931) (HTTP framing plus L7 rules) →
  [#932](https://github.com/hherb/kastellan/issues/932) (credential injection at the sidecar), and
  [#933](https://github.com/hherb/kastellan/issues/933) (delta classifier) →
  [#934](https://github.com/hherb/kastellan/issues/934) (Inbox grant proposals). They are **not** ranked above
  Next TODO's items.
- ⚠️ **Found while filing, and each binds the issue it is in:**
  - `mitm.rs::intercept` is a **byte relay**: the proxy never frames HTTP requests. So #931 and #932 start with a new
    attacker-facing parser, not a config field.
  - `tool_allowlists` is read **once**, at registry build (`core/src/registry_build.rs`). An allowlist `add`,
    from the CLI or from a future #934 approval, is dead until a daemon restart.
  - The live credential holder on the net path is `mail` (`KASTELLAN_MAIL_TOKEN_FILE`, outside the vault).
- **Not imported, on purpose:** OpenShell's fail-open defaults (L7 `audit` mode, Landlock `best_effort`, auto-approval
  of new public hosts) and its trust-on-first-use binary pinning.
- ⚠️ This file is already over the ~500-line target. It was not pruned this session; snapshot it at the next code
  session.

### Previous (2026-10-30): #921 — the plan summary shrinks evenly (PR #927)

- **#921:** over the 96 KiB `PLANS_SUMMARY_BUDGET`, outputs are now **water-filled to an even share** and
  re-rendered from the record's raw outcome (re-screened; a shrunk view that blocks, or overshoots, becomes
  the elided marker). The oldest are dropped whole only once a share would fall below
  `budget::MIN_SHRUNK_OUTPUT_BYTES` (2 KiB; also caps re-renders at 48/turn). Ten fetched messages keep id,
  sender, subject, attachment names. Marker reason is now `"output retrieved, dropped from this summary for
  space"` (the call-parameter marker keeps `"summary budget"`); the prompt says never call it unretrieved.
  Code: `core/src/scheduler/inner_loop/summary/budget.rs` (pure `plan_fit` + apply; movement-only split
  first). 7/7 mutants killed. ⚠️ A long task's newest output is now cut to its share too — **not yet
  measured live**: re-ask tasks 207–210 (209 is #921's case) after deploying.
- **Found by the sweep, fixed:** `protocol::Client::call` wrote the request with `serde_json::to_writer`, so a
  worker that died before reading surfaced as `Decode(Broken pipe)`, not `EarlyExit`, and skipped the
  last-words report. Now a `BrokenPipe` write falls through to the read (EOF → `EarlyExit`). It showed as a
  load-only flake of `worker_early_exit_stderr_fallback_e2e` (3/3 alone); the new protocol test is
  deterministic (waits for the child with `try_wait` — ⚠️ `wait` closes stdin).
- DGX redeployed to `main` (#924) first; localmail heartbeats fresh.
- **Review round (4 reviewers):** the longer marker (80 B) made the withheld/error "never elided" tests
  vacuous (both shapes now smaller than it) — re-armed; three mutants survived (unsorted water level,
  wrong-index drop loop, unscreened re-render) — now killed, with the re-render a named
  `PlanRecord::shrunk_output`; a shrink that falls back to `…view_unavailable` now elides (that wording is
  #921's misreading again); a sink block on a shrunk view is counted (`Fitted::blocked`) and `warn!`ed but
  **not audited — [#928](https://github.com/hherb/kastellan/issues/928)**; the swallowed EPIPE waits on a
  worker that closed stdin yet holds stdout — no producer in tree, unbounded only where no per-call deadline
  exists (gliner, persistent transport) — **[#929](https://github.com/hherb/kastellan/issues/929)**.
  +6 core lib, +1 protocol tests; no full sweep re-run for this round.
### This session (2026-10-31, latest): #923 — an empty completion is asked again once (PR #930)

- `AgentError::is_empty_completion` (a `Decode` with a blank `raw`, **not** `finish_reason: length` — out of room
  repeats at temperature 0); `formulate_turn` (`inner_loop/llm_failure.rs`)
  re-asks **the same door** (plan or synthesis) once — its own `plan.formulate_failed` row
  (`thinking_suppressed_retry: false`), the retry's `plan.formulate` row `retry_count: 1`. **One retry per turn**:
  an empty answer from the #774 thinking-off synthesis retry is not retried again. A non-JSON answer is never
  retried. A decode failure that ends the task reads `decode_failure_detail` (plain words, `[llm: plan decode
  failed: …]` last); every other non-timeout error stays `llm: …`. Loop cases 8–11 in `timeout_tests.rs`; 2
  mutants killed. Review round: scripted fakes' "out of script" `Decode` now has a non-blank `raw` (a blank one
  is retried); follow-ups filed as [#939](https://github.com/hherb/kastellan/issues/939) (`"content": null`
  skips the retry; a failed retry's row is unmarked).
- ⚠️ **Not seen live** — empties occurred only with thinking OFF (the live setting is ON).
- **DGX outage, 2026-10-31:** kernel **7.0.0-1024** (apt, 11:37 AEDT) hard-reset ~20 s into every boot — **~59
  boots**, no journal trace; it looked like a flaky network (1–2 s SSH sessions). Booted 6.17 via `grub-reboot`
  over the stalling link, then **7.0.0-1019** (GPU OK, driver 580.178.04) is pinned in `GRUB_DEFAULT` and
  `apt-mark manual`. ⚠️ **Never install `linux-modules-nvidia-580-open-6.17.0-1029-nvidia`** — it pins
  `nvidia-kernel-common-580` to 580.173.02. Postgres replayed WAL cleanly every reset; `git fsck` clean.
  [[dgx-kernel-7-0-1024-boot-loop]]. ⚠️ DGX time is **AEDT**, the Mac's **AEST** — an hour apart.
- **#927 deployed and re-measured (tasks 225–231 = 207–210, 222–224):** #921's case is **fixed** (227: 10/10
  with attachment names, 4 plans, 330 s — was 8/10, 6 plans, 548 s). **4 of 7 still hit the 5-plan cap**
  (225, 226, 229, 231); 225 still lacks Price/GST and orders dates wrongly; 231 now misses the NSW Planning
  *Payment overdue* (CFT-994474) and Optus; 226 lists a different invoice set than 208. **#922 is the binding
  limit now.** `kastellan-cli ask` blocks until the task ends and prints the answer.
- **#922 part C — PR #938:** the planner context carries `plan_budget {this_plan, max_plans}`; prompt rules
  (independent steps in ONE plan; figures are in attachments; an `OR` + recent-mail coverage search before a list
  answer); mail docs (no `account_ids` = every account, so no `list_accounts`; short phrases). The plan audit
  showed **every** task spent plan 1 on `list_accounts` and most plans held ONE step. Query syntax moved to
  localmail: **hherb/localmail#419 → #420** (`websearch_to_tsquery`: `OR`, `"phrase"`, `-word`; ⚠️ lowercase `or`
  is an operator; `api_minor` 4). #938 raises `MIN_API_MINOR` to **4** — ⚠️ **upgrade a host's localmail
  (pull + restart `localmail-serve`) BEFORE deploying #938 there**, or every mail search is refused. Mac
  localmail restarted on #420 (`launchctl kickstart -k gui/$(id -u)/com.localmail.serve`); live gate passed;
  `"payment overdue"` finds CFT-994474. Operator decisions: **A = a 15-minute time ceiling** instead of a plan
  count (next PR); C before A.

### Previous (2026-10-30): #921 — the plan summary shrinks evenly (PR #927, merged) — what still binds (prose: the `923` snapshot)

- Over the 96 KiB `PLANS_SUMMARY_BUDGET` outputs are **water-filled to an even share**, re-rendered and re-screened
  from the raw outcome (`inner_loop/summary/budget.rs`, `PlanRecord::shrunk_output`); the oldest drop whole only
  below `MIN_SHRUNK_OUTPUT_BYTES` (2 KiB). ⚠️ A long task's newest output is cut too — **not measured live**.
- `protocol::Client::call`: a `BrokenPipe` write falls through to the read (EOF → `EarlyExit`). ⚠️ In a test,
  `wait` closes stdin — use `try_wait`. Open: #928 (audit a block on a shrunk view), #929 (per-call deadline).

### Previous (2026-10-30): the #773 live re-measure (PR #924, merged) — what still binds (prose: the `921` snapshot)

- **Thinking stays ON** (OFF: 2/4 failed on an empty completion, #923). Measure with `kastellan-cli ask
  --classification-floor Public "<q>"`, **one question at a time**. ⚠️ localmail search is `plainto_tsquery`
  (**every word must match, `OR` is a word**; `sort: "date"` is keyword-only); the planner follows parameters, not
  prose — a `within_days` param if recency still fails. ⚠️ localmail sync can stall with the unit `active`
  (hherb/localmail#414–#416). ⚠️ A new `AgentError::Decode` field breaks 7 fakes in `core/tests/`.

### Previous (2026-10-29): #704 + #705 (PR #920, merged) — ⚠️ view **key** markers can't be spelled by a worker, string markers can (by design); **never prune a rendered view twice**; `summary_head` is rendered once and never shows part of a string the view shows whole. Prose: the `773remeasure` snapshot.

### Previous (2026-10-26 → 29): the boot arc — #909, #906, #913/#911, #915 (PRs #910, #912, #914, #916, merged) — what still binds

- **#909:** boot waits ≤ **60 s** for Postgres only on `ECONNREFUSED`/`ENOENT`/`57P03` (missing socket dir refuses
  at once; `probe::run`'s own dial is uncapped). ⚠️ A paused-clock loop test needs an outer `timeout` **and** a call
  cap in the fake [[async-fakes-must-yield-like-io]]. A daemon e2e on a cluster that never starts takes 60 s to refuse.
- **#906:** a guard boot refusal writes a thinned `policy / guard_tier.boot_refused` row (`{"refusal","error_kind"}`).
  ⚠️ A new guard key must name itself (`KASTELLAN_LLM_GUARD_…`) in its error, or `from_env` is misclassified.
- `tests_common::init_pg_cluster` → `PgInit` + `PgInit::start`. ⚠️ The daemon's JSON log is **stdout**, a boot
  refusal **stderr**.

### Previous (2026-10-24 → 25): #609 (PR #908) — ⚠️ guard port/alias are **derived** from `KASTELLAN_LLM_GUARD_URL`/`_MODEL` and read **at install time**; `scripted_llm`'s default **503s `/props`** (a guard e2e waits 60 s). Open: #907.
### Previous (2026-10-23 → 24): the supervisor arc — #904, #900/#902, #896, #898 (PRs #905, #903, #901, #899, merged) — what still binds

- `Supervisor::crash_evidence` is read **once, at timeout only**. ⚠️ systemd `show` answers an unknown unit
  with clean defaults — `LoadState=not-found` is the tell. ⚠️ **Unknown state words read `Transitioning`**,
  never `Inactive`; on launchd only `running` → `Active` [[guard-shares-the-census-blind-spot]].
- `uninstall` keeps the unit file / plist on a failed stop/bootout; `systemctl` is `run_capped` at 60 s. launchd
  no-GUI-domain (exit 112) is "not loaded" for `bootout`/`unload_wait` but ⚠️ **an error in `status`**.
- ⚠️ **A squash commit auto-closed #898** ("fail closed: #898") — run the closing regex over the squash
  message too [[pr-body-not-fixed-autocloses-issue]].

### Earlier (2026-10-19 → 21): sidecar/drain fixes (PRs #893, #889, #887) and the gate's exact counts (PRs #886 … #879) — what still binds (prose: the `906` snapshot)

- ⚠️ **After `ECHILD` never kill** a sidecar; a cut-short capture is `ProbeFailure::Unread`, but **one
  short capture is still `Ok` by design**. `[worker-undrained]` is the sixth `warn_and_fall_back!`
  marker. ⚠️ A torn-down pty is `EIO` on Linux, **EOF on macOS**. Next release is **0.3**. Open: #891.
- Gate: one whole `test result:` per `Running` section, each with ≥ 1 test. **`IGNORED`** is exact
  (`worker-report` = **13** — ⚠️ change it with any inner fixture); **`FILTERED_OUT`** is exact
  (`microvm` = **4**, else 0). ⚠️ An operator name filter after the profile name fails the verdict;
  pass `--no-fail-fast` after the profile name when measuring. ⚠️ A 0-byte gate script exits 0
  [[ssh-stdin-copy-can-write-zero-bytes]]. DGX SearxNG restart policy is `unless-stopped` (2026-10-24).

### Earlier (2026-10-17 → 19): #875 … #860 — ⚠️ any `Cargo.lock` mtime change makes `python-exec:dev` stale (`scripts/workers/python-exec/build-image.sh`); never put the launchd wait in `stop` [[mac-sysv-shm-exhaustion-from-killed-postgres]]; `report_unsaid_lost_rows()` — call once.
### Previous (2026-10-08 → 17): thinning (#829, #858/#861, #837/#859/#864/#865), #810, and the Matrix inbound path (#835, #852, #853, #825, #832, #846, #827/#826) — what still binds (prose: the `867`/`825`/`815` snapshots)

- **Thinning:** pure `kastellan_core::audit_thinning` (⚠️ `#[doc(hidden)] pub`, binary-only); one
  `Thinning` per **channel × action × kind**; ⚠️ **one process-wide `LOST_ROWS`** via
  `pg_events::production_writer()`. #813's abandoned rows and #832's abandoned work are **not** thinned.
  ⚠️ A crash loses the thinned tail (#817). ⚠️ **`ChannelEvents::audit` takes `action: &'static str`**
  — never widen it (#868).
- **`InsertCause`** (`Answered(SqlState)` / `Io` / `Client(ClientFault)` / `RowCount` — ⚠️ never the
  error text); one line per cause **per stretch**, ⚠️ not "differs from the last". `PANIC`/`FATAL`,
  `Io`, `Tls`, `Protocol` → "may not have been written". Wording: `channel::audit_text::insert_verdict`.
  Both `closing_pool` fixtures read the first client message before closing. #810: signals before
  `spawn_scheduler`; ⚠️ signal tests await a probe stream first.
- **Inbound:** `ParsePoll` → `PolledBatch` (envelope = `kastellan_matrix_wire::PollResult`); ⚠️ **on an
  ack channel one undecodable event makes the whole batch undecodable.** #835: `drain` on every driver
  exit; ⚠️ matrix-sdk 0.18 saves the sync token **before** handlers (narrowed, not closed); `sdk_live`
  needs `--features live-matrix`; **owed:** a live `LiveSdk::drain` leg (DGX). #846: ⚠️ tokio ≥ 1.52.3.
  #825: migration 0027, ⚠️ at-most-once; the terminal-state list lives in **four** places. #827:
  `PendingInsert` — ⚠️ no `.await` before it; `truncate_payload` stays IDEMPOTENT.

### Previous (2026-09-30 → 10-03): the audit sink — #788–#802 (PRs #791, #795, #801, #806) — what still binds

`core/src/main/audit_sink*.rs` (prose: `792`/`796`/`802`/`813` snapshots). ⚠️ **`Ledger::snapshot`
reads `starting`, then live, then `pending`** (load-bearing); mutate against the seams
(`snapshot_around`, `Lease::promote_around`, `close_and_count`, `SinkWriter::spawn_around`). `drain()`
waits **3 s**; inserts bounded (**4 connections, 1024 queued**); hooks 100 ms. ⚠️ A row that must
*stay pending* needs a **`connections: 0`** ledger; `stalled_pool` is built **inside** a runtime
[[stalled-postgres-test-fixture]]. ⚠️ #791's `Closes #N` did not fire — check issues after merges.

### Previous (2026-09-20 → 27): thinking (#773/#774), the mail worker (#760, #698, #763, #765), the worker-report arc (#725 → #750, #748, #755) — prose: the `773`/`673`/`748`/`755`/`698`/`905` snapshots

- **`KASTELLAN_LLM_THINKING_SWITCH`**: `chat_template_kwargs` (default) or `reasoning_effort` (**the only
  one Ollama honours**). ⚠️ **Never send both** (vLLM 0.15.1 400s). `llm_usage` on every
  `plan.formulate` row. ⚠️ **Thinking on vs off is not decided** — that is the live re-measure.
- **Mail:** below localmail API **1.3** search/attachments/header reads refuse — **no fallback**. Wire
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

0. ⚠️ When stopping a hand-started guard, never `pkill -f "llama-server.*--port 8081"` inside
   `ssh dgx '…'`: the pattern matches the remote shell's own command line [[pgrep-wait-loops-match-themselves]].
   #909 + #906 are deployed and **reboot-proven** (2026-10-29) — nothing owed.

1. **The planner's answer quality ([#922](https://github.com/hherb/kastellan/issues/922)).** Order: merge
   localmail #420 → on the DGX pull localmail + restart `localmail-serve` (check `/v1/version` says `api_minor` 4)
   → deploy #930 + #938 → `localmail daemon status` (#925) → re-ask the seven (225–231 are the baseline; the
   runner is `~/requery5.sh` on the DGX — ⚠️ its id parse reads the answer, use `tasks list`). Then **part A: a
   15-minute wall-clock ceiling** for the fast lane instead of `DEFAULT_MAX_PLANS_FAST = 5` (`db/src/tasks.rs`),
   with the remaining time in `plan_budget`, and a bigger `PLANS_SUMMARY_BUDGET` (96 KiB) to fit the 64k context.
   Then [#926](https://github.com/hherb/kastellan/issues/926).
   Re-run the same four questions afterwards (tasks 207–210 are the baseline). **Still owed from the
   re-measure:** a revoked mail key once (the planner's reaction to `UPSTREAM_AUTH_FAILED`; it touches the
   operator's localmail token, so ask first); the operator's verdict on 207/208's answers. Then #771 (carry
   localmail's own 401 reason to the operator — `polled_driver/refusal.rs`, via `emit_worker_refusal_report`)
   and #538 (the mail worker's second, hand-rolled localmail mock).

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
- **[#548](https://github.com/hherb/kastellan/issues/548)** / [#676](https://github.com/hherb/kastellan/issues/676)
  — per-test-cluster contention (`the database system is starting up`) under a full sweep. Blast
  radius, not teardown; restore the shared suffix with a `.suffix()` setter, **not** by reverting
  #641 [[issue-as-filed-can-carry-a-regression]].
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
`scripts/run-e2e-gate.sh` 866 (shell), `gate_script_tests/run.rs` 586, `gate_script_tests.rs` 538. ⚠️ **`core/tests/mail_live_shape_e2e.rs` 559 — split it before
its next leg** (the attachment half is the natural cut). ⚠️ `audit/truncate/tests.rs` is 833.
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
| **Mac** (#922 C) | PR #938 on `250001f8` | **5311 / 0 / 52**, **198** summaries, `TEST_EXIT=0`, same flags; `[WARN]` 0; log `~/sweep-922c-mac.log`. vs the #923 row: core lib 2636 → 2637 (−3 #923 not on that branch, +4), mail worker 213 → 214. Live mail gate passed against localmail #420 | exit 0, fresh target dir, **27** | **23** |
| **Mac** (#923) | PR #930 on `250001f8` | **5309 / 0 / 52**, **198** summaries, `TEST_EXIT=0`, primary checkout, `KASTELLAN_PG_BIN_DIR` set, `--test-threads=4`, `--no-fail-fast`, `--nocapture`; `[WARN]` 0; log `~/sweep-923-mac.log`. Per-suite vs `~/sweep-921b-mac.log`: core lib 2627 → 2636 (+6 #921's review round, never swept; +3 #923), protocol lib 21 → 22 (#921's review round) | exit 0, `CARGO_TARGET_DIR=$HOME/.cargo-clippy-923` (fresh), **27** | **23** (Apple `container` stopped) |

The DGX row predates #910's review round (+4 db lib), #906 (+4 core lib, +2 binaries),
#913/#911 (+6 core lib, +1 e2e), #915 (+1 db lib), #920 (+13 core lib), #924, #927 and #930; all are platform-neutral and were not swept on the DGX. Older Mac rows are in the `909deploy`/`921` snapshots.

Older rows (incl. #755, #726/#728 and the last DGX figures) are in the [`archive/`](archive/) snapshots.

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
`conversation_turns_e2e`, `secret_vault_e2e` under a full sweep (`the database system is starting up` / a vanished
per-test socket — #548/#676). A dropped ephemeral port is not a reserved one (fixed by confirming).

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

- **#927** (#921) — merged 2026-10-30, **deployed DGX 2026-10-31**, re-measured (227 fixed): the summary budget shrinks evenly; a broken pipe is an early exit.
- **#924** (#773 re-measure, #923 decode row) — merged 2026-10-30, **deployed DGX 2026-10-30**: a failed decode's row carries its cost; `mail.search`'s dialect doc.
- **#920** (#704, #705) — merged 2026-10-29, **deployed DGX 2026-10-30**: worker-unforgeable view markers; a labelled, screened handoff head.
- **#919 / #918 / #917 / #916** (merged 2026-10-29): deploy docs; dependabot rustls (⚠️ makes `python-exec:dev` stale) and uv; #915's paused-clock pin.
- **#914** (#913, #911) — merged 2026-10-28: `boot_report::record_refusal` in the lib, unit-tested; `PgInit` + the late-Postgres boot e2e.
- **#912** (#906) — merged 2026-10-28, deployed both hosts 2026-10-29: a guard boot refusal is a thinned `guard_tier.boot_refused` row.
- **#910** (#909) — merged 2026-10-27, deployed both hosts 2026-10-29: core waits up to 60 s for Postgres at boot.
- **#908** (#609) — merged 2026-10-24, deployed both hosts 2026-10-25: the supervised `kastellan-guard` unit and the `/props` boot wait.
- **#905 / #903 / #901 / #899** (merged 2026-10-23 → 24): health-wait timeouts name the crash-loop evidence; `ServiceStatus::Transitioning`; `uninstall` keeps the unit file / plist on a failed stop; headless launchd `uninstall`, `$HOME` uid check.
- **#897 … #879** (merged 2026-10-20 → 22): launchd `uninstall` keeps the plist on a failed bootout (#897); a failed sidecar exit poll retires the pair (#893); the broker's stdout drain warns, drains name their errno (#889, #887); the gate's per-binary summary/≥ 1-test/exact `IGNORED`/`FILTERED_OUT` checks (#886, #884, #883, #881, #879).
- **#877 … #766** (merged 2026-10-08 → 19, thinning family **deployed both hosts 2026-10-18**): the Apple-`container` tier + `container` profile, launchd knob + `launchd` profile, `stop_target` waits for launchd, the audit-loss thinning family (#855–#869), undecodable poll batches (#851), Matrix held counts, reply claim at send / catch-up (0027), audit-sink close-out, `[audit-lost]`, reply queues + `[worker-refusal]`, the thinking switch, `UPSTREAM_AUTH_FAILED`, the live mail shape gate. One-liners in the `802`/`815`/`825`/`867` archive snapshots; earlier: git history.

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
