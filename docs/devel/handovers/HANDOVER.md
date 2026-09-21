# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20260921_730_pre-prune.md`](archive/handover_20260921_730_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here.

**Last updated:** 2026-09-21 (#736: an `anyio` security floor in the gliner worker) ·
**Recent PRs, newest first:** this session's #736 PR (the `anyio` floor), [#735](https://github.com/hherb/kastellan/pull/735) (#730, + a movement-only `worker_stderr` split), [#731](https://github.com/hherb/kastellan/pull/731) (#725), [#728](https://github.com/hherb/kastellan/pull/728) (#699, #700), [#727](https://github.com/hherb/kastellan/pull/727) (#677/#560 live acceptance, docs), [#726](https://github.com/hherb/kastellan/pull/726) (#719, the gliner tier's two macOS import-time deaths + a gate
script that could not pass), [#720](https://github.com/hherb/kastellan/pull/720) (the REQUIRE-knob
contract: #714, #622, #664), [#717](https://github.com/hherb/kastellan/pull/717) (backlog triage),
[#709](https://github.com/hherb/kastellan/pull/709) (#701, conversational continuity),
[#702](https://github.com/hherb/kastellan/pull/702) (#677, the planner's labelled result view),
[#694](https://github.com/hherb/kastellan/pull/694) (#617). **Open issues these filed:**
[#737](https://github.com/hherb/kastellan/issues/737)–[#739](https://github.com/hherb/kastellan/issues/739) (from #735's review round);
[#732](https://github.com/hherb/kastellan/issues/732)–[#734](https://github.com/hherb/kastellan/issues/734) (from #731; #730 closed by this session);
[#718](https://github.com/hherb/kastellan/issues/718), [#721](https://github.com/hherb/kastellan/issues/721)–[#724](https://github.com/hherb/kastellan/issues/724) (from #720);
[#710](https://github.com/hherb/kastellan/issues/710)–[#713](https://github.com/hherb/kastellan/issues/713), [#715](https://github.com/hherb/kastellan/issues/715), [#716](https://github.com/hherb/kastellan/issues/716) (from #709);
[#698](https://github.com/hherb/kastellan/issues/698)–[#700](https://github.com/hherb/kastellan/issues/700),
[#703](https://github.com/hherb/kastellan/issues/703)–[#705](https://github.com/hherb/kastellan/issues/705) (from #702); [#693](https://github.com/hherb/kastellan/issues/693),
[#695](https://github.com/hherb/kastellan/issues/695)–[#697](https://github.com/hherb/kastellan/issues/697) (from #694);
[#691](https://github.com/hherb/kastellan/issues/691) (from #692). ·
**The DGX runs `main` as of #709**, redeployed 2026-09-17 via `scripts/upgrade_from_git.sh` and
verified (installed binaries byte-identical, units active, migration 0026's `tasks.turn_record`
present). **No redeploy is owed for anything merged since** — #720/#726/#728/#731 and #730 touch
only test-harness surface, docs, or a worker's startup-failure path; #730's daemon-visible deltas are
that a persistent worker's death line is now control-neutralised, carries its label inline, and —
since the review round — **waits for the stderr drain**, so it carries the dead worker's own words
instead of `no stderr captured`. Rootfs images last rebuilt 2026-09-08.

> **Header convention (since 2026-09-11, after three recurrences).** This header names **PRs and
> issues only — never a branch name, a HEAD sha, or the word OPEN.** A merge falsifies those with no
> actor in between; a PR number it cannot. **The tip and the open set are one command each:**
> `git log --oneline -1 origin/main` and `gh pr list --state open`. Run them before trusting a word
> of this file.

> ⚠️ **An issue's census can be wrong, and so can its diagnosis — read the rows, not the issue.**
> #679 named 7 call sites (12), #690 named 4 subprocesses (10), #677 misread a schema rejection as a
> duplicate search, and **#719 named 3 failing tests when 7 were broken** — the other 4 skip unless
> `KASTELLAN_GLINER_RELEX_ENABLE=1`. [[issue-as-filed-can-carry-a-regression]]

> ⚠️ **A fix for a reviewer's finding can carry the next defect.** #677's approved spec capped
> strings at 512 B, breaking the question that worked [[plan-text-is-a-defect-source]]; #720's review
> closed a fail-*open* hole with a check that made it fail *always* (fixed in #726). **A passing
> mutation proof is not a review either** [[mutation-proof-counts-only-mutants-you-tried]].

> ⚠️ **A forward reference auto-closed issue #718.** #720's body said the gate profile is the acceptance
> test for "whichever PR <closing-keyword> #718", and GitHub's scanner matched it — the fifth
> recurrence of this hazard and a new shape (no negation at all). Never quote the phrase literally. Reopened 2026-09-19. **Run the regex over every PR body and
> commit message before merging**, not only over the negations [[pr-body-not-fixed-autocloses-issue]].

> ⚠️ **A FIXTURE nobody rebuilds is a gate nobody runs** [[stale-fixture-turns-a-gate-into-a-formality]],
> and **a guard built from a census shares the census's blind spot** [[guard-shares-the-census-blind-spot]].

---

## Current state

### This session (2026-09-21, second): #736 — a security floor for `anyio`, not just a lock bump

Dependabot raised two advisories against `anyio 4.13.0` in
`workers/gliner-relex/uv.lock`. Both are patched in **4.14.2** and both list `< 4.14.2` as the
vulnerable range, so **one floor closes both**: **GHSA-82r6-8w77-94w6 (critical)** — `TLSStream`
encodes host names with IDNA 2003, which can map a hostile name onto a different one and so enable
TLS certificate spoofing — and GHSA-5p39-cfhj-2xmp (medium), an anyio process-pool worker blocking
indefinitely on undrained stderr. The fix is **one dependency line in `pyproject.toml`** plus the
re-lock: `anyio>=4.14.2`, then `uv lock` moved 4.13.0 → **4.15.1**. `anyio` and `typing-extensions`
(4.15.0 → 4.16.0) are the **entire** lock diff — 9 insertions, 7 deletions, two version lines.

- ⚠️ **The floor belongs in `pyproject.toml`, not only in `uv.lock`** — the lesson
  `transformers>=5.10.0` already carries three lines above it, where a lock-only bump landed on a
  still-vulnerable 5.6.2 while exiting 0 [[uv-lock-upgrade-can-land-still-vulnerable]]. **Proved,
  not asserted:** tampering the floor to `anyio>=4.99.0` makes `uv lock --check --offline` exit
  **1** with *"your project's requirements are unsatisfiable"*. That hard conflict **is** the
  deliverable; the version bump alone is a silent downgrade away from being undone. CI job
  `uv lock --check (gliner-relex)` is what enforces it on every push.
- ⚠️ **A positive control can fire for the wrong reason, and then it proves nothing.** The first one
  hand-edited a version inside `uv.lock` and got a satisfying exit **2** — from *"malformed wheel"*
  filename-consistency parsing, **not** from the floor. It showed `--check` is non-vacuous and
  nothing more. The control that means something tampers the **pyproject floor**, because the floor
  is the claim [[unreachable-success-path-proves-nothing]].
- **Reachability — answered, not deferred** (the issue asked for it): the gliner worker is
  **`Net::Deny` in BOTH entries** — host mode (`core/src/workers/gliner_relex/entry.rs:195`) and
  macOS container mode (`:269`) — with `HF_HUB_OFFLINE=1` + `TRANSFORMERS_OFFLINE=1` from the
  **shared** `build_runtime_env`, which both call. No TLS is spoken from inside the jail at all, so
  the exposure was **provisioning-time only**: the one-off model download, `huggingface_hub` →
  `httpx` → `anyio` (chain read from `uv tree --invert`, not assumed). `uv sync` is not a path to it
  — uv is Rust and never loads this `anyio`. That lowers the urgency; it does not make the floor
  optional, because provisioning is exactly when the weights arrive.
- ⚠️ **The recorded pytest count was wrong in three places at once.** Issue #736 says 71, this file's
  Build & test line said 70, and so did #726's (now-archived) row — the tree collects **81**, and
  `workers/gliner-relex/tests/` is **byte-identical to `577e2196`** (#726 itself), so it was 81 then
  too. The arithmetic: 66 `def test_` − the 3 parametrised ones + their 18 **static literal** cases
  = 81. Collection depends on no env var and no host, so **81 is a fixed number everywhere**.
  Corrected below and in the ROADMAP.
- **Zero Rust changed, and no Rust file reads either changed file** (grepped; only two *doc comments*
  name `pyproject`), so clippy is unchanged from #735's verified-clean tip and was not re-run.

### Previous (2026-09-20/21): #725 + #730 — a dying worker's last words reach a failing test

PRs [#731](https://github.com/hherb/kastellan/pull/731) (tool-worker early exit) and
[#735](https://github.com/hherb/kastellan/pull/735) (the **persistent** path, which runs the
**Matrix** and **email** channel workers). Full prose in the ROADMAP entries and
[`archive/handover_20260921_730_pre-prune.md`](archive/handover_20260921_730_pre-prune.md).
`emit_early_exit_report` and `emit_persistent_death_report` are the two producers; each logs through
`tracing` as before **and** `eprintln!`s the report when `has_been_set()` is false, through one
shared private renderer. Preceded by a movement-only split of the 697-line `worker_stderr.rs` into
`worker_stderr/{mod,report}.rs`, proven by byte-identity of every moved region, not by reading the diff.

- ⚠️ **`eprintln!` is load-bearing, not style** [[libtest-capture-only-print-macros]], and a
  *captured* one comes back on the child's **stdout** — a suite that merges the child's two streams
  cannot test its own claim. **Read them separately.**
- ⚠️ **A fallback marker must not begin with `[SKIP]`/`[WARN]`/`[E2E]`** — `run-e2e-gate.sh` greps
  those anchored at line start and asserts zero `[WARN]`, and every profile passes `--nocapture`.
  Hence `[worker-early-exit]` and `[worker-death]`: **two markers, one renderer**, so the
  neutralisation and the `has_been_set()` guard cannot drift (the bwrap-argv shape).
- ⚠️ **"Production" is not "the daemon".** `kastellan-cli` installs no subscriber anywhere and
  `guard capture` dispatches a real worker, so that shipped binary gains the line (intended).
- ⚠️ **Across the two PRs, four review rounds each found the suite unable to prove its own central
  claim:** merged streams; a `neutralise_controls` deletion that passed because the fixture's input
  was a clean literal; an `if let Some(r)` **`None` arm no fixture ever executed**; and a report that
  would usually have rendered `no stderr captured`, never having waited for the drain — the
  contentless line #730 exists to avoid [[mutation-proof-counts-only-mutants-you-tried]]
  [[unreachable-success-path-proves-nothing]].
- ⚠️ **`shutdown()` joins the driver thread, and that join is the only proof the report was
  emitted** — the driver answers the in-flight caller *first*, then reports, so a fixture asserting
  on `h.call(…)` returning is a race that passes on an idle machine.
- **Filed, all still open:** [#737](https://github.com/hherb/kastellan/issues/737) (**five of the six**
  `ClientError` variants `dispatch_indicates_worker_dead` calls dead are reported *nowhere*, daemon
  included — wider than #730), [#738](https://github.com/hherb/kastellan/issues/738) (respawn-failure
  and rate-alarm lines still `tracing`-only, so a worker that **cannot come back** loops in silence),
  [#739](https://github.com/hherb/kastellan/issues/739) (an `eprintln!` EPIPE panic on the **driver
  thread** permanently kills a channel, both joins swallow it),
  [#732](https://github.com/hherb/kastellan/issues/732), [#733](https://github.com/hherb/kastellan/issues/733),
  [#734](https://github.com/hherb/kastellan/issues/734) (**`has_been_set()` asks whether a subscriber
  exists, not whether the WARN will be delivered**).

### Previous (2026-09-19): #699 + #700, #677/#560, #719

Condensed at the #735 review; full text in
[`archive/handover_20260921_730_pre-prune.md`](archive/handover_20260921_730_pre-prune.md).

**#699 + #700** (PR [#728](https://github.com/hherb/kastellan/pull/728)) — each `plans_so_far` step
carries a screened `"call": {tool, method, parameters}`; calls' bytes come off
`PLANS_SUMMARY_BUDGET` first, then the **oldest calls' `parameters`** drop. ⚠️ Review found a real
fail-open: a call's deepest `parameters` level was rendered but never screened, out of reach only
because serde_json's recursion limit sits below `MAX_WALK_DEPTH` — **the invariant rested on an
unrelated parser's constant**. ⚠️ **A hardening that rewrites screened text must ADD readings, never
replace them** [[screen-hardening-must-add-readings]]. Deferred:
[#729](https://github.com/hherb/kastellan/issues/729). ⚠️ **Not yet measured live** — re-run a
multi-search mail question in a **fresh DM room**; task 186's dropped `has_attachment` is the shape.

**#677 / #560 closed by live measurement, no code change** — every figure from `audit_log` rows, not
the replies. ⚠️ **A live re-measure of a single-question issue needs a fresh DM room**: since #709 a
same-room question inherits the prior turns' calls, which voids the test.

**#719** (PR [#726](https://github.com/hherb/kastellan/pull/726)) — both causes inside `import
torch`: `getcwd()` → EPERM under Seatbelt (workers now start in `/`), and torch's compile cache at
import (host-mode gliner opts into `ephemeral_scratch`); the second would have broken production
macOS host-mode too. ⚠️ **A sandbox that restricts but does not relocate leaks the caller's context
into the jail** — when a worker dies at startup on one OS only, diff what the backends do
*implicitly* (cwd, `/tmp`, `$HOME`), not what the policy grants. ⚠️ **gliner is the first WARM worker
on `ephemeral_scratch`.** ⚠️ **After mutating a `.py`, delete its `__pycache__`**
[[mutation-testing-leaves-stale-pyc]].

### Previous: #720 / #717 / #709 — the REQUIRE contract, the triage, and conversational continuity

**#720** (closed #714, #622, #664). What binds:

> **Every gate needs a REQUIRE knob *and* a positive control that fails when zero tests ran.**

`tests_common::require::RequireKnob` is the one vocabulary, and the knob is **data**, so a tier is
one `const`. ⚠️ **A knob alone was never enough (#664):** it fires only inside a test body, so a
filtered-out run emits no `[SKIP]` and exits 0 — hence `[E2E]` markers and per-tier floors in
`scripts/run-e2e-gate.sh`. **Use it whenever a run is meant to be evidence.** ⚠️ **No `sandbox` or
`container` profile yet — both would be red on every host** (#718: **92 sites in 47 files** bypass
`skip_line`; #722: the container helpers have no knob).

**#717.** ⚠️ **The ROADMAP is the accurate source; GitHub issues are the stale mirror.** Every open
issue now carries one `area:*` label; `main` has required status checks (#655).

**#709 (#701).** A finishing channel task writes `tasks.turn_record` = `{calls, data_class}` in the
*same* `finalize` UPDATE; the next task in the same `(channel, peer, conversation)` reads up to 3,
screened and budgeted, and inherits their floor. **Calls, not results** — no tool output crosses a
task boundary. ⚠️ **The window has NO upper bound, deliberately.** ⚠️ **The floor comes from every
turn LOADED, not those the screen kept.** ⚠️ **A security property can be documented, tested, and
absent** — `inherit_floor` read the wrong field while three one-leg tests defended it. **Email is
still stateless.** Deferred: #710–#713, #715, #716.

**#702 (#677).** `inner_loop/result_view` is the planner's view of a successful step — pruned,
labelled JSON. ⚠️ **Keys never reach the guard model** (#703). Budgets: 16 KiB per step, 96 KiB
accumulated.
### Merged arcs — only what still binds

**#694 (#617).** `truncate_payload` derives `req_summary = {head, sha256, len}` centrally; the digest
covers the **whole** request. ⚠️ **No sink double can test it** [[audit-sink-doubles-hide-storage-transforms]].
⚠️ **Review subagents mutate the working tree** — give each its own `git worktree`.

**#692 / #688 / #685 / #683 / #680 — micro-VM budgets and freshness.** `kastellan_sandbox::bounded_command`
for any host probe; ⚠️ **a bounded runner must not join its drains** [[bounded-subprocess-must-not-join-drains]].
Both micro-VM guest kernels lack Landlock (seccomp-only by design). One producer for `target/release/`:
`bash scripts/build-release.sh`, run LAST [[cargo-package-selection-changes-binary-bytes]]; the freshness
reference is the **sha256 of the baked copy** [[cargo-relinks-identical-mtime-not-content]].

**#660 — the second pre-release security audit.** Three **fail-closed** lockdown rules bite careless
fixtures (missing `KASTELLAN_SECCOMP_PROFILE`, an unenforceable Landlock ruleset, a corrupt guest env
token). Per-spawn dirs via `create_private_dir` — **do not "fix" back to `create_dir_all`**.
**Before release: flip force-routing on.** Deferred list in `docs/security-audit-2026-09-02.md`.
**One-liners.** #681: a lean tail plus recovery beat a fat verbatim tail (68.3 % vs 45.8 %). #675: a
failed micro-VM boot leaves `console.log` in the kept run dir [[microvm-guest-failures-are-invisible]];
the launcher has no env [[microvm-launcher-knobs-must-be-argv]]; release is `panic = "abort"`
[[release-profile-panic-abort-kills-raii]]. #669: count the producers, make the const the only spelling.

### The guard tier — what still binds

- **D10 — ADVISORY defence-in-depth, NOT a gate.** 65 % recall (36/55) at FP-0; **5/8 missed** on
  narrative framing. **Nothing downstream may relax on it.**
- **τ = 0.79552656 is a REQUIRED operator input with no default**; **five misconfigurations STOP THE
  DAEMON** (D6), incl. a context below `SCAN_BYTE_CAP + 512 = 66 048` (D8).
- **`best_tau` returns NONE** on real captured content, so **corpus growth from production is the
  cheap path**. **Absence and loss must not render identically.**
- ⚠️ **#624 and #626 do NOT close [#612](https://github.com/hherb/kastellan/issues/612)** — small-sample
  extrapolation is non-linear on Metal [[metal-prompt-processing-is-nonlinear]].

### Standing hazards that have each cost a session

> ⚠️ **Clippy: parity is a `rustup update`, and a cached run lies** [[local-clippy-not-ci-parity-rust-version]].
> Count the `Checking kastellan` lines (27). Force cold with `CARGO_TARGET_DIR=$HOME/.cargo-clippy-<topic>`,
> **never** by `touch`ing sources (falsifies the #687 image gate, #691). Sweep first, lint after.

> ⚠️ **A private `CARGO_TARGET_DIR` breaks daemon e2es** [[custom-cargo-target-dir-breaks-daemon-e2e]];
> **rust-analyzer's `cargo check` holds `target/debug/.cargo-lock`** (a blocked build prints `Blocking
> waiting for file lock` — kill the IDE's cargo child, recurred 2026-09-19).

> ⚠️ **Do NOT edit the tree while a sweep compiles in it — and give every reviewer its own worktree**
> [[never-edit-tree-during-a-sweep]] [[worktree-cwd-lands-on-main]]. **A fresh worktree has no worker
> binaries** — `cargo build --workspace` first [[worktree-has-no-worker-binaries]]. ⚠️ **Copying files
> into a DGX worktree with macOS `tar` creates `._*` AppleDouble files** — `sandbox/tests/._x.rs` is a
> cargo test target; delete them before building.

> ⚠️ **`syspolicyd` saturates and no new executable can start on the Mac.** Tell: `ps -o time`
> exactly `0:00.00` against minutes of ELAPSED; positive control: a freshly compiled 20-byte C
> program hangs. Fix: the operator runs `sudo killall syspolicyd`. **But a slow build is usually
> CONTENTION** — check `uptime` and `%cpu` first [[mac-fresh-large-binaries-hang-in-dyld]].

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

1. **The last #677 follow-up:** [#698](https://github.com/hherb/kastellan/issues/698) (`mail.search`
   cannot express a filter-only search; check what localmail `/v1/search` does with an empty query
   first, and note localmail ignores `has_attachment` today). #699/#700 shipped in PR [#728](https://github.com/hherb/kastellan/pull/728) —
   **its live re-measure is owed** (operator DMs, a **fresh room**).

2. **#702 follow-ups.** [#703](https://github.com/hherb/kastellan/issues/703) — ⚠️ **the guard model
   never sees object keys**; any new worker passing a third-party JSON object through reopens it
   silently (needs a DGX guard calibration run). [#705](https://github.com/hherb/kastellan/issues/705),
   [#704](https://github.com/hherb/kastellan/issues/704). The localmail changes the operator offered
   (2026-09-14: ordered headers, 4xx on cursor restart, filter-only search, compact hits, attachments
   by `message_id` + name, a distinct expired-credential error) are not yet filed on `hherb/localmail`.

3. **The worker-death arc's own follow-ups, while the context is warm.**
   [#737](https://github.com/hherb/kastellan/issues/737) is the widest — **five of the six**
   `ClientError` variants `dispatch_indicates_worker_dead` calls dead are reported *nowhere*, daemon
   included. Then [#738](https://github.com/hherb/kastellan/issues/738) (a worker that **cannot come
   back** loops in silence) and [#739](https://github.com/hherb/kastellan/issues/739) (an `eprintln!`
   EPIPE panic on the **driver thread** permanently kills a channel; extends
   [#733](https://github.com/hherb/kastellan/issues/733)).

4. **Test-harness honesty, now that the gate itself is tested.**
   [#718](https://github.com/hherb/kastellan/issues/718)
   (92 hand-rolled `[SKIP]`s — adding the `sandbox` profile is its acceptance test),
   [#722](https://github.com/hherb/kastellan/issues/722) (container tier knob + profile),
   [#721](https://github.com/hherb/kastellan/issues/721), [#723](https://github.com/hherb/kastellan/issues/723),
   [#724](https://github.com/hherb/kastellan/issues/724), [#691](https://github.com/hherb/kastellan/issues/691)
   (a decision), #237's absent macOS CI leg.

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
  radius, not teardown; restore the shared suffix with a `.suffix()` setter, not by reverting #641
  [[issue-as-filed-can-carry-a-regression]].
- **Mail credential expiry — [#673](https://github.com/hherb/kastellan/issues/673) + [#674](https://github.com/hherb/kastellan/issues/674):**
  an upstream 401/403 reads as `POLICY_DENIED`, and nothing notices the expiry.
- **Web workers — [#706](https://github.com/hherb/kastellan/issues/706) before any release** (no rate
  limiting, backoff, conditional requests or `robots.txt`); [#707](https://github.com/hherb/kastellan/issues/707)
  blocked upstream — ⚠️ **do not adopt Obscura before its V8 bump lands**; our jail would be its only layer.
- **Also open, no gotcha beyond the issue text:** #551, #519, #554 (needs a live DGX gate), #534.
- **Email channel — slices 2 and 3.** Spec `docs/superpowers/specs/2026-07-28-email-fallback-channel-design.md`.
  Slice 2 = SMTP outbound (`lettre`, MIT) + round trip; slice 3 = DGX deploy + live tier —
  **restart `localmail-serve` (+ `localmail-daemon`) on the DGX first**.
- **A Mac daemon deployment is a deliberate decision, not a task** (#612 means the guard fails open
  on large documents there). ⚠️ **Host-mode gliner on macOS now works** ([#726](https://github.com/hherb/kastellan/pull/726)); before it, it could not
  have started under launchd either — cause 2 was never test-only.
- **Live guard-host facts:** DGX `llama-server … Shieldstral-1.0-3B-Q8_0.gguf --alias shieldstral
  --port 8081 -c 131072 -ngl 99`; `/props` reports `default_generation_settings.n_ctx` (no top-level
  `n_ctx`). Restart with **at least `-c 66048`** or the daemon refuses to boot. Guard keys live in
  `~/.config/kastellan/kastellan.env.local`, which `install` never rewrites.
- **Deferred with a reason:** macOS Seatbelt-loopback verification of mail tier 1a; Telegram inbound;
  MITM-of-browser via an NSS trust-store import, **not** `--ignore-certificate-errors-*`.

**File-split backlog (Item 9b)** — **`wc -l` before picking; the numbers drift.** Split **before** the
change that grows a file, in a movement-only commit. ⚠️ **Prove the movement, don't assert it** —
#730's split checked that every moved region was **byte-identical** to its range on `main` and that
the `fn`-name set matched, which a diff cannot show on its own. Pure test-lifts:
`core/src/channel/ask_message.rs` 956, `workers/mail/src/handler.rs` 670,
`sandbox/src/linux_firecracker/plan.rs` ~1160 (DGX-gated), `core/tests/guard_tier_e2e.rs` 1558+
(#639), `core/src/workers/gliner_relex/tests.rs` 1177. Clean seam: `core/src/scheduler/asks.rs` 801.
Judgement first: `db/src/asks.rs` 1127, `db/graph.rs` 926, `llm-router/src/config.rs` 843. Also over
cap: `core/src/scheduler/inner_loop.rs`, `core/src/channel/bus.rs`, `workers/matrix/src/sdk_live.rs`,
`llm-router/src/messages.rs`, `core/src/main.rs`, `tests-common/src/microvm/{mod,container}.rs`,
`tests-common/src/require.rs` 635, `sandbox/tests/macos_smoke.rs` ~450.

**Standing deferrals (no owner):** egress #242, #251, #304, #260; micro-VM #381 and **true `jailer`**
(seam exists in `confine.rs`); python-exec Phase 4 curated wheels; web-research polish; an ANN index
on `entities.embedding`. Generalising net-worker-in-VM needs no new work — 5c's `NetClientTransport`
is the reusable mechanism.

---

## Load-bearing findings that still bind

- **The four faults (2026-08-02).** One Matrix message, four independent faults, each masking the
  next. **A green stack with a silent output means look at every layer, one fix at a time.**
- **Egress / MITM traps.** The MITM upstream trusts **webpki roots only**
  [[egress-proxy-upstream-trusts-webpki-only]]; a force-routed loopback needs an **IP SAN**
  [[macos-force-routed-loopback-needs-ip-san]]; a bare-host `Net::Allowlist` entry is an **all-port
  grant** [[bare-host-net-allowlist-is-all-port-grant]].
- **A sandbox that restricts but does not relocate leaks the caller's context into the jail** —
  cwd under Seatbelt (#719). When a worker dies at startup only on one OS, diff what the two backends
  do *implicitly* (cwd, `/tmp`, `$HOME`), not just what the policy grants.
- **Process lessons that have each cost a re-run.** A truncated gate log is not a gate
  [[truncated-gate-log-is-not-a-gate]]. Mutation testing contaminates the git **index**
  [[mutation-testing-contaminates-the-index]]; revert by copying, never `git checkout`
  [[mutation-revert-never-git-checkout]]. `sqlx::migrate!` embeds at compile time
  [[sqlx-migrate-embeds-at-compile-time]].

---

## Working state

### Test baseline (authoritative)

| Host | Commit | Result | clippy `-D warnings` | `[SKIP]` |
| --- | --- | --- | --- | --- |
| **Mac** (#736, the `anyio` floor — **the gate that stands**) | branch tip | **4388 / 0 / 33**, **181** suites, `TEST_EXIT=0`, 0 `[WARN]`, **`[SKIP]` 23**. ⚠️ **This is the first full sweep ever run on what #735 actually MERGED.** The row below was measured at `~10:32`, before #735's own **14:00** review round; per-suite diff shows exactly one suite moved — `kastellan_core` unit tests **2182 → 2184**, that round's two added tests — and every other suite, including all container suites, is byte-identical. This change touches **zero Rust**, so the predicted delta was zero and it reconciled exactly. **`[SKIP]` 12 → 23 is not drift:** all 23 are Apple `container` (19) or gliner opt-in (4), **zero** "no Postgres install found" in either run (`KASTELLAN_PG_BIN_DIR` set), and no suite's passed/failed/ignored moved — the extra 11 are additional diagnostic lines in suites whose counts are unchanged. ⚠️ **The new markers DO appear in a sweep log: 8 `[worker-death]` + 2 `[worker-early-exit]` at column 0**, where #735's row recorded zero — because before its review round `death_report()` returned `None` and emitted nothing. Harmless (the gate greps only `[SKIP]`/`[WARN]`/`[E2E]`) but now **measured** rather than assumed; 6 of the 8 are `email_mitm_e2e`, whose premise is a retrying, repeatedly-failing poll. Sources **sha256-verified unchanged across the sweep** (664 files) [[never-edit-tree-during-a-sweep]] | **not re-run** — zero Rust changed, so clippy is unchanged from #735's verified-clean tip (exit 0, 27 `Checking kastellan`) | **23** Mac (19 container, 4 gliner) |
| **Mac** ([#735](https://github.com/hherb/kastellan/pull/735) — ⚠️ **superseded: this is the PRE-review-round tree, not what merged**) | branch tip (`~10:32`, before the 14:00 review round) | **4386 / 0 / 33**, **181** suites, `TEST_EXIT=0`, 0 `[WARN]`, **`[SKIP]` 12**. +5 passed / +2 ignored over the row below, **predicted before the run and reconciled exactly**: 3 new `worker_stderr` unit tests (17 → 20, one of the 17 replaced by its generalised form) and the new hermetic e2e's 2 parents + 2 `#[ignore]`d fixtures; +1 suite is that file. `KASTELLAN_PG_BIN_DIR` set, so zero "no Postgres install found" — the false-green tell. Zero column-0 `[worker-death]`/`[worker-early-exit]` lines in the whole log, so the new marker does not leak into a gate. Sources **sha256-verified unchanged across the sweep** (663 files) [[never-edit-tree-during-a-sweep]]. DGX not re-run (no Linux-only code touched; the change is platform-neutral, so the DGX number is simply older) | exit 0, cold (**214** total `Checking` lines — a cached crate prints none — of which 27 `Checking kastellan`), zero warnings, `CARGO_TARGET_DIR=$HOME/.cargo-clippy-730` | **12** Mac |
| **Mac** ([#731](https://github.com/hherb/kastellan/pull/731), #725 — **post-`/fixall`, the gate that stands**) | branch tip (2nd review round) | **4381 / 0 / 31**, 180 suites, `TEST_EXIT=0`, 0 `[WARN]`, **`[SKIP]` 12**. +1 over the row below, reconciled exactly: the one new unit test (`the_stderr_fallback_neutralises_a_model_authored_control_character`); the round's other work added **assertions**, not tests, so `ignored` is unchanged at 31. ⚠️ **`[SKIP]` 23 → 12 is an improvement, not drift** — run with `KASTELLAN_PG_BIN_DIR` set, so **zero** "no Postgres install found" (the false-green tell); the 12 remaining are all legitimately opt-in or unavailable (Apple `container` ×8, gliner ×4). Skip-as-pass means those 11 newly-*running* tests move no count, which is exactly why the count alone was never the evidence. Sources **sha256-verified unchanged across the whole sweep** [[never-edit-tree-during-a-sweep]] — an earlier sweep was killed and restarted after two files were edited mid-run. DGX not re-run this round (no Linux-only code touched) | exit 0, cold (27 `Checking kastellan` lines, dedicated `CARGO_TARGET_DIR`), zero warnings | **12** Mac |
| **Mac + DGX** ([#731](https://github.com/hherb/kastellan/pull/731), #725 — 1st review round; superseded by the row above) | branch tip (post-review) | **Mac 4380 / 0 / 31**, 180 suites, `TEST_EXIT=0`, 0 `[WARN]`, **`[SKIP]` 23** (baseline parity). **DGX 4515 / 0 / 63**, 180 suites, `TEST_EXIT=0`, `[SKIP]` 4. Both = +5 passed / +2 ignored, reconciled exactly: Mac 4375+5; DGX 4501 + 4 (#728 round 1) + 5 (round 2) + 5. ⚠️ **The first Mac sweep was a false green** — it matched the predicted total while **339 of its 361 `[SKIP]`s were "no Postgres install found"**, because skip-as-pass counts as passed. Set `KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"` on the Mac or the sweep is not evidence | exit 0, cold (27 `Checking kastellan` lines, `CARGO_TARGET_DIR=$HOME/.cargo-clippy-725`), zero warnings | **23** Mac, **4** DGX (gliner opt-in) |

Older rows (incl. #726/#728 and the last DGX figures) are in the [`archive/`](archive/) snapshots.

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
**gliner Python tests:** `cd workers/gliner-relex && uv run --frozen pytest -q` — **81** tests (70/71 was recorded here, in issue #736 and in #726's now-archived row, and was wrong in all three; see the #736 note above).

### The tree — 27 crates

Full layout in the root [`README.md`](../../../README.md) § Layout; load-bearing crates in
[`CLAUDE.md`](../../../CLAUDE.md) § Project shape.

### Integration-suite map

Only the rows that tell you *where to look when something goes red*.

| Suite | Tests | What's verified |
| ----- | ----- | --------------- |
| `sandbox` (`linux_smoke` / `macos_smoke` / `macos_container_smoke`) | 10 / 13 / 10 | **real** jails: fs invisibility, net deny, relative-path reject, OOM-kill, per-spawn `/tmp`, fresh session leader, **worker starts in `/` on both backends** (#719), the Firecracker VMM jail launching (#671), the #689 Landlock drift detector |
| `core` Firecracker (14 suites, `#[ignore]`, DGX) | 29 | **real KVM** round-trips, mem cap, net deny, warm idle, VMM confinement, egress + broker channels, persistent store. `bash scripts/run-e2e-gate.sh microvm` |
| `core` gliner (`gliner_relex_e2e`, `entity_extraction_e2e`, `memory_entity_link_e2e`) | 5 / 16 / 6 | the real model under the real sandbox; the latter two only with `KASTELLAN_GLINER_RELEX_ENABLE=1`. `bash scripts/run-e2e-gate.sh gliner` |
| `core` (`shell_exec_e2e`, `python_exec_e2e`, `python_exec_container_e2e`) | 4 / 4 / 4 | **real** core→sandbox→worker round-trips; per-spawn scratch; secret-scrub |
| `core` (`egress_proxy_e2e`, `egress_force_routing_e2e`, `email_mitm_e2e`) | 3 / 4 / 2 | real sidecar + CONNECT; Linux no-direct-route; hermetic MITM |
| `core` (`injection_guard_e2e`, `secret_vault_e2e`, `guard_boot_row_e2e`) | 10 / 9 / 1 | **PG-required** policy rows, privacy invariant, fail-closed redemption |
| `tests-common` `gate_script_tests` | 8 | the gate script's table **and** its verdict, run against a fake `cargo` |

## Key design decisions locked in

**Not restated here — they drift.** Hard constraints: [`CLAUDE.md`](../../../CLAUDE.md) § Hard
constraints; the rest: [`docs/architecture.md`](../../architecture.md) and the ROADMAP.
**The one worth repeating:** worst-case compromise reaches *at most* the agent's own OS user, its own
Postgres role, its own scratch FS, and the allowlisted endpoints for the *one* compromised tool
([`docs/threat-model.md`](../../threat-model.md)).

## Recently merged

Newest first; full prose in the [`archive/`](archive/) snapshots and git history.

- **#736** — an `anyio>=4.14.2` **security floor** in `workers/gliner-relex/pyproject.toml` (not just a
  lock bump), closing GHSA-82r6-8w77-94w6 (critical, TLS spoofing) + GHSA-5p39-cfhj-2xmp. Exposure was
  provisioning-only: the worker is `Net::Deny` in both entries.
- **[#735](https://github.com/hherb/kastellan/pull/735)** — the **persistent** worker's death report reaches a failing test too (#730);
  preceded by a movement-only split of `worker_stderr` into its capture and reporting halves.
- **[#731](https://github.com/hherb/kastellan/pull/731)** `579ac01a` — a dying tool worker's last
  words reach a failing test, not just a daemon (#725). Filed #730, #732–#734.
- **[#728](https://github.com/hherb/kastellan/pull/728)** `40c4adc4` — the planner sees each prior step's call; `decision` screened (#699, #700).
- **[#727](https://github.com/hherb/kastellan/pull/727)** `eb1c76ea` — #677/#560 live acceptance (docs only).
- **[#726](https://github.com/hherb/kastellan/pull/726)** `577e2196` — the gliner worker survives
  `import torch` on macOS; `run-e2e-gate.sh` can pass (#719). Filed #725.
- **[#720](https://github.com/hherb/kastellan/pull/720)** `0966a460` — one REQUIRE-knob contract and
  the gate script (#714, #622, #664). Filed #718 (then wrongly auto-closed; reopened), #719, #721–#724.
- **[#717](https://github.com/hherb/kastellan/pull/717)** `6c7fc45d` — backlog triage, label taxonomy,
  required status checks (#655).
- **[#709](https://github.com/hherb/kastellan/pull/709)** `bd23f6f5` — a follow-up reads its own
  conversation (#701). Filed #710–#716.
- **[#708](https://github.com/hherb/kastellan/pull/708)**, **[#702](https://github.com/hherb/kastellan/pull/702)**,
  **[#694](https://github.com/hherb/kastellan/pull/694)**, **[#692](https://github.com/hherb/kastellan/pull/692)**,
  **[#688](https://github.com/hherb/kastellan/pull/688)**, **[#685](https://github.com/hherb/kastellan/pull/685)**
  and earlier — see git history and the archive.

---

## How to update this document at session end

1. Move anything now shipped from [Next TODO](#next-todo) into [Recently merged](#recently-merged)
   and add the ROADMAP line.
2. Update the [Test baseline](#test-baseline-authoritative) with the gate that actually ran, on the
   host it ran on, and **reconcile the delta against the row above it**. An unexplained delta is a
   finding, not a rounding error.
3. Record what still binds — the finding, not the narrative.
4. Keep this file under ~500 lines. When it grows past that, snapshot it to
   `archive/handover_<date>_<topic>_pre-prune.md` and compress in place, leaving the archive link.
5. **Keep the header free of VCS state** — PRs and issues only, no branch names, HEAD shas or "OPEN".
6. **Grep the PR body and every commit message for closing keywords before merging:**
   `(close[sd]?|fix(e[sd])?|resolve[sd]?)[[:space:]:,]+#[0-9]+` — including forward references.
7. Update [`ROADMAP.md`](../ROADMAP.md) in the same commit, and commit both together.

### Pruning convention

The archive snapshots are the long-form record; this file is the working brief. Keep **what would
change a decision** and drop the narrative of how it was found — except where the *way* it was found
is itself the lesson, which is most of the ⚠️ blocks above.
