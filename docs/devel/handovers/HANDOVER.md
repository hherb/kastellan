# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20260927_773_pre-prune.md`](archive/handover_20260927_773_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here.
> ⚠️ **Repoint this line in the same commit as the snapshot.** It has been stale twice.

**Last updated:** 2026-09-28, latest (#769 — a live worker's refusal keeps the worker, PR #781,
with its review round; the operator is still running the #773 live re-measure) ·
**Recent PRs, newest first:** [#781](https://github.com/hherb/kastellan/pull/781) (#769), [#778](https://github.com/hherb/kastellan/pull/778) (#767, #768), [#776](https://github.com/hherb/kastellan/pull/776) (#773, #774), [#775](https://github.com/hherb/kastellan/pull/775) (handover), [#770](https://github.com/hherb/kastellan/pull/770) (#673, #674), [#766](https://github.com/hherb/kastellan/pull/766) (#763, #765), [#764](https://github.com/hherb/kastellan/pull/764) (#760), [#762](https://github.com/hherb/kastellan/pull/762) (#760), [#761](https://github.com/hherb/kastellan/pull/761) (#698, #561), [#758](https://github.com/hherb/kastellan/pull/758) (#755), [#756](https://github.com/hherb/kastellan/pull/756) (#748), [#750](https://github.com/hherb/kastellan/pull/750) (#746, #747, #749),
[#745](https://github.com/hherb/kastellan/pull/745) (#734, #733, #732, #742),
[#743](https://github.com/hherb/kastellan/pull/743) (#737, #738, #739),
[#740](https://github.com/hherb/kastellan/pull/740) (#736), [#735](https://github.com/hherb/kastellan/pull/735) (#730),
[#731](https://github.com/hherb/kastellan/pull/731) (#725), [#728](https://github.com/hherb/kastellan/pull/728) (#699, #700),
[#726](https://github.com/hherb/kastellan/pull/726) (#719), [#720](https://github.com/hherb/kastellan/pull/720) (the REQUIRE-knob contract).
**The #725 → #748 worker-report arc is closed**, #748 being its last piece. Its review residue
is filed as #751–#754 and #757. Older filings are in the [`archive/`](archive/) snapshots;
**`gh issue list --state open` is the live answer** and the only one worth trusting. ·
**The DGX runs `main` as of #776**, redeployed 2026-09-27 (evening) via `scripts/upgrade_from_git.sh`
(15 binaries; generated env **and** `.local` overlay byte-identical to the pre-deploy backups
`~/kastellan.env*.bak-pre776`; Matrix channel up; `NRestarts=0`). The live process's environ has
`KASTELLAN_LLM_DISABLE_THINKING=0`, `KASTELLAN_LLM_THINKING_SWITCH=reasoning_effort`,
`KASTELLAN_LLM_TIMEOUT_MS=600000`, and the boot line reads `"disable_thinking":false,
"thinking_switch":"reasoning_effort"` — so planning **thinks** (deliberately) and the #774 synthesis
retry can drop it. No sweep was re-run for #776 on the DGX beyond its PR gate (core `scheduler::`
369, llm-router 116 + 6, cold clippy, 0 `[SKIP]`). The #770 deploy's post-deploy full sweep: **187/187 suites, 4729 passed / 0 failed / 79 ignored**, 0
`[WARN]`, 4 `[SKIP]` (all the opt-in GLiNER tier); `mail-live` gate green as evidence; the
force-routed live round trip green with the real key and, with a **bogus key**, failing as
`-32004` "localmail rejected kastellan's credential (HTTP 401) … retrying will not help" through the
real sandboxed worker + MITM proxy. Rootfs images last rebuilt
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

### This session (2026-09-28, latest): #769 — a refusal is not a death (PR #781 + review)

- **One classifier, `worker_lifecycle/persistent/call_failure.rs`** (`classify_call_error` →
  `Gone` / `Refused` / `CredentialRefused`), read by the supervisor **and** the polled driver;
  pinned to the census over every `ClientError` variant (a `match` witness breaks the build on a
  new one). Its module doc lists **each job the old respawn did and where it went** — the review
  found three of four were lost, all on the email channel:
  - **Refused** → the worker is kept (no death report, respawn or alarm tick).
  - **CredentialRefused** → replaced **quietly**, so a renewed token is read. ⚠️ A worker reads its
    credential once, at spawn, and bwrap binds the token file **by inode** — an in-worker re-read
    would miss a rotation by rename.
  - **A refusal from behind a dead egress sidecar** → a death (`PersistentTransport::sidecar_exited`,
    `try_wait`), flattened so the driver reads "down". email-in reports every failed localmail
    request as a typed `OPERATION_FAILED "transport: …"`, so it can only ever *refuse*.
- **The polled driver paces refusals per method — send, poll and ack** (`polled_driver/refusal.rs`,
  pure `RefusalRun`; `REFUSAL_BACKOFF` 1 s → 60 s, checked at spawn); logged first, every 15 min, and
  at once on a new code. ⚠️ **A refused ack holds the next poll**: a poll with events does not wait,
  so an unpaced refused ack re-delivered the same email as fast as localmail answered.
- `kastellan-protocol`: `"result": null` is `Ok(Null)` (serde reads it as `None`); a response with
  neither member is `Decode`, not a client-built `RpcError` that read as a live refusal.
- A refused send keeps its place and polling continues — ⚠️ **every conversation's replies queue
  behind it** (#782). Refusal lines carry no stderr marker (#783). Matrix: no recycle rule
  (operator's call); its sync give-up (`sdk_live.rs`, policy `sync_retry.rs`) is a death.
  ⚠️ **Workspace MSRV is 1.78** (`Option::is_none_or` is 1.82).

### Previous (2026-09-27/28): #767 + #768 — the #766 review residue (PR #778)

Route spellings are pure `pub fn`s in `localmail_contract.rs`, used by the worker **and** the live
gate — ⚠️ still `include!`d three times: no `use`, no `//!`, every item used by the gate. The gate
pages from offset **2** (a respelled `offset` read as the default at 0), picks an attachment whose
paged `/text` answers, and hashes the hash route's bytes. A planner hash is lowercased at parse
time (`ShaPrefix`). Filed #779 (sha+filename fail-open vs index+filename — a policy call), #780.

### Previous (2026-09-27): #773 + #774 — thinking, and slow planning calls — what still binds

Full prose in git history (#776) and the `773` archive snapshot.

- **`KASTELLAN_LLM_THINKING_SWITCH`** (`llm-router/src/thinking.rs`): `chat_template_kwargs`
  (default) or `reasoning_effort` (**the only one Ollama honours**). ⚠️ **Never send both:** vLLM
  **0.15.1** (the DGX's `vllm:26.02`) 400s on `reasoning_effort: "none"`. `for_guard` **pins** kwargs.
- **`llm_usage` on every `plan.formulate` row** — Ollama reports **no** reasoning-token count; read
  `reasoning_chars`. Every failed planning call is counted and audited
  (`agent/plan.formulate_failed`).
- **Timeouts** (`inner_loop/llm_failure.rs`): after gathering, a timed-out plan spends the synthesis
  turn; a timed-out synthesis retries once with thinking suppressed; a final timeout is worded for
  the user (`like '%timed out%'` still matches).
- ⚠️ **Not decided, deliberately:** thinking on vs off — that is the live re-measure.

### Previous (2026-09-27): #673 + #674 — a refused credential says so

`codes::UPSTREAM_AUTH_FAILED = -32004` + pure `upstream_auth_refusal` (401/403) in `mail` and
`email-in`; the dispatcher logs `operator action needed: …`; the email channel's `OutageLog`
repeats the refusal every 15 min. ⚠️ Only `ClientError::Rpc` keeps its type through
`client_error_to_anyhow`. #769 fixed by #781 (see above). Full prose in the `673` archive snapshot.

### Previous (2026-09-26/27): the mail worker — #760, #698, #763, #765 — what still binds

Full prose in [`archive/handover_20260927_673_pre-prune.md`](archive/handover_20260927_673_pre-prune.md).

- **Version gate:** search, both attachment tools and header reads refuse below localmail API
  **1.3** — **no fallback** (operator's call). Params are parsed into a typed `Request` (pure
  `handler/request.rs`) **before** the gate, which takes `&Request` (#765).
- **Wire facts live in `workers/mail/src/localmail_contract.rs`**, `include!`d by the live gate and
  `mock_localmail`. ⚠️ **Compiled three times: `pub const`s only, no `use`, no `//!`.**
- **Slice E:** `fields` + `snippet_chars: 120` on every search. ⚠️ **A 50-hit page is still ~18 KB,
  over the 16 KiB step view.** **Headers:** `?headers=list`, checked fail-closed, never reshaped.
- **Slice D:** attachments fetched **by position** and **hashed against the listed sha**. Paged text:
  `next_offset` **copied, never computed**; inconsistent paging is a fault. ⚠️ 16 KiB view
  guaranteed only for Latin text.
- **#698:** a blank query defaults to `sort: date`. ⚠️ `sort::is_textless` cannot see an
  operator-only query.
- **Live gate:** `bash scripts/mail/live-shape-gate.sh` → the `mail-live` profile (knob
  `KASTELLAN_MAIL_LIVE_REQUIRE_E2E`). ⚠️ **A new `*_or_skip` helper must be classified** in
  `microvm/guard.rs` (`BANNED_HELPERS` or `REQUIRE_AWARE`) or the roster test refuses it.

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

### Previous (2026-09-20/23): the #725 → #750 worker-report arc and #748

Full prose in the `748`/`755` archive snapshots. What still binds:

- **Every profiled test binary must reach the panic hook, checked at RUN time** — call
  `panic_hook::install_once()` first; a knob installs it through **two** doors (`raw()`,
  `action_reporting_to`). `MAX_PANIC` measured 0 on all five profiles; blind before the first knob
  read (#757). ⚠️ **`grep -c` counts lines** [[grep-c-counts-lines-not-matches]].
- ⚠️ **`warn_and_fall_back!` must stay a MACRO** (the delivery check expands at the emitter's
  callsite, `message` field included). **`eprintln!` is load-bearing** [[libtest-capture-only-print-macros]].
- ⚠️ **No marker may begin with `[SKIP]`/`[WARN]`/`[E2E]`** (`[worker-failed]`, `[worker-death]`,
  `[worker-down]`, `[panic]`, `[panic-hook]` — disjoint, tested). `shutdown()`'s join is the only
  proof a report was emitted; `WorkerRetirementCause::from_client_error` is THE census.
- ⚠️ **Give a mutating reviewer its own worktree, and bracket every sweep with a source sha**
  [[never-edit-tree-during-a-sweep]].

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

1. **The live re-measure — the operator is running it** (DMs, a **fresh room**; #776 is deployed,
   thinking ON, `reasoning_effort` dialect, 600 s timeout). First measurement: re-ask the **Qantas question** (tasks 193/194 timed
   out on the synthesis turn), then read each plan's cost in one query:
   `select action, payload->'llm_usage', payload->>'latency_ms', payload->>'error' from audit_log
   where action in ('plan.formulate','plan.formulate_failed') and payload->>'task_id'='<id>' order by id`. Then #728's multi-search question, a filter-only one (#698), a
   long PDF read across pages (#760), and a revoked mail key once (the planner's reaction to
   `UPSTREAM_AUTH_FAILED`). **The thinking decision (#773) is made from these rows:** repeat the
   set with `KASTELLAN_LLM_DISABLE_THINKING=1` (now real on Ollama) and compare answer quality
   against latency — the #773 probe's non-thinking answer was *wrong*.
   Then #771 (carry localmail's own 401 reason to the operator — the channel's line is now in
   `polled_driver/refusal.rs`), #782, #783 and #538
   (the mail worker's second, hand-rolled localmail mock). **Owed: a DGX run of the `mail-live`
   profile** (`bash scripts/mail/live-shape-gate.sh`) — only the Mac has run it since #766.

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
`core/src/scheduler/asks.rs`, `core/src/tool_host.rs`,
`workers/mail/src/ids.rs`, `tests-common/src/require.rs`,
`core/src/worker_lifecycle/persistent.rs`, `core/src/scheduler/inner_loop.rs`,
`core/src/channel/bus.rs`, `workers/matrix/src/sdk_live.rs`, `llm-router/src/messages.rs`,
`core/src/main.rs`, `tests-common/src/microvm/{mod,container}.rs`, `sandbox/tests/macos_smoke.rs`.
⚠️ **#743 and #745 both grew files already over cap without splitting them** (`tool_host.rs`,
`worker_lifecycle/persistent.rs`); #750 split `worker_stderr/mod.rs` **first** instead. #748
pushed `panic_hook.rs` over (432→584) and split its tests out (357 + 229); it also grew
`scripts/run-e2e-gate.sh` to 561 (shell, not split) and `require.rs` 647→661 (already over).
#673/#674 grew three already-over files a little without splitting: `tool_dispatch.rs` 710→722,
`worker_lifecycle/persistent.rs` 590→662 (pure fn + its tests), `polled_driver/tests.rs` 670→733.
`channel/polled_driver.rs` would have crossed 500, so its failure logging went to a new
`polled_driver/outage.rs` (433 + 111). #773/#774 split tests out **first-class** instead:
`llm-router/src/config.rs` 924→385 and `messages.rs` 593→385, `scheduler/agent.rs` 542→429 — but
grew `inner_loop.rs` 878→923 (already over; its new logic went to `inner_loop/llm_failure.rs`).
#767/#768 split `workers/mail/src/attach.rs` **first** (717→559, `Picked` → `attach/picked.rs`)
and put its new tests in `attach/tests/hash_and_route.rs` (`attach/tests.rs` 719→727); it grew
`core/tests/mail_live_shape_e2e.rs` 508→529→**559** after the second review (one test fn — **split
it before the next leg**; the attachment half is the natural cut) and
`gate_script_tests.rs` 518→519 (its new check went to `gate_script_tests/callers.rs`).
#769 split `worker_lifecycle/persistent.rs` **first** (662→407 + tests 254, movement proven by
`cmp` with a negative control); its review put the classifier in `persistent/call_failure.rs`
(`persistent.rs` 474) and the ack calls in `polled_driver/ack.rs` (`polled_driver.rs` 497→469).

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
| **Mac** (#769 review round, PR #781 — **the gate that stands**) | the review-fix commit on PR #781 | **4684 / 0 / 47**, **188** suites, `TEST_EXIT=0`, `[WARN]` **0**, `[SKIP]` **23** (unchanged), tree hash identical before and after; `KASTELLAN_PG_BIN_DIR` set (a first sweep without it showed 362 `[SKIP]`s at the *same* 4684 — not evidence). **Delta +18, predicted exactly:** protocol lib +3, core lib +15 (polled driver +10, persistent +4, egress spawn +1). Mutants: 12 of 12 caught | exit 0, cold `CARGO_TARGET_DIR=$HOME/.cargo-clippy-781`, **27** distinct `Checking kastellan` | **23** Mac |
| **Mac** (#769, PR #781 first commit — superseded) | `f8461f66` | **4666 / 0 / 47**, **188** suites, `TEST_EXIT=0`, `[WARN]` **0**, `[SKIP]` **23** (unchanged), tree hash identical before and after. Same flags as below. **Delta +9, predicted exactly:** core lib only — persistent +2, polled driver −3 old `OutageLog` + 1 latch + 9 refusal. Mutants: 6 of 7 caught, the survivor near-equivalent (in the PR). After the MSRV fix (one line), core lib 2263 re-passed | exit 0 after the `is_none_or` fix; cold `CARGO_TARGET_DIR=$HOME/.cargo-clippy-769`, **27** distinct `Checking kastellan` across the two runs | **23** Mac |
| **Mac** (#778 second review — superseded) | `9ee5d1ee` | **4657 / 0 / 47**, **188** suites, `TEST_EXIT=0`, `[WARN]` **0**, `[SKIP]` **23** (unchanged), HEAD + tree status identical before and after. Same flags as below. **Delta +4, reconciled per suite:** mail bin 211→213 (+2), tests-common lib 452→454 (+2). `mail-live` gate green as evidence on the tip, plus two live mutants (`limit=`, `offset=` respelled in the contract) each failing it; mail rustdoc 0 warnings | exit 0, cold (`CARGO_TARGET_DIR=$HOME/.cargo-clippy-778fix2`, re-run after a first run overlapped a mutant), **27** `Checking kastellan` lines | **23** Mac |

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
| `core` `mail_live_shape_e2e` (`#[ignore]`) | 1 | our reading of localmail against the **live** service. `bash scripts/mail/live-shape-gate.sh` (both hosts) |
| `tests-common` `gate_script_tests` | 27 | the gate script's table **and** its verdict (floors, caps, per-binary hook check), run against a fake `cargo` |

## Key design decisions locked in

**Not restated here — they drift.** Hard constraints: [`CLAUDE.md`](../../../CLAUDE.md) § Hard
constraints; the rest: [`docs/architecture.md`](../../architecture.md) and the ROADMAP.
**The one worth repeating:** worst-case compromise reaches *at most* the agent's own OS user, its own
Postgres role, its own scratch FS, and the allowlisted endpoints for the *one* compromised tool
([`docs/threat-model.md`](../../threat-model.md)).

## Recently merged

Newest first; full prose in the [`archive/`](archive/) snapshots and git history.

- **[#781](https://github.com/hherb/kastellan/pull/781)** (#769) — a live worker's `RpcError` keeps the worker (no death
  report, respawn or alarm); a credential refusal replaces it quietly, a dead sidecar retires it;
  the polled driver backs off per method (send, poll, ack) and keeps polling behind a refused send.
  Filed #782, #783.
- **[#778](https://github.com/hherb/kastellan/pull/778)** (#767, #768) — route spellings as pure fns in `localmail_contract.rs`, used by the
  worker and the live gate (which now pages from offset 2 and checks the hash routes too); one
  lowercase rule for a planner hash (`ShaPrefix`); `NormalizedFilters`; private `Credentials`.
  Second review round: the gate picks an attachment with text, the profile refusal has a test.
  Filed #779, #780.
- **[#776](https://github.com/hherb/kastellan/pull/776)** (#773, #774) — `KASTELLAN_LLM_THINKING_SWITCH`
  (`reasoning_effort` for Ollama) + a once-per-process thinking-leak WARN; `llm_usage` on every
  `plan.formulate` row; a request timeout after gathering spends the synthesis turn, a timed-out
  synthesis retries once without thinking, a final timeout is worded for the user.
- **[#775](https://github.com/hherb/kastellan/pull/775)** — handover only: the DGX timeout raise and the #773 diagnosis.
- **[#770](https://github.com/hherb/kastellan/pull/770)** (#673, #674) — `codes::UPSTREAM_AUTH_FAILED` + pure
  `upstream_auth_refusal` replace `POLICY_DENIED` for localmail 401/403 in `mail` and `email-in`;
  the dispatcher and the email channel's polled driver log an operator ERROR; docs say API key, not
  login token. Filed #769.
- **[#766](https://github.com/hherb/kastellan/pull/766)** (#763, #765) — live shape gate: own suite, knob, `mail-live` profile,
  shared `localmail_contract.rs`; mail params parsed (pure `handler/request.rs`) before the gate.
- **#764, #762, #761, #758, #748, #750, #745, #743, #740, #735, #731, #728, #726, #720, #727, #717, #709, #708, #702, #694,
  #692, #688, #685** and earlier — see git history and the [`archive/`](archive/) snapshots.

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
