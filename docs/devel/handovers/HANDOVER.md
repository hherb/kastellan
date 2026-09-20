# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20260919_699_pre-prune.md`](archive/handover_20260919_699_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here.

**Last updated:** 2026-09-20 (#725: a dying worker's last words reach a failing test) ·
**Recent PRs, newest first:** [#731](https://github.com/hherb/kastellan/pull/731) (#725), [#728](https://github.com/hherb/kastellan/pull/728) (#699, #700), [#727](https://github.com/hherb/kastellan/pull/727) (#677/#560 live acceptance, docs), [#726](https://github.com/hherb/kastellan/pull/726) (#719, the gliner tier's two macOS import-time deaths + a gate
script that could not pass), [#720](https://github.com/hherb/kastellan/pull/720) (the REQUIRE-knob
contract: #714, #622, #664), [#717](https://github.com/hherb/kastellan/pull/717) (backlog triage),
[#709](https://github.com/hherb/kastellan/pull/709) (#701, conversational continuity),
[#702](https://github.com/hherb/kastellan/pull/702) (#677, the planner's labelled result view),
[#694](https://github.com/hherb/kastellan/pull/694) (#617). **Open issues these filed:**
[#730](https://github.com/hherb/kastellan/issues/730), [#732](https://github.com/hherb/kastellan/issues/732)–[#734](https://github.com/hherb/kastellan/issues/734) (from #731);
[#718](https://github.com/hherb/kastellan/issues/718), [#721](https://github.com/hherb/kastellan/issues/721)–[#724](https://github.com/hherb/kastellan/issues/724) (from #720);
[#710](https://github.com/hherb/kastellan/issues/710)–[#713](https://github.com/hherb/kastellan/issues/713), [#715](https://github.com/hherb/kastellan/issues/715), [#716](https://github.com/hherb/kastellan/issues/716) (from #709);
[#698](https://github.com/hherb/kastellan/issues/698)–[#700](https://github.com/hherb/kastellan/issues/700),
[#703](https://github.com/hherb/kastellan/issues/703)–[#705](https://github.com/hherb/kastellan/issues/705) (from #702); [#693](https://github.com/hherb/kastellan/issues/693),
[#695](https://github.com/hherb/kastellan/issues/695)–[#697](https://github.com/hherb/kastellan/issues/697) (from #694);
[#691](https://github.com/hherb/kastellan/issues/691) (from #692). ·
**The DGX runs `main` as of #709**, redeployed 2026-09-17 via `scripts/upgrade_from_git.sh` and
verified (installed binaries byte-identical, units active, migration 0026's `tasks.turn_record`
present). #720 changes nothing the Linux daemon runs. [#726](https://github.com/hherb/kastellan/pull/726)'s one Linux runtime change is on the gliner
worker's **startup-failure path only** (a failed `import torch` under `auto` now exits with
`MODEL_LOAD_FAILED` instead of falling back to cpu); a healthy start is unchanged, so no redeploy is owed.
[#731](https://github.com/hherb/kastellan/pull/731) leaves the **daemon** unchanged (it installs a subscriber, so the fallback never fires);
its only daemon-visible delta is that an early-exit log line is now control-neutralised. `kastellan-cli guard capture`
does gain the line. No redeploy owed. Rootfs images last rebuilt 2026-09-08.

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
> strings at 512 B, which would have broken the question that worked [[plan-text-is-a-defect-source]];
> #720's review round closed the gate script's fail-*open* hole with a `PIPESTATUS[1]` check that made
> it fail *always* (fixed in [#726](https://github.com/hherb/kastellan/pull/726)). **A passing mutation proof is not a review either**
> [[mutation-proof-counts-only-mutants-you-tried]].

> ⚠️ **A forward reference auto-closed issue #718.** #720's body said the gate profile is the acceptance
> test for "whichever PR <closing-keyword> #718", and GitHub's scanner matched it — the fifth
> recurrence of this hazard and a new shape (no negation at all). Never quote the phrase literally. Reopened 2026-09-19. **Run the regex over every PR body and
> commit message before merging**, not only over the negations [[pr-body-not-fixed-autocloses-issue]].

> ⚠️ **A FIXTURE nobody rebuilds is a gate nobody runs** [[stale-fixture-turns-a-gate-into-a-formality]],
> and **a guard built from a census shares the census's blind spot** [[guard-shares-the-census-blind-spot]].

---

## Current state

### This session (2026-09-20): #725 — a dying worker's last words reach a failing test

PR [#731](https://github.com/hherb/kastellan/pull/731). `worker_stderr::emit_early_exit_report` is
the one producer: it logs through `tracing` as before **and** `eprintln!`s the report when
`tracing::dispatcher::has_been_set()` is false.

- ⚠️ **`eprintln!` is load-bearing, not style.** libtest captures through
  `std::io::set_output_capture`, which the `print!`/`eprint!` **macros** consult and the
  `Stdout`/`Stderr` handles do not — a `writeln!(std::io::stderr(), …)` never appears under the
  failing test that needs it.
- ⚠️ **The marker `[worker-early-exit]` must not begin with `[SKIP]`/`[WARN]`/`[E2E]`.**
  `run-e2e-gate.sh` greps those anchored at line start and asserts zero `[WARN]`, and every profile
  passes `--nocapture`, so a borrowed marker would turn profiles red for working suites.
- ⚠️ **"Production" is not "the daemon".** The daemon installs a subscriber first thing in `main`,
  so it is unchanged — but `kastellan-cli` installs none anywhere and `guard capture` dispatches the
  real web-fetch worker, so **that shipped binary gains the line** (intended: no log to read).
- The whole report is now `neutralise_controls`'d. The tail was already stripped entering the ring,
  but `program`/`method` are interpolated raw and **`method` can be model-authored**
  (`qualified_method` returns `None` outside the advertised set, then the planner's string passes
  verbatim) — a `\n` could have forged a column-0 line in a gate log.
- ⚠️ **Review round (three read-only reviewers, own worktrees) found the suite could not prove its
  own central claim.** It merged the child's stdout and stderr, but a **captured `eprintln!` returns
  on stdout** (libtest reprints it into the failure block) and a `writeln!(stderr)` on **stderr** —
  so that mutant **survived**. Streams are read separately now; it dies. Also fixed in-branch: the
  census was **29 of 30**, not the issue's 27 of 28; `RUST_TEST_NOCAPTURE` is inherited and read as
  `!= "0"` (even empty disables capture) so the child `env_remove`s it; the `#[ignore]`d
  fail-on-purpose fixtures would have shown as two red tests under the documented
  `cargo test -- --ignored` recipe, so they are env-guarded too; the marker had **no** end-to-end
  pin; and the gate-marker test used `assert_ne!` where the gate greps a line-start prefix.
- ⚠️ **`block_in_place` does NOT hand off to another thread** — it runs the closure on the current
  one. Measured: under `rt.block_on` it reports the **test thread's** `ThreadId`; under
  `tokio::spawn` it differs. So the old #666 comment claiming otherwise is wrong, and the fixture now
  dispatches from a spawned task to cross the boundary at all. **That wrong comment is now also
  corrected in the tree** (`worker_early_exit_diagnostic_e2e.rs`) — the second review round found
  the refutation had been written into HANDOVER while the comment itself sat untouched two files
  away, which is the shape that makes a known-wrong claim outlive the session that disproved it.

#### Second review round (2026-09-20, four reviewers + controller verification)

⚠️ **The PR's central *security* claim was untested, and the mutant SURVIVED.** Deleting
`neutralise_controls` from `emit_early_exit_report` passed every test in the branch — proved by
running it, not by reading. The fixture's method was the literal `"anything"`, so no test anywhere
fed a control character down the one path that is interpolated raw. Now closed both ways:

- The e2e dispatches a **`HOSTILE_METHOD`** (`ESC[31m` + `\n[WARN] …FORGED-GATE-LINE`) and **both**
  parents assert, on their own channel, that the text survives while its *effects* do not — a
  **positive control** (the text arrived, so the other checks cannot pass vacuously), no ESC, and
  no forged **column-0** line. Checking *position*, not absence: neutralisation maps the class to a
  space, so the correct outcome is the phrase sitting mid-line.
- ⚠️ **`format_early_exit_stderr_fallback` now neutralises too.** The one-line property belonged to
  `emit_early_exit_report`'s *call order*, but the formatter is `pub` — and
  [#730](https://github.com/hherb/kastellan/issues/730) is a second producer already filed. Same
  drift-between-copies shape as the bwrap-argv pair.
- The marker's own **value** had no assertion: `EARLY_EXIT_STDERR_MARKER = ""` passed all four of
  its tests (`"".starts_with("")`, `contains("")`). Pinned now.
- `contains("1 failed")` also matches `"11 failed"` and **`"1 passed; 1 failed"`** — the last would
  have defeated the one-child-per-fixture rule. Now the full libtest phrase.
- `let _ = set_global_default(…)` → `expect`; `result.is_err()` → `matches!(Protocol(EarlyExit))`
  (the only variant reaching `warn_early_exit`); `is_the_child()` now honours the `1|true|yes|on`
  dialect, so an exported `…FIXTURE=0` no longer *arms* two fail-on-purpose tests.

⚠️ **Two reviewer findings were WRONG and were checked before being carried** —
[[handover-claims-verify-before-carrying]]. One recounted the census as "30 of 31" and flagged four
files: its grep did not strip comments, and `scheduler_step_dispatch_e2e.rs` matches `dispatch (`
only in prose. **29 of 30 is correct.** Another rated the discarded `wait_for_drain` bool CRITICAL
and blocking; `git show main:core/src/tool_host.rs` has the identical line, so it is pre-existing
(#666) and now filed rather than fixed here.

**Mutation proof (4/4 killed, each run):** drop `neutralise_controls` from `emit_early_exit_report`
→ dies on the `tracing` channel; drop it from the formatter → dies on the new unit test; marker
`""` → dies; marker `"[WARN] early-exit"` → dies. Restores verified by **sha256**, and the git
**index** checked clean [[mutation-testing-contaminates-the-index]].

**Filed, not fixed here:** [#732](https://github.com/hherb/kastellan/issues/732) (a timed-out drain
is reported as "wrote NOTHING", pre-existing #666),
[#733](https://github.com/hherb/kastellan/issues/733) (`eprintln!` SIGABRTs a release
`kastellan-cli` on a broken pipe — `panic = "abort"`),
[#734](https://github.com/hherb/kastellan/issues/734) (**`has_been_set()` asks whether a subscriber
exists, not whether the WARN will be delivered** — a target-scoped `RUST_LOG` in the operator
overlay silences *both* channels; verified against tracing-subscriber 0.3.23, whose default
directive applies only to an *empty* env string).
- **No security fail-open** (reviewer A): the tail is fully neutralised via `push_trimmed`, a
  compromised worker cannot forge an evidence line, and nothing new reaches the planner, a returned
  error, or an audit row.
- `format_unpiped_early_exit_report`'s arm is **unreachable today** (all four backends pipe stderr) —
  said outright in the doc so its unit test is not mistaken for coverage of the arm.
- **Filed: [#730](https://github.com/hherb/kastellan/issues/730)** — the persistent-worker death
  report (`worker_lifecycle/persistent.rs`) has the same defect one layer over, so **Matrix and
  egress workers still die silently in a test binary**.

### Previous (2026-09-19): #699 + #700 — the planner sees what it asked for

PR [#728](https://github.com/hherb/kastellan/pull/728). Each `plans_so_far` step outcome carries
`"call": {tool, method, parameters}`; `call` and `decision` pass the sink screen under `Strict`
whatever tool the step names (they are model text), rendering `[withheld: failed injection screen]`
on a block, with `tier: "sink"` rows gaining `part: decision|call|outcome`.

- **The call survives elision** — what was asked is what stops a repeat — and calls' bytes come off
  `PLANS_SUMMARY_BUDGET` before outputs compete; `call::apply_call_budget` then drops the **oldest
  calls' `parameters`** if the calls alone still overrun.
- ⚠️ **Two review rounds found one real fail-open and two tests weaker than their names.** A call's
  deepest `parameters` level was rendered but never screened (`render` prunes from the parameters'
  root, `screen_text` walked them one level down, both stop at `MAX_WALK_DEPTH`) — out of reach only
  because serde_json's recursion limit (128) sits below it, i.e. **the invariant rested on an
  unrelated parser's constant**. A hardening that rewrites screened text must **ADD readings, never
  replace them**. The budget test was satisfied by the *wrong* order too, and `apply_call_budget`'s
  `after < before` guard (a `parameters: {}` call *grows* when dropped) had no test.
- **Deferred: [#729](https://github.com/hherb/kastellan/issues/729)** — `decision` is neither clamped
  nor counted, so the one always-present part of the summary is unbounded.
- **Not yet measured live.** Re-run a multi-search mail question in a **fresh DM room**; task 186's
  dropped `has_attachment` is the shape to look for.

### Previous (2026-09-19, later): #677 and #560 closed by live measurement — no code change

The operator sent the DMs on the DGX (`main` as of #709; binaries verified byte-identical to
`target/release/`). **Every figure below is from the `audit_log` rows, not from the replies.**

- **#677 passes its own acceptance.** Task 189 (DM 1): 5 plans, 8 dispatches, all `ok`: 2 searches,
  3 `mail.get_message` by real id, 3 `mail.get_attachment_text` by exact filename, no `shell.exec`.
  Task 190 (the follow-up): **1 plan, 0 dispatches, 29 s**, `conversation_task_ids: [189]`, floor
  `Personal` via `conversation_inherited`. A third follow-up (191, "how much did they cost?") read
  `[189, 190]`, also 1 plan. The operator confirmed the facts and that the chat looked normal.
- **#560 did not recur.** Task 192, its original Qantas question, ran in a **fresh room**
  (`conversation_task_ids: []`, so nothing carried over). It passed a real id first time. Before #702:
  2 of 2 runs of the Qantas question made up an id. After: 0 of 3 runs across both mail questions
  (187/189 the flight-bookings one, 192 the Qantas one).
- ⚠️ **A live re-measure of a single-question issue must use a fresh DM room** (or wait out the 5 h
  window). Since #709 a same-room question inherits the prior turns' calls, which would void the test.

### Earlier (2026-09-19): #719 — the gliner tier died at `import torch` on macOS, twice

PR [#726](https://github.com/hherb/kastellan/pull/726) (full prose:
[`archive/handover_20260919_699_pre-prune.md`](archive/handover_20260919_699_pre-prune.md)).

- **Two causes, both inside `import torch` (torch 2.13):** `os.getcwd()` → EPERM under Seatbelt
  (fix: `macos_seatbelt::WORKER_CWD = "/"`, pinned on both backends by
  `worker_starts_in_root_whatever_the_parents_cwd`), and torch's compile cache at import
  (fix: host-mode gliner opts into `ephemeral_scratch`; `scratch.py` points
  `TORCHINDUCTOR_CACHE_DIR`/`TMPDIR`/`HOME` there). The second would have broken production
  host-mode gliner on macOS too.
- ⚠️ **Import order is load-bearing:** `main()` is exactly `apply_worker_scratch(); _serve()`, and
  the model import lives inside `_serve()`; `tests/test_scratch.py` pins both halves.
- ⚠️ **gliner is the first WARM worker on `ephemeral_scratch`** — acceptable only because it keeps
  no request data on disk; check the same property before opting in another.
- ⚠️ **`run-e2e-gate.sh` could never pass** (`PIPESTATUS` reset by the assignment); fixed and now
  run for real against a fake `cargo` by `gate_script_tests/run.rs`.
- ⚠️ **After mutating a `.py`, delete its `__pycache__`** — a same-size mutant restored within
  the same second left a *valid* `.pyc` the jailed worker ran [[mutation-testing-leaves-stale-pyc]].
- Diagnosis took one line each (a temporary `tracing_subscriber`); making that permanent is #725.

### Previous: #720 — one REQUIRE contract, and a gate that fails on zero tests

PR [#720](https://github.com/hherb/kastellan/pull/720) closed #714, #622, #664. What binds:

> **Every gate needs a REQUIRE knob *and* a positive control that fails when zero tests ran.**

- **`tests_common::require::RequireKnob` is the one vocabulary** (flag dialect, skip/fail split,
  out-of-dialect warning, `[E2E]` success marker); the knob is **data**, so a tier is one `const`.
  Knobs: `KASTELLAN_PG_REQUIRE_E2E` (Postgres **and** supervisor), `_SANDBOX_`, `_GUARD_`, `_GLINER_RELEX_`,
  `_MICROVM_`. ⚠️ **Deliberately no umbrella variable** — the Mac has no KVM.
- ⚠️ **A knob alone was never enough (#664):** it fires only inside a test body, so a filtered-out run
  emits no `[SKIP]` and exits 0. `RequireKnob::announce` emits `[E2E]` on the success path under a
  truthy knob, and `scripts/run-e2e-gate.sh <profile>` asserts per-tier `[E2E]` floors, tests passed,
  zero `[WARN]`, and a per-profile `[SKIP]` cap. **Use it whenever a run is meant to be evidence.**
- ⚠️ **No `sandbox` or `container` profile yet — both would be red on every host.**
  `kastellan-sandbox`'s suites hand-roll their skips (#718: **92 sites in 47 files** bypass
  `skip_line` and every knob), and the container helpers have no knob (#722).

### Earlier (2026-09-17): #717 backlog triage, #709 conversational continuity

**#717.** ⚠️ **The ROADMAP is the accurate source; GitHub issues are the stale mirror.** 58 roadmap-era
issues had gone untouched because ~7 % carried a label; `docs/devel/notes/label-backlog.sh` now gives
every open issue one `area:*` plus the `false-green` / `needs-live-host` / `roadmap` themes. `main`
has required status checks (#655).

**#709 (#701).** A finishing channel task writes `tasks.turn_record` = `{calls, data_class}` in the
*same* `finalize` UPDATE; the next task in the same `(channel, peer, conversation)` reads up to 3,
screened and budgeted, and inherits their floor. **Calls, not results** — no tool output crosses a
task boundary. ⚠️ **The window has NO upper bound, deliberately** (a user typing while the bot works
makes a task older than the turn it follows). ⚠️ **The floor comes from every turn LOADED, not those
the screen kept.** Screening is sealed (`view::admitted::Admitted`). ⚠️ **A security property can be
documented, tested, and absent** — `inherit_floor` read the wrong field while three one-leg tests
defended it. **Email is still stateless** (its `conversation` is the message id). Deferred:
#710–#713, #715, #716.

**#702 (#677).** `inner_loop/result_view` is the planner's view of a successful step — pruned,
labelled JSON; identifiers atomic, keys identifier-shaped or absent. ⚠️ **Keys never reach the guard
model** (#703). ⚠️ **A hardening that rewrites screened text must ADD readings, never replace them.**
Budgets: 16 KiB per step, 96 KiB accumulated.

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

**One-liners.** #681: a lean tail plus recovery beat a fat verbatim tail (68.3 % vs 45.8 % recall).
#675: a failed micro-VM boot leaves `console.log` in the kept run dir [[microvm-guest-failures-are-invisible]];
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

3. **Test-harness honesty, now that the gate itself is tested.** [#730](https://github.com/hherb/kastellan/issues/730)
   (the **persistent**-worker death report still never reaches a failing test, so Matrix and egress
   workers die silently — #725's defect one layer over; reuse the `emit_early_exit_report` shape).
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
change that grows a file, in a movement-only commit whose `#[test]` name set is verifiable either
side. Pure test-lifts: `core/src/channel/ask_message.rs` 956, `workers/mail/src/handler.rs` 670,
`sandbox/src/linux_firecracker/plan.rs` ~1160 (DGX-gated), `core/tests/guard_tier_e2e.rs` 1558+
(#639), `core/src/workers/gliner_relex/tests.rs` 1177. Clean seam: `core/src/scheduler/asks.rs` 801.
Judgement first: `db/src/asks.rs` 1127, `db/graph.rs` 926, `llm-router/src/config.rs` 843. Also over
cap: `core/src/scheduler/inner_loop.rs`, `core/src/channel/bus.rs`, `workers/matrix/src/sdk_live.rs`,
`llm-router/src/messages.rs`, `core/src/main.rs`, `tests-common/src/microvm/mod.rs`,
`tests-common/src/microvm/container.rs`, `tests-common/src/require.rs` 635, `sandbox/tests/macos_smoke.rs`
~450.

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
| **Mac** ([#731](https://github.com/hherb/kastellan/pull/731), #725 — **post-`/fixall`, the gate that stands**) | branch tip (2nd review round) | **4381 / 0 / 31**, 180 suites, `TEST_EXIT=0`, 0 `[WARN]`, **`[SKIP]` 12**. +1 over the row below, reconciled exactly: the one new unit test (`the_stderr_fallback_neutralises_a_model_authored_control_character`); the round's other work added **assertions**, not tests, so `ignored` is unchanged at 31. ⚠️ **`[SKIP]` 23 → 12 is an improvement, not drift** — run with `KASTELLAN_PG_BIN_DIR` set, so **zero** "no Postgres install found" (the false-green tell); the 12 remaining are all legitimately opt-in or unavailable (Apple `container` ×8, gliner ×4). Skip-as-pass means those 11 newly-*running* tests move no count, which is exactly why the count alone was never the evidence. Sources **sha256-verified unchanged across the whole sweep** [[never-edit-tree-during-a-sweep]] — an earlier sweep was killed and restarted after two files were edited mid-run. DGX not re-run this round (no Linux-only code touched) | exit 0, cold (27 `Checking kastellan` lines, dedicated `CARGO_TARGET_DIR`), zero warnings | **12** Mac |
| **Mac + DGX** ([#731](https://github.com/hherb/kastellan/pull/731), #725 — 1st review round; superseded by the row above) | branch tip (post-review) | **Mac 4380 / 0 / 31**, 180 suites, `TEST_EXIT=0`, 0 `[WARN]`, **`[SKIP]` 23** (baseline parity). **DGX 4515 / 0 / 63**, 180 suites, `TEST_EXIT=0`, `[SKIP]` 4. Both = +5 passed / +2 ignored, reconciled exactly: Mac 4375+5; DGX 4501 + 4 (#728 round 1) + 5 (round 2) + 5. ⚠️ **The first Mac sweep was a false green** — it matched the predicted total while **339 of its 361 `[SKIP]`s were "no Postgres install found"**, because skip-as-pass counts as passed. Set `KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"` on the Mac or the sweep is not evidence | exit 0, cold (27 `Checking kastellan` lines, `CARGO_TARGET_DIR=$HOME/.cargo-clippy-725`), zero warnings | **23** Mac, **4** DGX (gliner opt-in) |
| **Mac + DGX** ([#728](https://github.com/hherb/kastellan/pull/728), #699/#700) | branch tip (2nd review round) | **Mac 4375 / 0 / 29**, 179 suites, `TEST_EXIT=0`, 0 `[WARN]` — the round's 4370 + **5** (deep-parameter screen, multibyte clamp, the `{}`-parameters grow guard, the framing constant, the step-less outcome). Round 1 at `ce8bc49b`: Mac **4370 / 0 / 29** = #726's 4347 + 5 (grepped between `64d483e8` and `main`) + 18 new. **DGX** full sweep at `299ce103` (before round 1's +4): **4501 / 0 / 61**, 179 suites, exit 0 (= 4482 + 5 + 14) — ⚠️ **not re-run for round 2** (Mac-only changes, but `summary` is platform-neutral, so the DGX number is simply older) | exit 0, cold (27 `Checking kastellan` lines, `CARGO_TARGET_DIR=$HOME/.cargo-clippy-pr728`) at the round-2 tip | **4** DGX (gliner opt-in) |
| **Mac + DGX** ([#726](https://github.com/hherb/kastellan/pull/726), #719) | `64d483e8` (branch tip) | **DGX 4482 / 0 / 61**, 179 suites, `TEST_EXIT=0`, 0 `[WARN]` — **exactly** 4453 (#709) + 24 (#720: 23 `#[test]` + 1 doc-test, never run on the DGX until now) + 5 (this PR), each of the 5 grepped `ok` by name. **Mac 4347 / 0 / 29**, 179 suites — **exactly** #720's 4342 + 5. ⚠️ The sweep itself reported **3 gliner failures that were my own mutation testing**: a same-size Python mutant restored within the same second left its `.pyc` cached *and valid*, and the jailed worker (read-only src) ran it — confirmed by disassembling the cached `main()` [[mutation-testing-leaves-stale-pyc]]. With `__pycache__` cleared the **sweep-built** binaries pass 5/5, 16/16, 6/6 under the REQUIRE knob; source unchanged since the sweep built them. **Also:** `gliner` gate profile passes as evidence on both hosts (first time); DGX gliner ENABLE suites 16/16, 6/6; pytest 71 on both | **exit 0 on both hosts**, zero warnings, 27 `Checking kastellan` lines each, dedicated `CARGO_TARGET_DIR` | **4** DGX (gliner opt-in, ENABLE unset), **23** Mac |
| **Mac** ([#720](https://github.com/hherb/kastellan/pull/720)) | branch tip | **4322 / 8 / 29**, 179 suites, `TEST_EXIT=101`: 3 were #719, 5 were #548/#676 pool contention (pass individually), so effectively **4327 / 3 / 29**. +12 over the row below, reconciled. ⚠️ **Its first sweep was a false green:** `cargo test --workspace` fails fast, and one suite aborted it after 39 of 179 suites. **Use `--no-fail-fast`.** The DGX leg was never run | exit 0, 27 crates, cold (`CARGO_TARGET_DIR=$HOME/.cargo-clippy-720`) | 23 |
| **Mac + DGX** ([#709](https://github.com/hherb/kastellan/pull/709), third review round) | `d6698013` | **DGX 4453 / 0 / 61**, **Mac 4318 / 0 / 29**, both 179 suites, `TEST_EXIT=0`; +10 on each, each new test grepped out of the DGX log by name | exit 0 on both hosts, 27 crates | 4 DGX (gliner), 15 Mac |

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

- **[#731](https://github.com/hherb/kastellan/pull/731)** — a dying worker's last words reach a
  failing test, not just a daemon (#725). Filed #730.
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
