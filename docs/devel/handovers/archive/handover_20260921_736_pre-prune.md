# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20260921_730_pre-prune.md`](archive/handover_20260921_730_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here.

**Last updated:** 2026-09-21 (#730: the persistent worker's death report reaches a failing test) ·
**Recent PRs, newest first:** [#735](https://github.com/hherb/kastellan/pull/735) (#730, + a movement-only `worker_stderr` split), [#731](https://github.com/hherb/kastellan/pull/731) (#725), [#728](https://github.com/hherb/kastellan/pull/728) (#699, #700), [#727](https://github.com/hherb/kastellan/pull/727) (#677/#560 live acceptance, docs), [#726](https://github.com/hherb/kastellan/pull/726) (#719, the gliner tier's two macOS import-time deaths + a gate
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

### This session (2026-09-21): #730 — the persistent worker's death report reaches a failing test

[#735](https://github.com/hherb/kastellan/pull/735). #725 fixed the **tool**-worker early-exit path;
the **persistent** path kept its own hand-rolled `tracing::warn!` and inherited none of it. That path
runs the **Matrix** and **email** channel workers, so in a test binary those died in silence — tail
captured, report rendered, then discarded because nothing was listening.
`worker_stderr::emit_persistent_death_report` is now the one producer. Every constraint in the #725
section below binds it too. Written, then **reviewed hard and materially changed** — the review
findings are folded in below rather than appended, because several of them refute what the first
draft claimed.

- **Two markers, one renderer.** `[worker-death]` is deliberately **not** `[worker-early-exit]`: an
  early exit says a tool worker never answered *one call*; a death says a long-lived worker stopped
  and is being respawned. Both go through the private `format_stderr_fallback` /
  `emit_to_stderr_when_unheard`, so the neutralisation and the `has_been_set()` guard exist in
  **one** copy — the bwrap-argv drift shape, where the incomplete copy is the one that breaks.
- ⚠️ **The label is in the message text AND the `tracing` field.** The daemon's subscriber is
  `fmt().with_env_filter(…).json()`, so `%label` is a queryable field worth keeping — but the
  fallback carries no fields, and a label kept only there leaves an operator reading "persistent
  worker died" with `matrix` and `email` both live.
- ⚠️ **Neutralisation here is defence-in-depth at a PUBLIC TRAIT BOUNDARY, not a live hole — and the
  code says so.** Nothing reaching it today is attacker-controlled. What earns it is that
  `PersistentTransport::death_report` is a `pub` trait method — any implementor is a producer, and
  `egress::persistent_net` already delegates through it.
- ⚠️ **`shutdown()` joins the driver thread, and that join is the only proof the report was
  emitted.** The driver replies to the in-flight caller *first*, then reports — so returning from
  `h.call(…)` proves nothing, and a fixture asserting without the join is a race.
- **The e2e is hermetic** — no sandbox, so unlike its #725 sibling it has **no `[SKIP]` path and
  runs on every host**. A skip is the worse trade in a suite whose subject is a report that goes
  missing.
- **Census read from the rows** [[issue-as-filed-can-carry-a-regression]]: the issue said "Matrix
  and egress"; the two production `PersistentWorker` users are **`matrix` and `email`**.

⚠️ **The review's biggest finding: the report this PR routes to stderr would usually have been
EMPTY.** `ClientTransport::death_report` snapshotted the tail with **no `wait_for_drain`**, unlike
`tool_host::warn_early_exit` — so it would normally render `no stderr captured` for a worker that
explained itself a millisecond later, the contentless line #730 exists to avoid. The in-code
justification ("a poll loop would stall the driver up to half a second") was refuted by measurement:
the driver's next act is `thread::sleep(backoff.next_delay(0))` and **both** production users set
`base: 1s`, so the wait delays no respawn at all. Fixed; the decision is now the free function
`collect_death_tail` **only so a unit test can reach it**
[[unreachable-success-path-proves-nothing]] — inside `death_report` it needs a real `Client` over a
spawned child, and no test could stage the race.

⚠️ **Mutation: the shared-renderer keeper still holds, but the 6/6 counted only the mutants the
author thought of** [[mutation-proof-counts-only-mutants-you-tried]]. Dropping `neutralise_controls`
from `format_stderr_fallback` leaves *both* e2es green (each emitter neutralises first and masks it)
and dies only in the unit tests — **but say what that mutant IS**: both `format_*_stderr_fallback`
wrappers have **zero callers** outside those tests, so it is a `pub` API contract test for a future
caller, not evidence the e2es have a hole. Review found **four** further survivors, all now killed
(**5/5**, restores sha256-verified, index clean): the `if let Some(r)` **`None` arm was never
executed** (no fixture returned `None` — the e2e now drives a *second* death through such a
transport and asserts exactly one marked line); swapping the label/report interpolations passed every
`contains` (pinned by `assert_eq!` on the whole line now); and **both** the label's own
`neutralise_controls` and the `%label` field were untested, because the label was a clean literal and
the message text carries it too — the fixture's label is hostile now, the only way to reach either.

⚠️ **Three doc claims were false, two of them this tree's own recorded failure mode.** The emit site
cited the **dispatch** census to defend a line the dispatch suites cannot reach — measured
**disjoint**; the honest figure is **8 of the 9** `PersistentWorker` suites. `worker_stderr/mod.rs`
said "two consumers" (**four**), named the wrong one (`ClientTransport`, serving matrix *and* email),
and said "the driver can log the death cause" — which **this PR made false**
[[guard-shares-the-census-blind-spot]]. `29 of the 30` was stale too: #731's own suite joined **both**
sides, so it is 29 of **31**. Smaller: `neutralise_controls` is **char-count** preserving, not
byte-length (U+2028 is 3 bytes → 1); `from_client` promised "exit status only" from a path returning
`None` outright (the `?` fires before `try_wait`); the e2e hand-rolled a **third** copy of the
`1|true|yes|on` dialect while claiming it did not — now `env_flag_enabled`.

**Preceded by a movement-only split** (`7facab48`): `worker_stderr.rs` 697 lines → `worker_stderr/`
`mod.rs` (capture) + `report.rs` (formatters, markers, emitters), `pub use` so **no call site
changed**. ⚠️ **Proven, not asserted:** a sorted-line multiset diff against `main` shows **zero
removals**. ⚠️ **Known wart, deliberately not rewritten:** that commit's new module doc documents
four items that only exist in `3f66ae8a`, so it carries two dangling rustdoc links — the *code* is
movement-only, the *docs* forward-reference. HEAD is consistent, so the cost is `cargo doc` and
bisect on one intermediate commit; rewriting a pushed PR branch was judged the worse trade.

**Filed for a later session:** [#737](https://github.com/hherb/kastellan/issues/737) — **five of the
six** `ClientError` variants `dispatch_indicates_worker_dead` calls dead are reported *nowhere*, in
the daemon as well as in tests; wider than #730 itself.
[#738](https://github.com/hherb/kastellan/issues/738) — the same driver's respawn-failure and
rate-alarm lines are still `tracing`-only, so a worker that **cannot come back** loops in silence.
[#739](https://github.com/hherb/kastellan/issues/739) — an `eprintln!` EPIPE panic on the **driver
thread** permanently kills a channel, and both joins swallow it (extends #733).

### Previous (2026-09-20): #725 — a dying tool worker's last words reach a failing test

PR [#731](https://github.com/hherb/kastellan/pull/731). `worker_stderr::emit_early_exit_report` logs
through `tracing` **and** `eprintln!`s the report when `has_been_set()` is false. #730 above is the
same fix one layer over, and the constraints below bind both.

- ⚠️ **`eprintln!` is load-bearing, not style.** libtest captures through
  `std::io::set_output_capture`, which the `print!`/`eprint!` **macros** consult and the
  `Stdout`/`Stderr` handles do not — a `writeln!(std::io::stderr(), …)` never appears under the
  failing test that needs it. A captured `eprintln!` comes back on the child's **stdout**, so a
  suite that merges the child's two streams cannot tell the two apart and stops testing its own
  claim. **Read them separately.**
- ⚠️ **A fallback marker must not begin with `[SKIP]`/`[WARN]`/`[E2E]`.** `run-e2e-gate.sh` greps
  those anchored at line start and asserts zero `[WARN]`, and every profile passes `--nocapture`.
- ⚠️ **"Production" is not "the daemon".** The daemon installs a subscriber first thing in `main`;
  `kastellan-cli` installs none anywhere and `guard capture` dispatches a real worker, so **that
  shipped binary gains the line** (intended).
- ⚠️ **Two review rounds each found the suite unable to prove its own central claim** — first the
  merged streams, then the *security* claim: deleting `neutralise_controls` passed every test,
  because the fixture's method was the literal `"anything"`. Both closed; the e2e now dispatches a
  hostile method and asserts on **position**, with a positive control.
- ⚠️ **`block_in_place` does NOT hand off to another thread** — measured. It runs the closure on the
  current one, so an `rt.block_on` fixture crosses no boundary.
- **Filed:** [#732](https://github.com/hherb/kastellan/issues/732) (a timed-out drain reported as
  "wrote NOTHING", pre-existing #666), [#733](https://github.com/hherb/kastellan/issues/733)
  (`eprintln!` SIGABRTs a release `kastellan-cli` on a broken pipe under `panic = "abort"`),
  [#734](https://github.com/hherb/kastellan/issues/734) (**`has_been_set()` asks whether a subscriber
  exists, not whether the WARN will be delivered** — a target-scoped `RUST_LOG` silences *both*
  channels).
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

3. **Test-harness honesty, now that the gate itself is tested.**
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
| **Mac** ([#735](https://github.com/hherb/kastellan/pull/735) — **the gate that stands**) | branch tip | **4386 / 0 / 33**, **181** suites, `TEST_EXIT=0`, 0 `[WARN]`, **`[SKIP]` 12**. +5 passed / +2 ignored over the row below, **predicted before the run and reconciled exactly**: 3 new `worker_stderr` unit tests (17 → 20, one of the 17 replaced by its generalised form) and the new hermetic e2e's 2 parents + 2 `#[ignore]`d fixtures; +1 suite is that file. `KASTELLAN_PG_BIN_DIR` set, so zero "no Postgres install found" — the false-green tell. Zero column-0 `[worker-death]`/`[worker-early-exit]` lines in the whole log, so the new marker does not leak into a gate. Sources **sha256-verified unchanged across the sweep** (663 files) [[never-edit-tree-during-a-sweep]]. DGX not re-run (no Linux-only code touched; the change is platform-neutral, so the DGX number is simply older) | exit 0, cold (**214** total `Checking` lines — a cached crate prints none — of which 27 `Checking kastellan`), zero warnings, `CARGO_TARGET_DIR=$HOME/.cargo-clippy-730` | **12** Mac |
| **Mac** ([#731](https://github.com/hherb/kastellan/pull/731), #725 — **post-`/fixall`, the gate that stands**) | branch tip (2nd review round) | **4381 / 0 / 31**, 180 suites, `TEST_EXIT=0`, 0 `[WARN]`, **`[SKIP]` 12**. +1 over the row below, reconciled exactly: the one new unit test (`the_stderr_fallback_neutralises_a_model_authored_control_character`); the round's other work added **assertions**, not tests, so `ignored` is unchanged at 31. ⚠️ **`[SKIP]` 23 → 12 is an improvement, not drift** — run with `KASTELLAN_PG_BIN_DIR` set, so **zero** "no Postgres install found" (the false-green tell); the 12 remaining are all legitimately opt-in or unavailable (Apple `container` ×8, gliner ×4). Skip-as-pass means those 11 newly-*running* tests move no count, which is exactly why the count alone was never the evidence. Sources **sha256-verified unchanged across the whole sweep** [[never-edit-tree-during-a-sweep]] — an earlier sweep was killed and restarted after two files were edited mid-run. DGX not re-run this round (no Linux-only code touched) | exit 0, cold (27 `Checking kastellan` lines, dedicated `CARGO_TARGET_DIR`), zero warnings | **12** Mac |
| **Mac + DGX** ([#731](https://github.com/hherb/kastellan/pull/731), #725 — 1st review round; superseded by the row above) | branch tip (post-review) | **Mac 4380 / 0 / 31**, 180 suites, `TEST_EXIT=0`, 0 `[WARN]`, **`[SKIP]` 23** (baseline parity). **DGX 4515 / 0 / 63**, 180 suites, `TEST_EXIT=0`, `[SKIP]` 4. Both = +5 passed / +2 ignored, reconciled exactly: Mac 4375+5; DGX 4501 + 4 (#728 round 1) + 5 (round 2) + 5. ⚠️ **The first Mac sweep was a false green** — it matched the predicted total while **339 of its 361 `[SKIP]`s were "no Postgres install found"**, because skip-as-pass counts as passed. Set `KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"` on the Mac or the sweep is not evidence | exit 0, cold (27 `Checking kastellan` lines, `CARGO_TARGET_DIR=$HOME/.cargo-clippy-725`), zero warnings | **23** Mac, **4** DGX (gliner opt-in) |
| **Mac + DGX** ([#728](https://github.com/hherb/kastellan/pull/728), #699/#700) | branch tip (2nd review round) | **Mac 4375 / 0 / 29**, 179 suites, `TEST_EXIT=0`, 0 `[WARN]` — the round's 4370 + **5** (deep-parameter screen, multibyte clamp, the `{}`-parameters grow guard, the framing constant, the step-less outcome). Round 1 at `ce8bc49b`: Mac **4370 / 0 / 29** = #726's 4347 + 5 (grepped between `64d483e8` and `main`) + 18 new. **DGX** full sweep at `299ce103` (before round 1's +4): **4501 / 0 / 61**, 179 suites, exit 0 (= 4482 + 5 + 14) — ⚠️ **not re-run for round 2** (Mac-only changes, but `summary` is platform-neutral, so the DGX number is simply older) | exit 0, cold (27 `Checking kastellan` lines, `CARGO_TARGET_DIR=$HOME/.cargo-clippy-pr728`) at the round-2 tip | **4** DGX (gliner opt-in) |
| **Mac + DGX** ([#726](https://github.com/hherb/kastellan/pull/726), #719) | `64d483e8` (branch tip) | **DGX 4482 / 0 / 61**, 179 suites, `TEST_EXIT=0`, 0 `[WARN]` — **exactly** 4453 (#709) + 24 (#720: 23 `#[test]` + 1 doc-test, never run on the DGX until now) + 5 (this PR), each of the 5 grepped `ok` by name. **Mac 4347 / 0 / 29**, 179 suites — **exactly** #720's 4342 + 5. ⚠️ The sweep itself reported **3 gliner failures that were my own mutation testing**: a same-size Python mutant restored within the same second left its `.pyc` cached *and valid*, and the jailed worker (read-only src) ran it — confirmed by disassembling the cached `main()` [[mutation-testing-leaves-stale-pyc]]. With `__pycache__` cleared the **sweep-built** binaries pass 5/5, 16/16, 6/6 under the REQUIRE knob; source unchanged since the sweep built them. **Also:** `gliner` gate profile passes as evidence on both hosts (first time); DGX gliner ENABLE suites 16/16, 6/6; pytest 71 on both | **exit 0 on both hosts**, zero warnings, 27 `Checking kastellan` lines each, dedicated `CARGO_TARGET_DIR` | **4** DGX (gliner opt-in, ENABLE unset), **23** Mac |

Older rows are in the [`archive/`](archive/) snapshots.

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
**gliner Python tests:** `cd workers/gliner-relex && uv run --frozen pytest -q` (70 tests).

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
