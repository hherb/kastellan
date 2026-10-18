# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20261018_875_pre-prune.md`](archive/handover_20261018_875_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here (#722's full section and
> the #870/#872 baseline rows included).
> ⚠️ **Repoint this line in the same commit as the snapshot.** It has been stale twice.

**Last updated:** 2026-10-18 (#875: `macos_container_smoke` joins the `container` gate profile; earlier the same day #722 (PR #874), #872 (PR #873) and #870 (PR #871), all merged, and the deploy of #855–#869 on both hosts. The operator is still running the #773 live re-measure) ·
**Recent PRs, newest first:** [#874](https://github.com/hherb/kastellan/pull/874) (#722, merged), [#873](https://github.com/hherb/kastellan/pull/873) (#872, merged), [#871](https://github.com/hherb/kastellan/pull/871) (#870, merged), [#869](https://github.com/hherb/kastellan/pull/869) (#867, #862, #860, merged), [#866](https://github.com/hherb/kastellan/pull/866) (#837, #859, #864, #865, merged), [#863](https://github.com/hherb/kastellan/pull/863) (#858, #861, merged), [#857](https://github.com/hherb/kastellan/pull/857) (#829, merged), [#856](https://github.com/hherb/kastellan/pull/856) (#810, merged), [#855](https://github.com/hherb/kastellan/pull/855) (#852, #853, merged), [#854](https://github.com/hherb/kastellan/pull/854) (handover: deploy), [#851](https://github.com/hherb/kastellan/pull/851) (#836, merged), [#850](https://github.com/hherb/kastellan/pull/850) (#835, merged), [#849](https://github.com/hherb/kastellan/pull/849) (#846, merged), [#847](https://github.com/hherb/kastellan/pull/847) (#832, merged), [#842](https://github.com/hherb/kastellan/pull/842) (#825, merged), [#834](https://github.com/hherb/kastellan/pull/834) (#827, #826), [#830](https://github.com/hherb/kastellan/pull/830) (#813, #828), [#824](https://github.com/hherb/kastellan/pull/824) (#815, #814), [#820](https://github.com/hherb/kastellan/pull/820) (#818), [#819](https://github.com/hherb/kastellan/pull/819) (#816), [#812](https://github.com/hherb/kastellan/pull/812) (#807, #808), [#806](https://github.com/hherb/kastellan/pull/806) (#796–#800, #802), [#804](https://github.com/hherb/kastellan/pull/804) (clippy 1.99 lockfile bump), [#803](https://github.com/hherb/kastellan/pull/803) (TencentDB survey, docs), [#801](https://github.com/hherb/kastellan/pull/801) (#796–#800), [#795](https://github.com/hherb/kastellan/pull/795) (#792, #793).
Older PRs are in the [`archive/`](archive/) snapshots; **`gh issue list --state open` is the live
answer** and the only one worth trusting. ·
**Both hosts run PR #869's tree** (deployed 2026-10-18). **DGX:** plain `scripts/upgrade_from_git.sh`
(exit 0), 15 binaries, generated env **and** `.local` byte-identical to `~/kastellan.env*.bak-pre869`,
live-matrix worker digest `94c86940…` (unchanged — no worker crate changed), migrations 27/27,
`NRestarts=0`, Matrix up in ~31 s. **Mac:** manual recipe [[mac-kastellan-install-recipe]], env + plist
identical, migrations 27/27 — but `install` left `kastellan-postgres` **unloaded** (#870, bootstrapped by
hand). **localmail** on both hosts: its PR #397. The DGX boot ERROR `install dir is writable …
group-writable (mode 0775)` is **pre-existing** (since 2026-07-31) — not a deploy regression. The live
process runs thinking **ON** (`KASTELLAN_LLM_DISABLE_THINKING=0`, `THINKING_SWITCH=reasoning_effort`,
`TIMEOUT_MS=600000`). The last DGX full sweep (#770's deploy): **187/187 suites, 4729 / 0 / 79**.
Rootfs images last rebuilt 2026-09-08.

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

### This session (2026-10-18, latest): #875 — `macos_container_smoke` answers to the micro-VM knob

- **The issue's premise was wrong:** "the shared preflight cannot be used here (a dependency cycle)".
  A dev-only cycle is fine for an **integration** test (one `kastellan_sandbox` is linked), the same
  kind core and supervisor (#872) already have. So `sandbox` gets a **macOS-only** dev-dep on
  `kastellan-tests-common`, and the file-local helper is now one line:
  `kastellan_tests_common::microvm::skip_if_no_container(DEFAULT_IMAGE)` (alpine: no freshness rule).
- **`container` profile** now `-p kastellan-core -p kastellan-sandbox @CONTAINER_SUITES --test
  macos_container_smoke` (named, since `container_suites` greps `core/tests` only): **18 passed, 18
  `[E2E]`, 4 binaries, 0 `[SKIP]`/`[WARN]`/`[panic]`**. Negative control (old knob-free helper put
  back): the gate refuses `macos_container_smoke` as **unhooked** — the per-binary check sees it.
- ⚠️ **Any `Cargo.lock` edit makes `kastellan/python-exec:dev` "stale"** (it is in the #687 mtime
  closure): this PR's one-line lock change made the first gate run panic 4×. Remedy as named —
  `bash scripts/workers/python-exec/build-image.sh` (cached, ~1 min). Same family as #691.

### Earlier (2026-10-18): #722 (PR #874, merged) — what still binds

- `skip_if_no_container` reports through `KASTELLAN_MICROVM_REQUIRE_E2E` (tier `micro-VM`); one knob for
  both backends (they never compile on one host). Its met path prints `[E2E] micro-VM: container
  preflight met for <image> (image built <UTC date>)` (pure `microvm::met_detail`).
- `container_suites()` = the exact complement of `firecracker_suites()` (floor `MIN_CONTAINER_SUITES=3`);
  ⚠️ **not** a grep for `skip_if_no_container` — `gliner_relex_e2e` has a FILE-LOCAL helper of that name.
  The partition test takes its census from the import-parsing `microvm_suites()` and compares sets.
- ⚠️ A stopped Mac `container` service is why `[SKIP]` was 23; it is **5** with it running.
  Filed: [#876](https://github.com/hherb/kastellan/issues/876) (the gate never reads `N ignored`).

### Earlier (2026-10-18): #872 (PR #873) and #870 (PR #871), both merged — what still binds

- **#872:** the launchd smoke suites skip through `skip_if_no_supervisor` (macOS-only dev-dep on
  `tests-common` — ⚠️ not on Linux, its `ring` breaks cross-clippy); `launchd` profile needs a GUI login.
- **#870:** `LaunchAgents::stop_target` polls until each member leaves the domain. ⚠️ **Never put the
  wait in `stop`** — killed postmasters leak SysV segments [[mac-sysv-shm-exhaustion-from-killed-postgres]].

### Previous (2026-10-17): #867, #862, #860 — the shutdown residue (PR #869, merged + deployed) — what still binds

- **#867:** the daemon sinks' `Unreported` is per channel **per action** (`Vec<ActionUnsaid>`). ⚠️ **Grep
  break:** their lines no longer say `channel audit rows had no line`; `audit rows? .*had no line` matches
  both writers. Fixture **`garbage_pool`** → `Client(Protocol)`; ⚠️ one byte alone reads as EOF.
- **#862:** `channel::bus::report_unsaid_lost_rows()` runs **before** the channel stops and returns a
  `#[must_use]` `SaidLostRows`; `report_unsaid_lost_rows_since(&mut said)` after each stop says only the
  delta. ⚠️ Call it **once**. The sinks' twin: `report_unreported_before_stop` → `SaidUnreported`, then
  `report_drained` inside the drain's arm of the `join!`. `bootstrap_tests` pins both orders.
- **#860:** **`PgChannelEvents::new(pool, ChannelId)`** (pub API change); a channel-less row counts under
  `RowChannel::Unnamed(bus channel)`.

### Previous (2026-10-16/17): #829, #810, #858/#861, #837/#859/#864/#865 (PRs #856, #857, #863, #866, merged) — what still binds

- **Pure thinning core** `kastellan_core::audit_thinning` (`Thinning<C>`, `BurstTally`), ⚠️ **`#[doc(hidden)]
  pub`**, binary-only. Daemon sinks (`main/audit_sink_thinning.rs`): one `Thinning` per **channel × action
  × kind** (`shed`, `late`, `not_written`, `unknown`). Bus (`pg_events/lost_rows.rs`): `RowKey { writer,
  writer_channel, channel, action, outcome }` → one **`CountKey`**; ⚠️ **one process-wide `LOST_ROWS`**,
  every user takes it from `pg_events::production_writer()`. **Not thinned, on purpose:** #813's abandoned
  rows, #832's abandoned work. ⚠️ A crash loses the thinned tail (#817).
- ⚠️ **`ChannelEvents::audit` takes `action: &'static str`** — that bounds the per-action counts; never
  widen it to `&str` (a convention, not a construction → #868).
- **`kastellan_db::audit::InsertCause`** (`Answered(SqlState)` / `Io { kind, os }` / `Client(ClientFault)`
  closed enum / `RowCount` — ⚠️ **never the error text**). The first row of each cause **per stretch**
  (2^k-th line to the next) gets a line — ⚠️ **NOT** "differs from the last cause" (interleaved faults
  would print every row). `insert_without_id` (no `RETURNING`, checks one row) returns **`InsertError`**
  (`outcome()`, `cause()`; ⚠️ no `From` into `DbError`); `PANIC`/`FATAL`, `Io`, `Tls`, `Protocol` →
  Unknown = "may not have been written" — ⚠️ never one named cause. Wording in ONE place:
  `channel::audit_text::insert_verdict`. `postgres_e2e` (in the `pg` profile) proves two real causes.
- Both **`closing_pool`** fixtures read the client's first message before closing (else a RST races EOF).
- **#810:** `install_shutdown_signals()` before `spawn_scheduler`; ⚠️ signal tests await a probe stream first.

### Previous (2026-10-12 → 16): the Matrix inbound path — #835, #852, #853 (PRs #850, #855, merged)

- **`ParsePoll` returns a `PolledBatch`** (`spec.rs`): decoded `events` + `undecodable` (count + first
  error, **line only**); Matrix decodes each `Event` on its own — ⚠️ the envelope must follow
  `kastellan_matrix_wire::PollResult`. ⚠️ **On an ack channel a batch with undecodable events is
  wholly undecodable** (acking past a bad event loses it). `channel.inbound_undecodable` has **`part`**
  = `batch` (#836; pre-#852 rows none) / `events` + `undecodable` / `drain` (#853). ⚠️ The
  ack-channel ERROR prints `{e:#}`; its cadence hashes `e.root_cause()` only.
- **#835:** `PolledWorkerSpec::drain` runs once on every driver exit → `stage = 'worker'` rows.
  ⚠️ matrix-sdk 0.18 **saves the sync token before running handlers**; `sdk_live/sync_loop.rs`'s
  empty-`save_changes` barrier **narrows, not closes** it — re-check on an SDK upgrade. ⚠️ `sdk_live`
  compiles **only with `--features live-matrix`**. **Owed:** a live `LiveSdk::drain` leg (DGX).

### Previous (2026-10-08 → 12): #825, #832, #846, #827/#826 (PRs #834, #842, #847, #849, merged) — what still binds

- **#846:** `InboundReceiver` counts the bus's unread buffer on `Drop` (`stage = 'buffered'`), over-counts
  on purpose; ⚠️ needs tokio ≥ 1.52.3. **#832:** replies queue **unclaimed**, the pump claims at `send`.
- **#825:** migration 0027, one claim winner across both buses. ⚠️ **At-most-once**; ⚠️ **the
  terminal-state list lives in four places** — widening it needs a NEW migration.
- **#827/#826:** `PendingInsert` drop guard in `pg_events::audit_or_report` — ⚠️ no `.await` before it.
  ⚠️ `kastellan-db`'s next release must be **0.3.0**; `truncate_payload` must stay IDEMPOTENT.
- ⚠️ **A Mac sweep without `KASTELLAN_PG_BIN_DIR` is NOT evidence** [[postgres-app-bin-paths]].
  ⚠️ **`git push` over SSH fails here** — HTTPS via `gh` [[git-push-ssh-no-identities-use-gh-https]].

### Previous (2026-09-30 → 10-03): the audit sink — #788–#802 (PRs #791, #795, #801, #806) — what still binds

`core/src/main/audit_sink*.rs`: ledger + `spawn`, lease, pure thinning, report lines, tests. Full prose
in the `792`/`796`/`802`/`813` archive snapshots. ⚠️ **`Ledger::snapshot` reads `starting`, then the
live counts, then `pending`**, and `promote` counts live before uncounting `starting` — load-bearing;
mutate against the seams (`snapshot_around`, `Lease::promote_around`, `close_and_count`,
`SinkWriter::spawn_around`), not a stress test. `drain()` waits `DRAIN_BOUND` = **3 s**, then closes
the ledger; inserts are bounded (**4 connections, 1024 queued**); hooks timed (`HOOK_BUDGET` 100 ms).
⚠️ `SkippedId::message_id` is not capped on purpose (#809). ⚠️ A row that must *stay pending* in a
test needs a **`connections: 0`** ledger; the `stalled_pool` fixture is built **inside** a runtime
[[stalled-postgres-test-fixture]]. ⚠️ A test row under a `warn` base tests nothing for INFO.
⚠️ **#791's `Closes #N` keywords did not fire** — check the issues after every merge.
⚠️ **Still unreported by design:** a row lost to a crash.

### Previous (2026-09-20 → 27): thinking (#773/#774), the mail worker (#760, #698, #763, #765), the worker-report arc (#725 → #750, #748, #755) — prose: the `773`/`673`/`748`/`755`/`698` snapshots

- **`KASTELLAN_LLM_THINKING_SWITCH`** (`llm-router/src/thinking.rs`): `chat_template_kwargs` (default)
  or `reasoning_effort` (**the only one Ollama honours**). ⚠️ **Never send both:** vLLM **0.15.1**
  400s on `reasoning_effort: "none"`. `for_guard` **pins** kwargs.
- **`llm_usage` on every `plan.formulate` row** (Ollama: read `reasoning_chars`); failed planning
  calls are audited (`agent/plan.formulate_failed`). Timeouts (`inner_loop/llm_failure.rs`): a
  timed-out plan spends the synthesis turn; a timed-out synthesis retries once without thinking.
- ⚠️ **Not decided, deliberately:** thinking on vs off — that is the live re-measure.
- **Version gate:** search, attachments and header reads refuse below localmail API **1.3** — **no
  fallback**; params parse into a typed `Request` first (#765). Wire facts live in
  `workers/mail/src/localmail_contract.rs` — ⚠️ **`include!`d three times: `pub const`s only, no
  `use`, no `//!`.** Attachments are fetched **by position** and **hashed against the listed sha**;
  `next_offset` is **copied, never computed**. ⚠️ A 50-hit page is ~18 KB, over the 16 KiB step view.
- **Live gate:** `bash scripts/mail/live-shape-gate.sh`. ⚠️ **A new `*_or_skip` helper must be
  classified** in `microvm/guard.rs` or the roster test refuses it.
- **Every profiled test binary must reach the panic hook, checked at RUN time** — call
  `panic_hook::install_once()` first. `MAX_PANIC` is blind before the first knob read (#757).
- ⚠️ **`warn_and_fall_back!` must stay a MACRO** (the check expands at the emitter's callsite, every
  field incl. `message` named, **at the event's own level** — #788 added the INFO arm).
  **`eprintln!` is load-bearing** [[libtest-capture-only-print-macros]]. **No marker may begin with
  `[SKIP]`/`[WARN]`/`[E2E]`**; `WorkerRetirementCause::from_client_error` is THE census.
- **#755:** `run-e2e-gate.sh` refuses a counted marker stranded mid-line (libtest leaves **two**
  gaps). ⚠️ #718's unframed `[SKIP]`s would be refused the day a profile selects them. ⚠️ **The
  scan reads bytes** — `grep -a`, exit status, `LC_ALL=C` [[gnu-grep-binary-file-prints-nothing-exit-0]].
  ⚠️ **`grep -c` counts lines** [[grep-c-counts-lines-not-matches]].

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

1. **The live re-measure — the operator is running it** (DMs, a **fresh room**; #776 is deployed,
   thinking ON, `reasoning_effort` dialect, 600 s timeout). First measurement: re-ask the **Qantas question** (tasks 193/194 timed
   out on the synthesis turn), then read each plan's cost in one query:
   `select action, payload->'llm_usage', payload->>'latency_ms', payload->>'error' from audit_log
   where action in ('plan.formulate','plan.formulate_failed') and payload->>'task_id'='<id>' order by id`. Then #728's multi-search question, a filter-only one (#698), a
   long PDF read across pages (#760), and a revoked mail key once (the planner's reaction to
   `UPSTREAM_AUTH_FAILED`). **The thinking decision (#773) is made from these rows:** repeat the
   set with `KASTELLAN_LLM_DISABLE_THINKING=1` (now real on Ollama) and compare answer quality
   against latency — the #773 probe's non-thinking answer was *wrong*.
   Then #771 (carry localmail's own 401 reason to the operator — the channel's line is now in
   `polled_driver/refusal.rs`, and goes through `emit_worker_refusal_report`) and #538
   (the mail worker's second, hand-rolled localmail mock). **Owed: a DGX run of the `mail-live`
   profile** (`bash scripts/mail/live-shape-gate.sh`) — only the Mac has run it since #766.

2. **#702 follow-ups: #703, #704, #705.** ⚠️ **The guard model never sees object keys** (#703) — any
   new worker passing a third-party JSON object through reopens it silently; needs a DGX guard
   calibration run. The localmail changes the operator offered on 2026-09-14 have **shipped
   upstream** (slices A–E); the worker's side is #760.

3. **The #750 residue, #751–#754, and #757** (the `[panic]` cap's blind spot before the first knob
   read — needs a *measured* default-hook count per profile, DGX too).

4. **Test-harness honesty, now that the gate itself is tested.**
   [#718](https://github.com/hherb/kastellan/issues/718)
   (92 hand-rolled `[SKIP]`s — adding the `sandbox` profile is its acceptance test),
   [#721](https://github.com/hherb/kastellan/issues/721), [#723](https://github.com/hherb/kastellan/issues/723),
   [#724](https://github.com/hherb/kastellan/issues/724), [#691](https://github.com/hherb/kastellan/issues/691)
   (a decision), #237's absent macOS CI leg and
   [#876](https://github.com/hherb/kastellan/issues/876) (refuse `N ignored` — measure every profile first).

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
   **#870** (PR #871, merged): ⚠️ **unverified live** — the next Mac install should need no hand
   `bootstrap`; check `launchctl print gui/$(id -u)/kastellan-postgres` after it.

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
- **Live guard-host facts:** DGX `llama-server … Shieldstral-1.0-3B-Q8_0.gguf --alias shieldstral
  --port 8081 -c 131072 -ngl 99`; `/props` reports `default_generation_settings.n_ctx` (no top-level
  `n_ctx`). Restart with **at least `-c 66048`** or the daemon refuses to boot. Guard keys live in
  `~/.config/kastellan/kastellan.env.local`, which `install` never rewrites.
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
`worker_stderr/report/tool_worker.rs` 710, `core/src/channel/mod.rs` **569** (+10 in #849, +6 in #835, +16 in #836, +5 in #852, docs — the `actions` consts are the natural cut).
Also over (grew in #818 without a split): `db/src/tasks.rs` **819** (+11 in #825: `CRASHED_DETAIL`),
`db/src/graph.rs` **~935**, `core/src/entity_extraction/batch_upsert.rs` **~520**.
Also over: `core/src/memory/l3_surface.rs` 539 (+5 in #785, a doc paragraph), `core/src/scheduler/inner_loop.rs` 906,
`tool_dispatch.rs` 722, `attach/tests.rs` 727, `require.rs` 661,
`scripts/run-e2e-gate.sh` ~690 (shell). ⚠️ **`core/tests/mail_live_shape_e2e.rs` 559 — split it before
its next leg** (the attachment half is the natural cut). ⚠️ `audit/truncate/tests.rs` is 833.
Recent splits (the pattern to keep; list in the `867` snapshot): #869 `pg_events/{test_support,
lost_rows/tests}.rs` and `audit_sink_thinning_tests.rs`, #849, #835, #806 — each proved byte-identical.
⚠️ **At or near the cap:** `main/audit_sink.rs` **500**, `worker_stderr/report/delivery/tests.rs` **499**,
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
| **Mac** (#875) | this PR | **5137 / 0 / 52**, **195** suites, `TEST_EXIT=0`, `KASTELLAN_PG_BIN_DIR` set, `--test-threads=4`, `--no-fail-fast`, `--nocapture`; `[WARN]` 0, `[SKIP]` 5 (gliner), SysV shm 4 before = 4 after; log `~/.local/state/kastellan/gate-logs/sweep-875.log`. 5137 = #874's tree exactly (no new tests; one assertion added to an existing one) — also the first full sweep over #874's review fixes. `container` profile: 18 passed, 18 `[E2E]`, 0 `[SKIP]` | exit 0, `CARGO_TARGET_DIR=$HOME/.cargo-clippy-875` (fresh dir), **27** `Checking kastellan`; `kastellan-sandbox` cross-clippy'd for `aarch64-unknown-linux-gnu` | **5** Mac |
| **Mac** (#722) | PR [#874](https://github.com/hherb/kastellan/pull/874) | **5135 / 0 / 52**, **195** suites, `TEST_EXIT=0`, `KASTELLAN_PG_BIN_DIR` set, `--test-threads=4`, `--no-fail-fast`, `--nocapture`; `[WARN]` 0, **`[SKIP]` 5** (gliner 5; the 18 container-service skips are gone — service started, image rebuilt), SysV shm 4 before = 4 after; log `~/.local/state/kastellan/gate-logs/sweep-722.log`. 5135 = 5128 + #873's review (+2, after its sweep) + 5 (tests-common lib 454 → **461** = both); ⚠️ **not re-swept after the review fixes** (+2: tests-common lib **463**, tree **5137**; `tests-common` lib and clippy ran green alone); `cli_entities_e2e` 5 → 6 is #872's contention failure gone. `container` profile: 8 passed, 8 `[E2E]`, 0 `[SKIP]` | exit 0, `CARGO_TARGET_DIR=$HOME/.cargo-clippy-722` (fresh dir), **27** `Checking kastellan` | **5** Mac |

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
| `tests-common` `gate_script_tests` | 47 | the gate script's table **and** its verdict (floors, caps, per-binary hook check), run against a fake `cargo` |
| container (`core`: `python_exec_container_e2e`, `python_exec_warm_idle_e2e`, `lifecycle_container_routing_e2e`; `sandbox`: `macos_container_smoke`; macOS) | 4 / 3 / 1 / 10 | **real** Apple `container` micro-VM round-trips, mem cap, warm reuse. `bash scripts/run-e2e-gate.sh container` |

## Key design decisions locked in

**Not restated here — they drift.** Hard constraints: [`CLAUDE.md`](../../../CLAUDE.md) § Hard
constraints; the rest: [`docs/architecture.md`](../../architecture.md) and the ROADMAP.
**The one worth repeating:** worst-case compromise reaches *at most* the agent's own OS user, its own
Postgres role, its own scratch FS, and the allowlisted endpoints for the *one* compromised tool
([`docs/threat-model.md`](../../threat-model.md)).

## Recently merged

Newest first; full prose in the [`archive/`](archive/) snapshots and git history.

- **[#874](https://github.com/hherb/kastellan/pull/874)** (#722) — merged 2026-10-18: the Apple-`container` tier's `[E2E]` line and the Darwin `container` gate profile.
- **#873** (#872), **#871** (#870) — merged 2026-10-18: launchd suites through a knob + `launchd` profile; `stop_target` waits for launchd.
- **#869** (#867/#862/#860), **#866**, **#863**, **#857**, **#856**, **#855** — merged 2026-10-16/17, **deployed both hosts 2026-10-18**: the audit-loss thinning family.
- **[#851](https://github.com/hherb/kastellan/pull/851)** (#836, merged, deployed 2026-10-14) — an undecodable poll batch is ERROR + a `channel.inbound_undecodable` row on Matrix, and holds the poll on every channel.
- **#850, #849, #847, #842, #834, #830, #824, #820, #819, #812, #806, #804, #803, #801, #795, #791, #787, #786, #784, #781, #778, #776, #775, #770, #766** — Matrix worker held count (`worker` stage); unread bus buffer (`buffered`); reply claim at send; reply catch-up (0027); audit-sink close-out; clippy 1.99 lockfile; TencentDB survey; shutdown names pending rows; `[audit-lost]` + drain; recovery lines, bounded sinks; reply queues + `[worker-refusal]`; recall excludes L0/L3; cognee survey; a refusal is not a death; route spellings; the thinking switch; `UPSTREAM_AUTH_FAILED`; the live mail shape gate. One-liners in the `802`/`815`/`825` archive snapshots (#830: a stopped insert says `may not have been written`; #824: channel drops leave rows; #820: NUL refused/escaped beyond `audit_log`; #819: NUL escaped in audit rows; #812: per-channel thinning + bus `[audit-lost]`). **#764 … #685** and earlier: git history and the [`archive/`](archive/) snapshots.

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
