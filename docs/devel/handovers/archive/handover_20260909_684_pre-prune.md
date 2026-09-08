# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20260905_669_pre-prune.md`](archive/handover_20260905_669_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here.

**Last updated:** 2026-09-09 (session 2) · **`main` HEAD:** `0939e80c` —
[#685](https://github.com/hherb/kastellan/pull/685) MERGED (#682, one producer for
`target/release/`), on top of [#683](https://github.com/hherb/kastellan/pull/683) `ec9a2e94` (#679,
the micro-VM REQUIRE knob + its source guard) and
[#680](https://github.com/hherb/kastellan/pull/680) `fb560ab7` (#667, the rootfs freshness gate). ·
**OPEN BRANCH: `fix/684-687-macos-container-parity`** — see
[This session](#this-session-684--687--the-macos-container-tier-gets-what-the-linux-one-has). ·
**DGX RUNNING `fb560ab7`** — behind `main`, but everything since is tests + scripts + docs, so the
running daemon is unaffected; redeploy at the next core change. ⚠️ **Its eight rootfs images were
rebuilt 2026-09-08** and now bake the `--workspace` init (`8a21877a…`).

> ⚠️ **An issue's own census can be wrong, and so can the rule it proposes — and so can YOUR
> re-derivation.** #679 named 7 call sites, the property covered 11 across 6 kinds, and review of
> *that* found a **12th** of a 7th kind. #667 asked for mtimes and mtimes were measurably wrong
> (cargo relinks unchanged output, so six *correct* images read 5 h "stale").
> **Re-derive the property, and measure the proposed rule against the real host, before
> implementing either.** [[issue-as-filed-can-carry-a-regression]]

> ⚠️ **A census taken with a tool inherits that tool's blind spot, and a guard built from the same
> tool cannot see what the census missed.** The 12th #679 site was a hand-written `[SKIP]` that
> rustfmt had wrapped, so the `eprintln!` sat on one line and the literal on the next. The scanner
> written to catch exactly that class matched both on **one** line — so it reported the file clean,
> and the census that used the same reasoning never counted it. **When a check and the survey that
> scoped it share an implementation, they share its holes: test the check against a shape you did
> not write.** The fix is fail-closed on a *shape* (any `skip_if_*` / `*_or_skip` / `skip_line` not
> on a REQUIRE-aware allowlist) rather than a roster of known-bad names.

> ⚠️ **Two PRs each edited this file's header; the merge kept BOTH and `main` shipped a handover that
> contradicted itself** — 736 lines, two `Last updated:` blocks, three `### This session` sections,
> the `#669` list cut in half. Git saw no conflict worth showing and CI has nothing to say about
> prose. **A long-lived branch that edits a rolling doc must re-read that doc on `main` before
> merge.** Recovery took the *branch* version as base plus one re-added section, not a hand-merge.

> ⚠️ **A gate booked as "pure verification, not code" is not evidence until it has RUN**, and **an
> error with no content is a defect *multiplier*** — #660's two gates sat here as bookkeeping for two
> days while **`0 of 21`** Firecracker tests passed, and three independent production defects hid
> behind one identical contentless `Protocol(EarlyExit)`. Before adding a layer, ask what it says
> when it refuses.

> ⚠️ **A slow Mac cargo build is CONTENTION, not the `_dyld_start` wedge**, and `sample` alone cannot
> tell them apart — a thread that is never *scheduled* shows the same single frame. **Check `uptime`
> and `%cpu` first:** a wedge burns no CPU *and never finishes*; contention burns little and finishes.

---

## Current state

### This session: #684 + #687 — the macOS container tier gets what the Linux one has

Branch `fix/684-687-macos-container-parity`. #667/#679/#682 all landed on the **Firecracker** tier
only; `CLAUDE.md` makes cross-platform parity a hard constraint, and the macOS Apple-`container`
tier had **no REQUIRE knob, no freshness gate, and three byte-copied private preconditions**.

⚠️ **The gate's first act was to prove the tier had been DEAD FOR 69 DAYS, and nothing had noticed.**
`kastellan/python-exec:dev` on the dev Mac was built **2026-06-26**; the worker sources it bakes last
changed **2026-09-03**. So the image predates the **2026-09-02 security audit** (`62d98a00`, 29
fixes) *and* the #650 interpreter bind — and all eight container e2es passed against it, every run.
This is #667's thesis reproducing exactly, on the other platform.
[[stale-fixture-turns-a-gate-into-a-formality]]

- ⚠️ **The production defect the stale image was hiding is #669's, on the other guest kernel.**
  Rebuild the image and every container test dies at `Protocol(EarlyExit)`. The worker's own words,
  once asked for: `landlock: Landlock ruleset is not enforced by this kernel`. **Measured** on Apple
  `container` 1.1.0 / guest kernel 6.18.15: no `/sys/kernel/security/lsm` in the guest. The
  2026-09-02 audit made Landlock fail-closed, #669 gave the *Firecracker* guest kernel the sanctioned
  `KASTELLAN_LANDLOCK_PROFILE=none` opt-out, and **the container backend never got it**. Fixed the
  same way — injected in the backend as *a default that never overrides a caller*, with the same
  per-spawn `tracing::warn!` (because `warn_lockdown_overrides` inspects the derived policy *before*
  a backend runs, so it is structurally blind to a backend-side injection). ⚠️ **Seccomp is
  unaffected, and that is measured rather than argued** — the same hand-run worker reports
  `lockdown Linux { landlock: Disabled, seccomp: Installed, .. }`; the justification for disabling
  one layer rests entirely on the other surviving.
- ⚠️ **#667's digest rule CANNOT be carried across, and re-deriving that was the first real work.**
  `build-image.sh` **cross-builds** the worker for linux/arm64 inside a container with its own
  `--target-dir`; the host `target/release/` copy is a macOS Mach-O. Different arch, different object
  format — they can never compare equal, and nothing on the host reproduces the guest bytes without
  re-running the cross-build. Recording the digest at build time is the sidecar-manifest option #682
  measured and rejected (equal by construction).
- **So the reference is the image's build time against its SOURCE mtimes — and #667's mtime
  rejection does not transfer.** That rejection was about *build outputs*: cargo relinks
  byte-identical binaries and moves their mtime, so six correct images read stale. Nothing relinks a
  `.rs` file. Measured before it was written, and it returns the right verdict on the real host.
  ⚠️ **The verdict is ONE-SIDED and the naming says so:** the fresh arm is `NewerThanSources`, not
  `Fresh` — an image built *after* its sources was not necessarily built *from* them. It can refute
  staleness; it cannot certify freshness. Calling it `Fresh` would be the "certifies on partial
  evidence" collapse #680's review removed.
- ⚠️ **A staleness rule needs a source closure, and only an image THIS REPO BUILDS has one.** The
  first version applied the python-exec closure to `alpine:3.20` and told the operator to *"rebuild
  it with build-image.sh"* — an upstream image no script here produces. Caught by
  `lifecycle_container_routing_e2e` on the real host. Hence `BUILT_IMAGES`, the direct parallel to
  #667's `ROOTFS_IMAGES`; an unregistered image is never age-checked **at all**, and that lookup
  lives in the *pure* orchestrator rather than the injected closure (#680: a decision left in the
  impure half was replaceable with nothing failing).
- **The tree already had the right primitive and three test files bypassed it.**
  `MacosContainer::probe_image` exists in the **production** sandbox crate and its doc says it avoids
  *"the per-line substring-matching footgun (`devbox` matching `dev`)"* by construction — while all
  three suites hand-rolled `container image list` + `contains("python-exec")`. The preflight now uses
  `container image inspect` through the production argv producer `build_image_inspect_argv`, so this
  crate does not become a fourth spelling (#669's "count the producers"). One call yields **both**
  facts: presence (exit status) and build time (JSON).
- ⚠️ **Widening the source guard's discovery by helper NAME was wrong, and the tree told me so.**
  `core/tests/gliner_relex_e2e.rs` has its own private `fn skip_if_no_container()` for the
  gliner-relex worker image — different container, different knob (#653/#664). Keying on the name
  swept it in and reported **ten violations in a file that is correct as written**. Discovery now
  requires **both** a `MICROVM_PREFLIGHTS` name **and** the `kastellan_tests_common::microvm` import;
  measured, that is exactly the 15 Firecracker + 3 container suites and nothing else.
- **#683's fail-closed guard caught the new helper by itself.** Adding `skip_if_no_container` failed
  `the_banned_roster_matches_the_helpers_that_actually_exist` until it was declared REQUIRE-aware —
  the shape rule working as designed on a helper written after it.

### Merged arcs — only what still binds

Full prose in the [`archive/`](archive/) snapshots, one line each in the ROADMAP, the 2026-09-02
audit in [`docs/security-audit-2026-09-02.md`](../../security-audit-2026-09-02.md). Most of what
follows is also a memory note, auto-loaded; kept here where it changes a *first* move.

**#685 (`0939e80c`) — `target/release/` has one producer (#682).** The #667 freshness gate compares
an image's baked binary against `target/release/<binary>`, and those two were written by **different
cargo invocations**. ⚠️ **Cargo unifies features PER INVOCATION, so package selection changes the
bytes of an otherwise identical binary** — measured on the DGX, deterministic each way:
`-p kastellan-microvm-init` → `669821d3…` (what all eight images baked) against `--workspace` →
`8a21877a…`. So **after any deploy the gate declared all eight *correct* images stale**.
[[cargo-package-selection-changes-binary-bytes]] ⚠️ **The issue's own favoured fix does not work**: a
sidecar manifest of the baked digest gives `manifest`-vs-`target`, which is today's comparison with
an extra file. The ambiguity is removed **at the producer** — all eight rootfs scripts now call
`scripts/build-release.sh`. ⚠️ **That is ONE SCRIPT WITH TWO INVOCATIONS, and the second is
reversible**: a later bare `cargo build --release --workspace` re-uplifts the non-featured Matrix
worker in **0.32 s with no output but `Finished`**, so **run `build-release.sh` LAST** — the rule
that now leads `CLAUDE.md` § Build. `no_rootfs_build_script_runs_its_own_cargo_build` is the drift
pin and **both halves are load-bearing** (presence alone passes a script that keeps its old `-p`
line; absence alone passes a script that builds nothing). ⚠️ **Its review round is the reusable
lesson:** both existing script scanners read **comments as code**, and the first `code_of()` fix
claimed its error direction *"leaves more text to scan, never less"* — false, and false exactly where
it mattered. One quote-state-aware `microvm::script_scan::code_of` now serves **all six** scanners.
Filed, deferred: [#686](https://github.com/hherb/kastellan/issues/686),
[#687](https://github.com/hherb/kastellan/issues/687).

**#683 (`ec9a2e94`) — every micro-VM precondition answers to the REQUIRE knob (#679).** The knob
covered what `skip_if_no_microvm` checks and nothing asked *beside* it; `||` short-circuits, so on
exactly the host the operator cares about control reached `skip_if_no_supervisor()`. The vocabulary
that shipped is `microvm::skip_unless_ready` / `dep_or_skip` / `host_probes`. ⚠️ **The load-bearing
test is a source scanner** (`microvm::guard::bypassed_gates` over the real `core/tests`), because the
false green appears only on a host where the micro-VM preconditions are MET and a neighbouring one is
not — which no unit test and no Firecracker run can be. ⚠️ **Its review found the guard reporting
green on a live instance of the defect it was built for:** a hand-written `[SKIP]` that rustfmt had
wrapped, while the rule wanted macro and literal on one line. **A census taken with a tool inherits
that tool's blind spot, and a guard built the same way cannot see what the census missed** — so rule
1 is now fail-closed on a *shape* (`skip_if_*` / `*_or_skip` / `skip_line` not on the `REQUIRE_AWARE`
allowlist) rather than a roster of names, and the scan gained a **positive control** because
`assert!(violations.is_empty())` over a loop is green whether the loop found nothing or never ran.
[[guard-shares-the-census-blind-spot]] ⚠️ **The guard covers Firecracker only** — discovery keys on
`skip_if_no_microvm`, so the macOS Apple-`container` suites have **no REQUIRE knob at all** and one
folds a failed `container` spawn into "image not present"
([#684](https://github.com/hherb/kastellan/issues/684)). ⚠️ **The Mac cannot see any of this code** —
every converted file is `#![cfg(target_os = "linux")]`, so the file compiles to nothing, imports
included; the Linux leg caught an unused import no Mac gate could report
[[mac-compiles-zero-systemd-tests]].

**#680 (`fb560ab7`) — a stale rootfs image can no longer gate anything (#667).** Every image bakes
its **own** copy of `kastellan-microvm-init` and of its worker, so a guest-side change was invisible
to the FC e2es until that image was rebuilt; the check sits at the one chokepoint every FC e2e
funnels through. ⚠️ **The issue asked for mtimes and mtimes are WRONG here — measured, not deduced:**
six *correct* DGX images read 5 h "older" than an init they contained byte-identical copies of,
because cargo relinks unchanged output [[cargo-relinks-identical-mtime-not-content]]. **The reference
is the sha256 of the baked copy**, read with `debugfs -R "cat …"` — no mount, no loop device, no root
— which also catches an image built from a stale checkout. It compares against whatever the local
target dir holds, which was [#682](https://github.com/hherb/kastellan/issues/682) until this session
gave `target/release/` a single producer. **Four
verdicts, four treatments:** `Stale` and `Unusable` panic unconditionally (positive evidence the run
proves nothing); `Fresh`-with-caveats and `Indeterminate` `[WARN]` and still run, both naming *which*
binary and *why*. ⚠️ **A verdict that certifies on PARTIAL evidence is the original bug with better
manners** — `Fresh` used to return as soon as **one** binary matched, so a stable init silently
certified a June worker, and a unit test pinned that as intended. ⚠️ **Measured: every `debugfs`
failure exits 0** — missing path, not-ext4, unopenable, symlink — so benign causes are now separated
*structurally* (`ErrorKind::NotFound` on spawn), never by matching on wording. ⚠️ **"Mutation-proved
7 for 7" measured only the pure half**, and the wiring could be replaced with `false` with nothing
failing on Linux and the mutation *not attemptable* on the Mac
[[mutation-proof-counts-only-mutants-you-tried]]. **Prose accurate at commit *N* shipped stale at
*N+3*: re-read a branch's self-description against `git diff origin/main...HEAD`.**

**#681 (`aee2a7f0`) — the Hermes Agent survey, docs only.**
[`notes/2026-09-06-hermes-agent-survey.md`](../notes/2026-09-06-hermes-agent-survey.md). Four ROADMAP
entries, and **the ordering is the finding**: the **anchor index** (an LLM-free regex harvest of
exact identifiers rendered beside a summary) is #678 slice **(e)**, because `handoff` already pages
by byte *offset* and an offset is unguessable — the recovery path exists and is unusable; then a loop
guardrail for #677, a **planner A/B battery**, and skill lifecycle **last**, because their six eval
suites measure context economics and **not one** measures whether a skill made the next task go
better. The number worth remembering: a lean tail plus one recovery round-trip scored **68.3 % recall
on 49 K tokens** against **45.8 % on 162 K** for the fat verbatim tail — **a big verbatim tail is not
the safe choice; it is the expensive one that also loses the needles.** ⚠️ Their `execute_code` opens
an RPC socket from agent-authored Python back into the tool dispatcher — for us that turns one
compromised worker into every worker; §3.6 has the only shape that keeps the invariant.

**#675 (`f831b3d1`) — the micro-VM path can say why it failed** (#666, #670, #671, #672). **A failed
boot leaves `console.log` in the kept run dir** and the launcher echoes a redacted tail to its own
stderr — **read that before theorising** [[microvm-guest-failures-are-invisible]]; the kernel prints
its command line every boot carrying `kastellan.env=<hex>`, so the **value** is redacted and the
**key** kept, its absence still a signal. ⚠️ **`EarlyExit` carries the worker's last words — and the
first version broke what it protected:** `\n` is *in* #544's ANSI-neutralising class, so
neutralising the raw chunk killed `drain_reader`'s line split, one line forever, silently. **A
predicate correct for one renderer can be destructive in another; the shared class is right, the
shared application point is not.** ⚠️ **`bwrap --clearenv` means the launcher has NO environment**
[[microvm-launcher-knobs-must-be-argv]], and **the release profile is `panic = "abort"`**
[[release-profile-panic-abort-kills-raii]]. **The VMM jail has a real-bwrap gate** (#671,
`linux_smoke.rs`, **not** `#[ignore]`d) running `/bin/true` under the production argv and asserting
**exit status**, because no content assertion catches a flag *combination* bwrap refuses at
option-parse time; guest `/run` is `mode=0755` (#672 — a tmpfs with no `mode=` comes up **1777**),
and a failed relay-socket chown is fatal (#670).

**#669 (`4955a52c`) — the Firecracker gate.** The backend had been **entirely dead** at **0 of 21**
since the audit merged. **Count the producers, and make the const the only spelling** —
`build_vmm_jail_argv` was the **third** bwrap argv producer and #661's fix missed it; #671's gate
catches the class now. **The pinned guest kernel has no Landlock**
[[firecracker-guest-kernel-no-landlock]], so the plan states `KASTELLAN_LANDLOCK_PROFILE=none` as a
**default that never overrides a caller**; repinning is
[#668](https://github.com/hherb/kastellan/issues/668). ⚠️ The in-guest W-2 `groups` assertion is a
regression guard, **not** a proof — guest PID 1 has no supplementary groups either way. ⚠️ **A
non-hex `kastellan.mounts=` fixture fails OPEN, silently**
[[fail-safe-parsers-make-vacuous-fixtures]]. **`/run` is out of the chown set** and re-adding it
would be a regression: chowning a *sticky* directory is what lets the owner unlink entries it does
not own.

**#660 (`62d98a00`) — the second pre-release security audit.** 29 fixes, 80 files, all owed gates
discharged. What still binds: (H1) the dispatch chokepoint scrubs every redeemed secret out of both
result arms; (H2) agent-raised `l1_insight`s screened at promotion *and* prompt assembly; (H3) every
per-spawn `/tmp` dir minted with `create_private_dir` — **a pre-planted name from another uid FAILS
THE SPAWN CLOSED; that is the contract, do not "fix" it back to `create_dir_all`**; (H4) seccomp
admits `clone` only without `CLONE_NEW*`. ⚠️ **Three lockdown behaviours are FAIL-CLOSED and will
bite a careless fixture:** a missing `KASTELLAN_SECCOMP_PROFILE` is an error (`none` is the explicit
opt-out), an unenforceable Landlock ruleset is an error — **which is exactly what killed the macOS
container tier for 69 days, see this session** — and a corrupt `kastellan.env=` guest token refuses
the boot. **Every networked stdio worker builds its handler INSIDE `serve_stdio_with`** (Landlock is
per-thread); keep that order for any new worker. **Run the live-matrix clippy job before pushing
anything touching `sdk_live.rs`** — it catches lints the default-feature workspace clippy never
compiles. CodeQL reads NAMES [[codeql-flags-sanitisers-by-name]]. **Deferred with a reason** (all in
the audit doc): brokers not force-routed; the guard tier never sees bytes past 64 KiB; `secret://`
refs not tool-bound; `Host:` ≠ CONNECT authority; no email-replay freshness window; macOS worker-side
caps. **Before release: flip force-routing on.**

**#650 (`c03ec1a3`) — the interpreter alias bind. The admission rule is non-widening and that is the
load-bearing choice:** a `uv` minor-version **symlink alias** binds only when it canonicalizes to the
canonical prefix, because **a containment fix must not widen containment**. ⚠️ `Path::components()`
strips **interior** `.` only [[rust-path-components-normalizes-dot]]. Open: #657, #658, #659.

**#653 / #654 (`9ace57ad`) — the gliner-relex require knob. The reusable pattern is the `*_or_reason`
sibling:** return the reason **without rendering a verdict**, so one caller can skip where another
must fail. #667 was its second consumer, #679 its third. Open: #664, #665.

**#649 / #651 (`ef8144f8`) — the transformers advisory. The remedy an advisory states can be a no-op
that exits 0** [[uv-lock-upgrade-can-land-still-vulnerable]]: both floors moved in
**`pyproject.toml`**; the `python-lock-check` CI job catches a **weakened floor**, not an advisory.

### The guard tier — what still binds

- **D10 — the tier is ADVISORY defence-in-depth, NOT a gate.** 65 % recall (36/55) at FP-0; 6/6 on
  bare imperatives but **5/8 missed** on narrative framing. **Nothing downstream may relax on it.**
- **τ = 0.79552656 is a REQUIRED operator input with no default**, and **five misconfigurations STOP
  THE DAEMON** (D6): half-configured keys, τ outside `(0.0, 1.0]`, a pinned timeout of 0, an
  unreachable `/props`, and a context below `SCAN_BYTE_CAP + 512 = 66 048` (D8).
  `KASTELLAN_REQUIRE_GUARD=1` makes the *unconfigured* case fatal too.
- **`best_tau` returns NONE** — real captured content overlaps at every threshold, and that stratum
  was **catalogue-selected**, which is why **corpus growth from production is the cheap path**.
  Harvest it before designing another campaign.
- **`AuditSink::insert` applies `truncate_payload` before delegating to `insert_stored`**, so no sink
  double can record a payload Postgres never stored [[audit-sink-doubles-hide-storage-transforms]].
  **Absence and loss must not render identically.**
- ⚠️ **The stated mitigation for an issue can disarm the instrument built to check it** — the live
  probe passed having measured nothing under a *pinned* timeout, precisely what #612 tells a Metal
  operator to use. It now refuses a pin outright.
- ⚠️ **#624 and #626 do NOT close [#612](https://github.com/hherb/kastellan/issues/612).** #624
  removed the *contention* error (one post-arc boot spread **4 765.7** against **1 450.4** tok/s,
  **3.29x inside a single boot** — `TimeoutBasis::Saturated` does **not** mean every sample stalled);
  #612 is that extrapolating from a ~1 KiB sample is non-linear **on Metal whatever the load**
  [[metal-prompt-processing-is-nonlinear]].

### Standing hazards that have each cost a session

Most are also memory notes (auto-loaded); kept here because they change the *first* move.

> ⚠️ **Clippy parity is a `rustup update`, not a property of the hosts.** CI pins nothing
> (`dtolnay/rust-toolchain@stable`) and both dev hosts float, so they drift silently. `rustc
> --version` on the host you are gating on. **2026-09-07: DGX on 1.98.0.**
> [[local-clippy-not-ci-parity-rust-version]]

> ⚠️ **A cached `cargo clippy` reports a full-workspace pass it never ran.** Exit code alone does not
> distinguish it — **count the `Checking` lines** against the *reverse-dependency set* of your change.
> Cold is ~217–303; a warm dir can report exit 0 having linted 4. **`touch` the changed files and
> re-run if the count looks too small** — this session's first clippy linted 6 crates in 7 s, and the
> forced re-run linted the correct 3. And `cargo check`/`clippy --all-targets` do **not** warm the
> target dir for `cargo test` (metadata only, no linked binaries) — **run the sweep first, lint after.**

> ⚠️ **A private `CARGO_TARGET_DIR` does not build `examples/`,** so `email_channel_e2e`'s 6 tests
> fail with `fixture not built` at a perfectly green commit
> [[custom-cargo-target-dir-breaks-daemon-e2e]]. Read the failure text before believing a regression.

> ⚠️ **"Filed, not fixed: #N" in a PR body or commit message CLOSES #N.** GitHub matches the
> `fixed: #N` substring and has no notion of negation; it has cost three issues. Write **"deferred to
> #N"**, and before merging run
> `gh pr view <n> --json body --jq .body | grep -oiE '(close[sd]?|fix(e[sd])?|resolve[sd]?)[[:space:]:,]+#[0-9]+'`
> over the body *and* the commit message. [[pr-body-not-fixed-autocloses-issue]]

> ⚠️ **Squash-merge caveat:** every PR lands as one squash commit, so its *branch-tip* SHA (where the
> gate ran) is **not** an ancestor of `main`. Check content, not `merge-base`. **And two branches that
> both edit this file will merge without a conflict git thinks is worth showing** — see the ⚠️ at the
> top; re-read the doc on `main` before merging a long-lived branch.

> ⚠️ **Freshly-linked executables can hang forever in `_dyld_start` on macOS**, so every daemon e2e
> fails with the daemon's stdout **and** stderr **completely empty** — which reads exactly like a code
> defect. **Newness, not size**, and `sample` alone does not prove it: check `uptime` and `%cpu` first,
> because contention looks identical. [[mac-fresh-large-binaries-hang-in-dyld]]

> ⚠️ **`kastellan-worker-egress-proxy` leaks on the Mac** (five orphans in one sweep, across three
> target dirs — not investigated), and **a `pgrep -f '<cmd>'` wait loop matches itself** and never
> exits, because the Bash tool's `zsh -c` wrapper puts the pattern in the waiter's own argv: use
> `pgrep -x`. [[pgrep-wait-loops-match-themselves]]


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
   from `@horst` were received, planned and answered (tasks 185/186, both `channel.replied`), so the
   invite/two-party scoping does not break a normal DM. **But** task 186 spent three of six plan
   iterations on near-duplicate searches and a fourth on `shell.exec /usr/bin/ls` — the planner
   theorised an email attachment might be a file in its cwd — then blamed "the tool-step limit" for
   not reading the PDF, having never called `mail.get_attachment_text`, which task 185 had used
   successfully **four minutes earlier**. The two tasks reported **different booking references** for
   the same question with equal confidence. ⚠️ **Which answer was grounded could not be
   established**, because both large dispatches were audited `_truncated: true` with `req` and
   `result` dropped wholesale — [#617](https://github.com/hherb/kastellan/issues/617), the first time
   it has blocked a real investigation rather than a hypothetical one.

**THEN, on the micro-VM path — one issue left, and it needs a kernel build.**
[#686](https://github.com/hherb/kastellan/issues/686) is cheap and separable: seven of eight rootfs
build scripts assume `CWD == repo root`; the supported entry point already `cd`s there, so it bites
only a hand-run, and the fix is the four-line `REPO_ROOT` prologue `rebuild-all` and
`build-browser-driver-rootfs.sh` already carry (⚠️ `cd ""` **succeeds** in bash, which is why that
prologue tests for empty). Then [#668](https://github.com/hherb/kastellan/issues/668) — repin a
guest kernel built with `CONFIG_SECURITY_LANDLOCK`, the standing posture item. ⚠️ **#668 now has a
macOS twin with no issue of its own:** the Apple `container` guest kernel does not enforce Landlock
either (measured, 6.18.15), so *both* micro-VM tiers run seccomp-only by design. If #668 is ever
done, the container backend's injection has to be revisited in the same breath — it is deliberately
written as a default a caller can already override.

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
than a patch; #616 unblocked its favoured option (measure from the `ms` / `body_byte_len` the guard
rows now carry), and every cheap fix is closed off in the issue. Beside it, both cheap:
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
  marking it as *the id*
  [[tool-output-reaches-planner-key-stripped]] [[opaque-ids-are-unusable-tool-params]].
- **[#550](https://github.com/hherb/kastellan/issues/550)** — **the naive fix is wrong**: the overlay
  legitimately overrides `kastellan.env` keys, so it must compare the *folded* environment, which
  `fold_env_files` already computes for launchd.
- **[#548](https://github.com/hherb/kastellan/issues/548)** — not a teardown bug (`PgCluster`'s `Drop`
  guards are correct and cannot run on SIGKILL), so the fix is about blast radius. ⚠️ #641 removed the
  shared suffix between a test daemon's unit and its sibling PG cluster; restore it with a
  `.suffix()` setter rather than by reverting the constructor
  [[issue-as-filed-can-carry-a-regression]].
- **[#551](https://github.com/hherb/kastellan/issues/551)** (systemd `%` specifier, workspace-wide),
  **[#519](https://github.com/hherb/kastellan/issues/519)**,
  **[#554](https://github.com/hherb/kastellan/issues/554)** (needs a live DGX gate — it narrows what
  a deployed worker may do), **[#534](https://github.com/hherb/kastellan/issues/534)**.
- **Mail credential expiry — [#673](https://github.com/hherb/kastellan/issues/673) +
  [#674](https://github.com/hherb/kastellan/issues/674).** An upstream 401/403 is reported as
  `POLICY_DENIED`, so an expired localmail credential reads as a kastellan policy refusal, and
  nothing notices the expiry at all. Same family as everything above — a failure naming the wrong
  cause.
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
  **Telegram inbound** (still rejected as primary — no bot E2E, centralized, ban risk);
  **MITM-of-browser** via a proper NSS trust-store import, **not**
  `--ignore-certificate-errors-*`, since production must not be loosened to make a test pass.

**File-split backlog (Item 9b)** — **`wc -l` before picking; the numbers drift.** The rule: **split
BEFORE the change that grows a file**, in a movement-only commit whose `#[test]` name set is
verifiable either side (`tests-common/src/microvm/` is the worked example). Best first picks, each a
pure test-lift: `core/src/channel/ask_message.rs` **956**, `workers/mail/src/handler.rs` **670**,
`sandbox/src/linux_firecracker/plan.rs` ~**1160** (`cfg(linux)`, DGX-gated),
`core/tests/guard_tier_e2e.rs` **1558** ([#639](https://github.com/hherb/kastellan/issues/639)).
Clean seam visible: `core/src/scheduler/asks.rs` **801**. Judgement first, not movement:
`db/src/asks.rs` **1127**, `db/graph.rs` **926**, `llm-router/src/config.rs` **843** — a small
`mod tests` there means a split is a production reorganisation. Also over cap, no seam called yet:
`core/src/scheduler/inner_loop.rs`, `core/src/channel/bus.rs`, `workers/matrix/src/sdk_live.rs`,
`llm-router/src/messages.rs`, `core/src/main.rs`. ⚠️ `tests-common/src/microvm/mod.rs` is now **647**
— it is next, and #679's `require.rs` deliberately went in beside it rather than into it.

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
  [[mutation-revert-never-git-checkout]]; a mutation proof counts only the mutants you tried, drawn
  from the **changed** functions [[mutation-proof-counts-only-mutants-you-tried]]. Plan text is a
  defect source — subagents transcribe prose verbatim [[plan-text-is-a-defect-source]].
- **`sqlx::migrate!` embeds at compile time** [[sqlx-migrate-embeds-at-compile-time]].

---

## Working state

### Test baseline (authoritative)

| Host | Commit | Result | clippy `-D warnings` | `[SKIP]` |
| --- | --- | --- | --- | --- |
| **DGX** (#685/#682 **after its review round** — **the gate that stands**; merged as `0939e80c`) | **`10cb6761`** | **Full sweep:** `cargo test --workspace` **4206 / 0 / 60**, **177** suites, `TEST_EXIT=0`, **4 `[SKIP]`** (all the gliner tier, held) and **0 `[WARN]`**. **The delta reconciles exactly: +11** over the 4195 below — the review's new tests, all in `kastellan-tests-common` (Mac 281 → 292) — and **ignored is unchanged at 60**. **Firecracker tier, `KASTELLAN_MICROVM_REQUIRE_E2E=1`: 30 / 0** across all **15** suites (the set is discovered by grepping `core/tests/` for the micro-VM preconditions, not hand-listed), every suite exit 0, **0 `[WARN]`**, 2 `KASTELLAN_MATRIX_FC_LIVE_E2E` opt-in `[SKIP]`s — run **after a plain `bash scripts/build-release.sh`**. ⚠️ **These tests are `#[ignore]`d and need `-- --ignored`**: the first attempt without it reported `3 passed, 28 ignored` and every suite exit 0, which is a green run that booted no VM at all. ⚠️ **The C0 negative control ran on the real host against the real image:** `matrix.ext4` bakes `d7e6aee6…`, `build-release.sh` reproduces exactly that, and a bare `cargo build --release --workspace` then flipped `target/release/kastellan-worker-matrix` to `f606683b…` — a digest no image bakes — in **0.45 s with no compilation**, while the init stayed at `8a21877a…`. Blast radius is exactly one binary. A second `build-release.sh` restored it. Plus a live mutation control on the Mac: a `cargo build` planted **two hops away** in the sourced `lib/guest-kernel.sh` was caught and named, which the pre-review text scan could not see | `--workspace --all-targets --locked -D warnings` exit 0 on the Mac; DGX sweep `TEST_EXIT=0` | **4**, gliner tier. **0** `[WARN]` |
| **DGX** (#685/#682, first gate — superseded by the review round above) | **`2aa78b3e`** | **Full sweep:** `cargo test --workspace --no-fail-fast --locked -- --nocapture` **4195 / 0 / 60**, **177** suites, `TEST_EXIT=0`, **4 `[SKIP]`** (all the gliner tier, held) and **0 `[WARN]`**. **The delta reconciles exactly: +6** over the 4189 below, all in `kastellan-tests-common` (Mac 275 → 281), and **ignored is unchanged at 60** — this branch adds no ```` ```ignore ```` doc fence [[ignore-fenced-doc-example-moves-ignored-count]]. **Firecracker tier, `KASTELLAN_MICROVM_REQUIRE_E2E=1`: 29 / 0** across all **14** suites, **0 `[WARN]`** and the 2 usual `KASTELLAN_MATRIX_FC_LIVE_E2E` opt-in `[SKIP]`s — run **after a plain `bash scripts/build-release.sh`**, which is the state that made the whole tier unrunnable before this branch. ⚠️ **The negative control is the point and it ran first:** at that same state the *pre-existing* images panicked `DIFFERS` (#682 reproducing on demand), then `rebuild-all-rootfs.sh` rebuilt all 8, a **second** `build-release.sh` (a simulated later deploy) left `target/release/` at `8a21877a…`, and all 8 images now bake that digest. Four more mutation controls on the Mac, each watched to fail then restored: a narrow `cargo build` re-planted beside the canonical one; the canonical producer deleted (proving **both** halves of the pin are needed); a `cargo build` hidden behind a trailing comment; and a real `install` line deleted (positive control — comment-stripping did **not** make the existing scanner vacuous). ⚠️ **Two later commits are NOT in this sweep** — 4 inserted / 8 deleted lines of *shell comments* plus docs; re-verified at the shipped commit with `cargo test -p kastellan-tests-common` on the DGX, the only crate that reads build-script text | `--workspace --all-targets --locked -D warnings` exit 0 on the DGX, linting the correct reverse-dependency set (**3**: tests-common, core, db) after `touch`ing the changed files; Mac `-p kastellan-tests-common --all-targets -D warnings` exit 0 | **4**, gliner tier. **0** `[WARN]` |
| **DGX** (#683/#679 after its review round) | **`1c17eb4d`** | **Full sweep:** `cargo test --workspace --no-fail-fast` **4189 / 0 / 60**, **177** suites, `TEST_EXIT=0`, **4 `[SKIP]`** (all the gliner tier, held) and **0 `[WARN]`**. **Both deltas reconcile exactly.** **+21 passed** over the 4168 below — the review's new tests, all in `kastellan-tests-common` (Mac 254 → 275). ⚠️ **−1 ignored (61 → 60) is a doc-test that MOVED, not a test that started running:** the source guard was split out of `require.rs` into a `#[cfg(test)] mod guard`, and rustdoc does not collect doc-tests from a `cfg(test)` module, so `EXEMPT_WINDOW`'s ```` ```ignore ```` fence stopped being counted. Same mechanism as the +4 recorded below, in reverse. [[ignore-fenced-doc-example-moves-ignored-count]] **Two live negative controls**, both re-planted and watched to fail: the wrapped `[SKIP]` in `net_demo_firecracker_egress_e2e.rs` (rule 2) and deleting the tree's only `REQUIRE-EXEMPT` marker (the exemption path) | `--workspace --all-targets --locked -D warnings` exit 0 on the DGX; Mac `-p kastellan-tests-common --all-targets -D warnings` exit 0 | **4**, gliner tier. **0** `[WARN]` |
| **DGX** Firecracker gate, `KASTELLAN_MICROVM_REQUIRE_E2E=1` | **`63c886a6`** | **12 / 0** across web-fetch (2), web-search (2), python-exec (7), kv-demo (1) — **0 `[SKIP]`, 0 `[WARN]`**, so under REQUIRE every routed precondition was actually met rather than skipped. **Live negative control, both directions**, on a precondition #679 newly routed (`egress_proxy_bin_or_reason`, one of the four private copies it retired): with the binary moved aside, **REQUIRE=1 panicked** naming the knob *and* the reason (`EXIT=101`) where before it was a silent `[SKIP]`-as-pass; **REQUIRE unset printed `[SKIP]` and passed** (`EXIT=0`), so the default is unchanged. Binary restored | — | **0** |
| **DGX** (#680 after its review round) | **`4f268c14`** | **Full sweep:** `cargo test --workspace --no-fail-fast --locked -- --nocapture` **4142 / 0 / 57**, **177** suites, `TEST_EXIT=0`, **4 `[SKIP]`** (all the gliner tier, held) and **0 `[WARN]`**. **The delta reconciles exactly: +34** over the 4108 below, all in `kastellan-tests-common` (Linux **192 → 226**). **Linux gate** (the check CI does *not* run on this branch — `linux-check` last fired on `2411d241`, so it was run by hand): `cargo check --workspace --all-targets` exit 0, `clippy --workspace --all-targets -D warnings` exit 0, `cargo test -p kastellan-tests-common` **226 / 0**. ⚠️ **226 on Linux vs 228 on the Mac, and the 2 are pre-existing** — `serial.rs` is `cfg(target_os = "macos")`; the `microvm::` test set is **85 on both**, so nothing in this change compiles out on either host. **Firecracker gate** with `KASTELLAN_MICROVM_REQUIRE_E2E=1`: **10 / 0** across kv-demo + python-exec + web-fetch, **0 `[SKIP]`, 0 `[WARN]`** — and under REQUIRE a `Fresh`-with-caveats or `Indeterminate` verdict would have **panicked**, so this is positive evidence that both baked binaries in each image were actually compared, not that the check was skipped. **Live negative control:** appending one byte to `target/release/kastellan-microvm-init` turned the suite **red** with the full operator message naming `build-kv-demo-rootfs.sh` **and** `rebuild-all-rootfs.sh`; restoring the binary (digest re-verified identical to the baked copy) turned it green again | Mac: `--workspace --all-targets -D warnings` exit 0, **zero** warnings; `cargo doc` warnings **15 → 9**, none left in `microvm/` (the rest pre-existing, tracked by [#638](https://github.com/hherb/kastellan/issues/638)) | **0** `[SKIP]`, **0** `[WARN]` |
Older rows (#675 `4075`, #669 `4049`, #663 `4040`, #656 `4009`, `4269ff7e` 3997/13, and back to 2950) are in the [`archive/`](archive/) snapshots.

⚠️ **`scheduler_ask_expiry_e2e` flakes under a full sweep, and this file's diagnosis of it was WRONG
for two gates.** It said "widen the poll deadline"; the evidence says otherwise — the panic is at
`:251`, past both `await_state`s, and the next log line is `claim_one error: … No such file or
directory (os error 2)`: **the per-test cluster's unix socket went away underneath the test**. In
isolation it runs 62 s against a 20 s + 90 s budget, so it is nowhere near a deadline, and widening
it would leave the test green while the scheduler still loses its database mid-run. It did **not**
recur in this session's sweep. [#676](https://github.com/hherb/kastellan/issues/676); likely the same
ownership problem as [#548](https://github.com/hherb/kastellan/issues/548). **The general lesson: a
flake attributed once gets re-attributed forever — re-read the actual failure text on each
recurrence.**

**Both hosts are load-bearing, in opposite directions — always check both.** The two supervisor
backends compile on one host each: a `launchd_agents.rs` change is invisible to the DGX and a
`systemd_user.rs` change is invisible to the Mac, where `cargo test` compiles **zero**
`systemd_user` tests [[mac-compiles-zero-systemd-tests]]. The mirror is just as real: Mac clippy
compiles `cfg(target_os = "linux")` items *out*, so an unused cfg-linux helper fails only the DGX
`-D dead-code` gate [[cfg-linux-e2e-deadcode-dgx-clippy]]. ⚠️ **And a whole file can be
`#![cfg(target_os = "linux")]`**, in which case the Mac compiles *nothing* in it, imports included —
which is how #679's unused import survived a clean Mac run.

**Predict the count, then reconcile the delta exactly.** Every gate above was predicted from the
diff's new `#[test]` count and investigated when it missed — the cheapest detector for "a test I
think I added is not being compiled". **Reconcile by diffing PER-SUITE counts, not test names:**
`--nocapture` interleaves output so a `test … ok` name grep loses lines, and `#[should_panic]` tests
print `- should panic ... ok`, which a bare `… ok` grep reports missing.

⚠️ **A `[SKIP]` can hide a dead fixture for months, and a `[SKIP]` line is evidence nothing may
fake.** The four gliner-relex venv-shim skips were not "this host is unstaged" — the DGX's `.venv`
was a **copy of the Mac's**, `bin/python` pointing at a path that cannot exist on Linux, and a venv
is gitignored so nothing in the repo could say so: `readlink .venv/bin/python` before believing a
skip, and prefer a `REQUIRE_*=1` knob that turns the skip into a failure. And since
`grep -c '^\[SKIP\]'` over a `--nocapture` run is how a green sweep is audited, a unit test that
printed one would inflate exactly the number it protects — every `[SKIP]` renders through the pure
[`tests_common::skip::skip_line`](../../../tests-common/src/skip.rs), so **assert on `skip_line`;
call the `skip_if_*` wrappers only from real fixtures.**

**Two standing reading rules.** A green run with `[SKIP]` lines means tests *skipped*, not that the
sandbox contained anything — re-check with `-- --nocapture`. And skip-as-pass counts as passed, so
counts stay comparable either way.

**Mac verification runs under a private `CARGO_TARGET_DIR`** (the IDE's rust-analyzer holds
`target/debug/.cargo-lock` [[mac-cargo-buildlock-prefer-dgx]]), and **it must live under `$HOME`,
not `/tmp`** — macOS scrubbed a scratchpad target dir *mid-run* once, so a test binary vanished
between build and exec (`TEST_EXIT=101`) while every `test result:` line still said `ok`
[[dgx-run-logs-tmp-scrubbed]].

### Build & test

The cargo commands and the one-time Linux host setup are in [`CLAUDE.md`](../../../CLAUDE.md)
§ Build, test, run and § Linux host setup. Two things that file does not say:

**FC e2e gotchas (DGX) — read before running any Firecracker e2e.** Build the release binaries with
**`bash scripts/build-release.sh`** — since #682 that is the *only* supported way to write
`target/release/`, because cargo unifies features per invocation and a narrow `cargo build -p …`
therefore produces different **bytes** from identical source, which the #667 freshness gate reads as
staleness. It also rebuilds the launcher, which the gate structurally cannot check (below). AND
`export PATH=$HOME/.local/bin:$PATH` (firecracker is off the non-interactive ssh PATH).
Since #667/#679, `KASTELLAN_MICROVM_REQUIRE_E2E=1`
turns **every** unmet precondition in a micro-VM suite into a panic naming itself — the absent
`firecracker`, an unavailable supervisor or sandbox, a missing Postgres/egress-proxy/broker binary,
an unreachable origin — and a stale **image** fails the run naming
`bash scripts/workers/microvm/rebuild-all-rootfs.sh`. **Use that knob whenever a Firecracker run is
meant to be *evidence*.** ⚠️ **The stale release launcher is still invisible.**
`kastellan-microvm-run` is baked into **no** rootfs image, so the freshness gate structurally cannot
see it — the trap that already cost false bug report #362; `build-release.sh` rebuilds it, which is
the other reason to use that one command rather than a narrow `-p`. `kastellan-core` won't cross-compile on
the Mac (`ring` C dep), so core e2e are compile+run on the DGX only. A VM worker's
`WorkerSpec.program` must be the **in-rootfs** `/usr/local/bin/kastellan-worker-<name>`, never the
host target-dir path [[vm-worker-in-rootfs-binary-path]]. A failed boot leaves `console.log` in the
kept run dir; `KASTELLAN_MICROVM_KEEP_RUN_DIR=1` keeps it on a successful boot.

### The tree — 27 crates

Full layout in the root [`README.md`](../../../README.md) § Layout, and the load-bearing crates in
[`CLAUDE.md`](../../../CLAUDE.md) § Project shape. Not duplicated here — it drifts, and the README is
the one a fresh reader finds first.

### Integration-suite map

Only the rows that tell you *where to look when something goes red*; the full census is in the
[`archive/`](archive/) snapshots.

| Suite | Tests | What's verified |
| ----- | ----- | --------------- |
| `sandbox` integration (`linux_smoke` / `macos_smoke` / `macos_container_smoke`) | 8 / 10 / 7+ | **real** jails: fs invisibility, net deny, relative-path reject, OOM-kill under MemoryMax, per-spawn `/tmp` tmpfs, fresh session leader — **and the Firecracker VMM jail actually launching** (#671), the one gate that catches a flag combination bwrap refuses at option-parse time |
| `core` Firecracker (14 suites, `#[ignore]`, DGX) | 29 | **real KVM**: round-trip, mem cap, net deny, host-dir share, warm idle, VMM confinement, egress + broker reverse channels, persistent store, browser-driver, matrix; W-2's in-guest privilege drop from `/proc/self/status`; `/run` mode + relay-socket reachability. Run with `KASTELLAN_MICROVM_REQUIRE_E2E=1` to make it evidence (#667, #679) |
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

- **[#685](https://github.com/hherb/kastellan/pull/685)** `0939e80c` — `target/release/` gets one
  producer, so the #667 gate stops crying wolf (#682). Scripts + tests + docs; the only Rust is
  `tests-common`. Filed #686 and #687.
- **[#683](https://github.com/hherb/kastellan/pull/683)** `ec9a2e94` — every micro-VM precondition
  answers to `KASTELLAN_MICROVM_REQUIRE_E2E` (#679), enforced by a source guard over `core/tests`;
  plus the handover repair. Tests + docs only — no production code. Filed #682 and #684.
- **[#680](https://github.com/hherb/kastellan/pull/680)** `fb560ab7` — a stale micro-VM rootfs image
  can no longer gate anything (#667).
- **[#681](https://github.com/hherb/kastellan/pull/681)** `aee2a7f0` — the Hermes Agent survey, docs
  only; four ROADMAP entries, the anchor index folded into #678 as slice (e).
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
5. Update [`ROADMAP.md`](../ROADMAP.md) in the same commit, and commit both together.

### Pruning convention

The archive snapshots are the long-form record; this file is the working brief. Compress by keeping
**what would change a decision** and dropping the narrative of how it was found — except where the
*way* it was found is itself the lesson, which is most of the ⚠️ blocks above.
