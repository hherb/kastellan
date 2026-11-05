# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20261104_966_pre-prune.md`](archive/handover_20261104_966_pre-prune.md)
> (#963's session and its 329–342 measure in full),
> [`archive/handover_20261104_960code_pre-prune.md`](archive/handover_20261104_960code_pre-prune.md)
> (#961/#962's session, the #960 prompt try and every section below in full before this prune),
> [`archive/handover_20261103_955measure_pre-prune.md`](archive/handover_20261103_955measure_pre-prune.md)
> (the `sift` session — #955's design, review rounds and deferred minors — in full),
> [`archive/handover_20261103_sift_pre-prune.md`](archive/handover_20261103_sift_pre-prune.md)
> (#951's session in full, before the `sift` session's prune),
> [`archive/handover_20261102_922nb_pre-prune.md`](archive/handover_20261102_922nb_pre-prune.md)
> (#945's session, the notebook design session and every section below in full before this prune),
> [`archive/handover_20261101_945_pre-prune.md`](archive/handover_20261101_945_pre-prune.md)
> (#947's session and the 239–245 verdict in full),
> [`archive/handover_20261101_947_pre-prune.md`](archive/handover_20261101_947_pre-prune.md)
> (the prompt-cache session and the older sections in full) and
> [`archive/handover_20261031_922a_pre-prune.md`](archive/handover_20261031_922a_pre-prune.md)
> (#923's session, the 225–231 re-measure and #937's OpenShell notes in full; older detail is named in that snapshot's header).
> ⚠️ **Repoint this line in the same commit as the snapshot.** It has been stale twice.

**Last updated:** 2026-11-05 (#971 merged + deployed from `main`, #967 check passed; #965 items 3/8 prompt wording, PR #972) ·
**Recent PRs, newest first:** [#972](https://github.com/hherb/kastellan/pull/972) (#965 items 3/8, prompt wording), [#971](https://github.com/hherb/kastellan/pull/971) (#968, stashed sift read, merged), [#969](https://github.com/hherb/kastellan/pull/969) (#967, own table, #965 item 2, merged), [#966](https://github.com/hherb/kastellan/pull/966) (#960 forced table, #964, merged), [#963](https://github.com/hherb/kastellan/pull/963) (#960 by code, merged), [#962](https://github.com/hherb/kastellan/pull/962) (#922 sift `from`, merged), [#961](https://github.com/hherb/kastellan/pull/961) (#922 chain totals, merged), [#957](https://github.com/hherb/kastellan/pull/957) (#922 sift fixes, merged), [#955](https://github.com/hherb/kastellan/pull/955) (#922 `sift`, merged), [#951](https://github.com/hherb/kastellan/pull/951) (#941, #922 slice 1, merged), [#950](https://github.com/hherb/kastellan/pull/950) (#945, merged), [#949](https://github.com/hherb/kastellan/pull/949) (#947, merged); #946 … #795 are in the `960code` and older snapshots.
Older PRs are in the [`archive/`](archive/) snapshots; **`gh issue list --state open` is the live
answer** and the only one worth trusting. ·
**The DGX runs `main` with #971 (`upgrade_from_git.sh`, 2026-11-05) and `KASTELLAN_PLANNER_MODE=notebook` in `.local`** (`kastellan.env` identical after install. ⚠️ Its `git pull` over SSH fails: run it with `GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=url.https://github.com/.insteadOf GIT_CONFIG_VALUE_0=git@github.com:`; migration 0029 since #955; localmail `62bdec6`, `api_minor` 6) **on kernel 7.0.0-1019, pinned by name in `GRUB_DEFAULT`** — 7.0.0-1024 boot-loops (below); **the Mac runs #917's** (its localmail serves `api_minor` 4: after #949 the mail worker refuses there until it is upgraded). **DGX:**
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

### This session (2026-11-05, latest): #971 deployed from `main`; #967 checked; #965 items 3/8 (prompt wording)

- **#971 deployed** (`kastellan.env` identical, `NRestarts=0`, Matrix up). **#967 check passed:** over-cap `plan.formulate` rows of tasks 357/358 carry `task_id`; pre-deploy rows stay bare.
- **#965 item 3 kept as is, documented:** which read applies is decided from the item's fields before any read runs; a read that fails is not replaced. Live: **15 of 2302 reads failed** (2 of 61 sifts), so a second reviewed plan per sift isn't worth it. Item 8: `distinct` is per sift. Prompt + `reads.rs` doc only; the DGX still runs the old prompt (copy it to `~/.local/share/kastellan/prompts/` + restart core, or deploy).
- **Re-measure of #971 in flight:** `requery7.sh` started 22:44 AEDT, tasks from 357 (Q1: a table, 15 kept of 50). ⚠️ **Log it** in `notebook-experiments.md` (`~/q960.sh 357 363`); a second run is owed.

### Previous (2026-11-05): PRs #971 (#968) and #969 (#967, #965 item 2), merged, deployed — what still binds

- #968: a sift read over 64 KiB is read back with `StepDispatcher::stashed` and rendered by `sorter_view` at its per-item cap; an evicted stash is a **failed read** with a `warn`, never the placeholder sorted. #967: `task_id`/`plan_count` are in `db::audit::PRESERVED_KEYS`. A **forced** answer's own table is taken out under the code table (`sift::answer::without_own_tables`), **not** when the model named it; leftovers [#970](https://github.com/hherb/kastellan/issues/970).

### Previous (2026-11-04): #963 + PR #966 (#960's forced-answer table, #964, #965 items 1/5), merged, measured 343–356 — what still binds

- A forced answer with no table gets the newest sift's table by code (`sift::answer::forced_table`); a sift over a handoff-stashed step reads the stash back whole (`StepDispatcher::stashed`); a call on a **done** step is refused with the fix (`reply::not_pending`). ⚠️ The stash is purged whenever the loop returns, **a suspend too**: `repeat::already_made` ignores a placeholder whose stash is gone.
- Measured ([log #12/#13](../notes/notebook-experiments.md)): 3 of 3 forced answers had their table; both runs 7/7 (2436 s, 3286 s); neither #964 path came up live. `ask()` lives in `notebook/ask.rs`; `~/q960.sh <from> <to>` and `~/deploy-branch.sh <branch>` are on the DGX. Full prose: the `966` snapshot and PR #966.

### Previous (2026-11-04): #961 measured (315–321), a #960 answer-example try measured **worse** and reverted (322–328), #962 (a sift `from` past its list's end refused) — what still binds

- Judge a change over **≥ 2 runs**. ⚠️ **Never show a full answer example with a table** [[notebook-model-mirrors-rendered-shape]]. A prompt-only change needs **no rebuild**: copy to `~/.local/share/kastellan/prompts/`, restart core, check `agent_prompts`.
- #961: `sift/chain.rs::totals`. #957: `is_handoff_fetch` takes `fetch` **and** `handoff.fetch`; rows merge on **all** fields. ⚠️ A table naming an **earlier turn's** chain is not expanded (#956). Deferred: #958, #959. **Operator to check:** Q4's stated total.

### Previous (2026-11-03): `sift` + the scrapbook (#922, PR #955, merged, deployed) — what still binds

- Code in `core/src/scheduler/notebook/sift/`; reads are **one reviewed plan** (`StepPolicy::SiftReads`); rows land in
  **`task_scrapbook` (migration 0029, append-only — applied on the DGX: never edit it)**. Knobs
  `KASTELLAN_SIFT_MAX_READS` (64; also the most items one sift looks at), `KASTELLAN_SIFT_READ_METHODS`.
- ⚠️ **The sift number is the turn (`plan_count`)**; sift reads are marked by `decision` prefix `SIFT_READS_DECISION`,
  so `reply::validate` refuses a `returns` beginning `sift reads:`. Open minors: [#956](https://github.com/hherb/kastellan/issues/956).
- ⚠️ **The IDE's `cargo check --workspace` holds `target/`'s lock on every save** — iterate in a private
  `CARGO_TARGET_DIR` and keep `target/` for the sweep.

### Previous (2026-11-02): #941 + the task notebook, slice 1 (PR #951, merged, deployed DGX 2026-11-03) — what still binds

- `ChatRequest.timeout` + `max_tokens` 16384 on every planner call (#941). `KASTELLAN_PLANNER_MODE=classic|notebook`;
  migration **0028** `task_notebook` (⚠️ **applied on the DGX: never edit it**). ⚠️ **The model mirrors whatever shape
  it is shown** — any key added to the render must be accepted (and ignored) in the reply. Operator decision: gemma
  MoE, thinking ON, notebook mode. Follow-ups #952–#954. ⚠️ Kill a remote process in its own ssh call
  [[pgrep-wait-loops-match-themselves]].

### Previous (2026-10-31 → 11-01): #945, #947, #923, #922 parts C/A, the prompt cache, OpenShell (PRs #950, #949, #930, #938, #942, #943, #937, merged) — what still binds (prose: the `922nb`/`945`/`922a` snapshots)

- `plan.formulate_failed` keeps `raw_head` (⚠️ also in the 0600 JSONL mirror, accepted); a blank completion is re-asked
  once, a non-JSON one through `formulate_reformat` — never both in one turn (#939). ⚠️ Scripted fakes' "out of
  script" `Decode` needs a non-blank `raw`. The mail worker **requires localmail `api_minor` 6** (the Mac's serves 4).
  Stable-first prompt order, `now` last; ⚠️ **the context binds, not the plan count** (#940); #944.
- ⚠️ **DGX kernel 7.0.0-1024 boot-loops**; 7.0.0-1019 pinned (`GRUB_DEFAULT`, `apt-mark manual`); ⚠️ never install
  `linux-modules-nvidia-580-open-6.17.0-1029-nvidia` [[dgx-kernel-7-0-1024-boot-loop]]. DGX **AEDT**, Mac **AEST**.
  OpenShell: #931–#936; ⚠️ `tool_allowlists` is read **once** at registry build. Parallel HANDOVER edits merge clean
  and wrong [[handover-parallel-docs-merge-duplicates]].

### Previous (2026-10-30): #921 (PR #927), the #773 re-measure (PR #924) — over budget, outputs are **water-filled to an even share** (`inner_loop/summary/budget.rs`); **thinking stays ON**; measure with `kastellan-cli ask --classification-floor Public "<q>"`, one question at a time. ⚠️ localmail sync can stall with the unit `active` (#925). ⚠️ A new `AgentError::Decode` field breaks 7 fakes in `core/tests/`.

### Previous (2026-10-29): #704 + #705 (PR #920, merged) — ⚠️ view **key** markers can't be spelled by a worker, string markers can (by design); **never prune a rendered view twice**; `summary_head` is rendered once and never shows part of a string the view shows whole. Prose: the `773remeasure` snapshot.

### Previous (2026-10-26 → 29): the boot arc — #909, #906, #913/#911, #915 (PRs #910, #912, #914, #916, merged) — what still binds

- Boot waits ≤ **60 s** for Postgres on `ECONNREFUSED`/`ENOENT`/`57P03` (`db::boot_wait`; since #949 the PG test
  harness uses it too). ⚠️ A paused-clock loop test needs an outer `timeout` **and** a call cap in the fake
  [[async-fakes-must-yield-like-io]]. A guard boot refusal is a thinned `guard_tier.boot_refused` row; ⚠️ a new guard
  key must name itself in its error. ⚠️ The daemon's JSON log is **stdout**, a boot refusal **stderr**.

### Previous (2026-10-24 → 25): #609 (PR #908) — ⚠️ guard port/alias are **derived** from `KASTELLAN_LLM_GUARD_URL`/`_MODEL` and read **at install time**; `scripted_llm`'s default **503s `/props`** (a guard e2e waits 60 s). Open: #907.
### Previous (2026-10-23 → 24): the supervisor arc — #904, #900/#902, #896, #898 (PRs #905, #903, #901, #899, merged) — what still binds

- ⚠️ systemd `show` answers an unknown unit with clean defaults — `LoadState=not-found` is the tell; **unknown state
  words read `Transitioning`** [[guard-shares-the-census-blind-spot]]. `uninstall` keeps the unit file / plist on a
  failed stop. ⚠️ **A squash commit auto-closed #898** — run the closing regex over the squash message too
  [[pr-body-not-fixed-autocloses-issue]].

### Earlier (2026-10-19 → 21): sidecar/drain fixes (PRs #893, #889, #887) and the gate's exact counts (PRs #886 … #879) — what still binds (prose: the `906` snapshot)

- ⚠️ **After `ECHILD` never kill** a sidecar. A torn-down pty is `EIO` on Linux, **EOF on macOS**. Open: #891.
- Gate: **`IGNORED`** and **`FILTERED_OUT`** are exact (`worker-report` = **13**, `microvm` = **4**); ⚠️ a name filter
  after the profile name fails the verdict; ⚠️ a 0-byte gate script exits 0 [[ssh-stdin-copy-can-write-zero-bytes]].

### Earlier (2026-10-17 → 19): #875 … #860 — ⚠️ any `Cargo.lock` mtime change makes `python-exec:dev` stale (`scripts/workers/python-exec/build-image.sh`); never put the launchd wait in `stop` [[mac-sysv-shm-exhaustion-from-killed-postgres]]; `report_unsaid_lost_rows()` — call once.
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

0. ⚠️ When stopping a hand-started guard, never `pkill -f "llama-server.*--port 8081"` inside
   `ssh dgx '…'`: the pattern matches the remote shell's own command line [[pgrep-wait-loops-match-themselves]].
   #909 + #906 are deployed and **reboot-proven** (2026-10-29) — nothing owed.

1. **#922 next: log this session's re-measure (357–363) and run a second; merge #972 and deploy it.**
   Then #965's items 4, 6, 7 (test gaps; need harness work), #970, then slice 2 (`kastellan-cli session`). Re-measure with `bash ~/requery7.sh` (move `~/requery7.log`
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
| **Mac** (#965 wording) | `fix/965-sift-read-wording` on `acb187f4` | **5501 / 0 / 52**, **200** summaries, `TEST_EXIT=0`, primary checkout's own `target/`, `KASTELLAN_PG_BIN_DIR` set, `--test-threads=4`, `--no-fail-fast`, `--nocapture`; `[WARN]` 0; log `~/.local/state/kastellan/gate-logs/sweep-965.log`. vs #968's row: identical (prompt text + a doc comment) | not re-run (no code change; #968's cold run stands) | **23** (container service not running) |

The DGX row predates #910's review round (+4 db lib), #906 (+4 core lib, +2 binaries),
#913/#911 (+6 core lib, +1 e2e), #915 (+1 db lib), #920 (+13 core lib), #924 … #949, #945, #951, #955, #957, #961, #962, #963 and #966; all are platform-neutral and were not swept on the DGX. Older Mac rows (incl. #945, #922 nb, sift, sift-fix, chain totals) are in the `909deploy`/`921`/`922a`/`945`/`922nb`/`sift`/`960code` snapshots.

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

- **#971** (#968) — merged + deployed DGX 2026-11-05: a stashed sift read reaches the sorter as its text.
- **#969** (#967, #965 item 2) — merged + deployed DGX 2026-11-05: an over-cap row keeps `task_id`; a forced answer's own table taken out under the code table.
- **#966** (#960 forced table, #964) — merged 2026-11-04, deployed DGX from `main` 2026-11-05, measured 343–356: a forced answer gets the newest sift's table by code, a sift reads a stashed search back, a call on a done step refused with the fix.
- **#963** (#960 by code) — merged, deployed DGX 2026-11-04, measured 329–342: default table order, the table show-back, `distinct`, read lists.
- **#962** (#922) — merged, deployed DGX 2026-11-04: a sift `from` at/past its list's end is refused with the length.
- **#961** (#922 chain totals) — merged, deployed DGX 2026-11-04, re-measured 315–321: a continuing sift's summary carries the chain's counts.
- **#957** (#922 sift fixes) — merged, deployed DGX 2026-11-04, re-measured 308–314: chain tables, `handoff.fetch`, table dates/duplicates/limit/`now`.
- **#955 / #951 / #950** (merged + deployed DGX 2026-11-02 → 03): `sift` + the scrapbook (0029), a code-rendered `answer.table`; the task notebook behind `KASTELLAN_PLANNER_MODE`, bounded planner calls; a non-plan reply re-asked once.
- **#949 … #920** (merged + deployed DGX 2026-10-29 → 11-01, re-measured 227/232–252): attachment names in search hits, localmail `api_minor` 6, the PG harness waits out `57P03` (#949); stable-first prompts (#943); the 15-minute fast-lane ceiling, caps 20/25 (#942); `plan_budget`, batched steps, coverage searches (#938); OpenShell survey (#937); an empty completion asked again once (#930); the even summary shrink (#927); a failed decode's row carries its cost (#924); worker-unforgeable view markers (#920).
- **#919 … #908** (merged 2026-10-24 → 29, deployed both hosts): deploy docs; dependabot rustls (⚠️ makes `python-exec:dev` stale) and uv; #915's paused-clock pin; `boot_report::record_refusal` + the late-Postgres boot e2e (#914); a guard boot refusal is a thinned row (#912); core waits ≤ 60 s for Postgres (#910); the supervised `kastellan-guard` unit (#908).
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
