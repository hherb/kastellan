# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20260910_688_pre-prune.md`](archive/handover_20260910_688_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here.

**Last updated:** 2026-09-11 ·
**Recent PRs, newest first:** [#694](https://github.com/hherb/kastellan/pull/694) (#617, the
bounded request summary), [#692](https://github.com/hherb/kastellan/pull/692) (#690 + #689 + #686,
the micro-VM preflight budgets), [#688](https://github.com/hherb/kastellan/pull/688) (#684 + #687),
[#685](https://github.com/hherb/kastellan/pull/685) (#682),
[#683](https://github.com/hherb/kastellan/pull/683) (#679). **Open issues these filed:**
[#691](https://github.com/hherb/kastellan/issues/691) (from #692),
[#693](https://github.com/hherb/kastellan/issues/693) (from #694). ·
**DGX DEPLOYED FROM `fb560ab7`** — ⚠️ **behind `main` in PRODUCTION code, not just tests.** #692
bounded `LinuxBwrap::probe` and `linux_cgroup::cgroup_probe` and #694 changes the audit write path,
all of which the daemon links, so the long-standing "everything since is tests + scripts + docs, the
running daemon is unaffected" no longer holds. **Redeploy with `scripts/upgrade_from_git.sh`, which
deploys from `main`.** ⚠️ **Its eight rootfs images were rebuilt 2026-09-08** and bake the
`--workspace` init (`8a21877a…`).

> **Header convention — CHANGED 2026-09-11, after the third recurrence. Read this before editing
> the block above.** This header names **PRs and issues only. Never a branch name, never a HEAD
> sha, never the word OPEN.** Those are claims about VCS state that a merge falsifies with no
> actor in between, and this file has now shipped self-contradicting **three times** on exactly
> that class: the 2026-09-07 clean auto-merge that kept both branches' headers; #688, which
> described its own already-merged branch as open for a day with the sweep it declared owed never
> run; and #692, the same again. A PR number cannot be falsified by a merge — GitHub owns the
> merged-vs-open distinction, so let it. **The tip and the open set are questions for the tools
> that own them, and both are one command:** `git log --oneline -1 origin/main` and
> `gh pr list --state open`. Run them at the START of a session, before trusting a word of this
> file. What a *session* is doing belongs under [Current state](#current-state), not up here.

> ⚠️ **An issue's own census can be wrong, and so can the rule it proposes — and so can YOUR
> re-derivation.** #679 named 7 call sites, the property covered 11, review found a 12th. #667
> asked for mtimes and mtimes were measurably wrong. #684 named 2 files; there were 3, plus a 4th
> that must NOT be swept in. **#690 named 4 unbounded subprocesses; there were 10** — and the two
> it missed were the production `MacosContainer::probe_image` and the `debugfs` image read, while
> the *Linux* daemon-backed twin it never mentioned (`systemd-run`, D-Bus) is the same hazard class
> it filed the issue about. **Re-derive the property, and measure the proposed rule against the
> real host, before implementing either.** [[issue-as-filed-can-carry-a-regression]]

> ⚠️ **A FIXTURE nobody rebuilds is a gate nobody runs, and the tier can be 100 % dead for months
> with every test green.** The macOS container image on the dev Mac was 69 days old; it predated
> the 2026-09-02 security audit, whose own fail-closed Landlock rule had killed every container
> worker the moment anyone rebuilt it. Eight e2es passed throughout. **The freshness question is not
> "is this image current?" but "would this run prove anything?"**
> [[stale-fixture-turns-a-gate-into-a-formality]]

> ⚠️ **A census taken with a tool inherits that tool's blind spot, and a guard built from the same
> tool cannot see what the census missed.** #683's scanner reported a file clean that contained a
> live instance of the defect it was written for. **When a check and the survey that scoped it share
> an implementation, they share its holes: test the check against a shape you did not write**, and
> give it a positive control — `assert!(violations.is_empty())` over a loop is green whether the
> loop found nothing or never ran. [[guard-shares-the-census-blind-spot]]

> ⚠️ **A gate booked as "pure verification, not code" is not evidence until it has RUN**, and **an
> error with no content is a defect *multiplier*** — #660's two gates sat here as bookkeeping for
> two days while **`0 of 21`** Firecracker tests passed, and three independent production defects
> hid behind one identical contentless `Protocol(EarlyExit)`. Before adding a layer, ask what it
> says when it refuses.

> ⚠️ **A slow Mac cargo build is CONTENTION, not the `_dyld_start` wedge**, and `sample` alone cannot
> tell them apart — a thread that is never *scheduled* shows the same single frame. **Check `uptime`
> and `%cpu` first:** a wedge burns no CPU *and never finishes*; contention burns little and finishes.

---

## Current state

### This session: #617 — an oversized dispatch can say what it did

Branch `fix/617-req-summary`, PR [#694](https://github.com/hherb/kastellan/pull/694). Full prose in
the ROADMAP entry; what binds:

**[#617](https://github.com/hherb/kastellan/issues/617) — `req` was lost wholesale past the payload
cap**, so a `shell.exec` row past 4 KiB recorded the guard tier's *opinion of* the act and nothing
about the act. The stated premise — operators want "who did what", not the body — holds for
`web.fetch` and **fails for `shell.exec`, where the argv IS the audited act**. An over-cap payload
carrying a request now keeps `req_summary = {head, sha256, len}`: a prefix of the serialised request
capped at 512 B that still names the interpreter and its first arguments, the digest of the *whole*
request so two rows compare, and its length so `len` > the head's bytes makes an elision detectable.

- ⚠️ **Derived inside `truncate_payload`, NOT at the producer as the issue proposed — the issue's
  census was one producer and there are two.** `core::scheduler::tool_dispatch` writes `req` too and
  the issue never mentions it. **The scheduler test proves the central rule by adding nothing to that
  producer.** Same lesson as #690's 4-vs-10 census and #679's 7-vs-12.
- ⚠️ **The issue's proposed SHAPE was also wrong.** `argv0`/`argc` is `shell.exec` vocabulary and the
  chokepoint dispatches every tool — empty noise for the rest, and per-tool knowledge in the one
  place that must not have it. A byte prefix is generic *and* strictly more informative: it recovers
  argv0 **and** the head of a 40 KiB heredoc, which `argv0`/`argc` cannot.
- ⚠️ **The fingerprint is taken strictly BEFORE the summary is inserted**, or two rows for one body
  stop comparing equal — the one thing the envelope digest exists to do. A test pins that order.
- ⚠️ **`PRESERVED_KEYS` order is priority order and was moot at one member.** Live at two now:
  `guard` first and asserted to win — **behaviourally only since the review round below; the
  original assertion was vacuous** — because it is tiny, irrecoverable, and losing it was the
  measured live defect that created the allowlist.
- **Secrets: nothing new is exposed.** `req_for_audit` is the *pre-substitution* snapshot, so the
  head holds opaque `secret://` refs and never a redeemed plaintext.
- ⚠️ **No sink double can test any of this** [[audit-sink-doubles-hide-storage-transforms]] — every
  cross-crate assertion runs the **real** `truncate_payload` over the **real** producer's output.
- **Also:** `build_tool_audit_payload` lifts the chokepoint's payload construction out of a large
  async fn into a pure, previously untested function; both producers spell the key as the db crate's
  own `REQ_KEY`; one shared hex renderer for both digests. `db/src/audit.rs` was split first,
  movement-only, 1132 → 558 + 581, same 25 `#[test]` names either side.
- **Filed, deferred: [#693](https://github.com/hherb/kastellan/issues/693)** — an oversized
  **scheduler** step-failure row loses `tool`/`method`, which live *only* in its payload while the
  chokepoint's live in the `actor`/`action` **columns**. A `PRESERVED_KEYS` policy call, not a
  mechanical extension.

#### Review round on the same branch (2026-09-12) — two green tests were proving nothing

Five-agent review of #694, then the fixes, on the same branch. **Everything below was green before
and after; the point is what green was worth.** Both critical findings were established by
**mutation**, not by reading, and both mutations had previously left the whole suite passing.

- ⚠️ **The summary digest was never checked against an over-cap request.** Every digest assertion
  used a request small enough that `head == text`, so taking the digest over the **head** instead of
  the whole request passed 48/48. That is the one property the field exists for: two 40 KB generated
  scripts sharing a 512-byte prefix would have collided, and two rows that must differ would have
  compared equal. Now caught by two tests, one of them a head-sharing non-collision case.
- ⚠️ **`the_guard_record_is_admitted_before_the_req_summary` did not test the order.** Its fixture
  gave the summary a `PAYLOAD_MAX_BYTES`-sized head, which fits in **neither** slot — so the guard
  won under both orders and the test passed with the array reversed, while its docstring claimed to
  assert the order "behaviourally rather than by reading the array". Only the literal array pin ever
  caught a reorder. The fixture is now ~60 % each (fits alone, not together), with a reversed-order
  control, and a precondition asserting the contention actually exists.
- **A forged `req_summary` could reach a row.** The overwrite ran only when a summary was *derived*,
  so a payload carrying the key with **no `req`** had nothing overwrite it and `preserve_onto` copied
  its forged answer onto the envelope verbatim. The key is now cleared **unconditionally** before
  derivation. Mutation-proven. This also collapsed a nested `if let` whose two arms tested the same
  discriminant.
- **Two prose rules became compile errors.** `REQ_KEY ∉ PRESERVED_KEYS` (the module's central
  prohibition — allowlisting `req` would carry whole bodies past the cap under an allowlisted name)
  and the `HEAD_MAX_BYTES` budget relation. Both in the existing `const _: () = {}` block; the first
  **verified to fire** as `error[E0080]`.
- ⚠️ **Six doc claims were falsified by #694's own one-line change** from one preserved key to two.
  The worst (`preserve_onto`'s doc) declared the multi-key half unreachable scaffolding at the exact
  moment it became production behaviour. The safety conclusions survive, but **for a different
  reason than stated**: starvation is unreachable by *sizing*, not by cardinality.
- **Measured, not estimated:** `head` is a prefix of already-serialised JSON and is escaped **again**
  when stored as a JSON string, so 512 bytes can cost ~1026. The "order of magnitude under the cap"
  claim was ~3x. The budget-postcondition test carried **no `req` fixture at all** and now carries
  two, including a quote-dense one.
- **Filed, not carried in this branch** — all three are policy or cross-module calls:
  [#695](https://github.com/hherb/kastellan/issues/695) (an oversized tool row cannot say whether the
  dispatch succeeded, let alone why it failed: `err` is dropped unnamed, so success and failure carry
  the same key set — the other half of #617's own thesis),
  [#696](https://github.com/hherb/kastellan/issues/696) (`kastellan-db` defines `sha256_hex` twice;
  **not** the #591 duplication, and #591's workers-can't-depend-on-db excuse does not cover it),
  [#697](https://github.com/hherb/kastellan/issues/697) (truncation is never logged or counted).
- ⚠️ **Process, worth more than any single finding: review subagents mutate the working tree.** Three
  of the five planted mutants in the primary checkout — twice while a `cargo test` sweep was
  compiling in it — and **none mentioned it in its report**. The tell was the harness's "file changed
  on disk" notice. `git status` + `git diff --cached` before trusting any sweep that ran while agents
  were live; revert by copying the file, never `git checkout --`; run the gate in a `git worktree`
  the agents cannot reach. [[never-edit-tree-during-a-sweep]]

**Gate (Mac, after the fixes):** clippy `-p kastellan-db -p kastellan-core --all-targets -D warnings`
exit 0; `kastellan-db` + `kastellan-core --lib` **2339 / 0**, zero warnings, tree clean before and
after the run. Baseline before the fixes was 2335, so **+4**. The pre-fix gate was run in a
throwaway worktree at `8ecccb4a` precisely because the primary checkout was being mutated.

**The header stopped asserting VCS state** after `main` shipped this file calling an already-merged
branch OPEN for the **third** time. A branch name, a HEAD sha and the word OPEN are claims a merge
falsifies with **no actor in between**; a PR number is not. See the header block.

**#692 (`c5bf5e5f`) — the micro-VM preflight arc closed (#690 + #689 + #686).** What still binds:
**`kastellan_sandbox::bounded_command` is the shared vocabulary for any host probe** — `output_within`
→ `Bounded::{Exited, TimedOut}`, `probe_output` → `ProbeFailure::{Spawn, Wedged}`; **a non-zero exit
is `Exited`, not a timeout**. ⚠️ **A bounded runner must NOT join its drain threads** — a grandchild
inheriting the pipe blocks the read forever, measured at 17 minutes
[[bounded-subprocess-must-not-join-drains]]; `Command::output()` has the same bug. ⚠️ **Draining at
all is load-bearing:** without it a 512 KiB writer times out at the full budget. **Exempt, with the
reason at the site:** `mkfs.ext4` on the spawn path and `cargo metadata` (it takes cargo's own
package lock). `microvm::subprocess_guard` keeps it fixed — the defect has **no runtime signature**,
so the only place it is visible is the source. **#689:** the macOS container guest kernel does not
enforce Landlock either (`/sys/kernel/security` empty on `container` 1.1.0), so **both** micro-VM
tiers are seccomp-only by design and `macos_container_smoke` fails the day that changes; it
**refutes, it does not certify**. **#686:** all eight rootfs scripts carry one byte-identical
cwd-independent prologue. ⚠️ **`cd ""` exits 0 on bash 3.2.57 and 5.2.21 but 1 on 5.3.15** — a
premise this repo documented, false on one of its own hosts.

### Merged arcs — only what still binds

Full prose in the [`archive/`](archive/) snapshots, one line each in the ROADMAP, the 2026-09-02
audit in [`docs/security-audit-2026-09-02.md`](../../security-audit-2026-09-02.md). Most of what
follows is also a memory note, auto-loaded; kept here where it changes a *first* move.

**#688 (`09a4f924`) — the macOS Apple-`container` tier got the REQUIRE knob and a freshness gate
(#684 + #687).** The gate's first act was to prove the tier had been **dead for 69 days** (⚠️ above).
The production defect it was hiding is **#669's, on the other guest kernel** — the audit made an
unenforceable Landlock ruleset fail closed and the container backend never got the sanctioned
`KASTELLAN_LANDLOCK_PROFILE=none` opt-out, now injected as *a default that never overrides a caller*
(`warn_lockdown_overrides` inspects the derived policy *before* a backend runs, so it is structurally
blind to a backend-side injection). **Seccomp is unaffected and that is measured**, which is the
whole justification for disabling the other layer. ⚠️ **#667's digest rule cannot be carried across**
— `build-image.sh` *cross-builds* for linux/arm64 while the host copy is a macOS Mach-O — so the
reference is the image's build time against its **source** mtimes, and #667's mtime rejection does
not transfer (that was about *build outputs* cargo relinks; nothing relinks a `.rs`). The fresh arm
is `NewerThanSources`, **never** `Fresh`. ⚠️ **Only an image THIS REPO BUILDS has a source closure**
— hence `BUILT_IMAGES`, with the lookup in the *pure* orchestrator. ⚠️ **"Require a second
condition" is not automatically a tightening** — the first fix for the source guard's discovery rule
was **fail-open and strictly narrower** than what it replaced, because the literal it required is
absent from the brace-grouped `use` form.

**#685 / #683 / #680 — the rootfs-freshness trio.** ⚠️ **Cargo unifies features PER INVOCATION, so
package selection changes the bytes of an otherwise identical binary**
[[cargo-package-selection-changes-binary-bytes]] — hence **one producer for `target/release/`,
`scripts/build-release.sh`, run LAST** (its own second invocation is reversible: a later bare
`--workspace` re-uplifts the non-featured Matrix worker in 0.32 s with no output).
`KASTELLAN_MICROVM_REQUIRE_E2E=1` turns **every** unmet micro-VM precondition into a panic —
⚠️ `||` short-circuits, so the load-bearing test is a **source scanner**; no unit test and no
Firecracker run can see that false green. ⚠️ **#667 asked for mtimes and mtimes were WRONG** —
cargo relinks unchanged output [[cargo-relinks-identical-mtime-not-content]], so the reference is
the **sha256 of the baked copy**, read with `debugfs` (no mount, no root). ⚠️ **A verdict that
certifies on PARTIAL evidence is the original bug with better manners**, and **every `debugfs`
failure exits 0**, so benign causes are separated structurally, never by wording.

**#660 (`62d98a00`) — the second pre-release security audit.** 29 fixes, 80 files. What still binds:
the dispatch chokepoint scrubs every redeemed secret out of **both** result arms; agent-raised
`l1_insight`s are screened at promotion *and* prompt assembly; every per-spawn `/tmp` dir is minted
with `create_private_dir` — **a pre-planted name from another uid FAILS THE SPAWN CLOSED; do not
"fix" it back to `create_dir_all`**; seccomp admits `clone` only without `CLONE_NEW*`.
⚠️ **Three lockdown behaviours are FAIL-CLOSED and will bite a careless fixture:** a missing
`KASTELLAN_SECCOMP_PROFILE` is an error (`none` is the explicit opt-out), an unenforceable Landlock
ruleset is an error — **which killed the macOS container tier for 69 days** — and a corrupt
`kastellan.env=` guest token refuses the boot. **Every networked stdio worker builds its handler
INSIDE `serve_stdio_with`** (Landlock is per-thread). **Run the live-matrix clippy job before
pushing anything touching `sdk_live.rs`.** CodeQL reads NAMES [[codeql-flags-sanitisers-by-name]].
**Deferred with a reason** (all in the audit doc): brokers not force-routed; the guard tier never
sees bytes past 64 KiB; `secret://` refs not tool-bound; `Host:` ≠ CONNECT authority; no
email-replay freshness window; macOS worker-side caps. **Before release: flip force-routing on.**

**#681, #675, #669, #650/#653/#649 — the one-liners that still bind.** #681: a lean tail plus one
recovery round-trip scored **68.3 % recall on 49 K tokens** against **45.8 % on 162 K** — **a big
verbatim tail is not the safe choice; it is the expensive one that also loses the needles.**
#675: **a failed micro-VM boot leaves `console.log` in the kept run dir — read it before theorising**
[[microvm-guest-failures-are-invisible]]; `bwrap --clearenv` means the launcher has **no** environment
[[microvm-launcher-knobs-must-be-argv]]; the release profile is `panic = "abort"`, so RAII cleanup
never runs in shipped binaries [[release-profile-panic-abort-kills-raii]]. #669: **count the
producers, and make the const the only spelling** — `build_vmm_jail_argv` was the *third* bwrap argv
producer and #661's fix missed it; the pinned guest kernel has no Landlock
[[firecracker-guest-kernel-no-landlock]] (repin is #668); **`/run` is out of the chown set** and
re-adding it would be a regression (chowning a *sticky* dir lets the owner unlink others' entries);
⚠️ a non-hex `kastellan.mounts=` fixture fails OPEN, silently [[fail-safe-parsers-make-vacuous-fixtures]].
#650: **a containment fix must not widen containment**; ⚠️ `Path::components()` strips **interior**
`.` only [[rust-path-components-normalizes-dot]]; open: #657, #658, #659. #653/#654: **the reusable
pattern is the `*_or_reason` sibling** — return the reason **without rendering a verdict**; open:
#664, #665. #649/#651: **the remedy an advisory states can be a no-op that exits 0**
[[uv-lock-upgrade-can-land-still-vulnerable]].

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

Most are also memory notes (auto-loaded); kept here because they change the *first* move.

> ⚠️ **Clippy parity is a `rustup update`, not a property of the hosts.** CI pins nothing
> (`dtolnay/rust-toolchain@stable`) and both dev hosts float. **2026-09-10: both hosts on 1.98.0.**
> [[local-clippy-not-ci-parity-rust-version]]

> ⚠️ **A cached `cargo clippy` reports a full-workspace pass it never ran.** Exit code alone does not
> distinguish it — **count the `Checking` lines**. A warm dir can report exit 0 having linted 4.
> Force a real one with `find . -path ./target -prune -o -name lib.rs -print -o -name main.rs -print
> | xargs touch`; that re-lints all **27** workspace crates. And `cargo check`/`clippy --all-targets`
> do **not** warm the target dir for `cargo test` — **run the sweep first, lint after.**

> ⚠️ **Force a cold clippy with a dedicated `CARGO_TARGET_DIR`, NOT by touching sources.**
> `CARGO_TARGET_DIR=$HOME/.cargo-clippy-<topic> cargo clippy --workspace --all-targets --locked --
> -D warnings` proves the same thing (count the `Checking` lines — it linted all 27) and **mutates
> no file**, so it cannot falsify the #687 image gate. The old `find … -name main.rs | xargs touch`
> recipe moves `workers/python-exec/src/main.rs`, which *is* in that gate's source closure, so every
> container e2e then reports the image stale and **panics under REQUIRE for a file nobody edited** —
> a false **refusal**, the direction that gets a gate switched off
> ([#691](https://github.com/hherb/kastellan/issues/691); both recipes measured on both hosts in the
> issue). The trade is a slower first run: the fresh dir rebuilds dependencies the touch leaves warm
> (28 m on the Mac), paid once per topic dir.

> ⚠️ **A private `CARGO_TARGET_DIR` does not build `examples/`,** so `email_channel_e2e`'s 6 tests
> fail with `fixture not built` at a perfectly green commit
> [[custom-cargo-target-dir-breaks-daemon-e2e]]. Read the failure text before believing a regression.

> ⚠️ **rust-analyzer holds `target/debug/.cargo-lock` and will block a sweep indefinitely.**
> Its `cargo` is a child of the `rust-analyzer` server process, so
> `ps -eo pid,ppid,command | grep cargo` names it; **a blocked sweep has ZERO rustc children while
> the IDE's has sixteen**, which settles who holds the lock in one command. Killing that child frees
> it (the IDE re-runs later). Hit twice this session. For iterating on one crate, a private
> `CARGO_TARGET_DIR` sidesteps the fight entirely — but only for unit tests, see the `examples/`
> hazard above.

> ⚠️ **Do NOT edit the working tree while a sweep is compiling in it.** A sweep is a gate on one
> revision; an edit mid-compile makes it a gate on nothing. **Work on a branch in a `git worktree`**
> (`git worktree add …`) and leave the primary checkout clean for the gate. ⚠️ In a worktree session
> the Bash tool's cwd resets to the primary checkout, so use `git -C <worktree>` and absolute paths
> [[worktree-cwd-lands-on-main]].

> ⚠️ **"Filed, not fixed: #N" in a PR body or commit message CLOSES #N.** GitHub matches the
> `fixed: #N` substring and has no notion of negation; it has cost three issues. Write **"deferred to
> #N"**, and before merging run
> `gh pr view <n> --json body --jq .body | grep -oiE '(close[sd]?|fix(e[sd])?|resolve[sd]?)[[:space:]:,]+#[0-9]+'`
> over the body *and* the commit message. [[pr-body-not-fixed-autocloses-issue]]

> ⚠️ **Squash-merge caveat:** every PR lands as one squash commit, so its *branch-tip* SHA (where the
> gate ran) is **not** an ancestor of `main`. Check content, not `merge-base`.

> ⚠️ **`syspolicyd` saturates and then NO newly-built binary can start on the Mac** — the cause of
> the `_dyld_start` wedge this file has recorded only as a symptom. It gatekeeps `exec` of every
> newly written executable. **Measured 2026-09-11 at 20 days uptime: pid 699 at 69–95 % CPU with
> 110 hours accumulated; six consecutive suites wedged and a freshly compiled 20-byte C program
> hung too.** ⚠️ **The tell is CPU TIME, not `%cpu`:** `ps -o pid,etime,time` showing **`0:00.00`
> against a multi-minute ELAPSED** is conclusive, because contention always accumulates *some*
> CPU — which is what the 2026-09-02 "the `sample` signature is ambiguous" correction lacked.
> **Positive control, seconds:** `printf 'int main(){return 7;}' > /tmp/t.c && cc -o /tmp/t /tmp/t.c
> && /tmp/t`. If *that* hangs, nothing about this repo can explain it. **Fix: ask the operator to
> run `sudo killall syspolicyd`** — launchd respawns it and exec recovers immediately (verified; a
> binary hung 52 minutes then ran instantly). Claude Code cannot: sudo has no tty.
> ⚠️ **It re-saturates for a while afterwards** rebuilding its assessment cache, so a few suites
> still wedge — a watchdog killing any `target/debug/deps/*` at `0:00.00` CPU past **15 minutes**
> keeps the sweep moving and names each victim; re-run those suites individually.
> [[mac-fresh-large-binaries-hang-in-dyld]]

> ⚠️ **`kastellan-worker-egress-proxy` leaks on the Mac** (three orphans still alive after 17 days,
> across two target dirs — not investigated), and **a `pgrep -f '<cmd>'` wait loop matches itself**
> and never exits: use `pgrep -x`. [[pgrep-wait-loops-match-themselves]]

## Read these first

1. [`docs/architecture.md`](../../architecture.md) — process model, cross-platform table
2. [`docs/threat-model.md`](../../threat-model.md) — the invariant, scenarios, defence layers
3. [`docs/devel/ROADMAP.md`](../ROADMAP.md) — the master sequenced TODO with commit hashes
4. Memory notes (auto-loaded) — `~/.claude/projects/-Users-hherb-src-kastellan/memory/MEMORY.md`
5. [`archive/`](archive/) — the full prose for everything this file summarises

---

## Next TODO

> Only *open* work is listed. Shipped items move to [Recently merged](#recently-merged) or the ROADMAP.

1. **[#677](https://github.com/hherb/kastellan/issues/677) — the live DM round-trip worked and the
   answers were wrong, and #617 has now cleared the way to find out why.** #660's last owed gate is
   discharged (2026-09-05, DGX at `9ace57ad`): two DMs from `@horst` were received, planned and
   answered (tasks 185/186, both `channel.replied`). **But** task 186 spent three of six plan
   iterations on near-duplicate searches and a fourth on `shell.exec /usr/bin/ls`, then blamed "the
   tool-step limit" for not reading the PDF, having never called `mail.get_attachment_text`, which
   task 185 had used successfully **four minutes earlier**. The two tasks reported **different
   booking references** for the same question with equal confidence, and **which answer was grounded
   could not be established**, because both large dispatches were audited `_truncated: true` with
   `req` dropped wholesale. ⚠️ **That blocker is fixed but the EVIDENCE IS NOT RETROSPECTIVE** — the
   `req_summary` is computed at write time, so the existing rows for tasks 185/186 are as empty as
   they ever were. **Re-run the scenario on a deployed daemon carrying #694 and read the new rows;
   do not go back to the old ones.** Needs the DGX redeploy first.

**On the micro-VM path — one issue left, and it needs a kernel build.**
[#668](https://github.com/hherb/kastellan/issues/668) — repin a guest kernel built with
`CONFIG_SECURITY_LANDLOCK`, the standing posture item. ⚠️ **Its macOS twin now has a detector rather
than an issue** (#689, this session): the Apple `container` guest kernel does not enforce Landlock
either (re-measured 2026-09-10 — `/sys/kernel/security` exists and is empty), so *both* tiers run
seccomp-only by design, and `macos_container_smoke` fails the day that changes. If #668 is ever done,
the container backend's injection has to be revisited in the same breath — it is deliberately a
default a caller can already override.

**A standing architecture item, and the frame for several open issues:**
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

## Working state

### Test baseline (authoritative)

| Host | Commit | Result | clippy `-D warnings` | `[SKIP]` |
| --- | --- | --- | --- | --- |
| **Mac + DGX** ([#694](https://github.com/hherb/kastellan/pull/694), #617 — **the gate that stands**) | **`65899e9c`** | **Mac: 4185 / 0 / 29**, **177** suites, **0 `[WARN]`**, 296 `[SKIP]`. **DGX: 4320 / 0 / 61**, 177 suites, `TEST_EXIT=0`, **4 `[SKIP]`** (gliner tier, held), **0 `[WARN]`**. ⚠️ **Both deltas are +30 and reconcile exactly** — 23 in `kastellan-db` (15 `req_summary`, 8 truncation integration) and 7 in `kastellan-core` (6 `post_process`, 1 scheduler). DGX 4290 → 4320; Mac 4155 → 4185. **The host gap stays at −135**, unchanged, which is the one macOS-only smoke test and nothing else. Ignored unchanged on both. ⚠️ **The Mac run needed a host repair first and its `TEST_EXIT` is not 0** — see the `syspolicyd` hazard below. The sweep reported **173 of 177** suites at `TEST_EXIT=101` with **zero failed tests**; the other four (`kastellan_cli` 96, `search_broker_egress_e2e` 1, `asks_e2e` 35, `pairings_e2e` 2 = **134**) were SIGKILLed as wedged and **all four pass on an individual re-run**, which is where 4051 + 134 = 4185 comes from. **A non-zero `TEST_EXIT` with zero failed tests is the signature of that host fault, not of a regression** | **Mac** exit **0**, zero warnings, all **27** workspace crates. **DGX** the same, exit **0**, 27 crates, 345 units. ⚠️ **Both forced cold with a dedicated `CARGO_TARGET_DIR` rather than `xargs touch`** — same proof, and it mutates no file, so it cannot falsify the [#687](https://github.com/hherb/kastellan/issues/687) image gate the way this file's old recipe does ([#691](https://github.com/hherb/kastellan/issues/691), measurement posted there) | **296** Mac, **4** DGX. **0** `[WARN]` |
| **Mac + DGX** ([#692](https://github.com/hherb/kastellan/pull/692), #690/#689/#686 — **the gate that stands**) | **`499c9488`** (branch tip; squashed to `c5bf5e5f`) | **Mac full sweep:** `cargo test --workspace --no-fail-fast --locked -- --nocapture` **4155 / 0 / 29**, **177** suites, `TEST_EXIT=0`, **0 `[WARN]`**, 339 `[SKIP]` (328 of them the pre-existing absent-Postgres ones). **DGX full sweep: 4290 / 0 / 61**, 177 suites, `TEST_EXIT=0`, **4 `[SKIP]`** (gliner tier, held), **0 `[WARN]`**. ⚠️ **Both deltas reconcile exactly, and against different baselines.** DGX: **+40** over `main`'s 4250 — every new test is cross-platform (`bounded_command` 18, `subprocess_guard` 12, `container_tests` 4, `images` 3, `landlock_lsm` 3). Mac: 4104 at `0fa5b8b6` **+10** (#688's review round, which post-dated that gate) **+41** (the 40 above plus the one macOS-only smoke test) = **4155**. The host gap moved from **−136** to **−135**, which is that one macOS-only test and nothing else. Ignored unchanged on both. **Container tier under `KASTELLAN_MICROVM_REQUIRE_E2E=1`: 10 / 0** across all three suites (4 + 1 + 5), every suite exit 0, **0 `[WARN]`**, 4 opt-in gliner `[SKIP]`s. ⚠️ **That tier needed the image rebuilt twice, and the second rebuild is a finding, not a chore** — see [#691](https://github.com/hherb/kastellan/issues/691) below. **Live negative controls, all run:** the budget removed → the suite takes **30.01 s instead of 0.50 s** and both timeout tests fail; the drains removed → a 512 KiB writer **times out at the full 60 s budget** and 6 tests fail; the guard's rule planted with a violation → found and named; the #689 detector fed `capability,landlock,bpf` from a real container → red with the full operator message | **Mac** `--workspace --all-targets --locked -D warnings` exit **0**, zero warnings, all **27** workspace crates from a forced-cold `touch`. **DGX** the same, exit **0**, 27 crates. `kastellan-sandbox` also cross-clippied for `aarch64-unknown-linux-gnu` from the Mac — **which caught a Linux-only unused import the Mac run compiles out** | **339** Mac (328 absent-Postgres), **4** DGX. **0** `[WARN]` |
| **DGX** (`main`, the sweep #688 owed and never ran) | **`09a4f924`** | Superseded by the row above; kept because it is the only direct measurement of `main`. **4250 / 0 / 61**, 177 suites, `TEST_EXIT=0` | exit 0, 27 crates | **4** |
Older rows (#688 `0fa5b8b6`, #685 `10cb6761`, #683 `4189`, #680 `4142`, and back to 2950) are in the
[`archive/`](archive/) snapshots.

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

Newest first; substance is compressed under [Current state](#current-state), full prose in the
[`archive/`](archive/) snapshots and git history.

- **[#694](https://github.com/hherb/kastellan/pull/694)** — an oversized dispatch still records
  what ran (#617): a bounded `req_summary` derived inside `truncate_payload`, so every write site
  past and future is covered by one rule. Filed [#693](https://github.com/hherb/kastellan/issues/693).
- **[#692](https://github.com/hherb/kastellan/pull/692)** `c5bf5e5f` — every micro-VM
  preflight subprocess answers to a budget (#690), the macOS Landlock opt-out gets a drift detector
  (#689), and all eight rootfs build scripts become cwd-independent (#686). Two-host gate green,
  both deltas reconciled. Filed [#691](https://github.com/hherb/kastellan/issues/691).
- **[#688](https://github.com/hherb/kastellan/pull/688)** `09a4f924` — the macOS Apple-`container`
  tier gets the REQUIRE knob and a freshness gate (#684, #687), and the Landlock defect that gate
  immediately found. Filed #689 and #690.
- **[#685](https://github.com/hherb/kastellan/pull/685)** `0939e80c` — `target/release/` gets one
  producer, so the #667 gate stops crying wolf (#682). Filed #686 and #687.
- **[#683](https://github.com/hherb/kastellan/pull/683)** `ec9a2e94` — every micro-VM precondition
  answers to `KASTELLAN_MICROVM_REQUIRE_E2E` (#679), enforced by a source guard over `core/tests`.
  Filed #682 and #684.
- **[#680](https://github.com/hherb/kastellan/pull/680)** `fb560ab7` — a stale micro-VM rootfs image
  can no longer gate anything (#667).
- **[#681](https://github.com/hherb/kastellan/pull/681)** `aee2a7f0` — the Hermes Agent survey.
- **[#675](https://github.com/hherb/kastellan/pull/675)** `f831b3d1` — the micro-VM diagnostics
  cluster (#666, #670, #671, #672).
- **[#669](https://github.com/hherb/kastellan/pull/669)** `4955a52c` — the Firecracker gate #660
  owed, plus the three defects it found. 0/21 → 21/0.
- **[#663](https://github.com/hherb/kastellan/pull/663)** `9ace57ad` — the gliner-relex require knob
  (#653) and the one flag dialect (#654).
- **[#656](https://github.com/hherb/kastellan/pull/656)** `c03ec1a3` — the interpreter alias bind
  (#650), plus #661 and #662.
- **[#660](https://github.com/hherb/kastellan/pull/660)** `62d98a00` — the second pre-release
  security audit: 29 fixes across containment, secrets, prompt and egress.

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
