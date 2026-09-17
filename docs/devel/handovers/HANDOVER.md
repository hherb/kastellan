# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20260914_677_pre-prune.md`](archive/handover_20260914_677_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here.

**Last updated:** 2026-09-18 ·
**Recent PRs, newest first:** [#720](https://github.com/hherb/kastellan/pull/720) (the REQUIRE-knob contract + the positive control: #714, #622, #664),
[#717](https://github.com/hherb/kastellan/pull/717) (backlog triage + the stale ROADMAP line), [#709](https://github.com/hherb/kastellan/pull/709) (#701, conversational
continuity for channel tasks), [#708](https://github.com/hherb/kastellan/pull/708) (#707, docs only — the Obscura assessment corrected and the V8 bump attempted),
[#702](https://github.com/hherb/kastellan/pull/702) (#677, the planner's labelled result view),
[#694](https://github.com/hherb/kastellan/pull/694) (#617, the bounded request summary),
[#692](https://github.com/hherb/kastellan/pull/692) (#690 + #689 + #686),
[#688](https://github.com/hherb/kastellan/pull/688) (#684 + #687). **Open issues these filed:**
[#698](https://github.com/hherb/kastellan/issues/698), [#699](https://github.com/hherb/kastellan/issues/699),
[#700](https://github.com/hherb/kastellan/issues/700), localmail
[#364](https://github.com/hherb/localmail/issues/364) (from #677);
[#703](https://github.com/hherb/kastellan/issues/703)–[#705](https://github.com/hherb/kastellan/issues/705)
(from #702's second review round); [#710](https://github.com/hherb/kastellan/issues/710)–[#716](https://github.com/hherb/kastellan/issues/716)
(from #709's third review round); [#693](https://github.com/hherb/kastellan/issues/693),
[#695](https://github.com/hherb/kastellan/issues/695)–[#697](https://github.com/hherb/kastellan/issues/697)
(from #694); [#691](https://github.com/hherb/kastellan/issues/691) (from #692);
[#721](https://github.com/hherb/kastellan/issues/721)–[#724](https://github.com/hherb/kastellan/issues/724)
(from #720's review round). ·
**The DGX runs `main` and carries #709**, redeployed 2026-09-17 via `scripts/upgrade_from_git.sh`. Verified rather than assumed: checkout at `main`, `target/release/kastellan` rebuilt, the installed copy byte-identical to it (matrix worker sha256 matching on both sides), all three units active, and **migration 0026's `tasks.turn_record` column present in the live DB** — that last one is what would silently void a #677 re-measure. Rootfs images last rebuilt 2026-09-08.

> **Header convention (since 2026-09-11, after three recurrences).** This header names **PRs and
> issues only — never a branch name, a HEAD sha, or the word OPEN.** A merge falsifies those with no
> actor in between; a PR number it cannot. **The tip and the open set are one command each:**
> `git log --oneline -1 origin/main` and `gh pr list --state open`. Run them before trusting a word
> of this file.

> ⚠️ **An issue's own census can be wrong, and so can its diagnosis — read the rows, not the
> issue.** #679 named 7 call sites (12), #690 named 4 subprocesses (10). **#677 called task 186's
> iterations 2–4 "near-duplicate searches"; the audit rows show iteration 2 was a schema rejection**
> (`missing field query`), and the real cause was nowhere in the issue: the planner's view of every
> result was the injection guard's flattening, which drops keys, numbers and booleans.
> [[issue-as-filed-can-carry-a-regression]]

> ⚠️ **An approved design can carry a regression, and so can the fix for a reviewer's finding.**
> #677's approved spec capped every string at 512 B — which would have cut attachment text to 512 B
> where the planner had 4 KiB, breaking the question that *worked*. Caught while writing the plan,
> by reading what `mail.get_attachment_text` returns. **Before implementing a budget, list the real
> result shapes it will meet, including the success path.** [[plan-text-is-a-defect-source]]

> ⚠️ **A passing mutation proof is not a review.** Every planned mutant in #677 died; a read-only
> reviewer then found two *security* gaps (keys never reach the guard model; nothing screened below
> depth 1) and two surviving mutants in the search. [[mutation-proof-counts-only-mutants-you-tried]]

> ⚠️ **A FIXTURE nobody rebuilds is a gate nobody runs** (the macOS container image was 69 days
> stale behind eight green e2es) [[stale-fixture-turns-a-gate-into-a-formality]], and **a guard
> built from a census shares the census's blind spot** [[guard-shares-the-census-blind-spot]].

---

## Current state

### This session (2026-09-17): one REQUIRE contract, and a gate that fails on zero tests

PR [#720](https://github.com/hherb/kastellan/pull/720) — closes [#714](https://github.com/hherb/kastellan/issues/714),
[#622](https://github.com/hherb/kastellan/issues/622), [#664](https://github.com/hherb/kastellan/issues/664).
The false-green-gate cluster, fixed as **one contract** rather than three patches, because fixing it
one issue at a time is what kept regenerating it.

> **Every gate needs a REQUIRE knob *and* a positive control that fails when zero tests ran.**

- **`tests_common::require::RequireKnob` is the one vocabulary** — flag dialect, skip/fail split,
  out-of-dialect warning, and the new `[E2E]` success marker. The knob is **data**
  (`RequireKnob::new(env, tier)`), so a tier is one `const` and the panic still names the right
  variable. **Two hand-rolled copies folded in:** gliner (#653) and micro-VM (#667), plus three tiers that
  had no knob at all (Postgres, sandbox, guard) — the review round corrected "three copies". The micro-VM copy had *already* diverged once — it called
  `unmet_action` bare while its sibling warned, so `=y` silently degraded to `Skip`.
- ⚠️ **A knob alone was never enough, and that is the whole of #664.** It fires only from inside a
  test body, so a **filtered-out** or **renamed-out** run emits no `[SKIP]` and exits 0. Inferring
  "it ran" from the absence of a `[SKIP]` is unsound. `RequireKnob::announce` emits `[E2E]` on the
  **success** path and **only under a truthy knob**, so every such line means *a demanded
  precondition was actually met here* — and `scripts/run-e2e-gate.sh` asserts a **per-tier** floor on
  that count, plus tests passed, zero `[WARN]`, and a per-profile `[SKIP]` cap.
- ⚠️ **Measured, both directions, on `guard_tier_e2e` against a host with no Postgres:**

  | | `[SKIP]` | `[E2E]` | result | exit |
  | --- | --- | --- | --- | --- |
  | no knob | 11 | 0 | **`21 passed`** | **0** ← the false green |
  | knob set | 0 | **22** | `10 passed; 11 failed`, each naming the knob | **101** |
  | healthy host + knob | 0 | **44** | `21 passed` | 0 |

  The 44 reconciles exactly: 11 PG-dependent tests × 4 preconditions; the other 10 are hermetic.
  **That "21 passed / exit 0" row is precisely what #622 said `bootstrap()` could report.**

  ⚠️ **The middle row is a REVISED measurement; the first draft had `[E2E] 0` and `exit 1`, and
  both were wrong.** `bootstrap()` checks supervisor → sandbox → Postgres, so on a host with a
  working launchd and Seatbelt the first two announce *before* the PG lookup panics: 11 + 11 = 22,
  with `Postgres-backed` and `guard-tier` at 0. And cargo reports a test-binary failure as **101**,
  not 1. Re-measured directly with `KASTELLAN_PG_BIN_DIR=/nonexistent/pg/bin` and all three knobs
  set. The review caught it by reading the ordering against the claim — a reminder that a
  measurement table is as falsifiable as the code, and this one was the PR's headline evidence.
  It is also the per-tier floors working as intended: `guard-tier`'s profile demands
  `Postgres-backed=1` and `guard-tier=1`, both 0 here, so the gate fails for the right reason.
- **Negative control on the control:** a deliberate name-filter typo gives `0 passed; 21 filtered
  out`, `cargo exit 0` — and `run-e2e-gate.sh` **exits 1** naming #664's shape. Checked the script's
  own exit status directly, not through a pipe: ⚠️ **the Bash tool runs zsh, whose arrays are
  1-indexed, so `${PIPESTATUS[0]}` is EMPTY there** and a piped check silently reads `tail`'s status.
#### Review round: the PR shipped its own failure mode twice, both fixed in-branch

A five-agent review of #720 (code, tests, silent-failure, type-design, comments) found two defects of
exactly the class the PR exists to retire. Both were verified before being acted on, and both are
fixed on this branch.

- ⚠️ **Two of the four profiles could never pass: `microvm` on any host, `gliner` on the DGX.**
  `RequireKnob::announce` had **four** call sites in the whole tree — `skip.rs` ×2, `sandbox.rs`,
  `guard_tier_e2e.rs` — and **none** in `microvm/` or `gliner_e2e.rs`. All 18 suites the micro-VM
  grep selected called zero announcing helpers (that is *enforced*: `microvm::guard`'s
  `BANNED_HELPERS` is precisely those three helpers). So `grep -c '^\[E2E\]'` was structurally 0 and
  `MIN_E2E=1` unreachable on a DGX where every VM boots. **It went unnoticed because the profile is
  `os|Linux` and the authoring host is the Mac, which refuses it at exit 2 before running — the
  profile had never been crossed.** For `gliner`, the only announce-capable calls sit inside
  `#[cfg(target_os = "macos")] fn build_test_entry_container()`, so it was red on the one host that
  has the venv and the weights, and on macOS cleared its floor with one *supervisor* line.
  **Fixed:** `microvm::announce_microvm` on `skip_if_no_microvm` / `skip_unless_ready` /
  `dep_or_skip` success paths, and `KNOB.announce` on `gliner_host_env`'s `Ok` arm naming the
  resolved shim + weights paths.
- ⚠️ **The gate's verdict failed OPEN.** `set -uo pipefail` without `-e`; `mkdir -p` unchecked and
  `tee`'s `PIPESTATUS[1]` never read. With the log unwritable every count became the **empty
  string** (grep exits 2 printing nothing; the `|| true` that correctly preserves the zero-match
  case preserves this too), and `[ "" -lt 4 ]` **errors with status 2**, which `if` reads as false.
  All four assertions vanished and the script printed `✅ gate passed as evidence:` at **exit 0**.
  Reproduced end to end, then fixed: `${x:-0}` on every count, checked `mkdir`, a `: > "$LOG"`
  writability probe, a `PIPESTATUS[1]` check, a numeric-operand refusal at exit 3, and a `bash`
  guard (under zsh `${PIPESTATUS[0]}` is empty, so the cargo-exit check silently disappears).

**Also from the review, fixed here:** per-tier `[E2E]` floors (a single total let `gliner` clear its
floor on a *Postgres* line); `[WARN]` is now fatal to a gate run (a warned knob is a disarmed knob);
`MAX_SKIP=0` on `guard-tier`; `validate_profiles` enforces the "a floor of 0 is not a gate" rule the
script only asserted in a comment; the micro-VM discovery now excludes the three
`#![cfg(target_os = "macos")]` suites it was selecting into a Linux-only profile; and
`gate_script_tests` (6 tests) pins the script against `require::KNOBS` in both directions.

> ⚠️ **A source-scan guard I wrote for this gave a FALSE PASS on the very defect it was written
> for.** `every_knob_has_an_announce_call_site` scanned for `.announce` near a knob mention; deleting
> the micro-VM announce left it green, because the file still mentioned the knob and some *other*
> file had an announce. Deleted it and replaced it with two **behavioural** tests
> (`skip_unless_ready_announces_…`, `dep_or_skip_announces_…`) which kill that mutation. The census
> lesson applies to the guard you write from the census.
> [[guard-shares-the-census-blind-spot]]

**Deferred, filed:** [#721](https://github.com/hherb/kastellan/issues/721) (seal `UnmetAction` — it
is a bypassable parameter; plus the `-> bool` reporter so `let _: Option<()>` disappears),
[#722](https://github.com/hherb/kastellan/issues/722) (the macOS container tier — #684's own tier —
has no knob and no profile), [#723](https://github.com/hherb/kastellan/issues/723)
(`warn_if_out_of_dialect`'s "cannot be a hard failure" is a false dichotomy),
[#724](https://github.com/hherb/kastellan/issues/724) (crate-root re-exports are gliner-bound).

- ⚠️ **Deliberately no umbrella variable.** The two hosts differ in what they can legitimately run —
  the Mac has no KVM — so one "demand everything" flag would turn honest skips into failures and get
  exported `=0`. Profiles in the gate script set the right *set*, so the operator still types one
  command. Knobs: `KASTELLAN_PG_REQUIRE_E2E` (Postgres **and** the supervisor probe — one variable
  because they gate the same tier across ~65 near-always-paired suites each — the "298/303" of the
  first draft was not reproducible and had the two the wrong way round — and two variables would let
  a half-set gate look armed), `KASTELLAN_SANDBOX_REQUIRE_E2E`, `KASTELLAN_GUARD_REQUIRE_E2E`, plus the
  two that existed.
- ⚠️ **The micro-VM profile would have been a false gate as first written**, twice over: it named a
  `--test microvm_roundtrip_e2e` **that does not exist**, and it omitted `-- --ignored`, without
  which the whole Firecracker tier reports green having booted no VM. Its suite set is now
  discovered **by grep**, and the script refuses to run when discovery returns zero.
- ⚠️ **#691 is a decision, and the decision is already in the tree.** Its trigger — the
  `xargs touch` cold-clippy recipe — survives only in `archive/` snapshots; the live recipe is a
  dedicated `CARGO_TARGET_DIR`. Moved from this rolling file into `CLAUDE.md`, where it will not be
  pruned away. The mtime class itself is *not* removed; see the issue for the options.

⚠️ **The gate found #718 empirically on its first crossing, and there is deliberately NO `sandbox`
profile because of it.** Run against `-p kastellan-sandbox --all-targets` with the knob set: **147
tests passed, 0 `[E2E]`, 10 `[SKIP]`.** `kastellan-sandbox` does not depend on `tests-common`, so its
suites hand-roll their skips and cannot see the knob — **the tier `CLAUDE.md` calls the canonical
false green is the one that cannot be gated at all.** A profile red on every host trains everyone to
ignore a red gate, so the measurement is a comment in the script instead; adding the profile is the
acceptance test for whichever PR closes #718.

⚠️ **A finding much larger than #622 named, filed as [#718](https://github.com/hherb/kastellan/issues/718):**
**92 hand-rolled `[SKIP]` emissions across 47 files** bypass `skip_line` *and* every knob. This
file's own claim that "every `[SKIP]` renders through `skip_line`" is **false**. `microvm::guard`
already refuses that shape — for micro-VM suites only; generalising its scan is the fix, and it is
too large to bolt onto this PR. [[guard-shares-the-census-blind-spot]]

⚠️ **The Mac sweep is RED on `main`, and it is not this branch:**
[#719](https://github.com/hherb/kastellan/issues/719) — 3 of 5 `gliner_relex_e2e` host-mode tests
fail deterministically with a contentless `Protocol(EarlyExit)`. **Proven pre-existing by stashing
the whole branch and re-running on `6c7fc45d`: identical three failures, 3.20 s vs 3.16 s**; the pop
restored the branch byte-identically (md5-checked). Ruled out: load flake (fails in isolation), a
dead `.venv` (the #651 shape — `readlink` gives a real macOS interpreter and `import gliner` works),
absent weights (1.2 GB staged), a broken worker (spawned by hand it speaks clean JSON-RPC), and a
stale `kastellan-worker-lockdown-exec` (rebuilt; still fails). **The container variant passes in the
same run**, so it is specific to the host-mode sandboxed spawn. ⚠️ **And `cargo test --workspace`
fails FAST, so this one suite aborted the sweep after 39 of ~179 suites** — use `--no-fail-fast`
until it is fixed, or everything after it is invisible.

**Also this session:** the backlog taxonomy the triage note's step 3 asked for —
`docs/devel/notes/label-backlog.sh` gives every one of the 137 open issues exactly one `area:*` plus
the cross-cutting `false-green` / `needs-live-host` / `roadmap` themes, re-runnable, with the
keyword classifier's ~20 known misreads pinned in an override table (`Guard tool_doc() against
drift` is a **verb**; `per-task out dir` contains the asks rule's literal `"ask "`).

### Earlier this session (2026-09-17): backlog triage — 150 open issues, now 137

PR [#717](https://github.com/hherb/kastellan/pull/717). Full evidence:
[`notes/2026-09-17-backlog-triage.md`](../notes/2026-09-17-backlog-triage.md); the actions are the
two scripts beside it. Executed and re-verified against GitHub: 12 closures, 3 retitles, #655's
ruleset, and (this session) the label taxonomy.

⚠️ **The ROADMAP is the accurate source and the GitHub issues are the stale mirror** — the reverse
of what the six epics assumed. Closure is healthy for PR-driven work and was **zero for roadmap-era
work: 58 issues (39%) filed 2026-06 or earlier, untouched.** The cause was mechanical — **~7% of
issues carried any label** — which the taxonomy now fixes. One real ROADMAP defect found and fixed:
the Phase 4 `python-exec` micro-VM line had stayed `[ ]` while duplicating two `[x]` entries.

### Earlier: #701 — a follow-up reads its own conversation (PR #709, merged)

Design + plan under `docs/superpowers/`. What still binds:

- **A finishing channel task writes `tasks.turn_record` = `{calls, data_class}`** in the *same*
  `finalize` UPDATE that makes it terminal; the next task in the same `(channel, peer,
  conversation)` reads up to 3, renders them screened and budgeted, and inherits their floor.
- **Calls, not results** — `{tool, method, parameters, returns}`. The identifier the next turn needs
  is the one the last turn **passed**, so this is smaller than results *and* more useful, and no
  tool output crosses a task boundary.
- ⚠️ **The window has NO upper bound, and that is a decision.** A live turn takes 2.5–4.5 min, so a
  user typing again while the bot works produces a task whose `created_at` precedes the previous
  turn's `finished_at`. A bound there blinds the case the feature exists for.
- ⚠️ **The floor is inherited from every turn LOADED, not from those the screen kept** — otherwise
  one catalogue phrase in an earlier answer both withholds that turn *and* drops the conversation's
  floor, letting an attacker choose what the follow-up may do.
- **Screening is sealed, not conventional**: `view::admitted::Admitted` has a module-private field,
  so a candidate that skips the screen does not compile. The earlier free function was bypassable
  and a mutant proved it.
- ⚠️ **A security property can be documented, tested, and absent from the code.** `Turn::data_class`
  existed *specifically* so a record whose `calls` stop parsing still yields a class — and
  `inherit_floor` read `t.record.data_class` instead, so that row rendered its text with no floor:
  a `ClinicalConfidential` answer reaching a follow-up planned at `Public`. **The mutation proof
  defended the bug, because three tests each pinned one leg and none composed them.**
  [[mutation-proof-counts-only-mutants-you-tried]]
- ⚠️ **A denylist over an extensible enum is a guard with an expiry date** — #71's floor-source
  parser rejected `AgentRaised` structurally, said nothing about an *addition*, and #701's new
  `ConversationInherited` walked straight through. Now an allowlist plus a census test.
  [[guard-shares-the-census-blind-spot]]
- ⚠️ **A fresh worktree has NO worker binaries, and `cargo test --workspace` does not create them** —
  9 daemon e2es then fail closed at boot and look exactly like a regression.
  [[worktree-has-no-worker-binaries]]
- **Deferred, filed:** [#710](https://github.com/hherb/kastellan/issues/710)–[#713](https://github.com/hherb/kastellan/issues/713),
  [#715](https://github.com/hherb/kastellan/issues/715), [#716](https://github.com/hherb/kastellan/issues/716)
  (#714 closed by #720).


### Previous session: #677 — the planner reads a tool result as labelled JSON

PR [#702](https://github.com/hherb/kastellan/pull/702) `10164c22`. Spec + plan dated 2026-09-13; full
prose in [`archive/handover_20260914_677_pre-prune.md`](archive/handover_20260914_677_pre-prune.md).
What still binds:

- **`inner_loop/result_view` is the planner's view of a successful step**, not the injection guard's
  flattening (which drops every key, number and boolean). `prune` + a measured `render`: every
  candidate is measured, so the size bound needs no monotonicity argument. **Identifiers are atomic**
  (space-free ≤ 1 KiB shown whole or not at all) and **keys are identifier-shaped or absent**.
- ⚠️ **Keys never reach the guard model** — `post_process` screens `extract_scannable_text`, which
  drops them; the sink catalogue is a key's only screen ([#703](https://github.com/hherb/kastellan/issues/703)).
  A worker must not emit third-party text as object keys; `workers/mail/src/headers.rs` returns
  `[{name, values}]` for exactly that reason.
- ⚠️ **A hardening that rewrites screened text must ADD readings, never replace them** — the
  camel-case split first replaced the plain one and un-blocked `iGNORE_ALL_PREVIOUS…`.
- Budgets: per step 16 KiB, accumulated 96 KiB. `the_planner_prompt_documents_every_outcome_shape`
  fails if renderer and prompt drift. Deferred: [#704](https://github.com/hherb/kastellan/issues/704),
  [#705](https://github.com/hherb/kastellan/issues/705).
- ⚠️ **Live acceptance found #701, not a fault in the change under test** — which is what this
  session then fixed.

### Merged arcs — only what still binds

**#694 (`8e0c10f4`) — an oversized dispatch still records what ran (#617).** `truncate_payload`
derives `req_summary = {head, sha256, len}` for any over-cap payload carrying `req` — centrally,
because there are two producers, not the one the issue named. ⚠️ The digest is over the **whole**
request and the fingerprint is taken **before** the summary is inserted; `PRESERVED_KEYS` order is
priority order (`guard` first). ⚠️ **No sink double can test it**
[[audit-sink-doubles-hide-storage-transforms]]. ⚠️ **Review subagents mutate the working tree** —
give a reviewer its own `git worktree` [[never-edit-tree-during-a-sweep]]. Deferred: #693, #695–#697.

**#692 (`c5bf5e5f`) — every micro-VM preflight has a budget (#690, #689, #686).**
`kastellan_sandbox::bounded_command` is the vocabulary for any host probe; ⚠️ **a bounded runner must
not join its drains** [[bounded-subprocess-must-not-join-drains]]. Both micro-VM guest kernels lack
Landlock, so both tiers are seccomp-only by design; `macos_container_smoke` fails the day that changes.

**#688 / #685 / #683 / #680 — rootfs and image freshness.** One producer for `target/release/`:
`bash scripts/build-release.sh`, run LAST [[cargo-package-selection-changes-binary-bytes]].
`KASTELLAN_MICROVM_REQUIRE_E2E=1` turns every unmet micro-VM precondition into a panic; the reference
is the **sha256 of the baked copy** (mtimes lie [[cargo-relinks-identical-mtime-not-content]]); the
container tier compares build time against **source** mtimes.

**#660 (`62d98a00`) — the second pre-release security audit.** Three **fail-closed** lockdown rules
bite careless fixtures: missing `KASTELLAN_SECCOMP_PROFILE`, an unenforceable Landlock ruleset, a
corrupt guest env token. Per-spawn dirs via `create_private_dir` — **do not "fix" back to
`create_dir_all`**. Networked stdio workers build their handler **inside** `serve_stdio_with`.
**Before release: flip force-routing on.** Deferred list in `docs/security-audit-2026-09-02.md`.

**One-liners.** #681: a lean tail plus recovery beat a fat verbatim tail (68.3 % vs 45.8 % recall).
#675: a failed micro-VM boot leaves `console.log` in the kept run dir
[[microvm-guest-failures-are-invisible]]; the launcher has no env
[[microvm-launcher-knobs-must-be-argv]]; release is `panic = "abort"`
[[release-profile-panic-abort-kills-raii]]. #669: count the producers, make the const the only
spelling; `/run` stays out of the chown set. #650: a containment fix must not widen containment.
### The guard tier — what still binds

- **D10 — the tier is ADVISORY defence-in-depth, NOT a gate.** 65 % recall (36/55) at FP-0; 6/6 on
  bare imperatives but **5/8 missed** on narrative framing. **Nothing downstream may relax on it.**
- **τ = 0.79552656 is a REQUIRED operator input with no default**, and **five misconfigurations STOP
  THE DAEMON** (D6): half-configured keys, τ outside `(0.0, 1.0]`, a pinned timeout of 0, an
  unreachable `/props`, and a context below `SCAN_BYTE_CAP + 512 = 66 048` (D8).
- **`best_tau` returns NONE** — real captured content overlaps at every threshold, and that stratum
  was **catalogue-selected**, which is why **corpus growth from production is the cheap path**.
- **`AuditSink::insert` applies `truncate_payload` before delegating to `insert_stored`**, so no sink
  double can record a payload Postgres never stored [[audit-sink-doubles-hide-storage-transforms]].
  **Absence and loss must not render identically.**
- ⚠️ **The stated mitigation for an issue can disarm the instrument built to check it** — the live
  probe passed having measured nothing under a *pinned* timeout, precisely what #612 tells a Metal
  operator to use. It now refuses a pin outright.
- ⚠️ **#624 and #626 do NOT close [#612](https://github.com/hherb/kastellan/issues/612)** — that is
  that extrapolating from a ~1 KiB sample is non-linear **on Metal whatever the load**
  [[metal-prompt-processing-is-nonlinear]].

### Standing hazards that have each cost a session

Most are memory notes (auto-loaded); kept here because they change the *first* move.

> ⚠️ **Clippy: parity is a `rustup update`, and a cached run lies.** CI pins nothing
> [[local-clippy-not-ci-parity-rust-version]]. Exit 0 alone does not prove a full pass — **count the
> `Checking kastellan` lines (27)**. Force a cold run with a dedicated
> `CARGO_TARGET_DIR=$HOME/.cargo-clippy-<topic>`, **never** by `touch`ing sources: that moves
> `workers/python-exec/src/main.rs`, falsifying the #687 image gate
> ([#691](https://github.com/hherb/kastellan/issues/691)). Run the sweep first, lint after.

> ⚠️ **A private `CARGO_TARGET_DIR` breaks daemon e2es and does not build `examples/`**
> [[custom-cargo-target-dir-breaks-daemon-e2e]]; **rust-analyzer can hold `target/debug/.cargo-lock`**
> (a blocked sweep has zero rustc children while the IDE's cargo has many — kill that child).

> ⚠️ **Do NOT edit the tree while a sweep compiles in it — and give every reviewer its own
> `git worktree`.** Review subagents plant mutants and do not say so. In a worktree session the Bash
> cwd resets to the primary checkout: use `git -C` and absolute paths
> [[never-edit-tree-during-a-sweep]] [[worktree-cwd-lands-on-main]].

> ⚠️ **"Filed, not fixed: #N" CLOSES #N.** Write "deferred to #N"; before merging, grep the PR body
> and commit messages for `(close[sd]?|fix(e[sd])?|resolve[sd]?)[[:space:]:,]+#[0-9]+`
> [[pr-body-not-fixed-autocloses-issue]]. Squash merges mean a branch-tip sha is not on `main`.

> ⚠️ **`syspolicyd` saturates and no newly written executable or dylib can start on the Mac** — and it
> re-saturated within three days of the 2026-09-11 restart. **Tell: `ps -o time` exactly `0:00.00`
> against minutes of ELAPSED; positive control: a freshly compiled 20-byte C program hangs.** Fix: ask
> the operator for `sudo killall syspolicyd`, then kill any binary still waiting. A watchdog killing
> `target/debug/deps/*` at 0 CPU past 15 min keeps a sweep moving; re-run those suites individually.
> ⚠️ **Never `cp -cR` a target dir into a worktree** — 78 min for 241 GB, and every dylib becomes
> "new" to `syspolicyd` [[mac-fresh-large-binaries-hang-in-dyld]].
> ⚠️ **But a slow Mac cargo build is usually CONTENTION, not the wedge**, and `sample` alone cannot
> tell them apart — a thread that is never *scheduled* shows the same single frame. **Check `uptime`
> and `%cpu` first:** a wedge burns no CPU *and never finishes*; contention burns little and finishes.

> ⚠️ **`kastellan-worker-egress-proxy` leaks on the Mac** (orphans survive for weeks, not
> investigated), and **a `pgrep -f '<cmd>'` wait loop matches itself**: use `pgrep -x`
> [[pgrep-wait-loops-match-themselves]].

## Read these first

1. [`docs/architecture.md`](../../architecture.md) — process model, cross-platform table
2. [`docs/threat-model.md`](../../threat-model.md) — the invariant, scenarios, defence layers
3. [`docs/devel/ROADMAP.md`](../ROADMAP.md) — the master sequenced TODO with commit hashes
4. Memory notes (auto-loaded) — `~/.claude/projects/-Users-hherb-src-kastellan/memory/MEMORY.md`
5. [`archive/`](archive/) — the full prose for everything this file summarises

---

## Next TODO

> Only *open* work is listed. Shipped items move to [Recently merged](#recently-merged) or the ROADMAP.

1. **[#677](https://github.com/hherb/kastellan/issues/677) — re-measure it live now that #701 has shipped.** #701 was the missing half: a follow-up now carries the previous turns' calls, so the two DMs that failed should need one plan, not six. **Acceptance is the same two DMs** (the issue's own script). Until that run happens on a DGX carrying #709, #677 stays open and this is the first thing to do. Historical detail on the stateless-task defect follows, for when that run disagrees: Measured 2026-09-14 with #702 deployed: DM 1 (task 187) answered correctly; DM 2, a follow-up about "the last 3 flight bookings" (task 188), carried only its own sentence (`recall_count: 0`), searched from scratch, found a different booking and blamed the step budget. #677's original tasks 185/186 failed the same way. **Architectural — brainstorm first:** which turns; fencing the bot's own tool-derived answers as untrusted data; whether prior step outcomes (through the #702 view) or only final answers carry across; classification-floor inheritance; budget. Adjacent: ROADMAP `context_manager`, #629. **Acceptance: the same two DMs.** ⚠️ **Ask the operator how a live failure looked in the chat before blaming the change under test** — a screenshot settled in seconds what the audit rows had hidden for a week [[channel-dm-tasks-are-stateless]].

2. **The #677 follow-ups, each small and each measured by the same live question.**
   [#699](https://github.com/hherb/kastellan/issues/699) — the planner never sees the tool, method or
   parameters of its own prior steps (it dropped a filter and could not know); render them beside
   each outcome, through the sink screen. [#698](https://github.com/hherb/kastellan/issues/698) —
   `mail.search` cannot express a filter-only search; fix together with localmail
   [#364](https://github.com/hherb/localmail/issues/364), or an optional `query` returns unfiltered
   hits. [#700](https://github.com/hherb/kastellan/issues/700) — `plan.decision` reaches the prompt
   unscreened, contrary to what the sink screen's doc used to claim (`summary::sink_screen` now names #700). ⚠️ **#560 (fabricated `message_id`) is now
   worth re-measuring, not re-describing** — the labelled view is the mechanism its lead named.

3. **#702 follow-ups.** [#703](https://github.com/hherb/kastellan/issues/703) — ⚠️ **the guard model
   never sees object keys**; latent since mail headers were closed at the worker, but **any new worker
   passing a third-party JSON object through reopens it silently** (fix needs a DGX guard calibration
   run). [#705](https://github.com/hherb/kastellan/issues/705) — #677's defect one size class up, via
   `summary_head`. [#704](https://github.com/hherb/kastellan/issues/704) — forgeable markers, low impact.
   **localmail changes the operator offered to make** (2026-09-14): headers as an ordered list, a 4xx
   instead of a silent cursor restart (#561), filter-only search + `has_attachment` (#698, localmail
   #364), compact plain-text hits, attachments addressable by `message_id` + name, a distinct expired-
   credential error (#673/#674), the applied sort echoed — not yet filed on `hherb/localmail`.

**On the micro-VM path — one issue left, and it needs a kernel build.**
[#668](https://github.com/hherb/kastellan/issues/668) — repin a guest kernel built with
`CONFIG_SECURITY_LANDLOCK`, the standing posture item. ⚠️ **Its macOS twin now has a detector rather
than an issue** (#689, PR #692): the Apple `container` guest kernel does not enforce Landlock
either (re-measured 2026-09-10 — `/sys/kernel/security` exists and is empty), so *both* tiers run
seccomp-only by design, and `macos_container_smoke` fails the day that changes. If #668 is ever done,
the container backend's injection has to be revisited in the same breath — it is deliberately a
default a caller can already override.

**A standing architecture item, and the frame for several open issues:**
*(#702 delivered the planner-view part of slice (e)'s premise — results reach the planner labelled; the anchor index itself, audit spill (c) and handoff `query` (d) remain.)*
[#678](https://github.com/hherb/kastellan/issues/678) — **retire truncation as the answer to "bigger
than the budget".** The key move: truncation does **three different jobs** and only one becomes
map-reduce — a *control that stops seeing its evidence* (the guard's 64 KiB `SCAN_BYTE_CAP`; the
reduce `p = max(p_i)` is strictly more sensitive than today), a *record that must be faithful*
(`truncate_payload` — **spill, never summarise: an audit row is testimony**), and a *resource guard*
(`MAX_RECORD_BYTES` — **these stay**, containment against a compromised worker).
`core/src/handoff.rs` already stashes oversized results **whole**, so only the reduce is missing;
slice (e) is #681's anchor index, which makes `handoff`'s byte-offset recovery path reachable.
Likely subsumes #604 and #612 by removing their premise. ⚠️ **The polarity inverts to fail-closed** —
today a document past the cap is silently unscreened — which needs its own test.

**The false-green-gate cluster is now mostly closed** — #655 (required checks), #714, #622, #664 —
leaving [#691](https://github.com/hherb/kastellan/issues/691) (a decision: the mtime class itself,
whose trigger is gone) and [#718](https://github.com/hherb/kastellan/issues/718) (**92 hand-rolled
`[SKIP]`s in 47 files** bypass every knob; generalise `microvm::guard`'s scan). #237's absent macOS
CI leg is the remaining structural one.

**THEN the guard arc:** [#612](https://github.com/hherb/kastellan/issues/612) — a design call rather
than a patch; #616 unblocked its favoured option. Beside it, both cheap:
[#639](https://github.com/hherb/kastellan/issues/639) (split `guard_tier_e2e.rs`, 1558 lines, also
[#622](https://github.com/hherb/kastellan/issues/622)'s cheapest option) and
[#638](https://github.com/hherb/kastellan/issues/638) (214 rustdoc warnings, 67 broken intra-doc
links, in a tree that treats doc comments as the design record).

**Next up — operator's choice, each roughly one session.** Issue text is authoritative; below are
only the gotchas that are *not* in the issues.

- **[#560](https://github.com/hherb/kastellan/issues/560) — the planner fabricates a 16-hex
  `message_id`.** Do **not** close it by rewriting the parameter description: #536 already did
  exactly that, deployed, and both later runs still fabricated. The lead it was filed with — before
  #702 keys were stripped, so `"20973"` reached the planner as a bare line with nothing marking it
  as *the id* — is the mechanism #702 removed. **Re-measure it live before touching it**
  [[tool-output-reaches-planner-key-stripped]] [[opaque-ids-are-unusable-tool-params]].
- **[#550](https://github.com/hherb/kastellan/issues/550)** — **the naive fix is wrong**: the overlay
  legitimately overrides `kastellan.env` keys, so it must compare the *folded* environment, which
  `fold_env_files` already computes for launchd.
- **[#548](https://github.com/hherb/kastellan/issues/548)** — not a teardown bug (`PgCluster`'s `Drop`
  guards are correct and cannot run on SIGKILL), so the fix is about blast radius. ⚠️ #641 removed the
  shared suffix between a test daemon's unit and its sibling PG cluster; restore it with a
  `.suffix()` setter rather than by reverting the constructor
  [[issue-as-filed-can-carry-a-regression]].
- **Mail credential expiry — [#673](https://github.com/hherb/kastellan/issues/673) +
  [#674](https://github.com/hherb/kastellan/issues/674).** An upstream 401/403 is reported as
  `POLICY_DENIED`, so an expired localmail credential reads as a kastellan policy refusal, and
  nothing notices the expiry at all — a failure naming the wrong cause, like most of the above.
- **Web workers — [#706](https://github.com/hherb/kastellan/issues/706) before any release;
  [#707](https://github.com/hherb/kastellan/issues/707) blocked upstream.** #706: no rate limiting,
  backoff, conditional requests or `robots.txt` anywhere; one implementation in `web-common`. #707:
  Obscura now renders and speaks our IPC, but its V8 is ~15 Chrome milestones stale and it has no
  internal sandbox; #708 showed the bump is a port, not a patch. ⚠️ **Do not adopt it before the bump
  lands upstream** — our jail would be its only layer.
- **Also open, no gotcha beyond the issue text:** #551 (systemd `%` specifier, workspace-wide),
  #519, #554 (needs a live DGX gate — it narrows what a deployed worker may do), #534.
- **Email channel — slices 2 and 3.** Slice 1 (gated inbound) MERGED, #503 closed its MITM gap. Spec
  `docs/superpowers/specs/2026-07-28-email-fallback-channel-design.md`. **Slice 2** = SMTP outbound
  (`lettre`, MIT-verified) + full round trip; today `EmailChannel::send` refuses and every refusal is
  audited `channel.reply_undelivered`. **Slice 3** = DGX deploy + live tier; **restart
  `localmail-serve` (+ `localmail-daemon`) on the DGX first**.
- **A Mac daemon deployment is a deliberate decision, not a task.** The tier boots fine there
  (91.4 s derived, `n_ctx` 66 048) but #612 means it fails open on large documents.
- **Live guard-host facts:** the DGX guard server is `llama-server … Shieldstral-1.0-3B-Q8_0.gguf
  --alias shieldstral --port 8081 -c 131072 -ngl 99`; `/props` reports the per-request context at
  `default_generation_settings.n_ctx` with **no top-level `n_ctx`**. Restart it with **at least
  `-c 66048`** or the daemon refuses to boot. The three guard keys live in
  `~/.config/kastellan/kastellan.env.local`, which `install` never rewrites.
- **Deferred with a reason, not forgotten:** macOS Seatbelt-loopback verification of mail tier 1a;
  **Telegram inbound** (still rejected as primary); **MITM-of-browser** via a proper NSS trust-store
  import, **not** `--ignore-certificate-errors-*`, since production must not be loosened to make a
  test pass.

**File-split backlog (Item 9b)** — **`wc -l` before picking; the numbers drift.** The rule: **split
BEFORE the change that grows a file**, in a movement-only commit whose `#[test]` name set is
verifiable either side. Best first picks, each a pure test-lift: `core/src/channel/ask_message.rs`
**956**, `workers/mail/src/handler.rs` **670**, `sandbox/src/linux_firecracker/plan.rs` ~**1160**
(`cfg(linux)`, DGX-gated), `core/tests/guard_tier_e2e.rs` **1558**
([#639](https://github.com/hherb/kastellan/issues/639)). Clean seam visible:
`core/src/scheduler/asks.rs` **801**. Judgement first, not movement: `db/src/asks.rs` **1127**,
`db/graph.rs` **926**, `llm-router/src/config.rs` **843** — a small `mod tests` there means a split
is a production reorganisation. Also over cap, no seam called yet:
`core/src/scheduler/inner_loop.rs`, `core/src/channel/bus.rs`, `workers/matrix/src/sdk_live.rs`,
`llm-router/src/messages.rs`, `core/src/main.rs`, `tests-common/src/microvm/mod.rs` (**738**, grew
again) and `tests-common/src/microvm/container.rs` (**710**, grew again).

**Standing deferrals (no owner; pick up when a consumer appears)** — listed only so nobody
re-derives them: egress #242, #251, #304 (needs a controllable TLS origin), #260; micro-VM #381 and
**true `jailer`** (a privileged-tier `VmmConfinement::Jailer` sibling whose seam already exists in
`confine.rs`); python-exec Phase 4 (curated-wheels RO dir — stdlib-only today, flipped by
`KASTELLAN_PYTHON_EXEC_ENABLE=1`); web-research polish, all opus-triaged DEFER; an ANN index on
`entities.embedding` once cardinality warrants it.

**Generalizing net-worker-in-VM needs no new work** — 5c's `NetClientTransport` /
`spawn_net_transport` IS the reusable mechanism; a second consumer can adopt it directly.

---

## Load-bearing findings that still bind

- **The four faults (2026-08-02).** One real Matrix message, **four independent faults, only one a
  kastellan bug in the layer everyone suspected**, each masking the next. **A green stack with a
  silent output means look at every layer, and fix them one at a time so each fix's evidence is
  separable.**
- **Egress / MITM traps — read before touching the proxy.** The MITM upstream trusts **webpki roots
  only**, so no hermetic self-signed origin is possible for a MITM'd worker's e2e
  [[egress-proxy-upstream-trusts-webpki-only]]; a force-routed loopback endpoint needs an **IP SAN**
  [[macos-force-routed-loopback-needs-ip-san]]; a bare-host `Net::Allowlist` entry with no `:port` is
  an **all-port grant** [[bare-host-net-allowlist-is-all-port-grant]].
- **Process lessons that have each cost a re-run.** A truncated gate log is not a gate
  [[truncated-gate-log-is-not-a-gate]]. Mutation testing contaminates the git **index**
  [[mutation-testing-contaminates-the-index]]; revert by copying the file, never `git checkout`
  [[mutation-revert-never-git-checkout]]; a mutation proof counts only the mutants you tried
  [[mutation-proof-counts-only-mutants-you-tried]]. Plan text is a defect source — subagents
  transcribe prose verbatim [[plan-text-is-a-defect-source]].
- **`sqlx::migrate!` embeds at compile time** [[sqlx-migrate-embeds-at-compile-time]].

---

## Working state

### Test baseline (authoritative)

| Host | Commit | Result | clippy `-D warnings` | `[SKIP]` |
| --- | --- | --- | --- | --- |
| **Mac** ([#720](https://github.com/hherb/kastellan/pull/720), the REQUIRE contract — **the gate that stands**) | branch tip | **4322 / 8 / 29**, **179 suites**, `TEST_EXIT=101`, **23 `[SKIP]`**, **0 `[E2E]`** (correct: no knob was set, so the positive control stays silent in a plain run). ⚠️ **Of the 8, five pass individually** — `asks_e2e` 35/35 and `conversation_turns_e2e` 9/9, both failing under the sweep with `Connect("the database system is starting up")`, the per-test-cluster contention of [#548](https://github.com/hherb/kastellan/issues/548)/[#676](https://github.com/hherb/kastellan/issues/676). **The remaining 3 are [#719](https://github.com/hherb/kastellan/issues/719) and are NOT this branch** — proven by stashing the whole branch and re-running on `6c7fc45d`: identical three failures. So the effective result is **4327 / 3 / 29**. ⚠️ **The delta reconciles EXACTLY at +12** over the 4318 row below: **11 new `#[test]` in `require.rs` + 1 new doc-test** (`require.rs - require (line 45) - compile ... ok`) [[ignore-fenced-doc-example-moves-ignored-count]]. ⚠️ **The first sweep of this branch was itself a false green:** `cargo test --workspace` **fails fast**, so `gliner_relex_e2e` aborted it after **39 of 179 suites** — 127 suites invisible at a verdict that looked like a verdict. **Use `--no-fail-fast`.** ⚠️ **The DGX leg is OWED** — both supervisor backends compile on one host each | **exit 0**, zero warnings, all **27** crates by `Checking kastellan` count, forced cold with `CARGO_TARGET_DIR=$HOME/.cargo-clippy-720` — **never** by touching sources, which falsifies the #687 image gate (#691) | **23** |
| **Mac + DGX** ([#709](https://github.com/hherb/kastellan/pull/709), #701 — **third review round, the gate that stands**) | `d6698013` | **DGX 4453 / 0 / 61**, 179 suites, `TEST_EXIT=0`, **4 `[SKIP]`** (all gliner, opt-in). **Mac 4318 / 0 / 29**, 179 suites, `TEST_EXIT=0`. ⚠️ **Both deltas are +10 and reconcile exactly** against the row below (DGX 4443, Mac 4308): 10 new `#[test]` in the diff (3 conversation, 1 floor, 1 record, 2 view, 2 task_exec, 1 db e2e), and **each of the 10 was grepped out of the DGX log by name as `... ok`** rather than inferred from the total. `KASTELLAN_PG_BIN_DIR` set on both hosts, so every Postgres suite ran for real — on the DGX the proof is that **zero of the 4 skips are PG** (all four are the gliner tier), and the PG-gated `a_task_that_is_not_a_channel_task_is_not_a_turn` passed. ⚠️ **The Mac gate was re-run after a post-sweep edit** — the first sweep was green, then the clamp block was re-indented, which made that sweep a gate on a revision that no longer existed [[never-edit-tree-during-a-sweep]]. Both headline fixes verified by planting the original defect back and watching the new test fail, and the `kind='channel'` mutant killed live against PG | **DGX exit 0** and **Mac exit 0**, `--workspace --all-targets -D warnings`, zero warnings, all 27 crates. **This also clears the DGX clippy run the row below records as owed** | **4** DGX (gliner only), 15 Mac |
| **Mac + DGX** ([#709](https://github.com/hherb/kastellan/pull/709), #701 — **the gate that stands**) | branch tip `ff964274` | **DGX 4443 / 0 / 61**, 179 suites, `TEST_EXIT=0`, **4 `[SKIP]`** (gliner, held). **Mac 4308 / 0 / 29**, 179 suites, **86 `[SKIP]`**. ⚠️ **Both deltas are +61 and reconcile EXACTLY against a measured baseline, not an estimate:** `cargo test --workspace -- --list` on the DGX gives **4504 on the branch against 4443 on `main`**, and the static count of new `#[test]` + `#[tokio::test]` in the diff is also 61. 179 suites = 177 + the two new test binaries. ⚠️ **And the measurement corrected a carried estimate:** `main`'s DGX total is **4382 runnable + 61 ignored**, where the #702 row above implies ~4360. Extrapolating one row from the next drifts; `-- --list` on both revisions costs one compile and settles it. ⚠️ **The Mac's 86 skips (not ~340) are because `KASTELLAN_PG_BIN_DIR` was exported**, so every Postgres-gated suite ran for real here — including all 10 of this branch's own PG tests. ⚠️ **The Mac sweep first reported `TEST_EXIT=101` with 9 failures in three daemon-spawning suites, and that was a missing PREREQUISITE, not a regression:** a fresh worktree has no worker binaries and `cargo test --workspace` does not build them, so the daemon fails closed at boot (`Error: building egress force-routing config`). After `cargo build --workspace`, all 9 pass (6 + 2 + 1). The DGX, whose checkout had the binaries, was green throughout | **Mac** `--workspace --all-targets --locked -D warnings` exit **0**, zero warnings, all **27** workspace crates, forced cold with `CARGO_TARGET_DIR=$HOME/.cargo-clippy-701` (never by touching sources, which would falsify the #687 image gate). **The DGX clippy run is still owed** | **86** Mac (PG live), **4** DGX |
| **Mac + DGX** ([#702](https://github.com/hherb/kastellan/pull/702), #677 — **the gate that stands**) | **`cc555f90`** (branch tip) | **DGX 4357 / 0 / 61**, 177 suites, `TEST_EXIT=0`, 0 `[WARN]`. **Mac 4222 / 0 / 29**, 177 suites, 0 `[WARN]`. **Both exactly as predicted** from the static `#[test]` name diff: +33 over `main` (DGX 4324, Mac 4189) — 28 in `result_view`, 5 net in `summary`; host gap **135**, unchanged. ⚠️ **The Mac `TEST_EXIT` is 101 with one failure that is the host, not the branch:** `syspolicyd` had re-saturated; five suites wedged at exec (killed at 15 min, 0 CPU) and `egress_force_routing_e2e` timed out waiting for its sidecar to start right after the restart. All six pass individually, which is where 4213 + 1 + 8 = 4222 comes from | exit **0** on both, **27** workspace crates each by count of `Checking kastellan` lines; the DGX run forced cold with `CARGO_TARGET_DIR=$HOME/.cargo-clippy-677` after its first pass finished suspiciously in 12 s | **340** Mac (absent-Postgres), **4** DGX (gliner, held) |

Older rows are in the [`archive/`](archive/) snapshots.

⚠️ **The Mac `[SKIP]` 296 → 340 is two counting methods plus one stale image.** Per suite with
re-run counts substituted, the gates are **332 → 339**; the real **+7** are `python_exec_container_e2e`
(4) + `python_exec_warm_idle_e2e` (3), skipped by the #687 freshness gate because **the Mac's
`kastellan/python-exec:dev` image (2026-09-10) predates #692's `build-image.sh` change** — rebuild
with `bash scripts/workers/python-exec/build-image.sh` [[stale-fixture-turns-a-gate-into-a-formality]].
**Count skips per suite, re-runs substituted, or the column is not comparable.**

⚠️ **`scheduler_ask_expiry_e2e` flakes under a full sweep, and this file's diagnosis was WRONG for
two gates.** It said "widen the poll deadline"; the evidence says the per-test cluster's unix socket
went away underneath the test (`claim_one error: … No such file or directory`, past both
`await_state`s). In isolation it runs 62 s against a 20 s + 90 s budget.
[#676](https://github.com/hherb/kastellan/issues/676); likely the same ownership problem as
[#548](https://github.com/hherb/kastellan/issues/548). **The general lesson: a flake attributed once
gets re-attributed forever — re-read the actual failure text on each recurrence.** (This session it
wedged in `_dyld_start` instead, a *third* cause — see the `syspolicyd` hazard.)

⚠️ **A dropped ephemeral port is NOT a reserved one.** `skip::tests::the_resolve_and_reach_arms_…`
bound a loopback port, dropped the listener and assumed it was closed; the OS may hand a freed
ephemeral port straight to another process. Measured: 1 failure in 3 full sweeps, 0 in 40 isolated
runs. Fixed by *confirming* rather than assuming.

**Both hosts are load-bearing, in opposite directions — always check both.** The two supervisor
backends compile on one host each: a `launchd_agents.rs` change is invisible to the DGX, and the Mac
compiles **zero** `systemd_user` tests [[mac-compiles-zero-systemd-tests]]. The mirror is just as
real — Mac clippy compiles `cfg(target_os = "linux")` items *out*, so an unused cfg-linux helper
fails only the DGX gate; `cargo clippy --target aarch64-unknown-linux-gnu` from the Mac catches it in
seconds (pure-Rust crates only; `core` won't cross-compile, `ring` C dep)
[[cross-clippy-pure-rust-crates]]. ⚠️ **A whole file can be `#![cfg(target_os = "linux")]`**, in
which case the Mac compiles *nothing* in it, imports included.

**Predict the count, then reconcile the delta exactly.** Every gate above was predicted from the
diff's new `#[test]` count and investigated when it missed. **Reconcile by diffing PER-SUITE counts,
not test names:** `--nocapture` interleaves output, and `#[should_panic]` prints
`- should panic ... ok`, which a bare `… ok` grep reports missing. ⚠️ **An `ignored` delta with no
new `#[ignore]` is usually a doc-test** [[ignore-fenced-doc-example-moves-ignored-count]].

⚠️ **A `[SKIP]` can hide a dead fixture for months, and a `[SKIP]` line is evidence nothing may
fake.** The four gliner-relex venv skips were not "this host is unstaged" — the DGX's `.venv` was a
**copy of the Mac's**, `bin/python` pointing at a path that cannot exist on Linux. `readlink` before
believing a skip, and prefer a `REQUIRE_*=1` knob. And since `grep -c '^[SKIP]'` is how a green sweep
is audited, every `[SKIP]` **should** render through the pure
[`tests_common::skip::skip_line`](../../../tests-common/src/skip.rs) — **assert on `skip_line`; call
the `skip_if_*` wrappers only from real fixtures.**

⚠️ **That sentence used to say "does", and it was FALSE — measured 2026-09-17 at 92 hand-written
`[SKIP]` emissions across 47 files**, each bypassing `skip_line` *and* every REQUIRE knob
([#718](https://github.com/hherb/kastellan/issues/718)). A hand-written line is miscounted when its
shape drifts, and answers to no knob — which is #622 generalised, and is how
`guard_tier_e2e`'s worker-binary check stayed the one precondition in that suite no knob could see.
92 is a **floor**: the census greps `eprintln!`-shaped emissions only.

**Two standing reading rules.** A green run with `[SKIP]` lines means tests *skipped*, not that the
sandbox contained anything. And skip-as-pass counts as passed, so counts stay comparable either way.

**Mac verification runs from the repo's own `target/`, not a private `CARGO_TARGET_DIR`** — the
private dir breaks daemon e2e (above). If one is unavoidable it must live under `$HOME`, not `/tmp`
[[dgx-run-logs-tmp-scrubbed]]. Keep gate logs under `$HOME` for the same reason, and **whole** —
a truncated gate log is not a gate [[truncated-gate-log-is-not-a-gate]].

### Build & test

The cargo commands and the one-time Linux host setup are in [`CLAUDE.md`](../../../CLAUDE.md)
§ Build, test, run and § Linux host setup. Two things that file does not say:

**FC e2e gotchas (DGX) — read before running any Firecracker e2e.** Build the release binaries with
**`bash scripts/build-release.sh`** — since #682 that is the *only* supported way to write
`target/release/`, because cargo unifies features per invocation and a narrow `cargo build -p …`
therefore produces different **bytes** from identical source, which the #667 freshness gate reads as
staleness. AND `export PATH=$HOME/.local/bin:$PATH` (firecracker is off the non-interactive ssh
PATH). Since #667/#679, `KASTELLAN_MICROVM_REQUIRE_E2E=1` turns **every** unmet precondition in a
micro-VM suite into a panic naming itself, and a stale **image** fails the run naming
`bash scripts/workers/microvm/rebuild-all-rootfs.sh`. **Use that knob whenever a Firecracker run is
meant to be *evidence*.** ⚠️ **The stale release launcher is still invisible.**
`kastellan-microvm-run` is baked into **no** rootfs image, so the freshness gate structurally cannot
see it — the trap that already cost false bug report #362; `build-release.sh` rebuilds it.
`kastellan-core` won't cross-compile on the Mac (`ring` C dep), so core e2e are compile+run on the
DGX only. A VM worker's `WorkerSpec.program` must be the **in-rootfs**
`/usr/local/bin/kastellan-worker-<name>`, never the host target-dir path
[[vm-worker-in-rootfs-binary-path]]. A failed boot leaves `console.log` in the kept run dir;
`KASTELLAN_MICROVM_KEEP_RUN_DIR=1` keeps it on a successful boot.

### The tree — 27 crates

Full layout in the root [`README.md`](../../../README.md) § Layout, and the load-bearing crates in
[`CLAUDE.md`](../../../CLAUDE.md) § Project shape. Not duplicated here — it drifts, and the README is
the one a fresh reader finds first.

### Integration-suite map

Only the rows that tell you *where to look when something goes red*; the full census is in the
[`archive/`](archive/) snapshots.

| Suite | Tests | What's verified |
| ----- | ----- | --------------- |
| `sandbox` integration (`linux_smoke` / `macos_smoke` / `macos_container_smoke`) | 8 / 10 / 10 | **real** jails: fs invisibility, net deny, relative-path reject, OOM-kill under MemoryMax, per-spawn `/tmp` tmpfs, fresh session leader — **and the Firecracker VMM jail actually launching** (#671), the one gate that catches a flag combination bwrap refuses at option-parse time. Container row also carries the #689 Landlock drift detector, which boots a real container |
| `core` Firecracker (14 suites, `#[ignore]`, DGX) | 29 | **real KVM**: round-trip, mem cap, net deny, host-dir share, warm idle, VMM confinement, egress + broker reverse channels, persistent store, browser-driver, matrix; W-2's in-guest privilege drop; `/run` mode + relay-socket reachability. Run with `KASTELLAN_MICROVM_REQUIRE_E2E=1` **and `-- --ignored`** to make it evidence |
| `core` (`shell_exec_e2e`, `python_exec_e2e`, `python_exec_container_e2e`) | 4 / 4 / 4 | **real** core→sandbox→worker round-trips under production policy; jail-contained socket attempt; per-spawn scratch; secret-scrub to `[redacted:]` |
| `core` (`egress_proxy_e2e`, `egress_force_routing_e2e`, `email_mitm_e2e`) | 3 / 4 / 2 | **real** sandboxed sidecar + CONNECT client; Linux-only no-direct-route; a hermetic MITM asserting the round-tripped event plus `tls_intercepted:true` |
| `core` (`injection_guard_e2e`, `secret_vault_e2e`, `guard_boot_row_e2e`) | 10 / 9 / 1 | **PG-required**: policy rows, privacy invariant, per-tool profiles, materialize/redeem, fail-closed redemption; a real daemon's stored guard boot row asserted equal to `boot_payload(..)` |
| `core` (`memory_recall_e2e`, `cli_ask_e2e`, `cli_memory_l3*`, `email_channel_e2e`) | 1 / 2 / 17 / 8 | three-lane RRF recall + 1-hop expansion; full prod chain against a queued mock LLM; L3 lifecycle; the hermetic channel loop incl. its two regressions |

## Key design decisions locked in

**Not restated here — they drift.** The hard constraints are in [`CLAUDE.md`](../../../CLAUDE.md)
§ Hard constraints, which a fresh session loads automatically; the rest (hybrid LLM with policy
routing, OS-native user-level supervisors, JSON-RPC 2.0 over stdio, the operator→daemon channel being
the Postgres `tasks` queue, a human-approve gate on persisted skills) are in
[`docs/architecture.md`](../../architecture.md) and the ROADMAP entries that shipped them.

**The one worth repeating, because everything else is downstream of it:** worst-case compromise
reaches *at most* the agent's own OS user, its own Postgres role, its own scratch FS, and the
allowlisted endpoints for the *one* compromised tool. Nothing else.
([`docs/threat-model.md`](../../threat-model.md))


## Recently merged

Newest first; substance under [Current state](#current-state), full prose in the
[`archive/`](archive/) snapshots and git history.

- **[#709](https://github.com/hherb/kastellan/pull/709)** — a follow-up reads its own conversation
  (#701): `tasks.turn_record` written by `finalize`, a windowed lookup, and a screened, budgeted
  `conversation` block in the planner's input, with the floor inherited from the turns loaded.
- **[#708](https://github.com/hherb/kastellan/pull/708)** `81c52ace` — docs only: the Obscura assessment
  corrected and the V8 bump built for the first time (111 errors; a porting project for upstream). #707, #706 open.
- **[#702](https://github.com/hherb/kastellan/pull/702)** `10164c22` — the planner reads a tool result as pruned, labelled JSON (#677). Filed #698,
  #699, #700, localmail #364; its second review round filed #703, #704, #705.
- **[#694](https://github.com/hherb/kastellan/pull/694)** `8e0c10f4` — an oversized dispatch still
  records what ran (#617). Filed #693, #695, #696, #697.
- **[#692](https://github.com/hherb/kastellan/pull/692)** `c5bf5e5f` — micro-VM preflight budgets
  (#690), the macOS Landlock drift detector (#689), cwd-independent rootfs scripts (#686). Filed #691.
- **[#688](https://github.com/hherb/kastellan/pull/688)** `09a4f924` — the container tier's REQUIRE
  knob and freshness gate (#684, #687).
- **[#685](https://github.com/hherb/kastellan/pull/685)** `0939e80c` — one producer for
  `target/release/` (#682).
- **[#683](https://github.com/hherb/kastellan/pull/683)** `ec9a2e94`,
  **[#680](https://github.com/hherb/kastellan/pull/680)** `fb560ab7`,
  **[#681](https://github.com/hherb/kastellan/pull/681)** `aee2a7f0`,
  **[#675](https://github.com/hherb/kastellan/pull/675)** `f831b3d1`,
  **[#669](https://github.com/hherb/kastellan/pull/669)** `4955a52c`,
  **[#660](https://github.com/hherb/kastellan/pull/660)** `62d98a00` — see git history and archive.

---

## How to update this document at session end

1. Move anything now shipped from [Next TODO](#next-todo) into [Recently merged](#recently-merged)
   and add the ROADMAP line.
2. Update the [Test baseline](#test-baseline-authoritative) with the gate that actually ran, on the
   host it ran on, and **reconcile the delta against the row above it**. An unexplained delta is a
   finding, not a rounding error.
3. Record what still binds — the finding, not the narrative. A fact that would change the next
   session's first move belongs here; a fact recoverable from `git log` does not.
4. Keep this file under ~500 lines. When it grows past that, snapshot it to
   `archive/handover_<date>_<topic>_pre-prune.md` and compress in place, leaving the archive link.
5. **Keep the header free of VCS state.** Since 2026-09-11 it names PRs and issues only — no
   branch names, no HEAD shas, no "OPEN". Three recurrences established that a claim a merge can
   falsify *will* be merged unchanged, because no actor stands between the write and the merge.
   Put what this session is doing under [Current state](#current-state) instead, where it reads
   as history the moment it lands rather than as a false claim.
6. Update [`ROADMAP.md`](../ROADMAP.md) in the same commit, and commit both together.

### Pruning convention

The archive snapshots are the long-form record; this file is the working brief. Compress by keeping
**what would change a decision** and dropping the narrative of how it was found — except where the
*way* it was found is itself the lesson, which is most of the ⚠️ blocks above.
