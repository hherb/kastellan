# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20260922_734_pre-prune.md`](archive/handover_20260922_734_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here.
> ⚠️ **Repoint this line in the same commit as the snapshot.** It has been stale twice: #736's
> session wrote no snapshot at all, and #743's left this pointing two snapshots back.

**Last updated:** 2026-09-22 (#734/#733/#732/#742: the worker report ARRIVES, and a partial
capture says so — **plus the review round**, which found the fix had reopened #734 on the implicit
`message` field and had made `is_writable` fail CLOSED on macOS character devices) ·
**Recent PRs, newest first:** [#745](https://github.com/hherb/kastellan/pull/745) (#734, #733, #732, #742),
[#743](https://github.com/hherb/kastellan/pull/743) (#737, #738, #739),
[#740](https://github.com/hherb/kastellan/pull/740) (#736, the `anyio` security floor),
[#735](https://github.com/hherb/kastellan/pull/735) (#730, + a movement-only `worker_stderr` split),
[#731](https://github.com/hherb/kastellan/pull/731) (#725), [#728](https://github.com/hherb/kastellan/pull/728) (#699, #700),
[#727](https://github.com/hherb/kastellan/pull/727) (#677/#560 live acceptance, docs),
[#726](https://github.com/hherb/kastellan/pull/726) (#719), [#720](https://github.com/hherb/kastellan/pull/720)
(the REQUIRE-knob contract), [#717](https://github.com/hherb/kastellan/pull/717) (backlog triage),
[#709](https://github.com/hherb/kastellan/pull/709) (#701), [#702](https://github.com/hherb/kastellan/pull/702) (#677).
**The whole #725 → #745 worker-report arc is now closed:** #725, #730, #732, #733, #734, #737, #738,
#739 and #742 are all shipped. Older filings (#691, #693–#700, #703–#705, #710–#716, #718, #721–#724,
#729, #741) are in the [`archive/`](archive/) snapshots; **`gh issue list --state open` is the live
answer** and the only one worth trusting. ·
**The DGX runs `main` as of #709**, redeployed 2026-09-17 via `scripts/upgrade_from_git.sh` and
verified. **A redeploy is owed for #743 + #745**, and #745 raises the reason: #743 added
daemon-visible failure reporting, and #745 is what makes those reports survive an operator's
`RUST_LOG`. Not urgent (diagnostics plus one backoff-accounting fix), but the DGX is where the
Matrix and email channels actually run, so it is where this arc pays.
Rootfs images last rebuilt 2026-09-08.

> **Header convention (since 2026-09-11, after three recurrences).** This header names **PRs and
> issues only — never a branch name, a HEAD sha, or the word OPEN.** A merge falsifies those with no
> actor in between; a PR number it cannot. **The tip and the open set are one command each:**
> `git log --oneline -1 origin/main` and `gh pr list --state open`. Run them before trusting a word
> of this file.

> ⚠️ **An issue's census can be wrong, and so can its diagnosis — read the rows, not the issue.**
> #679 named 7 call sites (12), #690 named 4 subprocesses (10), #677 misread a schema rejection as a
> duplicate search, and **#719 named 3 failing tests when 7 were broken** — the other 4 skip unless
> `KASTELLAN_GLINER_RELEX_ENABLE=1`. [[issue-as-filed-can-carry-a-regression]]

> ⚠️ **A fix for a reviewer's finding can carry the next defect,** and **a passing mutation proof
> is not a review** [[mutation-proof-counts-only-mutants-you-tried]]. #677's approved spec capped
> strings at 512 B, breaking the question that worked [[plan-text-is-a-defect-source]]; #720's
> review closed a fail-*open* hole with a check that made it fail *always* (fixed in #726).

> ⚠️ **A forward reference auto-closed issue #718.** #720's body said the gate profile is the acceptance
> test for "whichever PR <closing-keyword> #718", and GitHub's scanner matched it — the fifth
> recurrence of this hazard and a new shape (no negation at all). Never quote the phrase literally. Reopened 2026-09-19. **Run the regex over every PR body and
> commit message before merging**, not only over the negations [[pr-body-not-fixed-autocloses-issue]].

> ⚠️ **A FIXTURE nobody rebuilds is a gate nobody runs** [[stale-fixture-turns-a-gate-into-a-formality]],
> and **a guard built from a census shares the census's blind spot** [[guard-shares-the-census-blind-spot]].

---

## Current state

### This session (2026-09-22, second): #734 + #733 + #732 + #742 — make the report ARRIVE

PR [#745](https://github.com/hherb/kastellan/pull/745). The #725/#730/#737 arc built the reports;
this one makes them reach a reader and stops two of them asserting more than they know. Full prose
in the ROADMAP entry and the commit messages.

- **#734 — the guard asked the wrong question.** `has_been_set()` answers "is a subscriber
  installed", not "will this WARN be **recorded**". A subscriber that *filters the event out*
  dropped it on both channels silently — reachable in the daemon, because `EnvFilter` applies its
  default directive only when the env string is **empty**, so an operator writing
  `RUST_LOG=kastellan_core::scheduler=debug` through the `kastellan.env.local` overlay silences the
  best diagnostic in the system *while debugging it*.
- ⚠️ **The delivery check must expand at the EMITTER's callsite — this is why
  `warn_and_fall_back!` is a MACRO and must stay one.** `event_enabled!` creates its own callsite
  carrying the `module_path!` of wherever it is *written*, and `EnvFilter` matches on exactly that.
  Measured: hoisted into the shared helper, `…::shared=warn` makes it report **delivered** for an
  event that was **not recorded** — #734 reproduced *inside its own fix*. ⚠️ The two `%label`
  emitters declare the field on the check too, or `warn,<target>[{label}]=off` is fail-open.
  Tables in the macro's doc. ⚠️ **And `message` is a field too** — see the review round below,
  where forgetting it reopened #734 inside its own fix for a second time.
- **#733 ships in the same commit because #734 CREATES it.** Before #734 the daemon always took the
  `tracing` branch, so it could never reach the `eprintln!` that panics on a broken pipe
  (`panic = "abort"` → silent `SIGABRT`). `eprintln!` **stays** — libtest capture depends on the
  macro [[libtest-capture-only-print-macros]] — and a `poll(2)` probe closes the abort in front of
  it. ⚠️ **Deliberately still a race**; it removes the *reproducible* abort
  (`kastellan-cli guard capture … | head`), not every possible one.
- ⚠️ **macOS reports a broken pipe write-end as `POLLHUP`, Linux as `POLLERR`.** Mutation-verified
  on both hosts, mirror-image: dropping either bit SURVIVES on the host that does not set it and is
  KILLED on the host that does. **Neither host alone can prove the mask** — a reviewer who prunes
  the "dead" bit from one machine breaks the other silently.
- **#732 — `collect_tail_after_drain` discarded `wait_for_drain`'s verdict**, so the renderers
  stated the opposite of what the system knew. Empty-and-timed-out read as "wrote NOTHING — suspect
  a kill (wall-clock/OOM/seccomp)", a **diagnosis**, for a worker that explained itself at 260 ms;
  non-empty-and-timed-out read as "its last words" when the ring evicts **oldest** first, so it held
  the **FIRST** lines. Now `CapturedTail {lines, complete}` with **private fields**.
  ⚠️ **`is_known_silent()` is the only predicate that licenses a DIAGNOSIS** — both renderers match
  it *first*, so their later `lines().is_empty()` arm can only be a partial. That ordering is a
  convention the compiler does not check, and the doc used to claim more (see the review round).
  Five states, and a test that all five read differently. Pre-existing, from #666.
- **#742 — a neutralising panic hook**, installed from `RequireKnob::action_reporting_to` (NOT
  `action` — see the review round), so gate-profile coverage follows **by construction** (a profile
  suite must consult its knob) rather than from a 30-file census. ⚠️ **Suites with no knob are NOT covered, deliberately** — they are in no profile.
  ⚠️ **The issue's own fix cannot work:** `PanicHookInfo` is borrowed from the runtime and
  unmodifiable, so "neutralise then delegate" hands the default the **raw** payload. Rendering it
  ourselves **costs libtest nothing** — it accounts for failures through `catch_unwind`, not the
  hook (measured). `core::untrusted_text` is now `pub` so the hook shares the ONE character class.
- **15 mutants, 15 killed**, but ⚠️ **two process lessons cost a re-run each.** (1) A placement
  test's first draft was wrong and **its own positive control caught it**: `EnvFilter` matches
  targets by **PREFIX**, so naming an ancestor module silently enabled the nested emitter it was
  isolating; only `warn,<emitter>=off` separates the designs. (2) A mutation harness that greps
  `^error` calls a **KILLED** mutant a build error — `cargo test` prints `error: test failed`.
  Distinguish "did not compile" (no `running N tests` line) from "compiled and failed"
  [[mutation-proof-counts-only-mutants-you-tried]].
- ⚠️ **Clippy caught two things `cargo test` did not, both after I believed the work finished:**
  caps-for-emphasis test names (`non_snake_case`), and **`PanicHookInfo` being stable only since
  1.81 against `rust-version = "1.78"`** (`clippy::incompatible_msrv`) — its predecessor
  `PanicInfo` is deprecated on current toolchains, so the type is **not named at all**
  (`payload_of` takes `&(dyn Any + Send)`). **Clippy here is a correctness gate, not a style pass.**

#### Review round on #745 — five reviewers, and the fix had reopened its own bug twice

Everything below is **measured on this Mac**, not argued. Nine findings fixed in-tree, four filed.

- ⚠️ **`message` is a field, and the check did not declare it — #734, reopened INSIDE its own
  fix.** A `warn!("{line}")` always carries an implicit `message` field. The checks declared
  `label` (second arm) or nothing (first), so under `warn,<target>[{message}]=off` **both** arms
  went silent on **both** channels. This is the *same* hole the `label` arm exists to close, one
  field further along, and the macro's own doc had already tabulated the lesson. **The rule, now
  stated as a rule: every field the `warn!` carries must be named in the check, implicit ones
  included.** With `message` declared, `fell_back` tracks `recorded` exactly across all seven
  directives on both arms.
- ⚠️ **`is_writable` failed CLOSED on macOS character devices**, which is the one direction the
  module's own doc forbids. Darwin's `poll` sets `POLLNVAL` on a live `/dev/null`, so **any process
  run `2>/dev/null` suppressed every worker report** while a real write succeeded. `POLLNVAL` is
  now believed only when `fcntl(F_GETFD)` agrees. The doc's "Not host-specific" was false: this is
  the *third* bit's version of the `POLLERR`/`POLLHUP` split, and it escaped the scrutiny the other
  two got.
- ⚠️ **`2>&-` is NOT part of #733's crash story, and the PR said it was.** Measured: `std` swallows
  `EBADF` on stdio (`handle_ebadf`), so `eprintln!` to a **closed** fd 2 returns `Ok` and does not
  panic. Only `EPIPE` panics. So the abort is reachable through the **broken-pipe** shape only —
  the one the issue actually names — and the `POLLNVAL` arm prevents a *pointless write*, not a
  crash. Which is why getting it wrong cost **reports** rather than crashes.
- ⚠️ **A new test failed 10/10 under the command CLAUDE.md documents, and 0/10 in a sweep.**
  `a_descriptor_that_was_never_open_is_not_writable` closed a `pipe(2)` and polled fds **3 and 4**;
  `open(2)` hands out the lowest free descriptor and libtest's parallel threads reclaimed fd 3.
  `cargo test -p kastellan-core --lib worker_stderr` → **10 failures in 10**; the full sweep → 0,
  because fd 3 is long taken. **A sweep-only green is how this would have shipped.** Now `dup2`s
  onto fd 900, which the lowest-free rule cannot hand back.
- ⚠️ **#733's fix had NO test**, and the mutant that matters survives the dead-code warning.
  Deleting the guard left the whole unit suite green; *probing `STDOUT_FILENO` instead of
  `STDERR_FILENO`* survived even `-D warnings`, because stdout is writable in every test binary.
  New `core/tests/worker_report_broken_stderr_e2e.rs` re-execs with a real broken fd 2 in both
  shapes. **2 mutants, 2 killed, each by both tests.** ⚠️ It needs `--nocapture`: libtest's capture
  is exactly what has to be out of the way for the bug to be reachable.
- ⚠️ **The panic hook's "coverage by construction" was coverage by ACCIDENT for the whole `microvm`
  profile.** `microvm::skip_unless_ready` → `report_unmet_microvm_to` → `require_action_to` calls
  `action_reporting_to` **directly**, bypassing `action` where the install lived. It worked only
  because the profile co-sets the PG and sandbox knobs. Moved one level down, to the chokepoint
  every knob read really funnels through [[guard-shares-the-census-blind-spot]]. ⚠️ **Residual and
  now documented:** the install is lazy, so a test that panics before *any* knob read still gets
  the default hook.
- ⚠️ **The hook flattened every multi-line panic message**, paying for the gate with the thing the
  gate protects. **59** assertion messages in `core/tests` interpolate a whole child transcript
  (`\n{both}`); each failure became one multi-kilobyte line. The property is "**no line but the
  first begins at column 0**", not "one line" — continuation lines are now indented.
- **`CapturedTail`'s fields are private.** `t.complete = true` compiled and forged the one claim
  the type exists to gate — the door `StderrTail::mark_drained` is `pub(crate)` to keep shut one
  layer down. The doc also claimed an enforcement it did not have ("the distinction *cannot* be
  lost"); it now says what is true.
- **~12 stale doc sites**, including a **33-line canonical block** on `emit_worker_failure_report`
  still describing the deleted `has_been_set()` guard and citing #734 and #733 as **open** — the
  doc `shared.rs` and `persistent.rs` point readers at. Also `tool_host.rs`'s "never the daemon",
  which #734 made false, and the `has_been_set()` rationale in both e2e suites (the one-child rule
  survives, for a *different* reason: `set_global_default` is once-per-process).
- Plus `.log_internal_errors(true)` on the daemon subscriber (the `fmt` layer silently discards its
  writer's `Result`, so a full disk drops every report on the one channel that could complain), and
  `#[doc(hidden)]` on the newly-`pub` `untrusted_text` — `kastellan-core` is published, and a bare
  `pub` is a permanent semver commitment taken on for one dev-dependency.
- **Filed:** [#746](https://github.com/hherb/kastellan/issues/746) (`egress::spawn::stderr_note`
  is a **third** renderer of the same tail, still rendering an un-drained empty tail as "no stderr
  captured" *and* collapsing the `None` arm into it — #732 in the egress path),
  [#747](https://github.com/hherb/kastellan/issues/747) (a drain that ends in a **read error** is
  marked complete, so an empty tail earns the "suspect a kill" diagnosis),
  [#748](https://github.com/hherb/kastellan/issues/748) (the worker-report e2e suites are in **no
  gate profile**; nothing enforces that a profiled suite reads a knob; `[panic]` is counted by
  nothing), [#749](https://github.com/hherb/kastellan/issues/749) (the hook's own `eprintln!` is
  unguarded, so a broken stderr turns a panic into a message-less abort).
- **Post-review sweep: 4434 / 0 / 40 over 184 suites**, `TEST_EXIT=0`, `[WARN]` **0**, `[SKIP]` 23
  (unchanged: 19 Apple container + 4 gliner opt-in), `[panic]` 19. Clippy
  `--workspace --all-targets -D warnings` clean. Delta against the pre-review 4427/1/38 over 183
  reconciles exactly: +1 suite, +2 ignored (the two new inner fixtures), +6 new tests, +1 from the
  **#744** flake passing this time (it is load-dependent; this branch touches no scheduler file).

### Previous (2026-09-22): #737 + #738 + #739 — say it when a worker is gone

[#743](https://github.com/hherb/kastellan/pull/743). Three defects of one shape: **the system
decides a worker is gone and then says nothing.** Condensed at #745; full text in
[`archive/handover_20260922_734_pre-prune.md`](archive/handover_20260922_734_pre-prune.md).

- **#737** — four of five fatal variants were reported nowhere, daemon included.
  `WorkerRetirementCause::from_client_error` is **the** census and
  `dispatch_indicates_worker_dead` **delegates** to it, so a variant cannot be
  fatal-but-unreportable. Marker `[worker-early-exit]` → **`[worker-failed]`**.
- ⚠️ **The issue's sixth variant was misfiled, and checking it was the useful part.**
  `ToolHostError::Io` is **pre-spawn**; a worker killed mid-response is `Protocol(ClientError::Io)`.
  `Io` lost its blanket `#[from]`, so the census is complete **by construction**, pinned by a
  `compile_fail` doctest [[issue-as-filed-can-carry-a-regression]].
- **#738 — `[worker-down]`**, a third marker for "is it coming back?" (respawn failed, rate alarm,
  driver gone). "Died once and came back" and "down for an hour" are different pages.
- **#739 — the joins no longer swallow a driver panic**; *any* panic there turned every later
  `call` into "persistent driver gone" forever, in silence.
- ⚠️ **Two findings came out of VERIFYING, not designing.** A conditional drain wait trades the
  report for a quarter second (the race is real); and deleting that wait **survived every suite in
  the tree** until it was hoisted into `collect_tail_after_drain` where a unit test can stage a slow
  drainer [[unreachable-success-path-proves-nothing]].

### Earlier in the arc (2026-09-20/21): #725 + #730

PRs [#731](https://github.com/hherb/kastellan/pull/731) (tool-worker) and
[#735](https://github.com/hherb/kastellan/pull/735) (**persistent** — the Matrix and email channel
workers). Full prose in
[`archive/handover_20260922_737_pre-prune.md`](archive/handover_20260922_737_pre-prune.md). What
still binds, re-confirmed by #743 and #745:

- ⚠️ **`eprintln!` is load-bearing, not style** [[libtest-capture-only-print-macros]], and a
  *captured* one returns on the child's **stdout** — a suite that merges the child's two streams
  cannot test its own claim. **Read them separately.**
- ⚠️ **No marker may begin with `[SKIP]`/`[WARN]`/`[E2E]`** — `run-e2e-gate.sh` greps those anchored
  at line start and asserts zero `[WARN]`. Hence the three `[worker-…]` markers and `[panic]`:
  one renderer, one delivery check, one neutralisation, so they cannot drift.
- ⚠️ **"Production" is not "the daemon".** `kastellan-cli` installs no subscriber anywhere.
- ⚠️ **`shutdown()` joins the driver thread, and that join is the only proof the report was
  emitted** — the driver answers the in-flight caller *first*, then reports, so a fixture asserting
  on `h.call(…)` returning is a race that passes on an idle machine.
- ⚠️ **Four review rounds each found the suite unable to prove its own central claim** — merged
  streams; a deleted `neutralise_controls` that passed on a clean literal; a `None` arm no fixture
  executed; a report that never waited for the drain
  [[mutation-proof-counts-only-mutants-you-tried]] [[unreachable-success-path-proves-nothing]].

### Previous (2026-09-19): #699 + #700, #677/#560, #719

Full text in [`archive/handover_20260921_730_pre-prune.md`](archive/handover_20260921_730_pre-prune.md).

**#699 + #700** (PR #728) — each `plans_so_far` step carries a screened
`"call": {tool, method, parameters}`; calls' bytes come off `PLANS_SUMMARY_BUDGET` first, then the
**oldest calls' `parameters`** drop. ⚠️ Review found a fail-open whose invariant rested on **an
unrelated parser's recursion limit**. ⚠️ **A hardening that rewrites screened text must ADD
readings, never replace them** [[screen-hardening-must-add-readings]]. Deferred: #729.
⚠️ **Not yet measured live** — re-run a multi-search mail question in a **fresh DM room**.

**#677 / #560 closed by live measurement, no code change.** ⚠️ **A live re-measure of a
single-question issue needs a fresh DM room**: since #709 a same-room question inherits the prior
turns' calls, which voids the test.

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
`scripts/run-e2e-gate.sh`. **Use it whenever a run is meant to be evidence.** Since #742,
`RequireKnob::action` also installs the neutralising panic hook. ⚠️ **No `sandbox` or `container`
profile yet — both would be red on every host** (#718: **92 sites in 47 files** bypass `skip_line`;
#722: the container helpers have no knob).

**#717.** ⚠️ **The ROADMAP is the accurate source; GitHub issues are the stale mirror.** Every open
issue carries one `area:*` label; `main` has required status checks (#655).

**#709 (#701).** A finishing channel task writes `tasks.turn_record` = `{calls, data_class}` in the
*same* `finalize` UPDATE; the next task in the same `(channel, peer, conversation)` reads up to 3,
screened and budgeted, and inherits their floor. **Calls, not results.** ⚠️ **No upper bound on the
window, deliberately.** ⚠️ **The floor comes from every turn LOADED, not those the screen kept.**
⚠️ **A security property can be documented, tested, and absent** — `inherit_floor` read the wrong
field while three one-leg tests defended it. **Email is still stateless.** Deferred: #710–#713,
#715, #716.

**#702 (#677).** `inner_loop/result_view` is the planner's view of a successful step — pruned,
labelled JSON. ⚠️ **Keys never reach the guard model** (#703). Budgets: 16 KiB/step, 96 KiB total.

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
   first — it ignores `has_attachment` today). **#728's live re-measure is still owed** (operator
   DMs, a **fresh room**).

2. **#702 follow-ups: #703, #704, #705.** ⚠️ **The guard model never sees object keys** (#703) — any
   new worker passing a third-party JSON object through reopens it silently; needs a DGX guard
   calibration run. The localmail changes the operator offered (2026-09-14: ordered headers, 4xx on
   cursor restart, filter-only search, compact hits, attachments by `message_id` + name, a distinct
   expired-credential error) are **not yet filed** on `hherb/localmail`.

3. **The worker-report arc is CLOSED — nothing left in it.** #725, #730, #732, #733, #734, #737,
   #738, #739 and #742 all shipped. What it leaves behind is three standing rules, each of which
   cost a session to learn: a delivery check must expand at the **emitter's** callsite (`EnvFilter`
   matches `module_path!`, so a hoisted check is fail-open); `POLLERR`/`POLLHUP` are **one bit per
   host** and neither host alone proves the mask; and `is_known_silent()` — never
   `lines.is_empty()` — is what licenses a report to state a *diagnosis*.

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
`main` and that the `fn`-name set matched, which a diff cannot show. Over cap today, biggest first:
`core/tests/guard_tier_e2e.rs` 1558+ (#639), `core/src/workers/gliner_relex/tests.rs`,
`db/src/asks.rs`, `sandbox/src/linux_firecracker/plan.rs` (DGX-gated), `core/src/channel/ask_message.rs`,
`db/graph.rs`, `llm-router/src/config.rs`, `core/src/scheduler/asks.rs`, `core/src/tool_host.rs`,
`workers/mail/src/handler.rs`, `tests-common/src/require.rs`, `core/src/worker_lifecycle/persistent.rs`,
plus `core/src/scheduler/inner_loop.rs`, `core/src/channel/bus.rs`, `workers/matrix/src/sdk_live.rs`,
`llm-router/src/messages.rs`, `core/src/main.rs`, `tests-common/src/microvm/{mod,container}.rs`,
`sandbox/tests/macos_smoke.rs`. ⚠️ **#743 and #745 both grew files already over cap without
splitting them** — `tool_host.rs` and `worker_lifecycle/persistent.rs`.

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
- **Process lessons that have each cost a re-run.** A truncated gate log is not a gate
  [[truncated-gate-log-is-not-a-gate]]. Mutation testing contaminates the git **index**
  [[mutation-testing-contaminates-the-index]]; revert by copying, never `git checkout`
  [[mutation-revert-never-git-checkout]]. `sqlx::migrate!` embeds at compile time
  [[sqlx-migrate-embeds-at-compile-time]]. ⚠️ **A mutation harness must tell "did not compile" from
  "compiled and failed"** — `cargo test` prints `error: test failed`, so an `^error` grep reports a
  killed mutant as a build error, and would report a survivor as one too (#745).

---

## Working state

### Test baseline (authoritative)

| Host | Commit | Result | clippy `-D warnings` | `[SKIP]` |
| --- | --- | --- | --- | --- |
| **Mac** (#734/#733/#732/#742 — **the gate that stands**) | branch tip | **4427 / 1 / 38**, **183** suites, `TEST_EXIT=101`, 0 `[WARN]`, **`[SKIP]` 23**, `[E2E]` 0 (no knobs set). ⚠️ **The one failure is `scheduler_lanes_e2e::two_lanes_run_concurrently`, filed as [#744](https://github.com/hherb/kastellan/issues/744) — a load flake, NOT a regression.** It asserts a wall-clock `elapsed < 1.7 s`; it got 2.79 s at load average **11**, then passed **3/3 isolated** (2.52/2.30/2.24 s) at load ~6. This PR touches no `core/src/scheduler/` file (empty `git diff --stat`). ⚠️ **New flake, not one of the three known ones** — re-read the text, do not re-attribute. **Delta reconciles EXACTLY: 4400 + 28 new tests − 1 (the flake moving passed→failed) = 4427.** The 28: 12 `delivery` + 7 `panic_hook` unit, +2 `worker_stderr`, +3 `tool_worker`, +2 `persistent`, and 2 e2e parents; +2 ignored are the two new inner fixtures, +1 suite is `panic_hook_gate_safety_e2e`. ⚠️ **`[panic]` appears 22× at column 0** — the new hook now renders `#[should_panic]` fixtures too. **Gate-safe and measured, not assumed:** `[WARN]` is **0** across all 183 suites. `[SKIP]` 23 = 19 Apple `container` + 4 gliner opt-in, **zero** "no Postgres install found" skips (`KASTELLAN_PG_BIN_DIR` set; the two textual matches are `#[should_panic]` fixture messages). DGX not re-swept — but its `delivery` tier WAS run (12/12) plus the two platform mutants | exit 0, `--workspace --all-targets`, zero warnings | **23** Mac (19 container, 4 gliner) |
| **Mac** (#737/#738/#739 — superseded by the row above) | branch tip | **4400 / 0 / 36**, **182** suites, `TEST_EXIT=0`, **0 `[WARN]`**, `[SKIP]` **23**. ⚠️ **Run with `-- --test-threads=4`; the DEFAULT sweep is red on this host and it is #548/#676, proven not asserted.** Two default sweeps on this exact tree failed **different** suites — 15 in `postgres_e2e`+`cli_memory_l3_run_e2e`, then 2 in `cli_entities_e2e` — every failure `the database system is starting up`, and each failing suite passes **in isolation** (60/60, 5/5, 6/6). Decisively: all three runs total **4400**, so contention only ever failed tests that otherwise pass, and the reduced-concurrency run has **zero** contention lines. ⚠️ **This session made the sweep redder by making it HONEST** — building the worktree's gliner `.venv` put three model-loading suites back in the run, which is what tips the concurrency over; the fix is fewer threads, never fewer suites. **Delta +11 passed / +3 ignored / +1 suite, reconciled BY NAME:** lib **+6** (25 added, 19 removed — the 7 `dispatch_classifier_*` tests re-homed in `liveness_tests.rs` in generalised form, plus the three-way `report.rs` test split), doctests **+2** (the `compile_fail` pinning the removed `#[from]` and its passing sibling), e2e **+3** (two new `persistent_worker_death_stderr_fallback_e2e` parents + the new `worker_failure_report_e2e`), ignored **+3** (their three inner fixtures). ⚠️ **`main`'s true total is 4389, not the 4388 the row below records** — its lib reports **2185** passed, measured directly, where that row's prose says 2184; 4389 + 11 = 4400 exactly. ⚠️ **The two "no Postgres install found" lines a grep finds are NOT the false-green tell** — they are `#[should_panic]` unit tests in `tests-common/src/microvm/require_tests.rs` rendering that message from a **hardcoded literal**. `KASTELLAN_PG_BIN_DIR` set. Gliner tier run separately and green: `run-e2e-gate.sh gliner` **5/5, exit 0, 4 demanded preconditions**, plus `KASTELLAN_GLINER_RELEX_ENABLE=1` on `entity_extraction_e2e` **16/16** and `memory_entity_link_e2e` **6/6**. Fallback markers in the log — 8 `[worker-death]`, 16 `[worker-down]`, 2 `[worker-failed]` — none matches the gate's three greps. Sources **sha256-verified unchanged across the sweep (670 files)** [[never-edit-tree-during-a-sweep]]. DGX not re-run (no Linux-only code touched) | exit 0, **cold** (`CARGO_TARGET_DIR=$HOME/.cargo-clippy-737cold`, **27** `Checking kastellan` lines), zero warnings. ⚠️ **Clippy caught two defects the green sweep did not** — a `redundant_guards` lint, and an **orphaned `#[test]`** left by a test-move that had attached itself to the NEXT test (a warn-level lint, so the suite stayed green while one test was registered twice) | **23** Mac (19 Apple `container`, 4 gliner opt-in) |

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
- **[#727](https://github.com/hherb/kastellan/pull/727)** `eb1c76ea` — #677/#560 live acceptance (docs only).
- **[#726](https://github.com/hherb/kastellan/pull/726)** `577e2196` — the gliner worker survives
  `import torch` on macOS (#719). Filed #725.
- **[#720](https://github.com/hherb/kastellan/pull/720)** `0966a460` — one REQUIRE-knob contract and the
  gate script (#714, #622, #664). Filed #718 (then wrongly auto-closed; reopened), #719, #721–#724.
- **[#717](https://github.com/hherb/kastellan/pull/717)**, **[#709](https://github.com/hherb/kastellan/pull/709)**,
  **[#708](https://github.com/hherb/kastellan/pull/708)**, **[#702](https://github.com/hherb/kastellan/pull/702)**,
  **[#694](https://github.com/hherb/kastellan/pull/694)**, **[#692](https://github.com/hherb/kastellan/pull/692)**,
  **[#688](https://github.com/hherb/kastellan/pull/688)**, **[#685](https://github.com/hherb/kastellan/pull/685)**
  and earlier — see git history and the [`archive/`](archive/) snapshots.

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
