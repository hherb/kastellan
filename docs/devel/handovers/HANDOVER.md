# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20260914_677_pre-prune.md`](archive/handover_20260914_677_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here.

**Last updated:** 2026-09-14 ·
**Recent PRs, newest first:** [#702](https://github.com/hherb/kastellan/pull/702) (#677, the planner's labelled result view),
[#694](https://github.com/hherb/kastellan/pull/694) (#617, the bounded request summary),
[#692](https://github.com/hherb/kastellan/pull/692) (#690 + #689 + #686),
[#688](https://github.com/hherb/kastellan/pull/688) (#684 + #687). **Open issues these filed:**
[#698](https://github.com/hherb/kastellan/issues/698), [#699](https://github.com/hherb/kastellan/issues/699),
[#700](https://github.com/hherb/kastellan/issues/700), localmail
[#364](https://github.com/hherb/localmail/issues/364) (from #677); [#693](https://github.com/hherb/kastellan/issues/693),
[#695](https://github.com/hherb/kastellan/issues/695)–[#697](https://github.com/hherb/kastellan/issues/697)
(from #694); [#691](https://github.com/hherb/kastellan/issues/691) (from #692). ·
**DGX RUNS #702's HEAD**, deployed 2026-09-14 for its live acceptance run — the daemon links the new planner view and has loaded the new `agent_planner.md`. ⚠️ **After #702 merges, redeploy from `main` with `scripts/upgrade_from_git.sh`** (its checkout sits on the PR's branch). Rootfs images last rebuilt 2026-09-08.

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

### This session: #677 — the planner reads a tool result as labelled JSON

PR [#702](https://github.com/hherb/kastellan/pull/702). Design `docs/superpowers/specs/2026-09-13-planner-result-view-design.md` (with a
review-round addendum), plan `docs/superpowers/plans/2026-09-13-planner-result-view.md`. What binds:

**The root cause, from the DGX audit rows of task 186 (2026-09-05) and the live localmail API.**
`summary::render_step_outcome` built the planner's view of every successful step with
`injection_guard::extract_scannable_text`, which exists to flatten a value for the *guard* and drops
every object key, number and boolean. A live `mail.search` hit carries `has_attachments` as a
**boolean** (gone) and `message_id` as a string that survived only as a bare line beside the account
id's bare `1`. So the planner could not tell which hit had the PDF, never called `mail.get_message`,
and never reached `mail.get_attachment_text`. Contributing, filed: `mail.search` requires `query`
(#698); the planner never sees its own prior parameters (#699); localmail ignores the
`has_attachment` filter (localmail #364).

**What ships.**
- `core/src/scheduler/inner_loop/result_view.rs` — `prune` (caps: string bytes, array items, object
  keys; numbers/booleans/null kept) and `render(value, total)`, a **measured** search: at each
  container size, strings whole, else the highest string cap that fits (binary search, one "water
  level"); narrow arrays then objects one step at a time; else `{"_view_unavailable": …}`. Every
  returned candidate is measured, so the size bound needs no monotonicity argument.
- **Identifiers are atomic**: a space-free string ≤ `ATOMIC_MAX` (1024) is shown whole or not at all.
- **Keys are identifier-shaped or absent** (operator decision): 1–64 bytes of `[A-Za-z0-9_.:\-@/]`;
  any other key is dropped with its value and counted in `_omitted_keys`. ⚠️ **Keys never reach the
  guard model** — `tool_host::post_process` screens `extract_scannable_text`, which drops them — so
  this filter, not the screen, is what keeps a sentence out of a key. Do not "simplify" it away.
- Step outcomes are objects: `{"status":"ok","output":…}` / `withheld` / `elided` /
  `{"status":"err","code","detail"}`. The sink screen checks `screen_text(view)` — keys (separators
  read as spaces) and string leaves. Screen placeholders lose `score`/`reason_codes` before the
  planner sees them; the three key spellings are shared consts in `tool_host::injection_placeholder`.
- Budgets: per step 16 KiB (was 4), accumulated 96 KiB (was 32), counted in serialised bytes.
- `prompts/agent_planner.md` documents every shape; `the_planner_prompt_documents_every_outcome_shape`
  fails if renderer and prompt drift.

**Evidence.** 33 new tests, every one watched failing. Mutation: 4 planned mutants in the first
pass, then 8 over two builds in the review round, 16 kills, each mutant with a test no other mutant in
its build explains. Two-host gate in the table below. **Live acceptance (DGX, #702 deployed): the question that worked still works** — task 187 went search → three `mail.get_message` by real id → three `mail.get_attachment_text` by exact filename, same answer as task 185. **Task 186's question still fails** (task 188), and not because of the view: each DM is a stateless task, so the follow-up had no referent (**#701**); two of its five plans then went to filter-only searches rejected by #698, the second identical to the first (#699). **#677 stays open.**

⚠️ **This Mac, this session: cloning a 241 GB `target/debug` into a worktree took 78 minutes and
made `syspolicyd` re-assess every dylib** — rustc blocked in `dlopen`→`fcntl` for ~90 minutes, and
`readdir` of the cloned `deps/` blocked too. A fresh worktree is cheaper to build cold than to clone.

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


---

## Next TODO

> Only *open* work is listed. Shipped items move to [Recently merged](#recently-merged) or the ROADMAP.

1. **[#701](https://github.com/hherb/kastellan/issues/701) — a channel message becomes a stateless task, so a follow-up in the same conversation has no referent. This is what #677 actually needs.** Measured 2026-09-14 with #702 deployed: DM 1 (task 187) answered correctly; DM 2, a follow-up about "the last 3 flight bookings" (task 188), carried only its own sentence (`recall_count: 0`), searched from scratch, found a different booking and blamed the step budget. #677's original tasks 185/186 failed the same way. **Architectural — brainstorm first:** which turns; fencing the bot's own tool-derived answers as untrusted data; whether prior step outcomes (through the #702 view) or only final answers carry across; classification-floor inheritance; budget. Adjacent: ROADMAP `context_manager`, #629. **Acceptance: the same two DMs.** ⚠️ **Ask the operator how a live failure looked in the chat before blaming the change under test** — a screenshot settled in seconds what the audit rows had hidden for a week [[channel-dm-tasks-are-stateless]].

2. **The #677 follow-ups, each small and each measured by the same live question.**
   [#699](https://github.com/hherb/kastellan/issues/699) — the planner never sees the tool, method or
   parameters of its own prior steps (it dropped a filter and could not know); render them beside
   each outcome, through the sink screen. [#698](https://github.com/hherb/kastellan/issues/698) —
   `mail.search` cannot express a filter-only search; fix together with localmail
   [#364](https://github.com/hherb/localmail/issues/364), or an optional `query` returns unfiltered
   hits. [#700](https://github.com/hherb/kastellan/issues/700) — `plan.decision` reaches the prompt
   unscreened, contrary to `sink_screen_blocks`' doc. ⚠️ **#560 (fabricated `message_id`) is now
   worth re-measuring, not re-describing** — the labelled view is the mechanism its lead named.

**On the micro-VM path — one issue left, and it needs a kernel build.**
[#668](https://github.com/hherb/kastellan/issues/668) — repin a guest kernel built with
`CONFIG_SECURITY_LANDLOCK`, the standing posture item. ⚠️ **Its macOS twin now has a detector rather
than an issue** (#689, this session): the Apple `container` guest kernel does not enforce Landlock
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
  exactly that, deployed, and both later runs still fabricated. The lead worth measuring: with keys
  stripped, `"20973"` reaches the planner as a bare line among subjects and dates, with nothing
  marking it as *the id* [[tool-output-reaches-planner-key-stripped]]
  [[opaque-ids-are-unusable-tool-params]].
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


---

## Working state

### Test baseline (authoritative)

| Host | Commit | Result | clippy `-D warnings` | `[SKIP]` |
| --- | --- | --- | --- | --- |
| **Mac + DGX** ([#702](https://github.com/hherb/kastellan/pull/702), #677 — **the gate that stands**) | **`cc555f90`** (branch tip) | **DGX 4357 / 0 / 61**, 177 suites, `TEST_EXIT=0`, 0 `[WARN]`. **Mac 4222 / 0 / 29**, 177 suites, 0 `[WARN]`. **Both exactly as predicted** from the static `#[test]` name diff: +33 over `main` (DGX 4324, Mac 4189) — 28 in `result_view`, 5 net in `summary`; host gap **135**, unchanged. ⚠️ **The Mac `TEST_EXIT` is 101 with one failure that is the host, not the branch:** `syspolicyd` had re-saturated; five suites wedged at exec (killed at 15 min, 0 CPU) and `egress_force_routing_e2e` timed out waiting for its sidecar to start right after the restart. All six pass individually, which is where 4213 + 1 + 8 = 4222 comes from | exit **0** on both, **27** workspace crates each by count of `Checking kastellan` lines; the DGX run forced cold with `CARGO_TARGET_DIR=$HOME/.cargo-clippy-677` after its first pass finished suspiciously in 12 s | **340** Mac (absent-Postgres), **4** DGX (gliner, held) |
| **Mac + DGX** ([#694](https://github.com/hherb/kastellan/pull/694), #617) | **`65899e9c`** (branch tip; squashed to `8e0c10f4`) | **Mac 4185 / 0 / 29**, **DGX 4320 / 0 / 61**, both 177 suites, 0 `[WARN]`. #694's own review round then added **+4** before merge, so `main` is **DGX 4324** — confirmed by #677's pre-review DGX sweep landing at exactly 4324 + 22 = **4346**. The Mac run needed a `syspolicyd` repair and four suites re-run individually | exit 0 on both, 27 crates | 296 Mac, 4 DGX |

Older rows are in the [`archive/`](archive/) snapshots.

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

- **[#702](https://github.com/hherb/kastellan/pull/702)** — the planner reads a tool result as pruned, labelled JSON (#677). Filed #698,
  #699, #700, localmail #364. *(Open at time of writing — see the header convention.)*
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
