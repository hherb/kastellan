# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20260923_748_pre-prune.md`](archive/handover_20260923_748_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here.
> ⚠️ **Repoint this line in the same commit as the snapshot.** It has been stale twice.

**Last updated:** 2026-09-23 (#748: a `worker-report` gate profile, a run-time check that every
profiled test binary reached the panic hook, and a measured `[panic]` cap) ·
**Recent PRs, newest first:** [#756](https://github.com/hherb/kastellan/pull/756) (#748), [#750](https://github.com/hherb/kastellan/pull/750) (#746, #747, #749),
[#745](https://github.com/hherb/kastellan/pull/745) (#734, #733, #732, #742),
[#743](https://github.com/hherb/kastellan/pull/743) (#737, #738, #739),
[#740](https://github.com/hherb/kastellan/pull/740) (#736), [#735](https://github.com/hherb/kastellan/pull/735) (#730),
[#731](https://github.com/hherb/kastellan/pull/731) (#725), [#728](https://github.com/hherb/kastellan/pull/728) (#699, #700),
[#726](https://github.com/hherb/kastellan/pull/726) (#719), [#720](https://github.com/hherb/kastellan/pull/720) (the REQUIRE-knob contract).
**The #725 → #748 worker-report arc is closed**, #748 being its last piece. Its review residue
is filed as #751–#754, and #748 filed **#755** (evidence markers landing mid-line — `[WARN]` and
`[SKIP]` fail open). Older filings are in the [`archive/`](archive/) snapshots;
**`gh issue list --state open` is the live answer** and the only one worth trusting. ·
**The DGX runs `main` as of #709**, redeployed 2026-09-17. **A redeploy is owed for #743 + #745 +
#750** (diagnostics only; #748 is test-harness only and needs none). Rootfs images last rebuilt
2026-09-08.

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

### This session (2026-09-23): #748 — the last piece of the worker-report arc

Three gaps in what `scripts/run-e2e-gate.sh` demanded. Full prose in the commit message.

- **A `worker-report` profile** selects the five suites that prove a dying worker's last words
  reach a failing test, across **two packages** (`-p kastellan-core -p kastellan-tests-common`;
  cargo accepts it, measured). Only the sandbox knob — one suite needs a sandbox, four are
  hermetic, none needs PG or a guard backend — so unlike `guard-tier` it runs on **both hosts**.
  ⚠️ The issue proposed appending to `guard-tier`; that would have run them only where Shieldstral is.
- **Every test binary a profile runs must reach the neutralising panic hook, checked at RUN time.**
  `install_once()` prints one `[panic-hook]` line per process; the gate splits its log at cargo's
  `Running` headers and refuses any section without one. ⚠️ **A static source scan was the
  approved design and was abandoned mid-task**: suites reach the knob through a dozen indirect
  `tests_common` helpers, so "references a knob helper" needs a list of helper names — a census
  [[guard-shares-the-census-blind-spot]]. The real profile, run **before** the hermetic suites were
  fixed, refused **exactly the four** binaries that never read a knob — including
  `panic_hook_gate_safety_e2e`, whose *child* reads one and whose parent does not.
  ⚠️ **A hermetic suite in a profile must call `panic_hook::install_once()` as the first statement
  of every parent test.** It has no knob to do it implicitly.
- **A 9th profile field, `MAX_PANIC`.** No suite any profile selects has a `#[should_panic]` (the
  issue's ~22 baseline is `src/` unit tests, which `--test` never selects). **Measured 0 on all
  five profiles**: `worker-report`, `pg`, `gliner` (Mac), `guard-tier` and `microvm` (DGX).
  ⚠️ **The cap has no floor and cannot have one** — a binary whose panics went through the
  *default* hook also counts zero. The per-binary hook check is what makes the cap sound.
- ⚠️ **A marker can land MID-LINE under `--nocapture`, and the anchored grep then misses it.**
  libtest's `test <name> ... ` (stdout, no newline yet) and our stderr share one merged pipe. It
  flaked the new check once — one run green, the next identical run red — and for `[panic]` it
  would be a false **green** against the cap. Fixed for the two markers #748 owns:
  `panic_hook::own_line` frames each as `\n<line>\n`, emitted as **one** `write_str` (a single
  write under `PIPE_BUF` to a pipe is atomic). ⚠️ **`eprintln!("\n{x}")` is TWO writes** — literal
  piece and argument — so the frame is built first and printed with `eprint!("{framed}")`. The
  pre-existing `[WARN]`/`[SKIP]`/`[E2E]` share the exposure: **#755**, where `[WARN]` fails open.
- ⚠️ **Found on the way: the `microvm` profile had refused EVERY run since #720** (2026-09-17).
  `firecracker_suites` emits all 15 `--test` targets on **one line**, and the floor check counted
  them with `grep -c`, which counts **lines** — so discovery read 1 against a floor of 12 and the
  script exited 2 before running anything. **No `microvm` gate log existed on either host.** Never
  reached on the Mac (the `os` check refuses first); no test ran it. Fixed (`count_test_targets`
  counts tokens), and `gate_script_tests` now runs the script's **real** discovery through the
  **real** count on the real tree — with the old body it reproduces the DGX refusal on the Mac.
  [[grep-c-counts-lines-not-matches]]
- ⚠️ **And the runtime check then caught a hole in #745's own hook, on its first real run.** The
  `microvm` profile (once runnable) refused **all 15** binaries while printing 65 knob-gated
  `[E2E]` lines: a healthy micro-VM host meets every precondition, never reaches
  `action_reporting_to`, and reads the knob only to announce — via `announce_demanded_to` →
  `raw()`, which did not install the hook. So **every green micro-VM gate run had the default
  hook**, while the docs claimed one chokepoint covered every read. **A knob has TWO doors**
  (`raw()`, `action_reporting_to`); both install now, and `knob_reads_install_the_panic_hook_e2e`
  proves each in its own child (the install is a per-process `Once`). A static scan would have
  passed all 15 — this is the case for choosing the runtime check.
- **Mutants: 7 of 8 killed.** The survivor is `worker-report`'s cap reverting to `any` — kept
  unpinned deliberately, since pinning a measured number in a test is a second copy of the census.
- ⚠️ **On this Mac the `pg`/`gliner` profiles need `KASTELLAN_PG_BIN_DIR`** set to Postgres.app
  v18, or they fail "no Postgres install found" — the memory note says so, and it was still
  forgotten on the first run [[postgres-app-bin-paths]].

### Previous (2026-09-22): #750 — #746, #747, #749 and its review round

Full prose in [`archive/handover_20260923_748_pre-prune.md`](archive/handover_20260923_748_pre-prune.md).
What still binds:

- ⚠️ **Renderers match an exhaustive `TailState`**, deliberately **not** `#[non_exhaustive]`: the
  documented arm-order convention it replaced failed twice out of three renderers.
- ⚠️ **`stderr_is_writable()` takes no fd, deliberately** — a surviving mutant probed `STDOUT_FILENO`,
  which is writable in every test binary. **`POLLERR`/`POLLHUP` are one bit per host**: neither host
  alone proves that mask.
- ⚠️ **A fix for a reviewer's finding carried the next defect, twice**: a harmless-looking hoist of
  `tail.snapshot()` *was* #730; a loud arm put ~125 `error!` lines per failure in a 2 ms poll loop.
  [[a-fix-for-a-reviewers-finding-can-carry-the-next-defect]]
- ⚠️ **Two reviewers in one worktree fabricated a "flake"** (one ran a mutant while the other
  tested). Cargo's `…-c4860b16…` suffix is a *metadata* hash, stable across content changes, so
  "same binary" proves nothing. **Give a mutating reviewer its own worktree, and bracket every sweep
  with a sha of the sources** [[never-edit-tree-during-a-sweep]].
- ⚠️ **Confidently-worded comments were the defect class** (six false ones fixed): the private
  fields of `CapturedTail` are a **clarity** boundary, not a security one — `complete(vec![])`
  forges `KnownSilent` in one call.

### Previous (2026-09-20/22): the #725 → #745 worker-report arc

PRs #731, #735, #743, #745. What still binds:

- ⚠️ **The delivery check must expand at the EMITTER's callsite — `warn_and_fall_back!` is a MACRO
  and must stay one**, and every field the `warn!` carries must be named in the check, **`message`
  included** (forgetting it reopened #734 inside its own fix).
- ⚠️ **`eprintln!` is load-bearing, not style** [[libtest-capture-only-print-macros]], and a
  *captured* one returns on the child's **stdout** — read a child's two streams separately.
- ⚠️ **No marker may begin with `[SKIP]`/`[WARN]`/`[E2E]`** — hence `[worker-failed]`,
  `[worker-death]`, `[worker-down]`, `[panic]` and now `[panic-hook]` (disjoint both ways, tested).
- ⚠️ **`shutdown()` joins the driver thread, and that join is the only proof the report was
  emitted.** `WorkerRetirementCause::from_client_error` is THE census; `ToolHostError::Io` is
  pre-spawn, pinned by a `compile_fail` doctest. `PanicHookInfo` cannot be named (MSRV 1.78), and
  the hook is installed from `action_reporting_to`, **not `action`**.

### Previous (2026-09-17/19): #699/#700, #719, #720, #717, #709, #702

Full text in [`archive/handover_20260921_730_pre-prune.md`](archive/handover_20260921_730_pre-prune.md).

- **#699 + #700** (PR #728) — each `plans_so_far` step carries a screened `"call"`; the **oldest
  calls' `parameters`** drop first under budget. ⚠️ **Hardening that rewrites screened text must ADD
  readings** [[screen-hardening-must-add-readings]]. ⚠️ **Not yet measured live** — needs a
  multi-search mail question in a **fresh DM room** (a same-room one inherits prior calls since #709).
- **#719** (PR #726) — workers start in `/` (Seatbelt `getcwd()` EPERM); host-mode gliner opts into
  `ephemeral_scratch`. ⚠️ **A sandbox that restricts but does not relocate leaks the caller's
  context into the jail.** Delete `__pycache__` after mutating a `.py` [[mutation-testing-leaves-stale-pyc]].
- **#720** — **Every gate needs a REQUIRE knob *and* a positive control that fails when zero tests
  ran.** One `RequireKnob` vocabulary; a tier is one `const`. ⚠️ **No `sandbox`/`container`
  profile yet — both would be red on every host** (#718: 92 hand-rolled `[SKIP]`s; #722).
- **#717** — ⚠️ **the ROADMAP is the accurate source; GitHub issues are the stale mirror.**
- **#709 (#701)** — a channel task writes `tasks.turn_record` = `{calls, data_class}`; the next task
  in the same conversation reads up to 3, screened, and inherits their floor — from every turn
  **loaded**, not those the screen kept. ⚠️ **A security property can be documented, tested, and
  absent** (`inherit_floor` read the wrong field). Email is still stateless.
- **#702 (#677)** — `result_view` is the planner's pruned, labelled JSON view of a step.
  ⚠️ **Keys never reach the guard model** (#703).

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

3. **#755 first — it is the one false-green left in the gate itself.** `[WARN]` (hard zero) and
   `[SKIP]` (capped at 0 on two profiles) can land mid-line and go uncounted. The fix exists
   (`panic_hook::own_line`); route `RequireKnob::announce*`, `skip_line` and
   `warn_if_out_of_dialect` through it, each with an emitter-level "starts with `\n`" test. ⚠️
   `warn_if_out_of_dialect` takes a `&mut dyn Write`, so `writeln!` there is two writes — one
   `write_all` of the framed string. Then the #750 residue, #751–#754.

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
`worker_lifecycle/persistent.rs`); #750 split `worker_stderr/mod.rs` **first** instead. #748
pushed `panic_hook.rs` over (432→584) and split its tests out (357 + 229); it also grew
`scripts/run-e2e-gate.sh` to 561 (shell, not split) and `require.rs` 647→661 (already over).

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
| **Mac** (#748 — **the gate that stands**) | branch tip | **4471 / 0 / 45**, **186** suites, `TEST_EXIT=0`, `[WARN]` **0**, `[SKIP]` **12** (8 container, 4 gliner opt-in), `[panic]` **0**, source sha **identical before and after**. `KASTELLAN_PG_BIN_DIR` set, `--no-fail-fast -- --test-threads=4`, primary checkout. **Delta vs the row below reconciles EXACTLY: +18 passed / +3 ignored / +1 suite** — `panic_hook` +3, `gate_script_tests` +6, `run.rs` +6, and `knob_reads_install_the_panic_hook_e2e` (+1 suite, +3 passed, +3 ignored fixtures). ⚠️ **`[SKIP]` 26→12 is environment** (primary checkout has the gliner `.venv`; container skips 19→8, helpers untouched). ⚠️ **The row below's `[panic]` 20 is not reproducible from any log on disk** — both #750-era sweep logs count 0, anchored or not — so it is recorded as unverified, not as a delta. ⚠️ **Two earlier sweeps this session were DISCARDED**: one had a test file appended under it (the sha bracket caught it, `a5e5…`→`cc49…`), the next was superseded by the two-door fix. **Gate profiles:** `worker-report` ✅ both hosts (Mac 3/3 consecutive after the framing fix), `pg` ✅ 19, `gliner` ✅ 5 (Mac), `guard-tier` ✅ 21 / 44 `[E2E]` (DGX), `microvm` ✅ **30 / 65 `[E2E]` / 15 of 15 hooked** (DGX — its first passing gated run ever). **Every profile's `[panic]` measured 0** | exit 0 **on BOTH hosts**, cold (`CARGO_TARGET_DIR=$HOME/.cargo-clippy-748`), **27** `Checking kastellan` lines each, zero warnings. The DGX run predates the movement-only `panic_hook/tests.rs` split, which the Mac re-linted | **12** Mac |
| **Mac** (#746/#747/#749 — superseded) | — | **4453 / 0 / 42**, **185** suites, `TEST_EXIT=0`, `[WARN]` 0, `[SKIP]` 26 (19 container, 4 gliner opt-in, 3 worktree venv), `[panic]` 20 (unverified, see above). Full row in the archive snapshot | exit 0 both hosts, cold, 27 `Checking kastellan` lines | **26** Mac |

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
| worker-report (5 suites, 2 crates) | 12 | a dying worker's last words reach a failing test; a real broken fd 2, re-exec'd. `bash scripts/run-e2e-gate.sh worker-report` (both hosts) |
| `tests-common` `gate_script_tests` | 22 | the gate script's table **and** its verdict (floors, caps, per-binary hook check), run against a fake `cargo` |

## Key design decisions locked in

**Not restated here — they drift.** Hard constraints: [`CLAUDE.md`](../../../CLAUDE.md) § Hard
constraints; the rest: [`docs/architecture.md`](../../architecture.md) and the ROADMAP.
**The one worth repeating:** worst-case compromise reaches *at most* the agent's own OS user, its own
Postgres role, its own scratch FS, and the allowlisted endpoints for the *one* compromised tool
([`docs/threat-model.md`](../../threat-model.md)).

## Recently merged

Newest first; full prose in the [`archive/`](archive/) snapshots and git history.

- **#748** — a `worker-report` gate profile; the gate refuses any test binary that never reached
  the panic hook (checked at run time); a measured `[panic]` cap (`MAX_PANIC`, measured 0 on all five profiles); a knob read installs the hook through **two** doors, not one; the `microvm` profile runs for the first time since #720.
  Filed #755.
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
