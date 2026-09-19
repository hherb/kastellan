# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20260919_719_pre-prune.md`](archive/handover_20260919_719_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here.

**Last updated:** 2026-09-19 ·
**Recent PRs, newest first:** [#726](https://github.com/hherb/kastellan/pull/726) (#719, the gliner tier's two macOS import-time deaths + a gate
script that could not pass), [#720](https://github.com/hherb/kastellan/pull/720) (the REQUIRE-knob
contract: #714, #622, #664), [#717](https://github.com/hherb/kastellan/pull/717) (backlog triage),
[#709](https://github.com/hherb/kastellan/pull/709) (#701, conversational continuity),
[#702](https://github.com/hherb/kastellan/pull/702) (#677, the planner's labelled result view),
[#694](https://github.com/hherb/kastellan/pull/694) (#617). **Open issues these filed:**
[#725](https://github.com/hherb/kastellan/issues/725) (from #719);
[#718](https://github.com/hherb/kastellan/issues/718), [#721](https://github.com/hherb/kastellan/issues/721)–[#724](https://github.com/hherb/kastellan/issues/724) (from #720);
[#710](https://github.com/hherb/kastellan/issues/710)–[#716](https://github.com/hherb/kastellan/issues/716) (from #709);
[#698](https://github.com/hherb/kastellan/issues/698)–[#700](https://github.com/hherb/kastellan/issues/700),
[#703](https://github.com/hherb/kastellan/issues/703)–[#705](https://github.com/hherb/kastellan/issues/705), localmail
[#364](https://github.com/hherb/localmail/issues/364) (from #702); [#693](https://github.com/hherb/kastellan/issues/693),
[#695](https://github.com/hherb/kastellan/issues/695)–[#697](https://github.com/hherb/kastellan/issues/697) (from #694);
[#691](https://github.com/hherb/kastellan/issues/691) (from #692). ·
**The DGX runs `main` as of #709**, redeployed 2026-09-17 via `scripts/upgrade_from_git.sh` and
verified (installed binaries byte-identical, units active, migration 0026's `tasks.turn_record`
present). #720 changes nothing the Linux daemon runs. [#726](https://github.com/hherb/kastellan/pull/726)'s one Linux runtime change is on the gliner
worker's **startup-failure path only** (a failed `import torch` under `auto` now exits with
`MODEL_LOAD_FAILED` instead of falling back to cpu); a healthy start is unchanged, so no redeploy is owed. Rootfs images last rebuilt 2026-09-08.

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

### This session (2026-09-19): #719 — the gliner tier died at `import torch` on macOS, twice

PR [#726](https://github.com/hherb/kastellan/pull/726). The Mac sweep had been red on `main` since #651's security bump pinned **torch 2.13**
(2026-09-02), which does two new things **while `import torch` runs**. Each killed the sandboxed
worker before it answered, and each surfaced only as `Protocol(EarlyExit)`.

- **Cause 1 — `os.getcwd()` → EPERM under Seatbelt.** `sandbox-exec` restricts but does not move a
  process, so the worker inherited the parent's cwd. On macOS `getcwd()` needs read access to the
  directory it names; under `cargo test` the cwd is the crate dir, which no policy grants. **Fix:**
  `macos_seatbelt::WORKER_CWD = "/"` (`cmd.current_dir`). Grants nothing new — the base profile
  already allows `file-read*` on the literal `/`. Production never saw it: launchd starts the daemon
  in `/` (its spec sets no `working_dir`); bwrap keeps the old cwd only if it is mapped, else tries
  the jail's `$HOME`, else `/` — and Linux `getcwd()` needs no read access anyway. Pinned on **both**
  backends by `{macos,linux}_smoke::worker_starts_in_root_whatever_the_parents_cwd`, each refusing to
  run from `/` (a vacuous fixture) rather than pass.
- **Cause 2 — torch creates its compile cache at import.** `TORCHINDUCTOR_CACHE_DIR=/tmp/torchinductor`
  works on Linux (bwrap's per-spawn tmpfs) and is unwritable under Seatbelt. **This one would also
  have broken production host-mode gliner on macOS.** **Fix:** the existing #283 mechanism — the
  host-mode entry sets `ephemeral_scratch: true` (a no-op on Linux), and the worker's new pure
  `scratch.py` points `TORCHINDUCTOR_CACHE_DIR`/`TMPDIR`/`HOME` into `KASTELLAN_WORKER_SCRATCH`.
  ⚠️ **Import order is load-bearing:** `main()` is now exactly `apply_worker_scratch(); _serve()`, and
  `from .model import GlinerModel` lives inside `_serve()`. `tests/test_scratch.py` pins both halves
  (importing `__main__` loads no torch — checked against a fake `torch` shadowing the real one — and
  the call order); CI's no-torch job runs it too.
- ⚠️ **gliner is the first WARM worker to opt into `ephemeral_scratch`.** Its dir lives as long as the
  worker, not one request — the same as the tmpfs a warm Linux worker keeps. Acceptable because the
  worker keeps no request data on disk by design (library caches and temp files land there), and a
  warm process already carries cross-request state in memory, so it adds no new channel. The
  `ToolEntry.ephemeral_scratch` doc says so and says to check the same property before opting in another.
- ⚠️ **`scripts/run-e2e-gate.sh` could never pass, on any profile.** `TEST_EXIT="${PIPESTATUS[0]}"` is
  itself a command, so it reset `PIPESTATUS`, and the next line's `${PIPESTATUS[1]}` died under
  `set -u` straight after every run. Nothing ever ran the script: `gate_script_tests` only parses
  its table. **Fixed** (copy the array once) and pinned by `gate_script_tests/run.rs`, which runs the
  real script against a fake `cargo` on `PATH` — pass, zero-tests (#664's shape) and cargo-failed.
  The mutant reading tee's status instead of cargo's is killed. **The `gliner` profile then passed as
  evidence for the first time — Mac 5 tests, DGX 4 (the container variant is macOS-only) — and the
  DGX gliner tier, a held `[SKIP]` in every recent gate, ran for real (16/16, 6/6 with ENABLE).**
- **Review round (one read-only reviewer, own worktree): no bug, no security regression — but one
  false comment and two surviving mutants.** The false comment claimed a `linux_smoke` twin that did
  not exist; it does now, and passes under real bwrap. ⚠️ **Both mutants were in the import-order
  guard:** a *guarded* `try: import torch` passed on CI's no-torch runner (the `sys.modules` check is
  trivially true there), and swapping the two calls in `main()` passed everywhere. Killed by
  shadowing torch with an empty fake package on the subprocess's `PYTHONPATH` (a real signal on
  every host) and by splitting `main()` into `apply_worker_scratch(); _serve()` with an order test.
- ⚠️ **My own mutation testing then turned the full Mac sweep red — through the bytecode cache.**
  Mutant B swapped two lines (same size) and was restored by `cp` within the same second, so the
  `.pyc` the mutant run wrote still matched the restored source (a pyc checks **whole-second** mtime
  + size). The jailed worker cannot rewrite bytecode, so every sandboxed spawn ran the mutant and
  died exactly like the original bug. I first blamed feature unification (different test-binary
  hashes) — wrong; running the *older* binary showed the build was irrelevant. Then an unsandboxed
  manual run created `/tmp/torchinductor` on the host, which made the jailed mutant pass: a false
  green hiding the cause. Settled by disassembling the cached `main()`. **After mutating a `.py`,
  delete its `__pycache__` as part of the restore** [[mutation-testing-leaves-stale-pyc]].
- **Second review round (4 parallel reviewers; `/fixall`): no bug; everything found is fixed in
  the PR, nothing filed.** A #719-shaped failure now says so instead of dying as a bare `EarlyExit`:
  - the model import in `_serve()` sits inside the structured-error path (on macOS `auto` resolves
    to cpu without touching torch, so *that* import is where torch first loads);
  - Linux `auto` no longer swallows a failed `import torch` (it fell back to cpu, then the model
    import failed again naming a half-initialised module);
  - `scratch.py::scratch_problem` refuses a named scratch dir that is relative, missing or
    unwritable, at startup. Under Seatbelt `os.access` answers correctly: the `gliner` profile
    passed as evidence afterwards (Mac 5/5).
  Also: `EphemeralScratch::drop` logs a failed removal; a core test pins both Python copies of
  `KASTELLAN_WORKER_SCRATCH` to the Rust constant; `run-e2e-gate.sh` refuses an empty `MAX_SKIP`;
  `run.rs` gained no-evidence, **wrong-tier evidence** and `[WARN]` cases (the tier-anchor and
  WARN-rule mutants are killed); two stale comments corrected. Python mutants restored with
  `__pycache__` cleared. pytest now **81** in the venv, **18** in CI's no-torch job.
- **ROADMAP 605 → 255 lines**: the 2026-09-14 prune had condensed the guard-tier entry's header and
  left its 351-line body behind. Removed only after checking it **verbatim and contiguous** in
  `archive/roadmap_20260914_pre-prune.md` (0 of 344 non-blank lines missing); its one open item,
  #597, moved into the summary line.
- **Diagnosis took one line each**: a temporary `tracing_subscriber` in the failing test printed the
  worker's full traceback, which #666 already logs at `WARN` — but 27 of 28 worker e2e suites
  install no subscriber, so it goes nowhere. Filed as [#725](https://github.com/hherb/kastellan/issues/725),
  with the trap the obvious fix walks into (a hidden subscriber breaks
  `worker_early_exit_diagnostic_e2e`).
- **Also:** #718 reopened (see the header); rust-analyzer's `cargo check` held the build lock
  again — kill that child, not the IDE.

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

1. **[#677](https://github.com/hherb/kastellan/issues/677) — re-measure it live; the DGX already
   carries #709.** ⚠️ **Needs the operator: the two DMs are sent from `@horst`** (the issue's own
   script: "What are my 3 most recent flight bookings, and how much did they cost?", then "From where
   to where did the last 3 flight bookings go (details in the pdf attachment!)"). Ask at session
   start. **Acceptance: the follow-up needs one plan, not six**, and reads the prior turn's calls.
   Then read `tasks.turn_record`, the follow-up's `conversation_task_ids`, and its `plan.formulate`
   rows. ⚠️ **Ask how the chat looked before blaming the change under test**
   [[channel-dm-tasks-are-stateless]]. Until that run passes, #677 stays open.

2. **The #677 follow-ups, each measured by the same live question.** [#699](https://github.com/hherb/kastellan/issues/699)
   (the planner never sees its own prior steps' tool/method/parameters), [#698](https://github.com/hherb/kastellan/issues/698)
   (`mail.search` cannot express a filter-only search; with localmail #364),
   [#700](https://github.com/hherb/kastellan/issues/700) (`plan.decision` reaches the prompt unscreened).
   ⚠️ **#560 (fabricated `message_id`) is worth re-measuring, not re-describing** — #702 removed the
   mechanism its lead named.

3. **#702 follow-ups.** [#703](https://github.com/hherb/kastellan/issues/703) — ⚠️ **the guard model
   never sees object keys**; any new worker passing a third-party JSON object through reopens it
   silently (needs a DGX guard calibration run). [#705](https://github.com/hherb/kastellan/issues/705),
   [#704](https://github.com/hherb/kastellan/issues/704). The localmail changes the operator offered
   (2026-09-14: ordered headers, 4xx on cursor restart, filter-only search, compact hits, attachments
   by `message_id` + name, a distinct expired-credential error) are not yet filed on `hherb/localmail`.

4. **Test-harness honesty, now that the gate itself is tested.** [#725](https://github.com/hherb/kastellan/issues/725)
   (a dying worker's last words never reach a failing e2e — the one-line diagnosis of #719, made
   permanent; option 2 fixes every suite in one place). [#718](https://github.com/hherb/kastellan/issues/718)
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

- **[#560](https://github.com/hherb/kastellan/issues/560)** — do **not** close it by rewriting the
  parameter description (#536 did, and both later runs still fabricated). Re-measure live first
  [[tool-output-reaches-planner-key-stripped]] [[opaque-ids-are-unusable-tool-params]].
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
| **Mac + DGX** ([#726](https://github.com/hherb/kastellan/pull/726), #719 — **the gate that stands**) | `64d483e8` (branch tip) | **DGX 4482 / 0 / 61**, 179 suites, `TEST_EXIT=0`, 0 `[WARN]` — **exactly** 4453 (#709) + 24 (#720: 23 `#[test]` + 1 doc-test, never run on the DGX until now) + 5 (this PR), each of the 5 grepped `ok` by name. **Mac 4347 / 0 / 29**, 179 suites — **exactly** #720's 4342 + 5. ⚠️ The sweep itself reported **3 gliner failures that were my own mutation testing**: a same-size Python mutant restored within the same second left its `.pyc` cached *and valid*, and the jailed worker (read-only src) ran it — confirmed by disassembling the cached `main()` [[mutation-testing-leaves-stale-pyc]]. With `__pycache__` cleared the **sweep-built** binaries pass 5/5, 16/16, 6/6 under the REQUIRE knob; source unchanged since the sweep built them. **Also:** `gliner` gate profile passes as evidence on both hosts (first time); DGX gliner ENABLE suites 16/16, 6/6; pytest 71 on both | **exit 0 on both hosts**, zero warnings, 27 `Checking kastellan` lines each, dedicated `CARGO_TARGET_DIR` | **4** DGX (gliner opt-in, ENABLE unset), **23** Mac |
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
