# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20260910_688_pre-prune.md`](archive/handover_20260910_688_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here.

**Last updated:** 2026-09-11 ·
**Recent PRs, newest first:** [#692](https://github.com/hherb/kastellan/pull/692) (#690 + #689 +
#686, the micro-VM preflight budgets), [#688](https://github.com/hherb/kastellan/pull/688) (#684 +
#687), [#685](https://github.com/hherb/kastellan/pull/685) (#682),
[#683](https://github.com/hherb/kastellan/pull/683) (#679),
[#680](https://github.com/hherb/kastellan/pull/680) (#667). #692 filed
[#691](https://github.com/hherb/kastellan/issues/691), still open. ·
**DGX DEPLOYED FROM `fb560ab7`** — ⚠️ **and it is now behind `main` in PRODUCTION code, not just
tests.** #692 bounded `LinuxBwrap::probe` and `linux_cgroup::cgroup_probe`, both of which the
daemon links, so the header's previous "everything since is tests + scripts + docs, the running
daemon is unaffected" no longer holds. ⚠️ **Its eight rootfs images were rebuilt 2026-09-08** and
bake the `--workspace` init (`8a21877a…`).

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

### Most recently merged: #690 + #689 + #686 — the micro-VM arc closes out ([#692](https://github.com/hherb/kastellan/pull/692))

Branch `fix/690-689-686-microvm-preflight-timeouts`. The three deferrals #688 filed, taken together
because they are one theme: **the micro-VM preconditions could fail in ways that said nothing.**

**[#690](https://github.com/hherb/kastellan/issues/690) — no preflight subprocess had a timeout.**
Every host probe used `Command::output()`, which waits forever. Apple `container` is daemon-backed
(the CLI talks XPC to `container-apiserver`), so a wedged apiserver made `container system status`
*block* rather than fail, and `cargo test --workspace` then stalled with **no `[SKIP]`, no `[WARN]`,
no panic and no message of any kind** — the most opaque form of exactly the failure the module
exists to prevent.

- **New shared vocabulary: `kastellan_sandbox::bounded_command`.** Pure `std`, no new dependency,
  deliberately **not** `cfg`-gated (same reasoning as `guest_kernel_pin`: both tiers need it and
  each runs on a different host). `output_within` returns `Bounded::{Exited, TimedOut}`;
  `probe_output` narrows that to `ProbeFailure::{Spawn, Wedged}`; `timed_out_reason` is the pure
  renderer. **A non-zero exit is `Exited`, not a timeout** — "no" is an answer, and folding the two
  would be #684's defect in a new place.
- ⚠️ **Draining both pipes on their own threads is load-bearing, and that is measured.** A child
  writing more than one pipe buffer *blocks on the write* until somebody reads, so a poller that
  does not drain reports a timeout for a process waiting on **us**. Live negative control: with the
  drains removed, a 512 KiB writer **timed out at the full 60 s budget** and 6 tests went red.
  Second control: with the budget removed, the suite took **30.01 s instead of 0.50 s** and the two
  timeout tests failed.
- ⚠️ **The issue's census was 4 sites; the real one is 10.** Bounded: the three Apple `container`
  calls (`--version`, `system status`, `image inspect`), `MacosContainer::probe_image` (**missed by
  the issue, and production**), `LinuxBwrap::probe`, **`linux_cgroup::cgroup_probe` — `systemd-run`
  is D-Bus-backed, the Linux twin of the wedged apiserver and never mentioned in the issue** —
  `MacosSeatbelt::probe`, the `debugfs` image read (**also missed**), and three test shell-outs.
  **Exempt, with the reason at the site:** `mkfs.ext4` on the *spawn* path (a multi-gigabyte image
  legitimately takes minutes) and `cargo metadata` (it takes cargo's own package lock, which a
  concurrent build may hold for minutes — a budget there converts a benign wait into a flake).
- **`PROBE_BUDGET` is 10 s and was measured, not guessed:** on the dev Mac under load average 27 the
  three `container` calls answer in 10–240 ms, and `debugfs` reads the 398 032-byte guest init out
  of the **1.3 GB** browser-driver image in ≤ 0.01 s. It is not a latency budget; it converts
  *forever* into a sentence.
- **`InspectFault` became `CliFault` and grew a third variant.** A wedge's remedy is `stop` **then**
  `start`, so `cli_unavailable_reason`'s "start it with `container system start`" is the *wrong*
  advice appended after the right one — the #684 folding defect in its politest form. One type now
  serves the probe arm and the inspect arm, and `cli_fault_reason` is the single place a fault
  becomes prose, so a fourth variant cannot be added with one arm quietly rendering it as something
  else. `MacosContainer::probe_fault()` is the `*_or_reason` sibling one step further: it returns
  the fault **classified**, because a flattened `SandboxError` could only be re-classified by
  matching on prose, which this module forbids everywhere else.
- **New drift guard: `microvm::subprocess_guard`.** The defect has **no runtime signature** — a
  process that never returns reaches no assertion — so the only place it is visible is the source.
  Fail-closed on a *shape*, with a `BOUNDED-EXEMPT: <reason>` marker each exception justifies
  itself with, a positive control on the file count, and a planted-violation control. ⚠️ **It
  cannot scan its own definition** (which names the shape it forbids, in a const and in fixtures);
  that exclusion is by exact file and **the count is asserted**, so a third file cannot join it.
  ⚠️ It deliberately does **not** reuse `guard.rs`'s string-aware stripper: that stripper models
  neither raw strings nor `'"'` char literals, both of which occur throughout the production
  sources scanned here, so it would blank whole files and report them clean.

**[#689](https://github.com/hherb/kastellan/issues/689) — the macOS Landlock opt-out now has a
drift detector.** Option (2) from the issue: `macos_container_smoke` boots a real container and
reads `/sys/kernel/security/lsm`, failing when Landlock appears. ⚠️ **The asymmetry it fixes ran the
wrong way** — the Firecracker guest kernel is a sha256 pin *this repo controls* whose bump fails a
test, while Apple's moves on a routine `brew upgrade container`, outside anybody's decision.
⚠️ **The detector refutes; it does not certify**, and the naming says so (`landlock_in_lsm_list`,
not `landlock_available`): an *enabled* LSM is not proof that `landlock_create_ruleset` accepts the
prelude's ABI, and the prelude's probe is what killed this tier before — so a failure means "go
re-measure with a real worker", never "the opt-out is wrong". Same rule as #687's `NewerThanSources`.
**Re-measured 2026-09-10 on `container` 1.1.0:** `/sys/kernel/security` exists and is **empty**.
Live negative control run: with the guest made to report `capability,landlock,bpf`, the test goes
red with the full operator message.

**[#686](https://github.com/hherb/kastellan/issues/686) — the eight rootfs build scripts are
cwd-independent.** Seven used bare `target/release/…` and died at `install: cannot stat` from any
other directory; the eighth had solved it a third way. All eight now carry **one byte-identical
prologue**, asserted against a single const (#669's "count the producers"), and it sits **after**
the `source` of `lib/guest-kernel.sh` because that line resolves its own relative path and must not
run post-`cd`.

- ⚠️ **A documented premise turned out to be false on one of the two dev hosts, and the test found
  it.** `rebuild-all-rootfs.sh` and the issue both state that `cd ""` succeeds in bash. **Measured
  2026-09-10:** bash **3.2.57** (macOS `/bin/bash`) exit **0**, bash **5.2.21** (DGX) exit **0**,
  bash **5.3.15** (Homebrew, dev Mac) exit **1**, `cd: null directory`. So the premise holds on two
  of three and the `[ -z "$REPO_ROOT" ]` guard is load-bearing — but a test asserting *bash's*
  behaviour passes on one host and fails on the other. The test therefore asserts the **guard's**
  behaviour, driving the real prologue text down its failure arm by shadowing `dirname` (`cd` and
  `pwd` are builtins, so `dirname` is the only external it depends on).
- **First behavioural coverage these eight files have ever had.** Every previous check reads what
  they *say*; a prologue that parses, contains the right words and lands in the wrong directory
  would have satisfied all of them.

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

**#685 (`0939e80c`) — `target/release/` has one producer (#682).** ⚠️ **Cargo unifies features PER
INVOCATION, so package selection changes the bytes of an otherwise identical binary**
[[cargo-package-selection-changes-binary-bytes]] — so after any deploy the #667 gate declared all
eight *correct* images stale. The ambiguity is removed **at the producer**: all eight rootfs scripts
call `scripts/build-release.sh`. ⚠️ **That is ONE SCRIPT WITH TWO INVOCATIONS and the second is
reversible** — a later bare `cargo build --release --workspace` re-uplifts the non-featured Matrix
worker in **0.32 s with no output but `Finished`**, so **run `build-release.sh` LAST**. ⚠️ Its
review's reusable lesson: both existing script scanners read **comments as code**, and the first fix
claimed its error direction *"leaves more text to scan, never less"* — false, exactly where it
mattered.

**#683 (`ec9a2e94`) — every micro-VM precondition answers to the REQUIRE knob (#679).** `||`
short-circuits, so on exactly the host the operator cares about control reached
`skip_if_no_supervisor()`. ⚠️ **The load-bearing test is a source scanner**, because the false green
appears only on a host where the micro-VM preconditions are MET and a neighbouring one is not —
which no unit test and no Firecracker run can be. ⚠️ **The Mac compiles none of that code**
[[mac-compiles-zero-systemd-tests]].

**#680 (`fb560ab7`) — a stale rootfs image can no longer gate anything (#667).** ⚠️ **The issue asked
for mtimes and mtimes were WRONG here** — six *correct* DGX images read 5 h "older" than an init
they contained byte-identical copies of, because cargo relinks unchanged output
[[cargo-relinks-identical-mtime-not-content]]. **The reference is the sha256 of the baked copy**,
read with `debugfs -R "cat …"` — no mount, no loop device, no root. **Four verdicts, four
treatments:** `Stale`/`Unusable` panic; `Fresh`-with-caveats and `Indeterminate` `[WARN]` and still
run. ⚠️ **A verdict that certifies on PARTIAL evidence is the original bug with better manners.**
⚠️ **Every `debugfs` failure exits 0**, so benign causes are separated *structurally*
(`ErrorKind::NotFound`), never by matching on wording.

**#681 (`aee2a7f0`) — the Hermes Agent survey.** The number worth remembering: a lean tail plus one
recovery round-trip scored **68.3 % recall on 49 K tokens** against **45.8 % on 162 K** for the fat
verbatim tail — **a big verbatim tail is not the safe choice; it is the expensive one that also
loses the needles.** ⚠️ Their `execute_code` opens an RPC socket from agent-authored Python back into
the tool dispatcher; for us that turns one compromised worker into every worker.

**#675 (`f831b3d1`) — the micro-VM path can say why it failed.** **A failed boot leaves
`console.log` in the kept run dir — read that before theorising**
[[microvm-guest-failures-are-invisible]]. ⚠️ **`bwrap --clearenv` means the launcher has NO
environment** [[microvm-launcher-knobs-must-be-argv]]; **the release profile is `panic = "abort"`**,
so RAII cleanup never runs in shipped binaries [[release-profile-panic-abort-kills-raii]].

**#669 (`4955a52c`) — the Firecracker gate.** The backend had been **entirely dead at 0 of 21** since
the audit merged. **Count the producers, and make the const the only spelling** —
`build_vmm_jail_argv` was the **third** bwrap argv producer and #661's fix missed it. **The pinned
guest kernel has no Landlock** [[firecracker-guest-kernel-no-landlock]]; repinning is #668. ⚠️ **A
non-hex `kastellan.mounts=` fixture fails OPEN, silently**
[[fail-safe-parsers-make-vacuous-fixtures]]. **`/run` is out of the chown set** and re-adding it
would be a regression — chowning a *sticky* directory lets the owner unlink entries it does not own.

**#660 (`62d98a00`) — the second pre-release security audit.** 29 fixes, 80 files. What still binds:
the dispatch chokepoint scrubs every redeemed secret out of **both** result arms; agent-raised
`l1_insight`s are screened at promotion *and* prompt assembly; every per-spawn `/tmp` dir is minted
with `create_private_dir` — **a pre-planted name from another uid FAILS THE SPAWN CLOSED; do not
"fix" it back to `create_dir_all`**; seccomp admits `clone` only without `CLONE_NEW*`. ⚠️ **Three
lockdown behaviours are FAIL-CLOSED and will bite a careless fixture:** a missing
`KASTELLAN_SECCOMP_PROFILE` is an error (`none` is the explicit opt-out), an unenforceable Landlock
ruleset is an error — **which is what killed the macOS container tier for 69 days** — and a corrupt
`kastellan.env=` guest token refuses the boot. **Every networked stdio worker builds its handler
INSIDE `serve_stdio_with`** (Landlock is per-thread). **Run the live-matrix clippy job before pushing
anything touching `sdk_live.rs`.** CodeQL reads NAMES [[codeql-flags-sanitisers-by-name]].
**Deferred with a reason** (all in the audit doc): brokers not force-routed; the guard tier never
sees bytes past 64 KiB; `secret://` refs not tool-bound; `Host:` ≠ CONNECT authority; no email-replay
freshness window; macOS worker-side caps. **Before release: flip force-routing on.**

**#650 / #653 / #649 — three one-line lessons.** #650: **a containment fix must not widen
containment**; ⚠️ `Path::components()` strips **interior** `.` only
[[rust-path-components-normalizes-dot]]. Open: #657, #658, #659. #653/#654: **the reusable pattern is
the `*_or_reason` sibling** — return the reason **without rendering a verdict**, so one caller can
skip where another must fail; #690's `probe_fault` is its fifth consumer. Open: #664, #665.
#649/#651: **the remedy an advisory states can be a no-op that exits 0**
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

> ⚠️ **The forced-cold clippy recipe FALSIFIES the #687 container freshness gate, and the two are
> both in this file.** `find … -name main.rs | xargs touch` moves
> `workers/python-exec/src/main.rs`, which is in the gate's source closure, so every container e2e
> then reports the image stale and **panics under REQUIRE** — naming a ten-minute cross-build as the
> remedy for a file nobody edited. Measured 2026-09-11: *"image 2026-09-10 16:36 UTC, source
> 2026-09-10 16:36 UTC"*, the same minute, on an image built from those exact bytes. It is #667's
> original objection to mtimes, reappearing on the one tier allowed to use them, and it is a false
> **refusal** — the direction that gets a gate switched off.
> **Order the two commands: clippy first, then rebuild the image, then run the container tier.**
> [#691](https://github.com/hherb/kastellan/issues/691) holds the four options and the measurement
> each needs.

> ⚠️ **A private `CARGO_TARGET_DIR` does not build `examples/`,** so `email_channel_e2e`'s 6 tests
> fail with `fixture not built` at a perfectly green commit
> [[custom-cargo-target-dir-breaks-daemon-e2e]]. Read the failure text before believing a regression.

> ⚠️ **rust-analyzer holds `target/debug/.cargo-lock` and will block a sweep indefinitely.** The Mac
> sweep sat at `Blocking waiting for file lock on build directory` for 7 minutes until the IDE's
> `cargo check --workspace` was killed. Check for a foreign `cargo check --workspace` on the same
> manifest before concluding a build is slow.

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

> ⚠️ **Freshly-linked executables can hang forever in `_dyld_start` on macOS**, so every daemon e2e
> fails with the daemon's stdout **and** stderr **completely empty** — which reads exactly like a code
> defect. **Newness, not size**, and `sample` alone does not prove it
> [[mac-fresh-large-binaries-hang-in-dyld]].

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
   answers were wrong.** #660's last owed gate is discharged (2026-09-05, DGX at `9ace57ad`): two DMs
   from `@horst` were received, planned and answered (tasks 185/186, both `channel.replied`). **But**
   task 186 spent three of six plan iterations on near-duplicate searches and a fourth on
   `shell.exec /usr/bin/ls`, then blamed "the tool-step limit" for not reading the PDF, having never
   called `mail.get_attachment_text`, which task 185 had used successfully **four minutes earlier**.
   The two tasks reported **different booking references** for the same question with equal
   confidence. ⚠️ **Which answer was grounded could not be established**, because both large
   dispatches were audited `_truncated: true` with `req` and `result` dropped wholesale —
   [#617](https://github.com/hherb/kastellan/issues/617), the first time it has blocked a real
   investigation rather than a hypothetical one. **Likely two issues: #617 first, then #677.**

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
| **Mac + DGX** ([#692](https://github.com/hherb/kastellan/pull/692), #690/#689/#686 — **the gate that stands**) | **`499c9488`** (branch tip; squashed to `c5bf5e5f`) | **Mac full sweep:** `cargo test --workspace --no-fail-fast --locked -- --nocapture` **4155 / 0 / 29**, **177** suites, `TEST_EXIT=0`, **0 `[WARN]`**, 339 `[SKIP]` (328 of them the pre-existing absent-Postgres ones). **DGX full sweep: 4290 / 0 / 61**, 177 suites, `TEST_EXIT=0`, **4 `[SKIP]`** (gliner tier, held), **0 `[WARN]`**. ⚠️ **Both deltas reconcile exactly, and against different baselines.** DGX: **+40** over `main`'s 4250 — every new test is cross-platform (`bounded_command` 18, `subprocess_guard` 12, `container_tests` 4, `images` 3, `landlock_lsm` 3). Mac: 4104 at `0fa5b8b6` **+10** (#688's review round, which post-dated that gate) **+41** (the 40 above plus the one macOS-only smoke test) = **4155**. The host gap moved from **−136** to **−135**, which is that one macOS-only test and nothing else. Ignored unchanged on both. **Container tier under `KASTELLAN_MICROVM_REQUIRE_E2E=1`: 10 / 0** across all three suites (4 + 1 + 5), every suite exit 0, **0 `[WARN]`**, 4 opt-in gliner `[SKIP]`s. ⚠️ **That tier needed the image rebuilt twice, and the second rebuild is a finding, not a chore** — see [#691](https://github.com/hherb/kastellan/issues/691) below. **Live negative controls, all run:** the budget removed → the suite takes **30.01 s instead of 0.50 s** and both timeout tests fail; the drains removed → a 512 KiB writer **times out at the full 60 s budget** and 6 tests fail; the guard's rule planted with a violation → found and named; the #689 detector fed `capability,landlock,bpf` from a real container → red with the full operator message | **Mac** `--workspace --all-targets --locked -D warnings` exit **0**, zero warnings, all **27** workspace crates from a forced-cold `touch`. **DGX** the same, exit **0**, 27 crates. `kastellan-sandbox` also cross-clippied for `aarch64-unknown-linux-gnu` from the Mac — **which caught a Linux-only unused import the Mac run compiles out** | **339** Mac (328 absent-Postgres), **4** DGX. **0** `[WARN]` |
| **DGX** (`main`, the sweep #688 owed and never ran) | **`09a4f924`** | Superseded by the row above; kept because it is the only direct measurement of `main`. **4250 / 0 / 61**, 177 suites, `TEST_EXIT=0` | exit 0, 27 crates | **4** |
| **Mac + DGX** (#688, the gate that stood) | **`0fa5b8b6`** | Mac **4104 / 0 / 29**, 177 suites, `TEST_EXIT=0`; DGX **4239** measured a commit early at `5afc88cd`, reconciled to **4240**. **Container tier under `KASTELLAN_MICROVM_REQUIRE_E2E=1`: 8 / 0** across all 3 suites, **0 `[SKIP]`, 0 `[WARN]`** — first time they exercised current code since June. Three live negative controls on the real host: the 2026-06-26 image `[SKIP]`ed by default and **panicked** under REQUIRE; `container system stop` produced "start the service", not "rebuild the image"; the rebuilt image turned all four python-exec tests red at `Protocol(EarlyExit)`, which the Landlock injection turned green | Both hosts exit 0; `kastellan-sandbox` also cross-clippied for `aarch64-unknown-linux-gnu` from the Mac | **349** Mac (326 absent-Postgres, pre-existing), **4** DGX |
| **DGX** (#685/#682) | **`10cb6761`** | **4206 / 0 / 60**, 177 suites, `TEST_EXIT=0`. **Firecracker tier under `KASTELLAN_MICROVM_REQUIRE_E2E=1`: 30 / 0** across all **15** suites (discovered by grep, not hand-listed), run **after a plain `bash scripts/build-release.sh`**. ⚠️ **These tests are `#[ignore]`d and need `-- --ignored`**: the first attempt without it reported `3 passed, 28 ignored` with every suite exit 0 — a green run that booted no VM at all | exit 0 | **4**, gliner |
Older rows (#683 `4189`, #680 `4142`, #675 `4075`, #669 `4049`, and back to 2950) are in the
[`archive/`](archive/) snapshots.

⚠️ **`scheduler_ask_expiry_e2e` flakes under a full sweep, and this file's diagnosis of it was WRONG
for two gates.** It said "widen the poll deadline"; the evidence says otherwise — the panic is past
both `await_state`s and the next log line is `claim_one error: … No such file or directory`: **the
per-test cluster's unix socket went away underneath the test**. In isolation it runs 62 s against a
20 s + 90 s budget. [#676](https://github.com/hherb/kastellan/issues/676); likely the same ownership
problem as [#548](https://github.com/hherb/kastellan/issues/548). **The general lesson: a flake
attributed once gets re-attributed forever — re-read the actual failure text on each recurrence.**

⚠️ **A dropped ephemeral port is NOT a reserved one.** `skip::tests::the_resolve_and_reach_arms_…`
bound a loopback port, dropped the listener and assumed it was then closed; the OS may hand a freed
ephemeral port straight to another process. **Measured: 1 failure in 3 full sweeps, 0 in 40 isolated
runs.** Fixed by *confirming* rather than assuming.

**Both hosts are load-bearing, in opposite directions — always check both.** The two supervisor
backends compile on one host each: a `launchd_agents.rs` change is invisible to the DGX and a
`systemd_user.rs` change is invisible to the Mac, where `cargo test` compiles **zero**
`systemd_user` tests [[mac-compiles-zero-systemd-tests]]. The mirror is just as real: Mac clippy
compiles `cfg(target_os = "linux")` items *out*, so an unused cfg-linux helper fails only the DGX
gate — **this session hit it again**, and a `cargo clippy --target aarch64-unknown-linux-gnu` from
the Mac caught it in seconds (pure-Rust crates only; `core` won't cross-compile, `ring` C dep)
[[cross-clippy-pure-rust-crates]]. ⚠️ **And a whole file can be `#![cfg(target_os = "linux")]`**, in
which case the Mac compiles *nothing* in it, imports included.

**Predict the count, then reconcile the delta exactly.** Every gate above was predicted from the
diff's new `#[test]` count and investigated when it missed — the cheapest detector for "a test I
think I added is not being compiled". **Reconcile by diffing PER-SUITE counts, not test names:**
`--nocapture` interleaves output so a `test … ok` name grep loses lines, and `#[should_panic]` tests
print `- should panic ... ok`, which a bare `… ok` grep reports missing. ⚠️ **An `ignored` delta with
no new `#[ignore]` is usually a doc-test**: a ```` ```ignore ```` fence counts, and moving one into a
`cfg(test)` module removes it from the count [[ignore-fenced-doc-example-moves-ignored-count]].

⚠️ **A `[SKIP]` can hide a dead fixture for months, and a `[SKIP]` line is evidence nothing may
fake.** The four gliner-relex venv-shim skips were not "this host is unstaged" — the DGX's `.venv`
was a **copy of the Mac's**, `bin/python` pointing at a path that cannot exist on Linux: `readlink
.venv/bin/python` before believing a skip, and prefer a `REQUIRE_*=1` knob. And since
`grep -c '^\[SKIP\]'` over a `--nocapture` run is how a green sweep is audited, a unit test that
printed one would inflate exactly the number it protects — every `[SKIP]` renders through the pure
[`tests_common::skip::skip_line`](../../../tests-common/src/skip.rs), so **assert on `skip_line`;
call the `skip_if_*` wrappers only from real fixtures.**

**Two standing reading rules.** A green run with `[SKIP]` lines means tests *skipped*, not that the
sandbox contained anything — re-check with `-- --nocapture`. And skip-as-pass counts as passed, so
counts stay comparable either way.

**Mac verification runs from the repo's own `target/`, not a private `CARGO_TARGET_DIR`** — the
private dir breaks daemon e2e (above). If one is unavoidable it must live under `$HOME`, not `/tmp`:
macOS scrubbed a scratchpad target dir *mid-run* once [[dgx-run-logs-tmp-scrubbed]]. Keep gate logs
under `$HOME` for the same reason.

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
