# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20261013_836_pre-prune.md`](archive/handover_20261013_836_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here.
> ⚠️ **Repoint this line in the same commit as the snapshot.** It has been stale twice.

**Last updated:** 2026-10-14 (#836 — an undecodable poll batch: a `channel.inbound_undecodable`
row on Matrix, ERROR, and a poll backoff on every channel; PR
[#851](https://github.com/hherb/kastellan/pull/851), Mac sweep green, two review rounds applied —
the second a five-agent review, follow-ups [#852](https://github.com/hherb/kastellan/issues/852) and
[#853](https://github.com/hherb/kastellan/issues/853). PR
#850 (#835) merged 2026-10-13. The operator is still running the #773 live re-measure) ·
**Recent PRs, newest first:** [#851](https://github.com/hherb/kastellan/pull/851) (#836), [#850](https://github.com/hherb/kastellan/pull/850) (#835, merged), [#849](https://github.com/hherb/kastellan/pull/849) (#846, merged), [#847](https://github.com/hherb/kastellan/pull/847) (#832, merged), [#842](https://github.com/hherb/kastellan/pull/842) (#825, merged), [#834](https://github.com/hherb/kastellan/pull/834) (#827, #826), [#830](https://github.com/hherb/kastellan/pull/830) (#813, #828), [#824](https://github.com/hherb/kastellan/pull/824) (#815, #814), [#820](https://github.com/hherb/kastellan/pull/820) (#818), [#819](https://github.com/hherb/kastellan/pull/819) (#816), [#812](https://github.com/hherb/kastellan/pull/812) (#807, #808), [#806](https://github.com/hherb/kastellan/pull/806) (#796–#800, #802), [#804](https://github.com/hherb/kastellan/pull/804) (clippy 1.99 lockfile bump), [#803](https://github.com/hherb/kastellan/pull/803) (TencentDB survey, docs), [#801](https://github.com/hherb/kastellan/pull/801) (#796–#800), [#795](https://github.com/hherb/kastellan/pull/795) (#792, #793).
Older PRs are in the [`archive/`](archive/) snapshots; **`gh issue list --state open` is the live
answer** and the only one worth trusting. ·
**Both hosts run PR #851's tree** (deployed 2026-10-14). **DGX:** plain `scripts/upgrade_from_git.sh`
(exit 0), 15 binaries, generated env **and** `.local` byte-identical to `~/kastellan.env*.bak-pre851`,
live-matrix worker digest `94c86940…`, migrations 27/27, `NRestarts=0`, Matrix up. **Mac:** first
redeploy since 2026-08-17 (install recipe in [[mac-kastellan-install-recipe]]), env identical,
migrations 27/27. **localmail** on both hosts: its PR #397 (security floors, pypdf 6.20.1), same day.
The DGX boot ERROR `install dir is writable … group-writable (mode 0775)` is **pre-existing** (72 of
111 boots since 2026-07-31; `~/.local` is 0700) — not a deploy regression. The live process runs thinking **ON**
(`KASTELLAN_LLM_DISABLE_THINKING=0`, `THINKING_SWITCH=reasoning_effort`, `TIMEOUT_MS=600000`). The
last DGX full sweep (#770's deploy): **187/187 suites, 4729 / 0 / 79**, 0 `[WARN]`, 4 `[SKIP]`
(gliner opt-in). Rootfs images last rebuilt 2026-09-08.

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

### This session (2026-10-13): #836 — an undecodable poll batch (PR [#851](https://github.com/hherb/kastellan/pull/851))

- **`polled_driver/undecodable.rs`.** A poll result `parse_poll` cannot read is said at **ERROR** on
  `[worker-refusal]` (decode error in the line, **never in a row** — it can quote a peer). On a
  channel **without** an ack method (Matrix) the batch is lost: every one is said and handed to the
  new `DriverAudit::inbound_undecodable` hook → **`channel.inbound_undecodable`** row (`channel`,
  `observed_at`; **no count** — a separate action so `sum(dropped)` stays true). Email redelivers,
  so no row (`EmailDriverAudit` has no such field, on purpose).
- **Every channel now holds the poll** after an undecodable batch: its own `UndecodableRun` (a
  `RefusalRun` in a newtype, so `run`'s three runs cannot be swapped) on `spec.refusal_backoff`
  (1 s → 60 s). Before, email re-polled an unacked bad batch **at once, forever**. A redelivered
  batch is said on the run's cadence (first, then every 15 min — `REFUSAL_REPEAT`, which the line
  now builds its minutes from), **and at once** when the decode error's text changes (`error_code`,
  a hash) or when an outage ended since the last batch (`OutageLog::ended`, a counter — whichever
  call noticed, a send included, so a "back up" line is never the last word). Its line says it
  stalls inbound and counts "N in a row". The decode error is capped at 256 chars
  (`DECODE_ERROR_CAP_CHARS`). The first good decode says one INFO `Recovered` line (an empty batch
  counts).
- **`spawn` refuses an undecodable hook on an ack channel** (`check_undecodable_audit`, beside
  `check_drain`): it would never be called. ⚠️ The Matrix hold is a deliberate trade-off — it
  enlarges what a repeat failure loses (the worker buffers meanwhile) but bounds the row rate of a
  worker answering garbage at once; per-event decoding is #852. An undecodable `matrix.drain`
  answer is still a WARN with no row: #853.
- Daemon: `inbound_undecodable_audit_sink` in `main/inbound_dropped_sink.rs`, on Matrix's one
  writer/lease; `RowLabel::named` for a row with neither id nor count.

### Previous (2026-10-12/13): #835 — what the worker still held (PR #850, merged)

- `PolledWorkerSpec::drain` is called once on every driver exit (`break 'drive`); answers land at the
  `worker` stage of `channel.inbound_dropped`. ⚠️ matrix-sdk 0.18 **saves the sync token before
  running handlers**: `workers/matrix/src/sdk_live/sync_loop.rs` compares the stored token (behind
  two empty `save_changes`, a mutual-exclusion barrier — **narrowed, not closed**; re-check on an SDK
  upgrade) with the last fully handled `next_batch` → `interrupted`. A worker **down** at the last call
  is not asked. ⚠️ `sdk_live` compiles **only with `--features live-matrix`** — sweep and clippy it
  with the feature. **Owed:** a live `matrix_live_e2e` leg for `LiveSdk::drain` (DGX).

### Previous (2026-10-08 → 12): #825, #832, #846 (PRs #842, #847, #849, merged) — what still binds

- **#846:** `polled_driver/inbound_rx.rs::InboundReceiver` counts the bus's unread buffer on `Drop`
  (`stage = 'buffered'`; pre-#849 rows have no stage = `polled`). ⚠️ Email IS counted (acked at
  hand-over) and **over-counts on purpose** (a refused-ack tail; on a full buffer possibly one slot
  whose send is then refused). ⚠️ **Needs tokio ≥ 1.52.3** (workspace floor). `InboundDroppedAudit`
  is an `Arc`; the daemon builder is `core/src/main/inbound_dropped_sink.rs`.
- **#832:** routes queue a reply **unclaimed**; the pump claims at `send` (`bus_deliver::deliver`).
  ⚠️ The bus no longer writes `queue_closed`. `bus_guards.rs` (`InFlightGuard`, `PumpQueue`) say
  what a stop abandons. Open: #848.
- **#825:** migration 0027 (`tasks.reply_settled_at` + `reply_disposition`), one claim winner
  across both buses; sweep at start + every 5 min. ⚠️ **At-most-once**; a new real
  `CompletedTasks` must override `settled`. ⚠️ **The terminal-state list lives in four places**;
  widening it needs a NEW migration. Follow-ups #841, #843.
- **#827/#826 (PR #834):** `RejectedUnverifiable` (fail-closed); `ack_method: None` = "does not
  redeliver". `pg_events::audit_or_report` holds a **`PendingInsert` drop guard** — ⚠️ no `.await`
  before it. **NUL (#819/#820):** records escape to `␀`; ⚠️ `kastellan-db`'s next release must be
  **0.3.0**; `truncate_payload` must stay IDEMPOTENT. Open: #821–#823, #829, #831, #833, #837–#840; #836
  in PR #851, its follow-ups #852, #853.
- ⚠️ **A Mac sweep without `KASTELLAN_PG_BIN_DIR` in its command line is NOT evidence** (372
  `[SKIP]`, exit 0) [[postgres-app-bin-paths]]; a sweep without `-- --nocapture` under-counts
  `[SKIP]`. ⚠️ **`git push` over SSH fails here** — HTTPS via `gh` [[git-push-ssh-no-identities-use-gh-https]].

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

- **Version gate:** search, attachments and header reads refuse below localmail API **1.3** — **no
  fallback**. Params parse into a typed `Request` **before** the gate (#765). Wire facts live in
  `workers/mail/src/localmail_contract.rs` — ⚠️ **`include!`d three times: `pub const`s only, no
  `use`, no `//!`.**
- ⚠️ A 50-hit search page is still ~18 KB, over the 16 KiB step view. Attachments are fetched **by
  position** and **hashed against the listed sha**; `next_offset` is **copied, never computed**.
  ⚠️ `sort::is_textless` cannot see an operator-only query (#698).
- **Live gate:** `bash scripts/mail/live-shape-gate.sh` (`mail-live` profile). ⚠️ **A new
  `*_or_skip` helper must be classified** in `microvm/guard.rs` or the roster test refuses it.

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

**#782/#783 (PR #787), #785 (PR #786):** **Replies queue per conversation** (`ReplyQueues`, cap 256); given up after ≥ 1 h **and** ≥ 3 refusals.
⚠️ A channel-wide failure RESTARTS every give-up clock. ⚠️ **The Matrix worker's 401 is
`UPSTREAM_UNAVAILABLE`, NOT `UPSTREAM_AUTH_FAILED`.** ⚠️ Never test a driver line through a scoped
`tracing` subscriber [[tracing-scoped-subscriber-interest-cache-flake]]. **#785:** recall excludes L0/L3.

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
> (never for daemon e2es or the gate sweep), or kill the IDE's cargo child — it re-spawns on **every
> save**, so `pkill -f "cargo check --workspace --message-format"` before each run (2026-10-07).

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
4. Memory notes (auto-loaded) and [`archive/`](archive/) — the full prose for everything summarised here

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

5. **First DGX deploy of #791 + #795 + #801 + #806 + #812 + #819 + #820 + #824 + #830 + #834 + #842 + #847 + #849 + #850 + #851:** watch one restart for the `[audit-lost]` /
   INFO drain lines (since #830 the traced line carries `[audit-lost]` too; a Matrix login still in
   progress at shutdown should be INFO "not finished starting", not a loss; a bus stopped under a
   wedged Postgres now says `may not have been written`), and query an `observed_at` on the next `channel.*` row. Then
   [#805](https://github.com/hherb/kastellan/issues/805) (the runtime drop can outlive the last
   line) and #809–#811. A deploy of #812 is the first time a thinned line's
   `refused row N of this burst` and a `bus` writer line can be seen live. The #820 deploy also
   adds a **boot-time `server_encoding` check** — the DGX cluster was made by `kastellan-db-init`
   (UTF8), but confirm the daemon comes up. A restart mid-poll can now leave a `channel.inbound_dropped`
   row, and a Postgres outage `channel.rejected_unverifiable` rows (#834). ⚠️ **#825's deploy runs
   migration 0027**, which backfills every finished channel task as `backfilled` — check the daemon
   comes up, then query `select payload->>'via', count(*) from audit_log where action =
   'channel.replied' group by 1` after a restart. The #832 deploy: a restart should show only
   INFO `channel pump ended with replies queued` and no `queue_closed` rows. The #849 deploy: a
   restart under inbound traffic can leave `channel.inbound_dropped` rows with `stage = 'buffered'`
   — on email too now (`select payload->>'channel', payload->>'stage', payload->>'dropped' from
   audit_log where action = 'channel.inbound_dropped' order by id desc`). The #835 deploy needs the
   Matrix worker **and `matrix.ext4` rebuilt** (`bash scripts/build-release.sh`, then the rootfs
   script): a stale one answers `matrix.drain` with METHOD_NOT_FOUND and every stop WARNs "could not
   ask the worker"; a restart under traffic can leave `stage = 'worker'` rows. The #851 deploy: a
   `channel.inbound_undecodable` row (or an ERROR `did not decode` line) is a **finding about the
   worker**, none expected. Natural next security items: #810,
   #833 (stderr panic in a `Drop`; needs a design call, now also `bus_guards` and `InboundReceiver`),
   #829, #831 and #817.

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
`core/src/main.rs`, `tests-common/src/microvm/{mod,container}.rs`, `sandbox/tests/macos_smoke.rs`,
`worker_stderr/report/tool_worker.rs` 710, `core/src/channel/mod.rs` **564** (+10 in #849, +6 in #835, +16 in #836, docs — the `actions` consts are the natural cut).
Also over (grew in #818 without a split): `db/src/tasks.rs` **819** (+11 in #825: `CRASHED_DETAIL`),
`db/src/graph.rs` **~935**, `core/src/entity_extraction/batch_upsert.rs` **~520**.
Also over: `core/src/memory/l3_surface.rs` 539 (+5 in #785, a doc paragraph), `core/src/scheduler/inner_loop.rs` 906,
`tool_dispatch.rs` 722, `attach/tests.rs` 727, `require.rs` 661,
`scripts/run-e2e-gate.sh` 561 (shell). ⚠️ **`core/tests/mail_live_shape_e2e.rs` 559 — split it before
its next leg** (the attachment half is the natural cut). Recent splits done **first** (the pattern to
keep): #750 `worker_stderr/`, #769 `persistent.rs`, #767 `attach.rs`, #785 `memories/search.rs`,
#835 `workers/matrix/src/sdk_live.rs` (→ `sdk_live/{mod,login,room_gate,tests}.rs`, proved byte-identical; then `sync_loop.rs`), #849 `channel/email/mod.rs` (→ `email/tests.rs`, proved byte-identical), #824/#825 `channel/bus.rs` (→ `bus_inbound.rs`, `bus_outbound.rs`), #782 `report/delivery.rs` and `polled_driver/tests.rs`, #818 `audit/nul_escape.rs` → `db/src/nul.rs`, #816 `db/src/audit.rs` (→ `audit/truncate.rs`; ⚠️
`audit/truncate/tests.rs` is still 833), #788 `channel/mod.rs` (→ `undelivered.rs`) and
`polled_driver.rs` (→ `polled_driver/audit.rs`), #792 `main/audit_sink.rs` (→ `audit_sink_tests.rs`,
`audit_sink_test_support.rs`), #806 `email_boot.rs`/`matrix_boot.rs` (→ `*_tests.rs`, proved byte-identical). ⚠️ **Near the cap:** `worker_stderr/report/delivery/tests.rs` **499** (one under), `polled_driver.rs`
**485** (+26 in #836), `polled_driver/replies.rs` **478** — split before the next change grows them. #826 split `polled_driver.rs` (→ `spec.rs`).
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
| **Mac** (#836 review round 2 — targeted, **not** a full sweep) | PR #851 (round-2 commit) | `cargo test -p kastellan-core --lib --bins`: lib **2497** / 0 / 1, bins 92 + 96, `TEST_EXIT=0`, `KASTELLAN_PG_BIN_DIR` set, `[SKIP]` 0; `--test email_channel_e2e --test matrix_channel_e2e --test email_mitm_e2e -- --nocapture`: 8 + 2 + 3, `[SKIP]` 0. **+10** core lib tests (`polled_driver::tests::undecodable` 9 → 19), so a full sweep should read **5042 / 0 / 52** (predicted, not run — only `kastellan-core` changed). Mutants killed **9/9** (re-arm removed, sleep instead of gate, Matrix on cadence, constant error code, `decoded()` on a refused poll, spawn check removed, hook untimed, no error cap, outage counter stuck) | exit 0, `CARGO_TARGET_DIR=$HOME/.cargo-clippy-836-cold`, **27** `Checking kastellan` | 0 (targeted) |
| **Mac** (#836 — **the full sweep that stands**) | PR #851 (`7270a1d5`; the review commit is doc-only, core clippy re-run) | **5032 / 0 / 52**, **195** suites, `TEST_EXIT=0`, `KASTELLAN_PG_BIN_DIR` set, `--test-threads=4`, exactly as predicted: #835's 5008 + its review round's 11 + **13** (core lib `polled_driver::tests::undecodable` 9; daemon bin 4: `inbound_dropped_sink` 3, `matrix_boot` 1). `[WARN]` 0, `[SKIP]` **23**; log `~/sweep-836.log`. Mutants killed 7/7 (poll gate removed, `decoded()` never called, record polarity inverted, cadence ignored, WARN for ERROR, bus-closed path silent, sink always claimed) | exit 0, `CARGO_TARGET_DIR=$HOME/.cargo-clippy-836`, **27** `Checking kastellan` | **23** Mac |
| **Mac** (#835 — superseded) | #850 (`6e63beac`) | **5008 / 0 / 52**, **195** suites, `[SKIP]` 23; log `~/sweep-835.log`. Its review round 2 added 11 (predicted 5019, confirmed by #836's reconcile) | exit 0, 27 `Checking kastellan`; `--features live-matrix` exit 0 | **23** Mac |

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

⚠️ **Every Mac sweep: `export KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"`
in the same command line**, then `grep -c '^\[SKIP\]'` against the baseline (23) — a three-digit
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

- **[#850](https://github.com/hherb/kastellan/pull/850)** (#835, merged 2026-10-13) — what the Matrix worker still held at a stop is counted (`stage = 'worker'`).
- **[#849](https://github.com/hherb/kastellan/pull/849)** (#846, merged 2026-10-12) — a stopping bus's unread inbound buffer is counted (`stage = 'buffered'`), on both channels.
- **[#847](https://github.com/hherb/kastellan/pull/847)** (#832, merged 2026-10-12) — a bus stop loses nothing silently: the reply claim at the send, drop guards for what a stop abandons.
- **[#842](https://github.com/hherb/kastellan/pull/842)** (#825, merged 2026-10-11) — the reply catch-up: a claim column + a sweep at start and every 5 min; late replies say so.
- **#834, #830, #824, #820, #819, #812, #806, #804, #803, #801, #795, #791, #787, #786, #784, #781, #778, #776, #775, #770, #766** — audit-sink close-out; clippy 1.99 lockfile; TencentDB survey; shutdown names pending rows; `[audit-lost]` + drain; recovery lines, bounded sinks; reply queues + `[worker-refusal]`; recall excludes L0/L3; cognee survey; a refusal is not a death; route spellings; the thinking switch; `UPSTREAM_AUTH_FAILED`; the live mail shape gate. One-liners in the `802`/`815`/`825` archive snapshots (#830: a stopped insert says `may not have been written`; #824: channel drops leave rows; #820: NUL refused/escaped beyond `audit_log`; #819: NUL escaped in audit rows; #812: per-channel thinning + bus `[audit-lost]`).
- **#764 … #685** and earlier — see git history and the [`archive/`](archive/) snapshots.

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
