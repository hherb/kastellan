# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20261030_773remeasure_pre-prune.md`](archive/handover_20261030_773remeasure_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here (#915's mutants and #913's
> detail included; #909's review round is in the `906` snapshot, #609's deploy notes in `909`).
> ⚠️ **Repoint this line in the same commit as the snapshot.** It has been stale twice.

**Last updated:** 2026-10-30 (the #773 live re-measure; #923's decode-row fix, PR #924) ·
**Recent PRs, newest first:** [#924](https://github.com/hherb/kastellan/pull/924) (#773 re-measure, #923 decode row), [#920](https://github.com/hherb/kastellan/pull/920) (#704, #705, merged), [#919](https://github.com/hherb/kastellan/pull/919) (deploy docs, merged), [#918](https://github.com/hherb/kastellan/pull/918) (dependabot rustls, merged), [#917](https://github.com/hherb/kastellan/pull/917) (dependabot uv, merged), [#916](https://github.com/hherb/kastellan/pull/916) (#915, merged), [#914](https://github.com/hherb/kastellan/pull/914) (#913, #911, merged), [#912](https://github.com/hherb/kastellan/pull/912) (#906, merged), [#910](https://github.com/hherb/kastellan/pull/910) (#909, merged), [#908](https://github.com/hherb/kastellan/pull/908) (#609, merged), [#905](https://github.com/hherb/kastellan/pull/905) (#904, merged), [#903](https://github.com/hherb/kastellan/pull/903) (#900, #902, merged), [#901](https://github.com/hherb/kastellan/pull/901) (#896, merged), [#899](https://github.com/hherb/kastellan/pull/899) (#898, merged), [#897](https://github.com/hherb/kastellan/pull/897) (#894, #895, merged), [#893](https://github.com/hherb/kastellan/pull/893) (#890, #892, merged), [#889](https://github.com/hherb/kastellan/pull/889) (#888, #754, merged), [#887](https://github.com/hherb/kastellan/pull/887) (#752, #753, merged), [#886](https://github.com/hherb/kastellan/pull/886) (#885, merged), [#884](https://github.com/hherb/kastellan/pull/884) (#823, merged), [#883](https://github.com/hherb/kastellan/pull/883) (#882, merged); #881 … #795 are in the 896/905/906 snapshots.
Older PRs are in the [`archive/`](archive/) snapshots; **`gh issue list --state open` is the live
answer** and the only one worth trusting. ·
**The DGX runs PR #924's branch tree** (unmerged; deployed 2026-10-30, env diff IDENTICAL, `NRestarts=0`; a plain `upgrade_from_git.sh` returns it to `main`); **the Mac runs #917's**. **DGX:**
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

### This session (2026-10-30, latest): the #773 live re-measure — thinking stays ON

- #920 deployed to the DGX first. The set ran through **`kastellan-cli ask --classification-floor Public "<q>"`**,
  not Matrix: it is the same fast lane and floor as a DM, and is stateless by construction, so no fresh room is needed.
  ⚠️ Run the questions **one at a time**. Readout: `scratchpad`-style `measure.sh <task_id>` (task state, then each
  plan's `latency_ms`/tokens/decision, steps, answer). Tasks **207–210 = ON, 212–215 = OFF**; 211 is an
  operator DM that overlapped 209.
- **ON 4/4 completed** (277–548 s, 10–18k generated tokens); **OFF 2/4 failed** on an empty completion
  (`raw_len 0`, `finish_reason stop`, 48 tokens) → [#923](https://github.com/hherb/kastellan/issues/923). OFF
  also sent `{{message_id}}`/`<message_id>` placeholders as params and dropped `has_attachment` between
  searches. **Decision: `KASTELLAN_LLM_DISABLE_THINKING=0` stays** (restored; overlay byte-identical to the backup).
- Filed: [#921](https://github.com/hherb/kastellan/issues/921) (an elided output reads as "not retrieved": the planner
  re-fetches it and the answer says so wrongly), [#922](https://github.com/hherb/kastellan/issues/922) (3/4 hit the
  fast lane's 5-plan cap; the Qantas table has Price/GST "Unknown"). #698 (filter-only search) and #760 (paged PDF)
  work live.
- **"Most recent" was answered wrong because of the query dialect, not the archive.** localmail's plain search is
  `plainto_tsquery('simple', …)`: **every word must match, and `OR` is just another word**. The planner's
  `flight booking OR … OR reservation` matched two 2025 emails. #924's `mail.search` `query` doc now says so;
  live re-ask (task 217): `query: "flight"` → the newest e-ticket (28 Oct), 119 s. Task 218 ("most recent
  invoice") hit the plan cap again (#922).
- **localmail on the DGX had stalled** (IDLE stuck `reconnecting` since 02:58, poll stale since 21:33 the evening
  before; the unit read `active` the whole time). Restarted 2026-10-30 08:50; heartbeats fresh. Check with
  `~/src/localmail/.venv/bin/localmail daemon status` — ⚠️ **`systemctl is-active` cannot see it.** Filed
  hherb/localmail#414 (pool connections unbounded; most likely cause), #415 (search-hit dates are INTERNALDATE
  in host local time labelled UTC, +11 h), #416 (Gmail's 29-min IDLE drop logged as a crash); kastellan
  [#925](https://github.com/hherb/kastellan/issues/925) (archive freshness is invisible to the bot).
- **Code (PR #924):** `AgentError::Decode` carries the completion's `usage` + `latency_ms`, so the
  `plan.formulate_failed` row has `llm_usage`/`latency_ms` (the `plan.formulate` keys). Both wiring points are
  mutation-checked. ⚠️ **A new `AgentError::Decode` field breaks 7 fakes in `core/tests/`**, not just the lib's
  two, and zsh's `grep --include=*.rs` silently matches nothing: quote the glob.

### Previous (2026-10-29): #704 + #705 — core markers and the handoff head (PR #920, merged) — what still binds (prose: the `773remeasure` snapshot)

- View **key** markers `…omitted_keys`/`…view_unavailable` cannot be spelled by a worker (no key may start
  with `.`); the **string** markers stay spellable (by design). ⚠️ **Never prune a rendered view twice** — the
  second pass drops the core's own `…` markers. A worker's top-level `injection_blocked` is a catalogue Block
  (`post_process::FORGED_MARKER_REASON`).
- `summary_head` = raw JSON head (`injection_guard::screened_head`), rendered **once**; it never shows part of
  a string the view shows whole (`result_view::is_atomic`). ⚠️ #705's option 1 as filed would have shown text
  past `SCAN_BYTE_CAP` [[issue-as-filed-can-carry-a-regression]]. Pre-#920 turn records carry `_omitted_keys` (ages out).
  ⚠️ **`git mv` stages the rename** — a later "add specific files" commit still carries it.
- ⚠️ The `probe::run … Connection refused` tail of the DGX `kastellan-core.err` is the 10-25 race — check mtime.

### Previous (2026-10-26 → 29): the boot arc — #909, #906, #913/#911, #915 (PRs #910, #912, #914, #916, merged) — what still binds

- **#909:** boot waits ≤ **60 s** for Postgres only on `ECONNREFUSED`/`ENOENT`/`57P03` (missing socket dir refuses
  at once; `probe::run`'s own dial is uncapped). ⚠️ A paused-clock loop test needs an outer `timeout` **and** a call
  cap in the fake [[async-fakes-must-yield-like-io]]. A daemon e2e on a cluster that never starts takes 60 s to refuse.
- **#906:** a guard boot refusal writes a thinned `policy / guard_tier.boot_refused` row (`{"refusal","error_kind"}`).
  ⚠️ A new guard key must name itself (`KASTELLAN_LLM_GUARD_…`) in its error, or `from_env` is misclassified.
- `tests_common::init_pg_cluster` → `PgInit` + `PgInit::start`. ⚠️ The daemon's JSON log is **stdout**, a boot
  refusal **stderr**.

### Previous (2026-10-24 → 25): #609 — the guard backend is supervised (PR #908, merged) — what still binds

- `kastellan-guard` installs only when `KASTELLAN_GUARD_SERVER_BIN`/`_MODEL` are set; ⚠️ port/alias are
  **derived** from `KASTELLAN_LLM_GUARD_URL`/`_MODEL`, read **at install time**. Boot waits ≤ 60 s on `/props`;
  ⚠️ `tests_common::scripted_llm`'s default **503s `/props`** (a guard-configured e2e waits 60 s). Open: #907.

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

### Earlier (2026-10-17 → 19): #875, #722, #872, #870, #867, #862, #860 (merged) — what still binds

⚠️ **Any `Cargo.lock` mtime change makes `kastellan/python-exec:dev` "stale"** — `bash
scripts/workers/python-exec/build-image.sh`. #870: ⚠️ **never put the launchd wait in `stop`**
[[mac-sysv-shm-exhaustion-from-killed-postgres]]. #862: `report_unsaid_lost_rows()` — ⚠️ call once.

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

1. **The planner's answer quality, from the re-measure (2026-10-30).** Before any live mail measurement run
   `localmail daemon status` on the DGX (a stale archive voids it; #925). [#921](https://github.com/hherb/kastellan/issues/921)
   first (cheapest; a wrong statement to the user), then [#922](https://github.com/hherb/kastellan/issues/922)'s
   option 3 (stop spending plans on `list_accounts` and re-fetches), then [#923](https://github.com/hherb/kastellan/issues/923).
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
| **Mac** (#923 decode row) | PR #924 on `dc1a8279` (re-swept at `426dc68c`: 5283 + 1 / 52, core lib **2613** = +1 doc test; the 1 = `asks_e2e` `the database system is starting up`, 35/35 alone — #548/#676) | **5283 / 0 / 52**, **198** summaries, `TEST_EXIT=0`, primary checkout, `KASTELLAN_PG_BIN_DIR` set, `--test-threads=4`, `--no-fail-fast`, `--nocapture`; `[WARN]` 0; log `~/sweep-decode-mac.log`. Per-suite vs `~/sweep-704-mac.log`: **only core lib**, 2607 → 2612 = +4 (#920's last review commit, after that row's sweep) +1 (the new payload test) | exit 0, `CARGO_TARGET_DIR=$HOME/.cargo-clippy-decode` (fresh), **27** | **12** (7 = `python-exec:dev` stale since #918's `Cargo.lock` — rebuild the image) |

The DGX row predates #910's review round (+4 db lib), #906 (+4 core lib, +2 binaries),
#913/#911 (+6 core lib, +1 e2e), #915 (+1 db lib), #920 (+13 core lib) and this PR (+1 core lib); all are platform-neutral and were not swept on the DGX. The #915 Mac row is in the `909deploy` snapshot.

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

- **#920** (#704, #705) — merged 2026-10-29, **deployed DGX 2026-10-30**: worker-unforgeable view markers; a labelled, screened handoff head.
- **#919 / #918 / #917 / #916** (merged 2026-10-29): deploy docs; dependabot rustls (⚠️ makes `python-exec:dev` stale) and uv; #915's paused-clock pin.
- **#914** (#913, #911) — merged 2026-10-28: `boot_report::record_refusal` in the lib, unit-tested; `PgInit` + the late-Postgres boot e2e.
- **#912** (#906) — merged 2026-10-28, deployed both hosts 2026-10-29: a guard boot refusal is a thinned `guard_tier.boot_refused` row.
- **#910** (#909) — merged 2026-10-27, deployed both hosts 2026-10-29: core waits up to 60 s for Postgres at boot.
- **#908** (#609) — merged 2026-10-24, deployed both hosts 2026-10-25: the supervised `kastellan-guard` unit and the `/props` boot wait.
- **#905** (#904) — merged 2026-10-24: health-wait timeouts name the crash-loop evidence.
- **#903** (#900, #902) — merged 2026-10-23: `ServiceStatus::Transitioning`; the manager's state word in timeouts.
- **#901** (#896) — merged 2026-10-23: systemd `uninstall` keeps the unit file on a failed stop; an empty `is-active` is not `Inactive`; `systemctl` bounded at 60 s.
- **#899** (#898) — merged 2026-10-23: headless launchd `uninstall`, lenient state words, one timeout text, `$HOME` uid check.
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
