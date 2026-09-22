# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20260922_745_pre-prune.md`](archive/handover_20260922_745_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here.
> ⚠️ **Repoint this line in the same commit as the snapshot.** It has been stale twice.

**Last updated:** 2026-09-22 (#746/#747/#749: the review round's three code follow-ups —
a read-error drain stops earning the "suspect a kill" diagnosis, the **third** renderer of the
same tail is fixed, and the panic hook can no longer SIGABRT) ·
**Recent PRs, newest first:** [#750](https://github.com/hherb/kastellan/pull/750) (#746, #747, #749),
[#745](https://github.com/hherb/kastellan/pull/745) (#734, #733, #732, #742),
[#743](https://github.com/hherb/kastellan/pull/743) (#737, #738, #739),
[#740](https://github.com/hherb/kastellan/pull/740) (#736, the `anyio` security floor),
[#735](https://github.com/hherb/kastellan/pull/735) (#730), [#731](https://github.com/hherb/kastellan/pull/731) (#725),
[#728](https://github.com/hherb/kastellan/pull/728) (#699, #700), [#726](https://github.com/hherb/kastellan/pull/726) (#719),
[#720](https://github.com/hherb/kastellan/pull/720) (the REQUIRE-knob contract),
[#709](https://github.com/hherb/kastellan/pull/709) (#701), [#702](https://github.com/hherb/kastellan/pull/702) (#677).
**The whole #725 → #750 worker-report arc is now closed**, including its review residue:
#725, #730, #732, #733, #734, #737, #738, #739, #742, #746, #747 and #749 are all shipped.
**#748 is the one piece left** (the worker-report e2e suites are in no gate profile).
Older filings are in the [`archive/`](archive/) snapshots; **`gh issue list --state open` is the
live answer** and the only one worth trusting. ·
**The DGX runs `main` as of #709**, redeployed 2026-09-17 and verified. **A redeploy is owed for
#743 + #745 + #750** — #743 added daemon-visible failure reporting, #745 made those reports survive
an operator's `RUST_LOG`, and #750 stops three of them asserting more than they know. Not urgent
(diagnostics only), but the DGX is where the Matrix and email channels run, so it is where this
arc pays. Rootfs images last rebuilt 2026-09-08.

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
> harmless-looking reorder and is #730 exactly — one existing test caught it.

> ⚠️ **A forward reference auto-closed issue #718.** #720's body said the gate profile is the acceptance
> test for "whichever PR <closing-keyword> #718", and GitHub's scanner matched it — the fifth
> recurrence of this hazard and a new shape (no negation at all). Never quote the phrase literally.
> Reopened 2026-09-19. **Run the regex over every PR body and commit message before merging**, not
> only over the negations [[pr-body-not-fixed-autocloses-issue]].

> ⚠️ **A FIXTURE nobody rebuilds is a gate nobody runs** [[stale-fixture-turns-a-gate-into-a-formality]],
> and **a guard built from a census shares the census's blind spot** [[guard-shares-the-census-blind-spot]].

---

## Current state

### This session (2026-09-22, third): #746 + #747 + #749 — the review round's code residue

PR [#750](https://github.com/hherb/kastellan/pull/750). Three defects #745's review filed rather
than fixed. Full prose in the ROADMAP entry and the commit messages.

- **#747 — a drain that ends in a READ ERROR is not a silent worker.** `mark_drained()` after a
  failed read is deliberate (a waiter must not burn the 250 ms cap because the pipe broke), but
  recording it as **EOF** made `is_known_silent()` true for an empty tail, so the report printed
  the one sentence only a *known* silence earns. A guest fd returning `EIO` on teardown therefore
  sent an operator to audit cgroups and seccomp for a worker that spoke fine into a pipe we lost.
  `drain_reader` now returns `DrainEnd::{Eof, ReadError}` (`#[must_use]`) and the tail stores it.
- ⚠️ **The predicate pair is GONE, replaced by an exhaustive `TailState`.** `is_complete()` +
  `is_known_silent()` + `lines()` carried their combining rule in a **doc comment** — *match
  `is_known_silent()` first* — and that convention had failed **twice out of three** renderers
  (#732 was one, #746 the other). All three renderers now match `CapturedTail::state()`, so a
  forgotten case does not compile. **Deliberately not `#[non_exhaustive]`**: breaking every
  renderer when a state is added is the entire point.
- **#746 — the third renderer.** `egress::spawn::stderr_note` polled for a *non-empty* snapshot
  rather than for EOF, and collapsed the `None` (unpiped) arm into the same "no stderr captured"
  string that describes a silent proxy. Now routed through `collect_tail_after_drain` with the
  full seven-state rendering. `STDERR_SETTLE` is deleted — it was a second spelling of
  `TAIL_DRAIN_WAIT` (both 250 ms), so **the issue's "the wait semantics differ" caveat is about
  the PREDICATE, not the duration**.
- **#749 — the panic hook could SIGABRT.** Its `eprintln!` panics on a failed write, and a panic
  *inside a panic hook* is a panic while panicking: immediate abort, **no output on any stream**,
  and libtest's own `test result: FAILED` line lost too. Strictly worse than #733, where only a
  report went missing. Measured on this Mac: unguarded → **signal 6, nothing anywhere**; guarded →
  exit 101, message intact. Fixed by reusing the ONE probe, exported as
  `#[doc(hidden)] pub fn worker_stderr::stderr_is_writable()` the way `untrusted_text` already is.
- ⚠️ **It takes no `fd`, deliberately.** The mutant #745's review found surviving even
  `-D warnings` was probing `STDOUT_FILENO` — stdout is writable in every test binary. A
  parameterless export cannot be called wrongly that way.
- ⚠️ **`spawn_drain_with_tail`'s closure had to be extracted (`drain_and_mark`) to be testable at
  all** — its reader is a `ChildStderr` no unit test can construct, so a mutant hard-coding
  `DrainEnd::Eof` (i.e. #747 in full) survives every suite until the extraction. Same shape #737
  hit [[unreachable-success-path-proves-nothing]].
- ⚠️ **The movement-only commit introduced a real bug and one test caught it.** Binding
  `let lines = tail.snapshot();` above the wait reads as a harmless reorder and *is* #730. Only
  `collecting_a_worker_tail_waits_for_the_drainer_instead_of_racing_it` pins that order; it now
  says so at the site.
- **Mutants: 14/14 (#747), 5/5 (#746), 4/4 across the host PAIR (#749).** ⚠️ The last is why both
  hosts ran: pruning `POLLHUP` is **KILLED on the Mac and SURVIVES on Linux**, pruning `POLLERR`
  the mirror image — measured on both this session. **Neither host alone can prove that mask.**
- ⚠️ **One mutant needed a fixture that stages the production race.** Deleting `stderr_note`'s
  wait survives every *other* test in that file, because they all hand it an already-drained tail.
- **Files:** `worker_stderr/mod.rs` was 617 lines against the 500 cap, so `CapturedTail` moved to
  `captured.rs` in a **movement-only commit first**, proven by byte-identity of both moved regions
  plus a negative control. mod.rs is now 578.

### Previous (2026-09-20/22): the #725 → #745 worker-report arc

PRs [#731](https://github.com/hherb/kastellan/pull/731) (#725),
[#735](https://github.com/hherb/kastellan/pull/735) (#730),
[#743](https://github.com/hherb/kastellan/pull/743) (#737, #738, #739),
[#745](https://github.com/hherb/kastellan/pull/745) (#734, #733, #732, #742). Full prose in
[`archive/handover_20260922_745_pre-prune.md`](archive/handover_20260922_745_pre-prune.md) and its
siblings. What still binds:

- ⚠️ **The delivery check must expand at the EMITTER's callsite — `warn_and_fall_back!` is a MACRO
  and must stay one.** `event_enabled!` carries the `module_path!` of wherever it is *written* and
  `EnvFilter` matches exactly that, so a hoisted check reports "delivered" for a dropped event.
  ⚠️ **Every field the `warn!` carries must be named in the check, implicit ones included** — a
  `warn!("{line}")` always carries `message`, and forgetting it reopened #734 *inside its own fix*.
- ⚠️ **`is_known_silent()` used to be the only predicate licensing a DIAGNOSIS; since #750 that is
  `TailState::KnownSilent`**, and the renderers are exhaustive rather than order-dependent.
- ⚠️ **`eprintln!` is load-bearing, not style** [[libtest-capture-only-print-macros]], and a
  *captured* one returns on the child's **stdout** — a suite that merges the child's two streams
  cannot test its own claim. **Read them separately.**
- ⚠️ **No marker may begin with `[SKIP]`/`[WARN]`/`[E2E]`** — `run-e2e-gate.sh` greps those anchored
  at line start and asserts zero `[WARN]`. Hence `[worker-failed]`, `[worker-death]`,
  `[worker-down]` and `[panic]`: one renderer, one delivery check, one neutralisation.
- ⚠️ **"Production" is not "the daemon".** `kastellan-cli` installs no subscriber anywhere.
- ⚠️ **`shutdown()` joins the driver thread, and that join is the only proof the report was
  emitted** — a fixture asserting on `h.call(…)` returning is a race that passes on an idle machine.
- ⚠️ **`WorkerRetirementCause::from_client_error` is THE census** and `dispatch_indicates_worker_dead`
  delegates to it, so a variant cannot be fatal-but-unreportable. `ToolHostError::Io` is **pre-spawn**
  and lost its blanket `#[from]`, pinned by a `compile_fail` doctest.
- ⚠️ **`PanicHookInfo` cannot be named** (stable 1.81 vs `rust-version = "1.78"`), and the hook is
  installed from `RequireKnob::action_reporting_to` — **not `action`**, which the whole `microvm`
  profile goes round. ⚠️ Suites with **no knob are not covered**, deliberately.
- ⚠️ **Review rounds repeatedly found suites unable to prove their own central claim** — merged
  streams; a deleted `neutralise_controls` passing on a clean literal; a `None` arm no fixture
  executed; a report that never waited for the drain [[mutation-proof-counts-only-mutants-you-tried]].

### Previous (2026-09-19): #699 + #700, #677/#560, #719

Full text in [`archive/handover_20260921_730_pre-prune.md`](archive/handover_20260921_730_pre-prune.md).

**#699 + #700** (PR #728) — each `plans_so_far` step carries a screened
`"call": {tool, method, parameters}`; calls' bytes come off `PLANS_SUMMARY_BUDGET` first, then the
**oldest calls' `parameters`** drop. ⚠️ **A hardening that rewrites screened text must ADD
readings, never replace them** [[screen-hardening-must-add-readings]]. Deferred: #729.
⚠️ **Not yet measured live** — re-run a multi-search mail question in a **fresh DM room**, which
is also what #677/#560's live re-measure needs: since #709 a same-room question inherits the prior
turns' calls, voiding the test.

**#719** (PR #726) — both causes inside `import torch`: `getcwd()` → EPERM under Seatbelt (workers
now start in `/`), and torch's compile cache at import (host-mode gliner opts into
`ephemeral_scratch`). ⚠️ **A sandbox that restricts but does not relocate leaks the caller's context
into the jail.** ⚠️ **gliner is the first WARM worker on `ephemeral_scratch`.** ⚠️ **After mutating
a `.py`, delete its `__pycache__`** [[mutation-testing-leaves-stale-pyc]].

### Previous: #720 / #717 / #709 / #702 — contract, triage, continuity, result view

**#720** (closed #714, #622, #664):

> **Every gate needs a REQUIRE knob *and* a positive control that fails when zero tests ran.**

`tests_common::require::RequireKnob` is the one vocabulary and the knob is **data**, so a tier is one
`const`. ⚠️ **A knob alone was never enough (#664):** it fires only inside a test body, so a
filtered-out run emits no `[SKIP]` and exits 0 — hence `[E2E]` markers and per-tier floors in
`scripts/run-e2e-gate.sh`. **Use it whenever a run is meant to be evidence.** ⚠️ **No `sandbox` or
`container` profile yet — both would be red on every host** (#718: **92 sites in 47 files** bypass
`skip_line`; #722: the container helpers have no knob).

**#717.** ⚠️ **The ROADMAP is the accurate source; GitHub issues are the stale mirror.** Every open
issue carries one `area:*` label; `main` has required status checks (#655).

**#709 (#701).** A finishing channel task writes `tasks.turn_record` = `{calls, data_class}` in the
*same* `finalize` UPDATE; the next task in the same `(channel, peer, conversation)` reads up to 3,
screened and budgeted, and inherits their floor. **Calls, not results.** ⚠️ **No upper bound on the
window, deliberately**, and **the floor comes from every turn LOADED, not those the screen kept.**
⚠️ **A security property can be documented, tested, and absent** — `inherit_floor` read the wrong
field while three one-leg tests defended it. **Email is still stateless.** Deferred: #710–#713,
#715, #716. **#702 (#677):** `inner_loop/result_view` is the planner's view of a successful step —
pruned, labelled JSON. ⚠️ **Keys never reach the guard model** (#703). Budgets 16 KiB/step, 96 KiB.

### Merged arcs — only what still binds

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
> **rust-analyzer's `cargo check` holds `target/debug/.cargo-lock`** (a blocked build prints `Blocking
> waiting for file lock` — kill the IDE's cargo child, recurred 2026-09-19).

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

1. **The last #677 follow-up:** [#698](https://github.com/hherb/kastellan/issues/698) (`mail.search`
   cannot express a filter-only search; check what localmail `/v1/search` does with an empty query
   first — it ignores `has_attachment` today). **#728's live re-measure is still owed** (operator
   DMs, a **fresh room**).

2. **#702 follow-ups: #703, #704, #705.** ⚠️ **The guard model never sees object keys** (#703) — any
   new worker passing a third-party JSON object through reopens it silently; needs a DGX guard
   calibration run. The localmail changes the operator offered (2026-09-14: ordered headers, 4xx on
   cursor restart, filter-only search, compact hits, attachments by `message_id` + name, a distinct
   expired-credential error) are **not yet filed** on `hherb/localmail`.

3. **The worker-report arc is CLOSED except [#748](https://github.com/hherb/kastellan/issues/748)**
   (the worker-report e2e suites are in **no gate profile**; nothing enforces that a profiled suite
   reads a knob; `[panic]` is counted by nothing). #750's own new suite,
   `tests-common/tests/panic_hook_broken_stderr_e2e.rs`, joins that list — it is in no profile
   either. What the arc leaves behind is four standing rules, each of which cost a session: a
   delivery check must expand at the **emitter's** callsite; every field the `warn!` carries must
   be named in the check, **`message` included**; `POLLERR`/`POLLHUP` are **one bit per host** and
   neither host alone proves the mask; and a renderer must match an **exhaustive** `TailState`,
   because the documented arm-order convention failed twice out of three.

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
  radius, not teardown; restore the shared suffix with a `.suffix()` setter, **not** by reverting
  #641 [[issue-as-filed-can-carry-a-regression]].
- **Mail credential expiry — [#673](https://github.com/hherb/kastellan/issues/673) + [#674](https://github.com/hherb/kastellan/issues/674):**
  an upstream 401/403 reads as `POLICY_DENIED`, and nothing notices the expiry.
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
`llm-router/src/config.rs`, `core/src/scheduler/asks.rs`, `core/src/tool_host.rs`,
`workers/mail/src/handler.rs`, `tests-common/src/require.rs`,
`core/src/worker_lifecycle/persistent.rs`, `core/src/scheduler/inner_loop.rs`,
`core/src/channel/bus.rs`, `workers/matrix/src/sdk_live.rs`, `llm-router/src/messages.rs`,
`core/src/main.rs`, `tests-common/src/microvm/{mod,container}.rs`, `sandbox/tests/macos_smoke.rs`.
⚠️ **#743 and #745 both grew files already over cap without splitting them** (`tool_host.rs`,
`worker_lifecycle/persistent.rs`); #750 split `worker_stderr/mod.rs` **first** instead.

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
| **Mac** (#746/#747/#749 — **the gate that stands**) | branch tip | **4451 / 0 / 41**, **185** suites, `TEST_EXIT=0`, `[WARN]` **0**, `[SKIP]` **26**, `[E2E]` 0 (no knobs set), `[panic]` 20. Run `--no-fail-fast -- --test-threads=4`. ⚠️ **`cargo test --workspace` is FAIL-FAST** — the first attempt stopped after **59** suites on a `the database system is starting up` flake (#548/#676; that suite then passed **6/6 isolated**), and a 59-suite run reports a plausible-looking 2552 passed. **Always `--no-fail-fast`.** **Delta vs the row below reconciles EXACTLY: +17 passed / +1 ignored / +1 suite**, by name — captured.rs +3, worker_stderr/mod.rs +5, tool_worker +2, persistent +2, egress/spawn +4 (16 unit), plus the new `panic_hook_broken_stderr_e2e` (+1 suite, +1 passed, +1 ignored inner fixture). ⚠️ **`[SKIP]` 23→26 is the WORKTREE, not a regression**: 19 Apple `container` + 4 gliner opt-in + **3 "gliner-relex venv shim not built"**, because a fresh worktree has no `.venv`. Built it (`scripts/workers/gliner-relex/install.sh`, `readlink`-verified as this host's python, not a copied dead fixture) and ran the tier separately, all green: gate profile `gliner` **5 tests / 4 demanded preconditions / `[WARN]` 0**, `entity_extraction_e2e` **16/16**, `memory_entity_link_e2e` **6/6**. DGX ran the touched suites (worker_stderr **60/60**, egress::spawn **23/23**, both broken-stderr e2es) plus the mirror-image `POLL*` mutants | exit 0 **on BOTH hosts**, cold (`CARGO_TARGET_DIR=$HOME/.cargo-clippy-746{mac,cold}`), **27** `Checking kastellan` lines each, zero warnings | **26** Mac (19 container, 4 gliner opt-in, 3 worktree venv) |
| **Mac** (#734/#733/#732/#742 — superseded by the row above) | — | **4434 / 0 / 40**, **184** suites, `TEST_EXIT=0`, `[WARN]` **0**, `[SKIP]` 23 (19 Apple container + 4 gliner opt-in), `[panic]` 19. ⚠️ The pre-review figure `4427 / 1 / 38` over 183 that this row used to carry was the **pre-review** run, and its one failure was the #744 load flake, not a regression | exit 0, `--workspace --all-targets`, zero warnings | **23** Mac |

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
| `core` Firecracker (14 suites, `#[ignore]`, DGX) | 29 | **real KVM** round-trips, mem cap, net deny, warm idle, VMM confinement, egress + broker channels, persistent store. `bash scripts/run-e2e-gate.sh microvm` |
| `core` gliner (`gliner_relex_e2e`, `entity_extraction_e2e`, `memory_entity_link_e2e`) | 5 / 16 / 6 | the real model under the real sandbox; the latter two only with `KASTELLAN_GLINER_RELEX_ENABLE=1`. `bash scripts/run-e2e-gate.sh gliner` |
| `core` (`shell_exec_e2e`, `python_exec_e2e`, `python_exec_container_e2e`) | 4 / 4 / 4 | **real** core→sandbox→worker round-trips; per-spawn scratch; secret-scrub |
| `core` (`egress_proxy_e2e`, `egress_force_routing_e2e`, `email_mitm_e2e`) | 3 / 4 / 2 | real sidecar + CONNECT; Linux no-direct-route; hermetic MITM |
| `core` (`injection_guard_e2e`, `secret_vault_e2e`, `guard_boot_row_e2e`) | 10 / 9 / 1 | **PG-required** policy rows, privacy invariant, fail-closed redemption |
| broken-stderr e2es (`core/tests/worker_report_…`, `tests-common/tests/panic_hook_…`) | 2 / 1 | a real broken fd 2, re-exec'd; **need `--nocapture`**, and are in no gate profile (#748) |
| `tests-common` `gate_script_tests` | 8 | the gate script's table **and** its verdict, run against a fake `cargo` |

## Key design decisions locked in

**Not restated here — they drift.** Hard constraints: [`CLAUDE.md`](../../../CLAUDE.md) § Hard
constraints; the rest: [`docs/architecture.md`](../../architecture.md) and the ROADMAP.
**The one worth repeating:** worst-case compromise reaches *at most* the agent's own OS user, its own
Postgres role, its own scratch FS, and the allowlisted endpoints for the *one* compromised tool
([`docs/threat-model.md`](../../threat-model.md)).

## Recently merged

Newest first; full prose in the [`archive/`](archive/) snapshots and git history.

- **[#750](https://github.com/hherb/kastellan/pull/750)** — a read-error drain stops earning the
  "suspect a kill" diagnosis, the **third** renderer of the same tail is fixed, and the panic hook
  can no longer SIGABRT on a broken stderr (#747, #746, #749). The order-dependent
  `is_known_silent()`/`is_complete()` pair is replaced by an exhaustive `TailState`. Closes the
  #725 → #750 arc bar #748.
- **[#743](https://github.com/hherb/kastellan/pull/743)** — every retired worker reports, and every
  worker that stays down says so (#737, #738, #739). One census behind both "retire it" and "say
  so"; `[worker-early-exit]` → **`[worker-failed]`** plus a new **`[worker-down]`**; a panicked
  driver is reported instead of swallowed; `ToolHostError::Io` reclassified pre-spawn and its
  blanket `#[from]` removed. Filed #742.
- **[#745](https://github.com/hherb/kastellan/pull/745)** — the worker report reaches a reader whose
  `RUST_LOG` would have dropped it, cannot `SIGABRT` on a broken pipe, and stops calling a
  timed-out drain "the worker wrote NOTHING"; plus a panic hook that cannot forge a gate line
  (#734, #733, #732, #742). Closes the #725 → #745 arc.
- **[#740](https://github.com/hherb/kastellan/pull/740)** — an `anyio>=4.14.2` **security floor** in `workers/gliner-relex/pyproject.toml` (not just a
  lock bump), closing GHSA-82r6-8w77-94w6 (critical, TLS spoofing) + GHSA-5p39-cfhj-2xmp. Exposure was
  provisioning-only: the worker is `Net::Deny` in both entries.
- **[#735](https://github.com/hherb/kastellan/pull/735)** — the **persistent** worker's death report reaches a failing test too (#730);
  preceded by a movement-only split of `worker_stderr` into its capture and reporting halves.
- **[#731](https://github.com/hherb/kastellan/pull/731)** `579ac01a` — a dying tool worker's last
  words reach a failing test, not just a daemon (#725). Filed #730, #732–#734.
- **[#728](https://github.com/hherb/kastellan/pull/728)** `40c4adc4` — the planner sees each prior step's call; `decision` screened (#699, #700).
- **[#726](https://github.com/hherb/kastellan/pull/726)** `577e2196` — the gliner worker survives `import torch` on macOS (#719).
- **[#720](https://github.com/hherb/kastellan/pull/720)** `0966a460` — one REQUIRE-knob contract and the
  gate script (#714, #622, #664). Filed #718 (then wrongly auto-closed; reopened), #719, #721–#724.
- **#727, #717, #709, #708, #702, #694, #692, #688, #685** and earlier — see git history and the
  [`archive/`](archive/) snapshots.

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
