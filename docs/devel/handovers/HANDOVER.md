# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20260926_698_pre-prune.md`](archive/handover_20260926_698_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here.
> ⚠️ **Repoint this line in the same commit as the snapshot.** It has been stale twice.

**Last updated:** 2026-09-27 (#763: the live localmail shape gate joins the evidence machinery;
#765: mail params are checked before the version gate) ·
**Recent PRs, newest first:** [#766](https://github.com/hherb/kastellan/pull/766) (#763, #765), [#764](https://github.com/hherb/kastellan/pull/764) (#760), [#762](https://github.com/hherb/kastellan/pull/762) (#760), [#761](https://github.com/hherb/kastellan/pull/761) (#698, #561), [#758](https://github.com/hherb/kastellan/pull/758) (#755), [#756](https://github.com/hherb/kastellan/pull/756) (#748), [#750](https://github.com/hherb/kastellan/pull/750) (#746, #747, #749),
[#745](https://github.com/hherb/kastellan/pull/745) (#734, #733, #732, #742),
[#743](https://github.com/hherb/kastellan/pull/743) (#737, #738, #739),
[#740](https://github.com/hherb/kastellan/pull/740) (#736), [#735](https://github.com/hherb/kastellan/pull/735) (#730),
[#731](https://github.com/hherb/kastellan/pull/731) (#725), [#728](https://github.com/hherb/kastellan/pull/728) (#699, #700),
[#726](https://github.com/hherb/kastellan/pull/726) (#719), [#720](https://github.com/hherb/kastellan/pull/720) (the REQUIRE-knob contract).
**The #725 → #748 worker-report arc is closed**, #748 being its last piece. Its review residue
is filed as #751–#754 and #757. Older filings are in the [`archive/`](archive/) snapshots;
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

### This session (2026-09-27, latest): #763 + #765 — the mail gate and param order

- **#763, the live shape gate is evidence now.** It moved to its own suite,
  `core/tests/mail_live_shape_e2e.rs` (movement-only commit, byte-identical). New knob
  `KASTELLAN_MAIL_LIVE_REQUIRE_E2E` (tier `live-localmail`, in `KNOBS`) via
  `tests_common::live_localmail::credentials_or_skip`, plus a **`mail-live` profile** (floor 1,
  `MAX_SKIP` 0, `MAX_PANIC` 0 *measured*, `--ignored`). `scripts/mail/live-shape-gate.sh`
  finds the credentials and then **runs the profile**; the profile itself sets no credentials.
  ✅ Green as evidence on the Mac. The negative control holds: with no credentials the run fails
  on the knob, the pass floor and the `[E2E]` floor. **Not yet run on the DGX.**
- **#763, no hand copies.** `API_MAJOR`/`MIN_API_MINOR`/`HIT_FIELDS`/`SNIPPET_CHARS` live in
  `workers/mail/src/localmail_contract.rs`, which the live gate and `mock_localmail`'s tests
  `include!`. ⚠️ **It is compiled three times: `pub const`s only, no `use`, no `//!`.** The live
  gate does not allow dead code, so an unchecked wire constant is a `-D warnings` error (verified).
- **#765** — `handler/request.rs` (pure) parses every call into a typed `Request` **before** the
  version gate; `needs_current_api` is on `Request` and reads the parsed `full_headers`;
  `attach::choose` validates a planner sha (`Selector::Sha(Picked)`). `handler.rs` 555→415.
- ⚠️ **A new `*_or_skip` helper must be classified** in `microvm/guard.rs` (`BANNED_HELPERS` or
  `REQUIRE_AWARE`) — `the_banned_roster_matches_the_helpers_that_actually_exist` refuses it
  otherwise. It caught `credentials_or_skip`.

### Previous (2026-09-26): #760's last piece — `headers=list` (PR #764)

- `full_headers: true` sends `?headers=list` (per occurrence, wire order); `header_list_error`
  **checks and fails closed** (a name-keyed object, a missing block, or a non-`{name,value}` entry
  is a fault; refusals name the structural cause, never served text). The live gate checks one
  entry per occurrence against `full`.

### Previous (2026-09-26): #760 slices D/E (PR #762) and #698 (PR #761) — what still binds

Full prose in git history (PR bodies) and the ROADMAP.

- **Version gate:** `mail.search`, both attachment tools and now header reads refuse below API
  **1.3**, naming the upgrade — **no fallback** (operator's call; an old server answers wrong, not
  loudly). A pass is cached per worker; a refusal is asked again. `Tool` is an exhaustive enum.
- **Slice E:** every search sends `fields` = `search_params::HIT_FIELDS` + `snippet_chars: 120`.
  ⚠️ **A 50-hit page is still ~18 KB, over the 16 KiB step view.**
- **Slice D:** `get_message` writes `index` into each attachment; a message-resolved attachment
  is fetched **by position** and its bytes **hashed against the listed sha** (`Picked::verify_bytes`).
- **Paged text:** 8,000-char pages; `next_offset` is **copied, never computed** (code points); a
  body without paging fields, or inconsistent paging, is a **fault**. ⚠️ Guarantees the 16 KiB
  view only for Latin text.
- **#698:** `query` optional; a blank query with no `sort`/cursor defaults to `date` (localmail
  400s a stated `rank` there). ⚠️ `sort::is_textless` cannot see an operator-only query.
- Live gate: `scripts/mail/live-shape-gate.sh` (reads `kastellan.env.local`; runs with nothing
  exported). Its attachment leg falls back to a `has_attachment` search and **fails** if it finds
  none. Now a gate profile (`mail-live`, #763).

### Previous (2026-09-23, later): #755 — a marker stranded mid-line is refused

Full prose in [`archive/handover_20260926_698_pre-prune.md`](archive/handover_20260926_698_pre-prune.md).
What still binds:

- **The fix is structural:** `run-e2e-gate.sh` refuses a counted marker as the first `[` on a
  `test <name> ... ` line — ⚠️ libtest leaves **two** gaps (before the result word, and between it
  and its `\n`). Counts stay anchored. Tests in `gate_script_tests/mid_line.rs`.
- ⚠️ **#718's unframed `[SKIP]`s outside every profile would be refused** the day a profile selects
  them — frame them when you do.
- ⚠️ **The scan reads bytes:** `grep -a`, verdict from the **exit status**, assertions under
  `LC_ALL=C`. One NUL made GNU grep print nothing and exit 0 (the Mac hid it)
  [[gnu-grep-binary-file-prints-nothing-exit-0]]. Deferred hardening: #759.

### Previous (2026-09-22/23): #748 and #750

Full prose in [`archive/handover_20260923_755_pre-prune.md`](archive/handover_20260923_755_pre-prune.md)
and [`archive/handover_20260923_748_pre-prune.md`](archive/handover_20260923_748_pre-prune.md).
What still binds:

- **Every profiled test binary must reach the panic hook, checked at RUN time.** ⚠️ A hermetic
  suite in a profile must call `panic_hook::install_once()` first in every parent test. A knob has
  **two** doors that install it (`raw()`, `action_reporting_to`). `MAX_PANIC` measured 0 on all five
  profiles; blind before the first knob read (#757).
- ⚠️ **`grep -c` counts lines** — the `microvm` profile refused every run from #720 to #748
  [[grep-c-counts-lines-not-matches]]. `KASTELLAN_PG_BIN_DIR` for `pg`/`gliner` on this Mac.
- ⚠️ **Renderers match an exhaustive `TailState`**, deliberately not `#[non_exhaustive]`.
  `stderr_is_writable()` takes no fd, deliberately.
- ⚠️ **Give a mutating reviewer its own worktree, and bracket every sweep with a sha of the
  sources** — two reviewers in one worktree fabricated a "flake" [[never-edit-tree-during-a-sweep]].

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

1. **Mail worker — a live re-measure, then credentials.** #760, #763 and #765 are done. Run
   `bash scripts/mail/live-shape-gate.sh` **on the DGX** once (only the Mac has run the `mail-live`
   profile). **A live planner re-measure is owed** (operator DMs, a **fresh room**): #728's
   multi-search question, a filter-only one (#698), and a long PDF read across pages (#760). Then
   #673/#674 (both hosts use non-expiring API keys; #674 is about noticing a revocation), and #538
   (the mail worker's second, hand-rolled localmail mock).

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
`workers/mail/src/ids.rs`, `tests-common/src/require.rs`,
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
| **Mac** (#763 + #765 — **the gate that stands**) | branch tip | **4572 / 0 / 47**, **187** suites, `TEST_EXIT=0`, `[WARN]` **0**, `[SKIP]` **23** (unchanged), source sha identical before and after. `KASTELLAN_PG_BIN_DIR` set, `--no-fail-fast -- --test-threads=4 --nocapture`, after `cargo build --workspace`. **Delta reconciles EXACTLY against the row below (4560 + its 2 flakes = 4562 run): +10** — mail worker +2 (199→201: attach +1, handler +1), tests-common +8 (`live_localmail`); **+1 suite** (`mail_live_shape_e2e`, its one test `#[ignore]`d, moved from `mail_daemon_e2e` — ignored unchanged). Plus `mail-live` gate green on the Mac | exit 0, cold (`CARGO_TARGET_DIR=$HOME/.cargo-clippy-763`), **27** `Checking kastellan` lines, zero warnings | **23** Mac |
| **Mac** (#764 review fix-up — superseded) | branch tip | **4560 / 2 / 47**, **186** suites, `TEST_EXIT=101`, `[WARN]` **0**, `[SKIP]` **23** (unchanged), source sha identical before and after. Same recipe. The **2 failures are the known per-test-cluster flake** (`the database system is starting up`: `memory_l0_seed_e2e`, `python_exec_e2e`) — both suites green on an isolated re-run (13/13, 5/5). **Delta reconciles EXACTLY: +4** — mail worker +2 (197→199: `headers` 6→7, handler +1), tests-common +2 (`mock_localmail` 19→21). Live shape gate green on the Mac with the new per-occurrence check | exit 0 (workspace, incremental after a full per-crate run of mail/tests-common/core), zero warnings | **23** Mac |
| **Mac** (#760 `headers=list` — superseded) | branch tip | **4558 / 0 / 47**, **186** suites, `TEST_EXIT=0`, `[WARN]` **0**, `[SKIP]` **23** (unchanged), source sha identical before and after. Same recipe as below. **Delta reconciles EXACTLY: +7** — mail worker +5 (192→197 unit: `headers` 3→6, handler messages +2), tests-common +2 (`mock_localmail` 17→19). Live shape gate green on the Mac with the new `?headers=list` leg | exit 0, cold (`CARGO_TARGET_DIR=$HOME/.cargo-clippy-760h`), **27** `Checking kastellan` lines, zero warnings | **23** Mac |

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
| `core` `mail_live_shape_e2e` (`#[ignore]`) | 1 | our reading of localmail against the **live** service. `bash scripts/mail/live-shape-gate.sh` (Mac; DGX not yet) |
| `tests-common` `gate_script_tests` | 27 | the gate script's table **and** its verdict (floors, caps, per-binary hook check), run against a fake `cargo` |

## Key design decisions locked in

**Not restated here — they drift.** Hard constraints: [`CLAUDE.md`](../../../CLAUDE.md) § Hard
constraints; the rest: [`docs/architecture.md`](../../architecture.md) and the ROADMAP.
**The one worth repeating:** worst-case compromise reaches *at most* the agent's own OS user, its own
Postgres role, its own scratch FS, and the allowlisted endpoints for the *one* compromised tool
([`docs/threat-model.md`](../../threat-model.md)).

## Recently merged

Newest first; full prose in the [`archive/`](archive/) snapshots and git history.

- **[#766](https://github.com/hherb/kastellan/pull/766)** (#763, #765) — the live localmail shape gate gets its own
  suite, a REQUIRE knob and a `mail-live` profile, and reads the worker's wire constants from one
  `include!`d file; mail params are parsed (pure `handler/request.rs`) before the version gate.
- **[#764](https://github.com/hherb/kastellan/pull/764)** (#760, last piece) — `mail.get_message` asks localmail for `?headers=list` and
  passes the per-occurrence list on after a fail-closed shape check; the `{name, values}` reshaping
  is gone; the headers path joins the version gate.

- **[#762](https://github.com/hherb/kastellan/pull/762)** (#760) — the mail worker adopts localmail slices D and E: a `/v1/version` gate (≥ 1.3, no
  fallback), `fields`/`snippet_chars` on every search, `index` on attachments (written by
  `get_message`, taken by both attachment tools, fetched by position), and 8,000-char paged text
  with localmail's `next_offset`. Three movement-only splits first. `headers=list` left on #760.
  Review fix-up: position-fetched bytes hashed against the listing, paging consistency enforced,
  exhaustive gate enum, live gate un-vacuoused. Filed #763.
- **[#761](https://github.com/hherb/kastellan/pull/761)** (#698, #561) — `mail.search` takes a filter-only search: `query` optional, a
  blank query defaults to `sort: "date"` (localmail 400s a stated `rank` there, measured). #561
  measured fixed upstream. `handler.rs` tests split out first. Filed #760.
- **[#758](https://github.com/hherb/kastellan/pull/758)** (#755) — the gate refuses a counted marker stranded mid-line after libtest's `test <name> ... `
  (both gaps); emitter one-write tests; `microvm::require_action_to` no longer skips a non-UTF-8
  knob silently. The issue's "unframed emitters" premise was wrong. Review round: the scan reads
  bytes (`grep -a`, `LC_ALL=C`) and decides on grep's exit status — a NUL had blinded it on GNU
  grep. Follow-ups filed as #759.
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
- **#740, #735, #731, #728, #726, #720, #727, #717, #709, #708, #702, #694, #692, #688, #685** and
  earlier — see git history and the [`archive/`](archive/) snapshots.

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
