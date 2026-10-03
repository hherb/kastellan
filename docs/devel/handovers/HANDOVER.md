# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20261003_802_pre-prune.md`](archive/handover_20261003_802_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here.
> ⚠️ **Repoint this line in the same commit as the snapshot.** It has been stale twice.

**Last updated:** 2026-10-03 (#796–#800 + #802 — the audit-sink close-out, PR #806; the operator
is still running the #773 live re-measure) ·
**Recent PRs, newest first:** [#806](https://github.com/hherb/kastellan/pull/806) (#796–#800, #802), [#804](https://github.com/hherb/kastellan/pull/804) (clippy 1.99 lockfile bump), [#803](https://github.com/hherb/kastellan/pull/803) (TencentDB survey, docs), [#801](https://github.com/hherb/kastellan/pull/801) (#796–#800), [#795](https://github.com/hherb/kastellan/pull/795) (#792, #793), [#791](https://github.com/hherb/kastellan/pull/791) (#788, #789, #790), [#787](https://github.com/hherb/kastellan/pull/787) (#782, #783), [#786](https://github.com/hherb/kastellan/pull/786) (#785), [#784](https://github.com/hherb/kastellan/pull/784) (cognee survey), [#781](https://github.com/hherb/kastellan/pull/781) (#769), [#778](https://github.com/hherb/kastellan/pull/778) (#767, #768), [#776](https://github.com/hherb/kastellan/pull/776) (#773, #774).
Older PRs are in the [`archive/`](archive/) snapshots; **`gh issue list --state open` is the live
answer** and the only one worth trusting. ·
**The DGX runs PR #787's tree** (deployed 2026-09-29 evening from its branch, which is `main` @
#787 since the merge): 15 binaries, generated env **and** `.local` byte-identical to
`~/kastellan.env*.bak-pre787`, live-matrix worker digest `4b60a6ce…`, `NRestarts=0`, Matrix up at
attempt 1. `scripts/upgrade_from_git.sh` switches its checkout back to `main` by itself, so the next
plain run is right. ⚠️ **None of #791, #795, #801 or #806 is deployed.** The live process runs thinking **ON**
(`KASTELLAN_LLM_DISABLE_THINKING=0`, `THINKING_SWITCH=reasoning_effort`, `TIMEOUT_MS=600000`). The
last DGX full sweep (#770's deploy): **187/187 suites, 4729 / 0 / 79**, 0 `[WARN]`, 4 `[SKIP]`
(gliner opt-in). Rootfs images last rebuilt 2026-09-08.

> **Header convention (since 2026-09-11, after three recurrences).** This header names **PRs and
> issues only — never a branch name, a HEAD sha, or the word OPEN.** A merge falsifies those with no
> actor in between; a PR number it cannot. **The tip and the open set are one command each:**
> `git log --oneline -1 origin/main` and `gh pr list --state open`. Run them before trusting a word
> of this file.

> ⚠️ **An issue's census can be wrong, and so can its diagnosis — read the rows, not the issue.**
> #679 named 7 call sites (12), #690 named 4 subprocesses (10), #677 misread a schema rejection as a
> duplicate search, #719 named 3 failing tests when 7 were broken, and **#749's "related" claim was
> simply false** — the real default panic hook *does* print the `RUST_BACKTRACE=1` note at
> `RUST_BACKTRACE=0`, measured, so "fixing" it would have created the divergence it alleged.
> [[issue-as-filed-can-carry-a-regression]]

> ⚠️ **A fix for a reviewer's finding can carry the next defect,** and **a passing mutation proof
> is not a review** [[mutation-proof-counts-only-mutants-you-tried]]. #677's approved spec capped
> strings at 512 B, breaking the question that worked [[plan-text-is-a-defect-source]]; #720's
> review closed a fail-*open* hole with a check that made it fail *always* (fixed in #726).
> **#750's own movement-only commit did it again**: hoisting `tail.snapshot()` above the wait is a
> harmless-looking reorder and is #730 exactly — an existing test caught it. **And #750's REVIEW
> round did it a third time**: making `decode_drain_end`'s unknown-byte arm loud put a
> `tracing::error!` inside a function polled every 2 ms for 250 ms (~125 lines per failure) and
> falsified the word `Pure:` one line above the edit. Self-review caught that one.

> ⚠️ **A forward reference auto-closed issue #718.** #720's body said the gate profile is the acceptance
> test for "whichever PR <closing-keyword> #718", and GitHub's scanner matched it — the fifth
> recurrence of this hazard and a new shape (no negation at all). Never quote the phrase literally.
> Reopened 2026-09-19. **Run the regex over every PR body and commit message before merging**, not
> only over the negations [[pr-body-not-fixed-autocloses-issue]].

> ⚠️ **A FIXTURE nobody rebuilds is a gate nobody runs** [[stale-fixture-turns-a-gate-into-a-formality]],
> and **a guard built from a census shares the census's blind spot** [[guard-shares-the-census-blind-spot]].

---

## Current state

### This session (2026-10-03): the audit-sink close-out — #796–#800, #802 (PR #806)

Closes the #792 → #802 residue chain. `core/src/main/audit_sink*.rs` is now six files:
ledger + `spawn` (`audit_sink.rs`), lease (`_lease.rs`), pure thinning (`_thinning.rs`), report
lines (`_report.rs`), and their tests. What binds:

- **A lease is *starting* until its driver is up** (`SinkWriter::starting_with_ledger` → `Starting`);
  `login_outcome` calls `started()` **only** for a login that came up. Not waited for by `drain`;
  said at INFO. Email's lease is live at once (`skipped_sink`; its bring-up is synchronous).
  `SinkKind::{AuditsOnExit, SilentOnExit}`. ⚠️ **`Ledger::snapshot` reads `starting`, then the
  live counts, then `pending`**, and `promote` counts live before uncounting `starting` — all
  load-bearing; seams `snapshot_around` (two closures) and `Lease::promote_around`.
- **Close-vs-spawn handshake** has deterministic seams: `close_and_count` (drain: close, then count)
  and `SinkWriter::spawn_around` (spawn: count, then read `closed`). ⚠️ A contention stress test
  could not kill a reversed read order — mutate against the seams, not the stress.
- **Thinning is per burst** (pure `Thinning`, 60 s quiet gap, clock injected); `shed` and `late`
  apart. Rows thinned **after** the drain: `close_then_report_unreported` in `main` says them
  **before** the mirror + pool close (sqlx's `Pool::close` has no bound) and again after. It takes
  `Drained` by value; `Drained`'s fields are private, `Default` test-only. ⚠️ Gap: rows refused
  after the last line, while the runtime drop waits on an abandoned login (#805).
- `spawn` → `Result<JoinHandle, Unwritten<'static>>`, label a `RowLabel` (always `quoted_id`'d).
  `UndeliveredReply::payload` is the reply row's **only** definition (the bus builds the view; the
  lib's `pub fn reply_undelivered_payload` is gone — a 0.x API break for the next release notes).
  `observed_at_json` warns when it stores null. ⚠️ `SkippedId::message_id` is **not**
  capped on purpose — the audit layer's 4 KiB cap stores an oversized payload as sha256 + len of the
  WHOLE payload, which loses channel/reason/time too ([#809](https://github.com/hherb/kastellan/issues/809)).
- ⚠️ **Fixture lesson:** the stalled pool fails an insert after 1.5 s; a row that must *stay
  pending* needs a **`connections: 0`** ledger (both boot tests' label checks now do).
- New `core/tests/audit_lost_stderr_fallback_e2e.rs` (re-exec, hostile id, ERROR level) — not in
  any gate profile, like its `worker_refusal` sibling. Mutants 10/10 + 7/7 (review round). Filed:
  #807 (flood counts said only at a clean stop), #808 (bus insert failure off `[audit-lost]`), #809–#811.

### Previous (2026-09-30/10-01): #792 + #793 (PR #795), #796–#800 (PR #801) — what still binds

Full prose in the `796` and `802` archive snapshots. `drain()` runs beside `scheduler.shutdown()`,
waits `DRAIN_BOUND` = **3 s**, then **closes** the ledger; `[audit-lost]` is the fifth stderr
marker (`worker_stderr/report/audit_lost.rs`, typed `AuditLostWriter`); a poll answered after the
bus went away acks nothing; the driver times every audit hook (`HOOK_BUDGET` 100 ms); both rows
carry `observed_at`. ⚠️ **Still unreported by design:** a row lost to a crash.

### Previous (2026-09-30): #788 + #789 + #790 — the review residue of #787 (PR #791)

Full prose in the `792` archive snapshot. What still binds:

- **A refusal that ends says so** on `[worker-refusal]` at `RefusalSeverity::Recovered` (INFO arm
  of `warn_and_fall_back!`); ⚠️ it falls back under `RUST_LOG=warn` too, on purpose. The outage
  lines (`note_answer`, `report_down`) stay unmarked — the supervisor's markers cover them.
- **No audit sink blocks the driver thread**; spawned inserts are bounded (**4 connections, 1024
  queued**, then shed and reported). Test fixture: `audit_sink::test_support::stalled_pool` —
  ⚠️ build it **inside** a runtime [[stalled-postgres-test-fixture]]; assert the insert was
  *attempted*, not only that the call returned.
- **`ReplyUndeliveredAudit` takes a body-less `UndeliveredReply` view**; driver tests tell
  audited replies apart by **peer**. ⚠️ A test row under a `warn` base tests nothing for INFO.
- ⚠️ **#791's `Closes #N` keywords did not fire** — check the issues after every merge (#806 too).

### Previous (2026-09-29): #782 + #783 (PR #787) and #785 (PR #786) — what still binds

- **Replies queue per conversation** (pure `ReplyQueues`); given up after ≥ 1 h refusing **and**
  ≥ 3 refusals, then audited `channel.reply_undelivered`; each queue is capped at 256. ⚠️ **A channel-wide failure holds every
  conversation and RESTARTS every give-up clock.** ⚠️ **The Matrix worker's 401 is
  `UPSTREAM_UNAVAILABLE`, NOT `UPSTREAM_AUTH_FAILED`** (the latter would run two matrix-sdk clients
  on one store). ⚠️ **Never test a driver line through a scoped `tracing` subscriber** — read the
  emitter's per-label test record [[tracing-scoped-subscriber-interest-cache-flake]].
- **#785:** pure `MemoryLayer::is_recallable` (**L1, L2, L4 yes; L0, L3 no**) in all four recall
  lanes' SQL before each `LIMIT`. ⚠️ **L1 stays recallable on purpose.**

### Previous (2026-09-27): #773 + #774 — thinking, and slow planning calls — what still binds

Full prose in git history (#776) and the `773` archive snapshot.

- **`KASTELLAN_LLM_THINKING_SWITCH`** (`llm-router/src/thinking.rs`): `chat_template_kwargs`
  (default) or `reasoning_effort` (**the only one Ollama honours**). ⚠️ **Never send both:** vLLM
  **0.15.1** (the DGX's `vllm:26.02`) 400s on `reasoning_effort: "none"`. `for_guard` **pins** kwargs.
- **`llm_usage` on every `plan.formulate` row** — Ollama reports **no** reasoning-token count; read
  `reasoning_chars`. Every failed planning call is counted and audited
  (`agent/plan.formulate_failed`).
- **Timeouts** (`inner_loop/llm_failure.rs`): after gathering, a timed-out plan spends the synthesis
  turn; a timed-out synthesis retries once with thinking suppressed; a final timeout is worded for
  the user (`like '%timed out%'` still matches).
- ⚠️ **Not decided, deliberately:** thinking on vs off — that is the live re-measure.

### Previous (2026-09-26/27): the mail worker — #760, #698, #763, #765 — what still binds

Full prose in [`archive/handover_20260927_673_pre-prune.md`](archive/handover_20260927_673_pre-prune.md).

- **Version gate:** search, both attachment tools and header reads refuse below localmail API
  **1.3** — **no fallback** (operator's call). Params are parsed into a typed `Request` (pure
  `handler/request.rs`) **before** the gate, which takes `&Request` (#765).
- **Wire facts live in `workers/mail/src/localmail_contract.rs`**, `include!`d by the live gate and
  `mock_localmail`. ⚠️ **Compiled three times: `pub const`s only, no `use`, no `//!`.**
- **Slice E:** `fields` + `snippet_chars: 120` on every search. ⚠️ **A 50-hit page is still ~18 KB,
  over the 16 KiB step view.** **Headers:** `?headers=list`, checked fail-closed, never reshaped.
- **Slice D:** attachments fetched **by position** and **hashed against the listed sha**. Paged text:
  `next_offset` **copied, never computed**; inconsistent paging is a fault. ⚠️ 16 KiB view
  guaranteed only for Latin text.
- **#698:** a blank query defaults to `sort: date`. ⚠️ `sort::is_textless` cannot see an
  operator-only query.
- **Live gate:** `bash scripts/mail/live-shape-gate.sh` → the `mail-live` profile (knob
  `KASTELLAN_MAIL_LIVE_REQUIRE_E2E`). ⚠️ **A new `*_or_skip` helper must be classified** in
  `microvm/guard.rs` (`BANNED_HELPERS` or `REQUIRE_AWARE`) or the roster test refuses it.

### Previous (2026-09-20/23): the worker-report arc (#725 → #750, #748) and #755

Full prose in the `748`/`755`/`698` archive snapshots. What still binds:

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

**#673/#674 (PR #770):** `UPSTREAM_AUTH_FAILED = -32004` for localmail 401/403; ⚠️ only
`ClientError::Rpc` keeps its type through `client_error_to_anyhow`.

**#769 (PR #781) — a refusal is not a death.** One classifier, `persistent/call_failure.rs`
(`Gone` / `Refused` / `CredentialRefused` / `Unavailable`), read by the supervisor **and** the polled
driver. ⚠️ A worker reads its credential **once, at spawn**. ⚠️ **A refused ack holds the next poll.**
⚠️ **Workspace MSRV is 1.78.** The cognee survey (#784, [notes](../notes/2026-09-28-cognee-survey.md))
is **not a dependency**. **#767/#768:** route spellings are pure `pub fn`s in `localmail_contract.rs`;
the live gate pages from offset **2** [[probe-value-equal-to-default-hides-dropped-param]].

**#694 (#617).** `truncate_payload` derives `req_summary = {head, sha256, len}` centrally, over the
**whole** request. ⚠️ **No sink double can test it** [[audit-sink-doubles-hide-storage-transforms]].
⚠️ **Review subagents mutate the working tree** — give each its own `git worktree`.

**#692 / #688 / #685 / #683 / #680 — micro-VM budgets and freshness.** `kastellan_sandbox::bounded_command`
for any host probe; ⚠️ **a bounded runner must not join its drains** [[bounded-subprocess-must-not-join-drains]].
Both guest kernels lack Landlock (seccomp-only by design). One producer for `target/release/`:
`bash scripts/build-release.sh`, run LAST [[cargo-package-selection-changes-binary-bytes]]; the
freshness reference is the **sha256 of the baked copy** [[cargo-relinks-identical-mtime-not-content]].

**#660 — the second pre-release security audit.** Three **fail-closed** lockdown rules bite careless
fixtures (missing `KASTELLAN_SECCOMP_PROFILE`, an unenforceable Landlock ruleset, a corrupt guest env
token). Per-spawn dirs via `create_private_dir` — **do not "fix" back to `create_dir_all`**.
**Before release: flip force-routing on.** Deferred list in `docs/security-audit-2026-09-02.md`.

**One-liners.** #681: a lean tail plus recovery beat a fat verbatim tail (68.3 % vs 45.8 %). #675: a
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
> (never for daemon e2es or the gate sweep), or kill the IDE's cargo child (recurred 2026-09-30).

> ⚠️ **Do NOT edit the tree while a sweep compiles in it — and give every reviewer its own worktree**
> [[never-edit-tree-during-a-sweep]] [[worktree-cwd-lands-on-main]]. **A fresh worktree has no worker
> binaries** — `cargo build --workspace` first [[worktree-has-no-worker-binaries]]. ⚠️ **Copying files
> into a DGX worktree with macOS `tar` creates `._*` AppleDouble files** — `sandbox/tests/._x.rs` is a
> cargo test target; delete them before building.

> ⚠️ **`syspolicyd` saturates and no new executable can start on the Mac.** Tell: `ps -o time`
> exactly `0:00.00` against minutes of ELAPSED. Fix: the operator runs `sudo killall syspolicyd`.
> **But a slow build is usually CONTENTION** [[mac-fresh-large-binaries-hang-in-dyld]].

> ⚠️ **`kastellan-worker-egress-proxy` leaks on the Mac** (not investigated), and **a `pgrep -f`
> wait loop matches itself** — use `pgrep -x` [[pgrep-wait-loops-match-themselves]].

## Read these first

1. [`docs/architecture.md`](../../architecture.md) — process model, cross-platform table
2. [`docs/threat-model.md`](../../threat-model.md) — the invariant, scenarios, defence layers
3. [`docs/devel/ROADMAP.md`](../ROADMAP.md) — the master sequenced TODO with commit hashes
4. Memory notes (auto-loaded) — `~/.claude/projects/-Users-hherb-src-kastellan/memory/MEMORY.md`
5. [`archive/`](archive/) — the full prose for everything this file summarises

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
   [#722](https://github.com/hherb/kastellan/issues/722) (container tier knob + profile),
   [#721](https://github.com/hherb/kastellan/issues/721), [#723](https://github.com/hherb/kastellan/issues/723),
   [#724](https://github.com/hherb/kastellan/issues/724), [#691](https://github.com/hherb/kastellan/issues/691)
   (a decision), #237's absent macOS CI leg.

5. **First DGX deploy of #791 + #795 + #801 + #806:** watch one restart for the `[audit-lost]` /
   INFO drain lines (a Matrix login still in progress at shutdown should now be INFO "not finished
   starting", not a loss), and query an `observed_at` on the next `channel.*` row. Then
   [#805](https://github.com/hherb/kastellan/issues/805) (the runtime drop can outlive the last
   line) and #807–#811 above — #807 and #808 first.

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
`core/src/channel/bus.rs`, `workers/matrix/src/sdk_live.rs`, `llm-router/src/messages.rs`,
`core/src/main.rs`, `tests-common/src/microvm/{mod,container}.rs`, `sandbox/tests/macos_smoke.rs`,
`core/src/channel/email/mod.rs` 542, `worker_stderr/report/tool_worker.rs` 710.
Also over: `core/src/memory/l3_surface.rs` 539 (+5 in #785, a doc paragraph), `core/src/scheduler/inner_loop.rs` 906,
`tool_dispatch.rs` 722, `attach/tests.rs` 727, `require.rs` 661,
`scripts/run-e2e-gate.sh` 561 (shell). ⚠️ **`core/tests/mail_live_shape_e2e.rs` 559 — split it before
its next leg** (the attachment half is the natural cut). Recent splits done **first** (the pattern to
keep): #750 `worker_stderr/`, #769 `persistent.rs`, #767 `attach.rs`, #785 `memories/search.rs`,
#782 `report/delivery.rs` and `polled_driver/tests.rs`, #788 `channel/mod.rs` (→ `undelivered.rs`) and
`polled_driver.rs` (→ `polled_driver/audit.rs`), #792 `main/audit_sink.rs` (→ `audit_sink_tests.rs`,
`audit_sink_test_support.rs`), #806 `email_boot.rs`/`matrix_boot.rs` (→ `*_tests.rs`, proved byte-identical). ⚠️ **Near the cap:** `polled_driver.rs` **484**, `worker_stderr/report/delivery/tests.rs`
**484**, `polled_driver/replies.rs` **478** — split before the next change grows them.
Per-PR growth history: the [`785` archive snapshot](archive/handover_20260929_785_pre-prune.md).

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
| **Mac** (#796–#800 + #802 — **the gate that stands**) | PR #806 (`54e882b0`) | **4808 / 0 / 52**, **191** suites, `TEST_EXIT=0`, `[WARN]` **0**, `[SKIP]` **23**; `KASTELLAN_PG_BIN_DIR` set, `--no-fail-fast -- --test-threads=4 --nocapture`. **Predicted exactly** against `main` ≈ 4782 (the row below + #801's review round's 2 bin tests, recorded after it): bin 47 → 66 (+19), core lib +5, new `audit_lost_stderr_fallback_e2e` +2 / +2 ignored (its inner fixtures). Mutants 10/10. **Review round (not a full sweep):** bin 66 → **73**, core lib 2336 / 1 ignored and the e2e unchanged — expect **4815** next sweep; mutants 7/7 | exit 0, `CARGO_TARGET_DIR=$HOME/.cargo-clippy-802`, **27** | **23** Mac |
| **Mac** (#796–#800 — superseded) | PR #801 | **4780 / 0 / 50** after the one flaky assertion was fixed: the sweep read **4779 / 1 / 50**, **190** suites, `[WARN]` **0**, `[SKIP]` **23**; the failure was my new stress test's `==` (see above), re-run 5× green + the whole bin suite (45) after the fix, not a second full sweep. **Predicted exactly** against the row below: bin 32 → 45 (+13), core lib +1 (`audit_lost`). Mutants 4/4 | exit 0, `CARGO_TARGET_DIR=$HOME/.cargo-clippy-796` | **23** Mac |
| **Mac** (#792 + #793 — superseded) | the doc-fix commit on PR #795 (`c9cffe77`) | **4766 / 0 / 50**, **190** suites, `TEST_EXIT=0`, `[WARN]` **0**, `[SKIP]` **23** (unchanged), tree hash identical before and after; `KASTELLAN_PG_BIN_DIR` set, `--no-fail-fast -- --test-threads=4 --nocapture`. **Predicted exactly** against the row below: #791's review round +4 (`kastellan` bin 20 → 24, measured here first) and this PR +21 (core lib +13: `audit_lost` 3, `skipped` 2, `undelivered` 1, driver `tests/audit` 5, `tests/ack` 1, delivery 1; bin 24 → 32). Mutants 15/15 | exit 0, cold `CARGO_TARGET_DIR=$HOME/.cargo-clippy-792`, **27** `Checking kastellan` | **23** Mac |

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
`conversation_turns_e2e` under a full sweep (`the database system is starting up` / a vanished
per-test socket — #548/#676). A dropped ephemeral port is not a reserved one (fixed by confirming).

⚠️ **On this Mac the DEFAULT full sweep is red and `-- --test-threads=4` is green** — #548/#676
per-test-cluster contention, not a regression. The tell is `the database system is starting up`, the
failing suites **move between runs**, and each passes in isolation. Reduce threads; never reduce suites.

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

### The tree — 27 crates

Full layout in the root [`README.md`](../../../README.md) § Layout; load-bearing crates in
[`CLAUDE.md`](../../../CLAUDE.md) § Project shape.

### Integration-suite map

Only the rows that tell you *where to look when something goes red*.

| Suite | Tests | What's verified |
| ----- | ----- | --------------- |
| `sandbox` (`linux_smoke` / `macos_smoke` / `macos_container_smoke`) | 10 / 13 / 10 | **real** jails: fs invisibility, net deny, relative-path reject, OOM-kill, per-spawn `/tmp`, fresh session leader, **worker starts in `/` on both backends** (#719), the Firecracker VMM jail launching (#671), the #689 Landlock drift detector |
| `core` Firecracker (15 suites, `#[ignore]`, DGX) | 30 | **real KVM** round-trips, mem cap, net deny, warm idle, VMM confinement, egress + broker channels, persistent store. `bash scripts/run-e2e-gate.sh microvm` |
| `core` gliner (`gliner_relex_e2e`, `entity_extraction_e2e`, `memory_entity_link_e2e`) | 5 / 16 / 6 | the real model under the real sandbox; the latter two only with `KASTELLAN_GLINER_RELEX_ENABLE=1`. `bash scripts/run-e2e-gate.sh gliner` |
| `core` (`shell_exec_e2e`, `python_exec_e2e`, `python_exec_container_e2e`) | 4 / 4 / 4 | **real** core→sandbox→worker round-trips; per-spawn scratch; secret-scrub |
| `core` (`egress_proxy_e2e`, `egress_force_routing_e2e`, `email_mitm_e2e`) | 3 / 4 / 2 | real sidecar + CONNECT; Linux no-direct-route; hermetic MITM |
| `core` (`injection_guard_e2e`, `secret_vault_e2e`, `guard_boot_row_e2e`) | 10 / 9 / 1 | **PG-required** policy rows, privacy invariant, fail-closed redemption |
| worker-report (5 suites, 2 crates) | 13 | a dying worker's last words reach a failing test; a real broken fd 2, re-exec'd. `bash scripts/run-e2e-gate.sh worker-report` (both hosts) |
| `core` `mail_live_shape_e2e` (`#[ignore]`) | 1 | our reading of localmail against the **live** service. `bash scripts/mail/live-shape-gate.sh` (both hosts) |
| `tests-common` `gate_script_tests` | 27 | the gate script's table **and** its verdict (floors, caps, per-binary hook check), run against a fake `cargo` |

## Key design decisions locked in

**Not restated here — they drift.** Hard constraints: [`CLAUDE.md`](../../../CLAUDE.md) § Hard
constraints; the rest: [`docs/architecture.md`](../../architecture.md) and the ROADMAP.
**The one worth repeating:** worst-case compromise reaches *at most* the agent's own OS user, its own
Postgres role, its own scratch FS, and the allowlisted endpoints for the *one* compromised tool
([`docs/threat-model.md`](../../threat-model.md)).

## Recently merged

Newest first; full prose in the [`archive/`](archive/) snapshots and git history.

- **[#804](https://github.com/hherb/kastellan/pull/804)** — `Cargo.lock` only: async-trait 0.1.92 + thiserror 2.0.21, for clippy 1.99's `double_must_use` / `redundant_field_names` on macro output. **[#803](https://github.com/hherb/kastellan/pull/803)** — docs only: the TencentDB Agent Memory survey (`docs/devel/notes/2026-10-01-tencentdb-agent-memory-survey.md`).
- **[#801](https://github.com/hherb/kastellan/pull/801)** (#796–#800) — shutdown names pending rows; a stuck Matrix driver is a loss; refused-row reports thinned; typed `AuditLostWriter`.
- **[#795](https://github.com/hherb/kastellan/pull/795)** (#792, #793) — `[audit-lost]` marker and shutdown drain.
- **[#791](https://github.com/hherb/kastellan/pull/791)** (#788, #789, #790) — recovery lines marked, audit sinks spawn (bounded), a body-less reply view. Filed #792, #793.
- **[#787](https://github.com/hherb/kastellan/pull/787)** (#782, #783) — replies queue per conversation; a refused one is given up and audited; `[worker-refusal]` marker. Filed #788, #789, #790.
- **[#786](https://github.com/hherb/kastellan/pull/786)** (#785) — the four recall lanes stop returning L0 and L3 rows.
- **[#784](https://github.com/hherb/kastellan/pull/784)** — docs only: the cognee / cognee-rs survey; its own-tree finding filed as #785.
- **[#781](https://github.com/hherb/kastellan/pull/781)** (#769) — a live worker's `RpcError` keeps the worker; the polled driver backs off per method. Filed #782, #783.
- **[#778](https://github.com/hherb/kastellan/pull/778)** (#767, #768) — route spellings from `localmail_contract.rs`; one hash-case rule. Filed #779, #780.
- **[#776](https://github.com/hherb/kastellan/pull/776)** (#773, #774) — `KASTELLAN_LLM_THINKING_SWITCH`; `llm_usage` per plan; timeouts spend the synthesis turn. **[#775](https://github.com/hherb/kastellan/pull/775)** — handover only.
- **[#770](https://github.com/hherb/kastellan/pull/770)** (#673, #674) — `UPSTREAM_AUTH_FAILED` for localmail 401/403; an operator ERROR. Filed #769.
- **[#766](https://github.com/hherb/kastellan/pull/766)** (#763, #765) — live shape gate: own suite, knob, `mail-live` profile,
  shared `localmail_contract.rs`; mail params parsed (pure `handler/request.rs`) before the gate.
- **#764, #762, #761, #758, #748, #750, #745, #743, #740, #735, #731, #728, #726, #720, #727, #717, #709, #708, #702, #694,
  #692, #688, #685** and earlier — see git history and the [`archive/`](archive/) snapshots.

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
