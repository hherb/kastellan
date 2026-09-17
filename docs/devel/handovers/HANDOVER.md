# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20260914_677_pre-prune.md`](archive/handover_20260914_677_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here.

**Last updated:** 2026-09-17 ·
**Recent PRs, newest first:** [#709](https://github.com/hherb/kastellan/pull/709) (#701, conversational
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
(from #694); [#691](https://github.com/hherb/kastellan/issues/691) (from #692). ·
**The DGX runs #702's code**, deployed 2026-09-14 from the PR branch for its live acceptance run. #702's branch tip and its squash commit on `main` are **content-identical** (verified 2026-09-15, empty `git diff`), and #708 touched no code, so the running daemon already matches `main`. Only the DGX checkout is off `main`; re-point it with `scripts/upgrade_from_git.sh` at the next deploy. Rootfs images last rebuilt 2026-09-08.

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

### This session (later, 2026-09-17): backlog triage — 150 open issues

No review target existed (clean tree, `main` in sync with `origin`, no PR), so the session became a
triage of every open issue. Full record with the evidence behind each decision:
[`notes/2026-09-17-backlog-triage.md`](../notes/2026-09-17-backlog-triage.md).

⚠️ **The ROADMAP is the accurate source and the GitHub issues are the stale mirror** — the reverse
of what the six epics assumed. Closure is healthy for PR-driven work (#661–#701 were each closed by
their fixing PR) and **zero for roadmap-era work: 58 issues (39%) were filed 2026-06 or earlier and
never touched since.** The cause is mechanical — **~7% of issues carry any label** (10 of 150), so
the old cluster cannot be filtered for and stays invisible.

⚠️ **Staged, NOT executed.** The session's sandbox blocked `gh`, so **every issue below is still
OPEN**: close #227/#215/#216 (shipped, each against a ROADMAP `[x]`), #214/#219 (struck through as
*rejected 2026-06-12*), #213 (the ROADMAP supersedes the IMAP framing in words; `workers/email-in`
is a localmail REST client and nothing here speaks IMAP), #203–#208 (epic mirrors whose every link
still points at `hherb/hhagent`, renamed two days after they were filed), #655. The runnable script
and the per-issue rationale are in the note.

⚠️ **#655 is the precondition for the whole false-green-gate cluster** (#714, #664, #622, #691, and
#237's absent macOS leg — against #679, #667, #682, #684, #687 already closed in the same class).
Verified still true: `protect_main` carries only `deletion` + `non_fast_forward`, so **every CI job
is advisory** — a gate that cannot block a merge is a notification. Safe to turn on:
`linux-check.yml` triggers on `pull_request:` with **no `paths` filter**, so all three jobs report
on every PR and no docs-only PR can deadlock on a check that never reports. Fixing the cluster
one issue at a time keeps regenerating it; one contract — every gate needs a REQUIRE knob **and** a
positive control that fails when zero tests ran — retires the class.

**One real ROADMAP defect found and fixed — the only tree change this session made:** the Phase 4
line for the `python-exec` micro-VM backend had stayed `[ ]` since the original seeding while
duplicating two `[x]` entries (Firecracker slice 1, PR #364; Apple `container`, 2026-05-21), its
text still describing the work at "discovery spike … verdict COMMIT" stage.

### Earlier this session: #701 — a follow-up reads its own conversation

Design `docs/superpowers/specs/2026-09-16-conversation-continuity-design.md` (with a D2
amendment), plan `docs/superpowers/plans/2026-09-16-conversation-continuity.md`. What binds:

**The defect, measured:** every channel message became a task carrying only its own sentence, so
"from where to where did those bookings go?" had no referent. A finishing channel task now writes
`tasks.turn_record` = `{calls, data_class}` in the **same `finalize` UPDATE** that makes it
terminal, and the next task in the same `(channel, peer, conversation)` reads up to 3 such turns,
renders them screened and budgeted, and inherits their classification.

- **Calls, not results.** A turn carries the successful steps' `{tool, method, parameters,
  returns}`. The identifier the next turn needs is the one the last turn **passed**
  (`{message_id, filename}`), so this is both smaller than results and more useful — and no tool
  output crosses a task boundary.
- ⚠️ **The window has NO upper bound, and that is a decision, not an oversight.** The back edge is
  anchored on the asking task's `created_at` (so a task suspended on an operator ask never *loses*
  turns), but an upper bound at that same instant **blinds the case the feature exists for**: a live
  turn takes 2.5–4.5 min, so a user typing again while the bot works produces a task whose
  `created_at` precedes the previous turn's `finished_at`. A mutant restoring the bound is killed by
  `a_turn_that_finished_after_the_asking_task_arrived_is_still_a_turn`.
- ⚠️ **A turn we cannot classify is NOT shown** (`status: "unclassified"`). The class is parsed
  **independently of the calls**, so a future shape change to `calls` can cost the calls but never
  the class. Before that fix, a turn with a NULL or unparseable record rendered its *text* while
  contributing nothing to the floor — and **every turn predating migration 0026 has a NULL record**,
  so the first follow-up in every live room would have hit it. ⚠️ **That sentence was true of the
  renderer and FALSE of the floor until the third review round** — see below; the independent field
  existed and nothing read it.
- ⚠️ **The floor is inherited from every turn LOADED**, not from those the screen kept. Otherwise one
  catalogue phrase in an earlier answer would both withhold that turn *and* drop the conversation's
  floor to `Public` — letting an attacker choose what the follow-up may do. New provenance
  `conversation_inherited`; **rule I2 then requires every step at or above it**, so a follow-up can
  be stricter than the same sentence sent fresh.
- **Screening is sealed, not conventional.** `view::admitted::Admitted` has a field private to its
  inner module, so a candidate that skips the screen does not compile. The earlier free function was
  bypassable and a mutant proved it: it routed the budget-clamped renderings straight to the output
  and survived the whole suite.
- **`plan.formulate` gains `conversation_task_ids`:** `[]` when the lookup found nothing, **`null`
  when it failed** (a failed read fails open — nothing is carried, so nothing needs classifying).
  Key-set pin 28 → 29.
- ⚠️ **Two reviews, 46 mutants, 19 survivors — all now dead.** The db review found the window, the
  exclude clause, the channel filter and 4 of 7 states untested behind a `LIMIT 3` that excluded the
  out-of-window row regardless. The core review found the sealed-screen gap, inheritance-over-
  rendered (**the design's own planned mutant**), an unscreened `user` key, and a last-turn-drop
  test whose assertion the omission marker itself satisfied.
- ⚠️ **My own first mutation harness was a false green:** `--exact` with a bare test name matches
  **nothing** for a lib test, so 9 mutants "survived" against **zero tests**, exit 0. Every mutation
  run now requires its killer to run and pass on unmutated code first.
- ⚠️ **A fresh worktree has NO worker binaries, and `cargo test --workspace` does not create them.**
  Daemon-spawning e2es then fail closed at boot with `Error: building egress force-routing config`
  — 9 failures that look like a regression and are a missing prerequisite. **`cargo build
  --workspace` first**; all 9 passed after it.

#### Third review round on the same branch (2026-09-17) — the split parse was never wired

Five read-only reviewers (code, tests, error handling, type design, comments), each finding the same
defect independently. Fixes then verified by planting each original defect back and watching the new
test fail.

- ⚠️ **A security property can be documented, tested, and absent from the code.** `Turn::data_class`
  was added *specifically* so a record whose `calls` stop parsing still yields a class — with a long
  comment saying "the floor a follow-up inherits comes from here". **`inherit_floor` read
  `t.record.data_class` instead**, so for that exact row the text rendered and the floor did not:
  a `ClinicalConfidential` answer reaching a follow-up planned at `Public`, which is verbatim the
  scenario the comment said was prevented. `git show ff964274 -- .../conversation/floor.rs` is
  **empty** — the commit that introduced the field and its rationale never touched the consumer.
- ⚠️ **The mutation proof defended the bug**, because a test encoded it.
  `a_turn_with_no_record_contributes_nothing` built the divergent state (`record: None`,
  `data_class: Some(Secret)`) and asserted the floor stays `Public`; two further tests pinned the
  other legs in two other files. **Three tests, one property each, none composing them.** A mutant
  flipping the field would have been *killed*. The fix is one line; the test correction is the real
  work. New `a_turn_that_renders_its_text_always_contributes_its_class` states it as an invariant
  over any row — if the rendered turn carries text, the floor must have risen — so a future
  divergence fails whatever shape it takes. [[mutation-proof-counts-only-mutants-you-tried]]
- ⚠️ **A denylist over an extensible enum is a guard with an expiry date.** #71's
  `parse_classification_floor_source_from_payload` rejected `AgentRaised` on a *structural* match,
  with a comment arguing that binding to the variant survives a rename. It does — and says nothing
  about an **addition**, so #701's new `ConversationInherited` walked straight through and a producer
  could stamp a floor as inherited from a conversation never read. **Now an allowlist**
  (`Operator | CliInferred | Default`), so every future variant is reserved until admitted on
  purpose, plus a census test that makes the decision compulsory. [[guard-shares-the-census-blind-spot]]
- ⚠️ **A test helper measured the quantity the production comment disowns.** `render`'s doc says the
  element sum "does make the doc's bound untrue" and `array_len` exists to measure the array — while
  `rendered_bytes` in the tests summed elements, so a regression back to element-summing passed.
  Also fixed: the drop loop measured an omission marker it had not yet earned (`omitted + 1`), so a
  conversation that fit *exactly* lost its oldest turn to ~22 bytes.
- Also: conversation `injection.blocked` rows are written on the **first run only** — `run_one` is
  re-entered on every resume and re-screens the same turns, and `sink_block_audit_payloads`
  documents that identical hazard one module over; the clamp branch is guarded on `len() == 1`
  rather than assuming it; `window_hours` out of range now errors instead of becoming a
  245,000-year window; `ORDER BY` gained an `id DESC` tiebreaker; the silent `calls` serialisation
  drop now warns and marks `_omitted_calls`; and the prompt documents that `calls` may be **absent**
  and that `parameters` obeys the `result_view` pruning rules.
- **Deferred, filed:** [#710](https://github.com/hherb/kastellan/issues/710) (a failed, crashed or
  denied channel turn leaves no record — so "what went wrong with that?" inherits nothing; the
  `finish!` comment claiming every exit was corrected),
  [#711](https://github.com/hherb/kastellan/issues/711) (the prompt drift guard's key names are
  literals with no constant to bind to — its blind spot is the set its comment claimed),
  [#712](https://github.com/hherb/kastellan/issues/712) (`REPLIED_STATES` ↔ `notify_task_completed`
  is enforced by review, and the test is blind to the direction that loses turns),
  [#713](https://github.com/hherb/kastellan/issues/713) (a resumed task loses its floor, storing a
  turn record whose class is **wrong** rather than missing — so the renderer cannot withhold it),
  [#714](https://github.com/hherb/kastellan/issues/714) (no `KASTELLAN_PG_REQUIRE_E2E` knob, so a
  mis-provisioned host reports the same count having asserted nothing),
  [#715](https://github.com/hherb/kastellan/issues/715) (the `Admitted` seal ends at `render`'s
  return; a `ScreenedTurns` newtype would carry it to the prompt),
  [#716](https://github.com/hherb/kastellan/issues/716) (`conversation_task_ids` names the shown set
  and holds the loaded one).

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

**THEN, cheap and long overdue:** [#655](https://github.com/hherb/kastellan/issues/655) — `main` has
**no required status checks**, so clippy, the matrix build and `python-lock-check` can all go red and
still merge. A repo-settings change, not code.

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
| **Mac + DGX** ([#709](https://github.com/hherb/kastellan/pull/709), #701 — **third review round, the gate that stands**) | `d6698013` | **DGX 4453 / 0 / 61**, 179 suites, `TEST_EXIT=0`, **4 `[SKIP]`** (all gliner, opt-in). **Mac 4318 / 0 / 29**, 179 suites, `TEST_EXIT=0`. ⚠️ **Both deltas are +10 and reconcile exactly** against the row below (DGX 4443, Mac 4308): 10 new `#[test]` in the diff (3 conversation, 1 floor, 1 record, 2 view, 2 task_exec, 1 db e2e), and **each of the 10 was grepped out of the DGX log by name as `... ok`** rather than inferred from the total. `KASTELLAN_PG_BIN_DIR` set on both hosts, so every Postgres suite ran for real — on the DGX the proof is that **zero of the 4 skips are PG** (all four are the gliner tier), and the PG-gated `a_task_that_is_not_a_channel_task_is_not_a_turn` passed. ⚠️ **The Mac gate was re-run after a post-sweep edit** — the first sweep was green, then the clamp block was re-indented, which made that sweep a gate on a revision that no longer existed [[never-edit-tree-during-a-sweep]]. Both headline fixes verified by planting the original defect back and watching the new test fail, and the `kind='channel'` mutant killed live against PG | **DGX exit 0** and **Mac exit 0**, `--workspace --all-targets -D warnings`, zero warnings, all 27 crates. **This also clears the DGX clippy run the row below records as owed** | **4** DGX (gliner only), 15 Mac |
| **Mac + DGX** ([#709](https://github.com/hherb/kastellan/pull/709), #701 — **the gate that stands**) | branch tip `ff964274` | **DGX 4443 / 0 / 61**, 179 suites, `TEST_EXIT=0`, **4 `[SKIP]`** (gliner, held). **Mac 4308 / 0 / 29**, 179 suites, **86 `[SKIP]`**. ⚠️ **Both deltas are +61 and reconcile EXACTLY against a measured baseline, not an estimate:** `cargo test --workspace -- --list` on the DGX gives **4504 on the branch against 4443 on `main`**, and the static count of new `#[test]` + `#[tokio::test]` in the diff is also 61. 179 suites = 177 + the two new test binaries. ⚠️ **And the measurement corrected a carried estimate:** `main`'s DGX total is **4382 runnable + 61 ignored**, where the #702 row above implies ~4360. Extrapolating one row from the next drifts; `-- --list` on both revisions costs one compile and settles it. ⚠️ **The Mac's 86 skips (not ~340) are because `KASTELLAN_PG_BIN_DIR` was exported**, so every Postgres-gated suite ran for real here — including all 10 of this branch's own PG tests. ⚠️ **The Mac sweep first reported `TEST_EXIT=101` with 9 failures in three daemon-spawning suites, and that was a missing PREREQUISITE, not a regression:** a fresh worktree has no worker binaries and `cargo test --workspace` does not build them, so the daemon fails closed at boot (`Error: building egress force-routing config`). After `cargo build --workspace`, all 9 pass (6 + 2 + 1). The DGX, whose checkout had the binaries, was green throughout | **Mac** `--workspace --all-targets --locked -D warnings` exit **0**, zero warnings, all **27** workspace crates, forced cold with `CARGO_TARGET_DIR=$HOME/.cargo-clippy-701` (never by touching sources, which would falsify the #687 image gate). **The DGX clippy run is still owed** | **86** Mac (PG live), **4** DGX |
| **Mac** ([#702](https://github.com/hherb/kastellan/pull/702) second review round) | the round's commit on the branch | **Full sweep 4244 / 0 / 29**, 177 suites, `TEST_EXIT=0` — exactly the predicted +22 over the row below — run **before** the review of the fix round, whose fixes then added **+3** tests (a full sweep would be **4247**). After those fixes, re-run in full: `kastellan-core --lib` **2090 / 0 / 1**, `kastellan-cli` 96, `kastellan-worker-mail` 141 + 3 e2e. **The DGX was not re-run this round.** | exit **0**, **27** crates, cold (`CARGO_TARGET_DIR=$HOME/.cargo-clippy-702`), after the last source edit | **not audited**: the sweep ran without `--nocapture`, so libtest swallowed the `[SKIP]` lines — do not compare this row's skip column |
| **Mac + DGX** ([#702](https://github.com/hherb/kastellan/pull/702), #677 — **the gate that stands**) | **`cc555f90`** (branch tip) | **DGX 4357 / 0 / 61**, 177 suites, `TEST_EXIT=0`, 0 `[WARN]`. **Mac 4222 / 0 / 29**, 177 suites, 0 `[WARN]`. **Both exactly as predicted** from the static `#[test]` name diff: +33 over `main` (DGX 4324, Mac 4189) — 28 in `result_view`, 5 net in `summary`; host gap **135**, unchanged. ⚠️ **The Mac `TEST_EXIT` is 101 with one failure that is the host, not the branch:** `syspolicyd` had re-saturated; five suites wedged at exec (killed at 15 min, 0 CPU) and `egress_force_routing_e2e` timed out waiting for its sidecar to start right after the restart. All six pass individually, which is where 4213 + 1 + 8 = 4222 comes from | exit **0** on both, **27** workspace crates each by count of `Checking kastellan` lines; the DGX run forced cold with `CARGO_TARGET_DIR=$HOME/.cargo-clippy-677` after its first pass finished suspiciously in 12 s | **340** Mac (absent-Postgres), **4** DGX (gliner, held) |
| **Mac + DGX** ([#694](https://github.com/hherb/kastellan/pull/694), #617) | **`65899e9c`** (branch tip; squashed to `8e0c10f4`) | **Mac 4185 / 0 / 29**, **DGX 4320 / 0 / 61**, both 177 suites, 0 `[WARN]`. #694's own review round then added **+4** before merge, so `main` is **DGX 4324** — confirmed by #677's pre-review DGX sweep landing at exactly 4324 + 22 = **4346**. The Mac run needed a `syspolicyd` repair and four suites re-run individually | exit 0 on both, 27 crates | 296 Mac, 4 DGX |

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
is audited, every `[SKIP]` renders through the pure
[`tests_common::skip::skip_line`](../../../tests-common/src/skip.rs) — **assert on `skip_line`; call
the `skip_if_*` wrappers only from real fixtures.**

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
