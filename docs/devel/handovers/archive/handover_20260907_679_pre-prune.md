# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20260905_669_pre-prune.md`](archive/handover_20260905_669_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here.

**Last updated:** 2026-09-07 · **`main` HEAD:** `fb560ab7` —
[#680](https://github.com/hherb/kastellan/pull/680) MERGED (#667, the rootfs freshness gate), on top
of [#681](https://github.com/hherb/kastellan/pull/681) `aee2a7f0` (the Hermes survey, docs only). ·
**OPEN BRANCH: `chore/handover-repair-679`** — see
[This session](#this-session-a-self-contradicting-handover-and-679). ·
**DGX RUNNING `fb560ab7`** — redeployed this session and level with `main`.

> ⚠️ **Two PRs each edited this file's header; the merge kept BOTH and `main` shipped a handover that
> contradicted itself.** 736 lines, two `Last updated:` blocks, three `### This session` sections, and
> the `#669` bullet list cut in half by a stale copy of an older session. Git reported no conflict
> worth a human's attention and CI has nothing to say about prose. **A long-lived branch that edits a
> rolling doc must re-read that doc on `main` before merge — the conflict is semantic, and the tools
> that guard code do not see it.** Recovering it needed the *branch* version as the base plus one
> re-added section, not a hand-merge of the shipped file.

> ⚠️ **A fixture-staleness rule proposed in an issue was measured WRONG before it was written.**
> #667 asked for mtimes; on the DGX six *correct* images were 5 hours "older" than a binary they
> contained **byte-identical** copies of, because cargo relinks unchanged output. **Measure the
> proposed rule against the real host before implementing it** — a check that cries wolf on the
> common case is a check somebody switches off. [[issue-as-filed-can-carry-a-regression]]

> ⚠️ **The lesson #669 and this session share: an error with no content is a defect *multiplier*.**
> Three independent production defects hid behind one identical `Protocol(EarlyExit)` for two days,
> not because any of them was subtle but because nothing anywhere carried a reason. Before adding a
> layer, ask what it says when it refuses.

> ⚠️ **A gate booked as "pure verification, not code" is not evidence until it has RUN.** #660's two
> gates sat in this file as bookkeeping for two days; one of them turned out to be three production
> defects and **`0 of 21`** Firecracker tests passing the whole time.

> ⚠️ **A slow Mac cargo build is CONTENTION, not the `_dyld_start` wedge.** The two are **not**
> distinguishable by `sample` alone — a thread that is never *scheduled* shows the same single
> `_dyld_start` frame, because `sample` reports where a stack is, not why it is not moving. What
> separates them is **load**: check `uptime` and the `%cpu` in `time` output first. A wedge burns no
> CPU *and never finishes*; contention burns little CPU and finishes.

---

## Current state

### This session: a self-contradicting handover, and #679

Branch `chore/handover-repair-679`. Two things, one theme — **a check that is present but does not
check**.

1. **The handover on `main` was repaired** (see the ⚠️ at the top). #680 and #681 both edited the
   header and both landed; the resolution kept both sides. Rebuilt from the *branch* version plus a
   compressed Hermes entry, not by hand-merging the shipped file.
2. **[#679](https://github.com/hherb/kastellan/issues/679) — every micro-VM precondition now
   answers to `KASTELLAN_MICROVM_REQUIRE_E2E`.**
3. **The DGX is redeployed** and no longer behind (`fb560ab7`, 15 binaries, all units active,
   Matrix channel back up). TODO 1 discharged.

⚠️ **The issue counted one syntactic shape; the property was four times larger.** Grepping for the
`||` chain it described found **7** call sites. Asking instead *"which preconditions inside a
micro-VM-gated test bypass the knob"* found **11 tests across 6 kinds** — the `||` chain, the *same
three helpers written as sequential `if`s* (which the grep cannot see), `pg_bin_dir_or_skip`,
`skip_if_origin_unreachable`, `egress_proxy_bin_or_skip`, and four hand-written
`eprintln!("[SKIP] …")` broker-binary checks. One whole file
(`web_research_firecracker_egress_e2e.rs`) was not in the issue at all. **When an issue hands you a
grep, re-derive the property before trusting the census.**
[[mutation-proof-counts-only-mutants-you-tried]]

- **What shipped is a vocabulary, not 11 fixes.** `microvm::skip_unless_ready(&[Probe])` for a
  `bool` precondition, `microvm::dep_or_skip(Result<T, String>)` for one that yields a value, and
  `microvm::host_probes()` for the supervisor+sandbox pair all 11 sites ask for. Both combinators
  route through `report_unmet_microvm`, so a `[SKIP]` and a REQUIRE panic cannot disagree about what
  was unmet. Three `*_or_reason` siblings the #653 split had never grown —
  `origin_unreachable_reason`, `egress_proxy_bin_or_reason`, `workspace_binary_or_reason` — and the
  last of those retired **four byte-identical private copies** of `egress_proxy_bin_or_skip` that
  had grown inside the suites, whose bare `eprintln!` rendered *differently* from `skip_line` and so
  was weaker against `grep -c '^\[SKIP\]'`.
- ⚠️ **The load-bearing test is a source scanner, and that is the point.** #680's review found the
  *last* knob's wiring could be replaced with `false` and the suite stayed green; this defect is one
  step further out again — the knob works perfectly and the call site asks something else first. **No
  unit test can see that, and no Firecracker run can either:** the false green appears only on a host
  where the micro-VM preconditions are MET and a neighbouring one is not, which is by definition not
  the host anybody gates on. So `bypassed_gates()` is a pure scanner over the real `core/tests`
  sources, run by `call_site_tests` — it catches call sites nobody has written yet, and **runs on
  both hosts**, which matters because every file it scans is `#![cfg(target_os = "linux")]` and thus
  invisible to a Mac `cargo test`. Its file discovery is asserted against a known-minimum roster
  *first*, or a glob matching nothing would make every assertion under it vacuous.
  [[fail-safe-parsers-make-vacuous-fixtures]]
- **One exemption, inline and reasoned.** `matrix_firecracker_live_e2e`'s `GATE` is that tier's
  **opt-in**, not a host precondition — demanding a micro-VM run is not demanding a live homeserver
  round-trip with credentials only the operator has, the same category as `#[ignore]`. Everything
  *below* that check in the same function is a precondition on a run that **was** asked for, so all
  three now route through the knob. `EXEMPT_WINDOW` is **2** because the idiomatic marker placement
  is above the guarding `if`, which puts one line in between — a window of 1 rejected the only real
  exemption in the tree. Measured, then documented.
- ⚠️ **The Mac cannot see any of this code.** Every converted file is `#![cfg(target_os = "linux")]`,
  so a `#![cfg]`d-out file compiles to nothing — imports included. The Linux leg caught an unused
  import the rewrite left behind that no Mac gate could ever have reported. The mirror of
  [[mac-compiles-zero-systemd-tests]].

### #680 (`fb560ab7`) — a stale rootfs image can no longer gate anything (#667)

Every image bakes its **own** copy of `kastellan-microvm-init`
and of its worker, so a guest-side change was invisible to the Firecracker e2es until that image was
rebuilt — the whole W-2 gate could have run green having tested none of it. The check now lives at
the one chokepoint every FC e2e already funnels through (`skip_if_no_microvm`).

- ⚠️ **The issue asked for mtimes, and mtimes are WRONG here — measured, not deduced.** On the DGX
  `target/release/kastellan-microvm-init` was **19:08** while six images were **14:26–14:28**, and
  the init inside every one of them was **byte-identical** (sha256). Cargo had relinked an unchanged
  binary. An mtime rule would have refused six correct images on the one host it exists for, and a
  check that cries wolf on the common case is a check somebody switches off. **The reference is the
  sha256 of the baked copy**, read back with `debugfs -R "cat …"` — no mount, no loop device, no
  root, one read of the file against a gigabyte image. That is also strictly *stronger* than what
  the issue asked for: it catches an image built from a stale checkout, which mtime cannot.
  [[issue-as-filed-can-carry-a-regression]]
- **Four verdicts, four treatments, and the asymmetry is the design.** `Stale` and `Unusable`
  **panic unconditionally** — neither is a precondition an operator may reasonably lack, both are
  positive evidence the run would prove nothing, and a `[SKIP]` there is #667 wearing a different
  hat. `Fresh`-with-caveats and `Indeterminate` **`[WARN]` and still run** — absence of a comparable
  digest is not evidence of staleness — and both carry *which* binary and *why*, because "build the
  binary", "install e2fsprogs" and "your image is corrupt" are three different remedies.
- **`KASTELLAN_MICROVM_REQUIRE_E2E`** (the #653 convention, same `env_flag_enabled` dialect) also
  covers the *other* documented false green on this path: `firecracker` off the non-interactive ssh
  `PATH` made the whole suite skip-as-pass. ⚠️ It does **not** yet cover the `skip_if_no_supervisor()`
  / `skip_if_sandbox_unavailable()` helpers OR-ed beside it in 7 suites —
  [#679](https://github.com/hherb/kastellan/issues/679), filed with the call sites.
- **The registry now records where each binary lands *inside* the image**, since the init is renamed
  to `/sbin/init` and neither field derives from the other. Both halves are pinned against the
  scripts bidirectionally: a wrong in-image path yields `Indeterminate` **forever** — a check that
  silently stops checking, which is #667 with extra steps.
- **`scripts/workers/microvm/rebuild-all-rootfs.sh`** — the build scripts live in **two**
  directories, and every staleness message now names one command instead of asking the reader to
  assemble eight paths.

**The review round found the gate had two silent holes and one untested half.** All fixed before
merge; the lesson is worth more than the diff:

- ⚠️ **A verdict that certifies on PARTIAL evidence is the original bug with better manners.**
  `Fresh` returned as soon as **one** binary compared equal. Seven of the eight images bake an init
  *and* a worker — the init is stable, the worker is exactly what goes stale — so a matching init
  silently certified a June worker, and a unit test *pinned that as intended*. `Fresh` now carries
  what it could not check and a non-empty list is a `[WARN]`. **When a check aggregates, ask what it
  says when only some of its inputs are available.**
- ⚠️ **Measured on the DGX: every `debugfs` failure exits 0.** A missing path, an image that is not
  ext4, an unopenable image, a symlink — all rc=0, all empty output, indistinguishable from a
  missing `debugfs`. All four rendered as "install e2fsprogs" on a host that has it, and the first
  three then **booted the VM anyway**. The two benign causes are now separated *structurally*
  (`ErrorKind::NotFound` on spawn), never by matching on wording, and an image a working reader
  cannot read is `Unusable` — it panics. Switching `dump <path> <outfile>` → `cat` (stdout) removed
  a temp file, a `$TMPDIR`-with-a-space bug, and the cleanup path along with it.
- ⚠️ **"Mutation-proved 7 for 7" measured only the pure half.** Seven simultaneous mutations to the
  impure half left the suite green, and the wiring in `skip_if_no_microvm` could be replaced with
  `false` — #667 fully restored — with nothing failing on Linux and the mutation *not even
  attemptable* on the Mac, because `cfg(linux)` compiles it out. The ordering now lives in a pure
  `preflight()` over injected closures, and `debugfs_argv` / `release_binary_path` /
  `debugfs_complaint` are named seams. [[mutation-proof-counts-only-mutants-you-tried]]
- **Prose accurate at commit *N* shipped stale at commit *N+3*.** Four "movement-only" / "this
  replaces two copies" / "no longer a trap" sentences were true when written and false at the tip of
  the same branch. **Re-read a branch's self-description against `git diff origin/main...HEAD`, not
  against the commit that wrote it.**

### #675 — the micro-VM path can now say why it failed, MERGED (`f831b3d1`)

Four issues (#666, #670, #671, #672), each **proved to fail against un-hardened code**. What still
binds:

- **A failed boot now leaves `console.log` in the kept run dir** and the launcher echoes a redacted
  tail to its own stderr — **read that before theorising**. The kernel prints its command line at
  every boot and that line carries `kastellan.env=<hex>`, the worker's whole environment, so the
  **value** is redacted and the **key** kept: its absence stays a signal.
- **`EarlyExit` carries the worker's last words**, logged at **warn**. Promoting untrusted worker
  bytes to an operator's terminal forced #544's ANSI-neutralising class out of
  `prompt_assembly::assemble` into the shared `core/src/untrusted_text.rs`. ⚠️ **Its first version
  broke what it protected:** `\n` is *in* that class, so neutralising the raw chunk replaced every
  newline and `drain_reader`'s line split never fired again — one line, forever, silently. **A
  predicate correct for one renderer can be destructive in another; the shared class is right, the
  shared application point is not.**
- ⚠️ **`bwrap --clearenv` means the launcher has NO environment**, so an env-var knob there is
  silently inert; every launcher setting travels by **argv**. [[microvm-launcher-knobs-must-be-argv]]
- ⚠️ **The release profile is `panic = "abort"`**, so a failure path relying on an RAII scopeguard
  runs **no destructor** — firecracker was left holding KVM and the vsock device. **Check the
  profile before trusting RAII on a failure path.** [[release-profile-panic-abort-kills-raii]]
- **The VMM jail has a real-bwrap gate** (#671, in `linux_smoke.rs`, **not** `#[ignore]`d): it runs
  `/bin/true` under the production argv and asserts **exit status**, because no content assertion
  can catch a flag *combination* bwrap refuses at option-parse time. Guest `/run` is mounted
  `mode=0755` (#672 — a tmpfs with no `mode=` comes up **1777**), and a failed relay-socket chown is
  now fatal (#670).
### Merged arcs — only what still binds

Full prose in the [`archive/`](archive/) snapshots, one line each in the ROADMAP, and the
2026-09-02 audit in [`docs/security-audit-2026-09-02.md`](../../security-audit-2026-09-02.md).

**#681 (`aee2a7f0`) — the Hermes Agent survey, docs only.** Full note:
[`notes/2026-09-06-hermes-agent-survey.md`](../notes/2026-09-06-hermes-agent-survey.md)
([`NousResearch/hermes-agent`](https://github.com/NousResearch/hermes-agent) @ `9a84bee2`, **MIT** —
we take shapes, not code). Four ROADMAP entries came out of it and **the ordering is the finding**:
the **anchor index** (an LLM-free regex harvest of exact identifiers rendered beside a summary) went
into [#678](https://github.com/hherb/kastellan/issues/678) as slice **(e)**, because `handoff`
already pages by byte *offset* and an offset is unguessable — the recovery path exists and is
unusable; then a per-dispatch loop guardrail for
[#677](https://github.com/hherb/kastellan/issues/677), then a **planner A/B battery**, then skill
lifecycle **last**, because their six eval suites measure context economics and **not one** measures
whether a skill created from experience makes the next task go better.

- **The number worth remembering:** a lean tail plus one recovery round-trip scored **68.3 % recall
  on 49 K retained tokens** against **45.8 % on 162 K** for the fat verbatim tail it replaced, with
  the needle half attributed to the mechanical index. **A big verbatim tail is not the safe choice;
  it is the expensive one that also loses the needles.**
- ⚠️ **The one idea that must not be taken as offered.** Their `execute_code` opens an RPC socket
  from agent-authored Python back into the tool dispatcher — their largest single token saving, and
  for us it would turn one compromised worker into every worker. §3.6 records the only shape that
  keeps the invariant, and notes `web.search_batch` already buys most of it with no new channel.

**#669 (`4955a52c`) — the Firecracker gate.** The micro-VM backend had been **entirely dead** since
the audit merged, at **0 of 21**, in three ways each masking the next.

- **Count the producers, and make the const the only spelling.** `build_vmm_jail_argv` was the
  **third** bwrap argv producer and #661's fix missed it. #671's gate is what catches the class now.
- **The pinned guest kernel has no Landlock** (read out of the pinned `vmlinux`'s embedded IKCONFIG
  **without booting anything** [[dgx-guest-kernel-config-inspection]]), so the launch plan states
  `KASTELLAN_LANDLOCK_PROFILE=none` as a **default that never overrides a caller**. Seccomp is
  unaffected; repinning is [#668](https://github.com/hherb/kastellan/issues/668).
- **W-2 is proved from inside the guest**, including the **saved-set uid** and **`Seccomp: 2`**. ⚠️
  The `groups` assertion is a regression guard, **not** a proof — guest PID 1 has no supplementary
  groups either way.
- **A non-hex `kastellan.mounts=` fixture fails OPEN, silently**, so a loop over it asserts nothing;
  all five `worker_owned_paths` tests are exact-set `assert_eq!`s.
  [[fail-safe-parsers-make-vacuous-fixtures]]
- **`/run` is out of the chown set** and re-adding it would be a regression: chowning a *sticky*
  directory is what lets the owner unlink entries it does not own.

**#660 (`62d98a00`) — the second pre-release security audit.** 29 fixes, 80 files. **All owed gates
now discharged.**

- **The four load-bearing fixes.** (H1) the dispatch chokepoint scrubs every redeemed secret out of
  the worker's `Ok` value **and** its `RpcError`. (H2) agent-raised `l1_insight`s are screened at
  promotion and at prompt assembly. (H3) every per-spawn `/tmp` dir is minted with
  `create_private_dir` (exclusive `mkdir` 0700, owner-verified) and secret files with `O_EXCL` 0600
  — **a pre-planted name from another uid FAILS THE SPAWN CLOSED; that is the contract, do not
  "fix" it back to `create_dir_all`**. (H4) seccomp admits `clone` only without `CLONE_NEW*`.
- **Three lockdown behaviours are FAIL-CLOSED and will bite a careless fixture:** a missing
  `KASTELLAN_SECCOMP_PROFILE` is an error (`none` is the explicit opt-out), an unenforceable Landlock
  ruleset is an error, and a corrupt `kastellan.env=` guest token refuses the VM boot.
- **Every networked stdio worker builds its handler INSIDE `serve_stdio_with`** — Landlock is
  per-thread, and a runtime built in `from_env()` ran unrestricted on the threads that parse the
  network. **Keep that order for any new worker.**
- **CodeQL reads NAMES** [[codeql-flags-sanitisers-by-name]]. And the **live-matrix clippy job**
  catches lints the default-feature workspace clippy never compiles: run
  `cargo clippy -p kastellan-worker-matrix --all-targets --features live-matrix --locked -- -D
  warnings` before pushing anything touching `sdk_live.rs`.
- **Its two real-bwrap defects:** [#661](https://github.com/hherb/kastellan/issues/661) (a bare
  `--disable-userns` beside `--unshare-all` is refused at bwrap's *option-parse* time, so no skip
  guard can see it — 66 failures across 23 suites) and
  [#662](https://github.com/hherb/kastellan/issues/662) (**any `pre_exec` closure forces Rust std off
  `posix_spawn` onto its fork path, which opens a `socketpair` the `strict` profile pins out**).
  ⚠️ **A core e2e does not rebuild a worker package.** [[core-e2e-does-not-rebuild-worker-binaries]]
- **Deferred with a reason** (all in the audit doc): brokers not force-routed; the guard tier never
  sees bytes past 64 KiB; `secret://` refs not tool-bound; `Host:` ≠ CONNECT authority; no
  email-replay freshness window; macOS worker-side caps. **Recommendation before release: flip
  force-routing to default-on.**

**#650 (`c03ec1a3`) — the interpreter alias bind.** `uv` lays a managed CPython out with a
minor-version **symlink alias** and the venv's `bin/python` names the **alias**, so canonicalizing
made `execve` return **ENOENT for a file that is present and readable**. **The admission rule is
non-widening and that is the load-bearing choice:** an alias binds only when it canonicalizes to the
canonical prefix — **a containment fix must not widen containment**. ⚠️ `Path::components()` strips
**interior** `.` only [[rust-path-components-normalizes-dot]]. Open: #657, #658, #659.

**#653 / #654 (`9ace57ad`) — the gliner-relex require knob.** **The reusable pattern is the
`*_or_reason` sibling:** return the reason **without rendering a verdict**, so one caller can skip
where another must fail — #667 above is its second consumer. #654 was a real operator-facing skew:
fixtures gated on `!= Some("1")` while production reads `env_flag_enabled`. Open: #664, #665.

**#649 / #651 (`ef8144f8`) — the transformers advisory. The remedy an advisory states can be a no-op
that exits 0:** `uv lock --upgrade-package transformers` reached **5.6.2, still inside the vulnerable
range**, because `gliner 0.2.27` capped it. Both floors moved in **`pyproject.toml`**; the
`python-lock-check` CI job catches a **weakened floor**, not an advisory.
[[uv-lock-upgrade-can-land-still-vulnerable]]


### The guard tier — what still binds

- **D10 — the tier is ADVISORY defence-in-depth, NOT a gate.** 65% recall (36/55) at FP-0; 6/6 on
  bare imperatives but **5/8 missed** on narrative framing. **Nothing downstream may relax on it.**
- **τ = 0.79552656 is a REQUIRED operator input with no default**, and **five misconfigurations STOP
  THE DAEMON** (D6): half-configured keys, τ outside `(0.0, 1.0]`, a pinned timeout of 0, an
  unreachable `/props`, and a context below `SCAN_BYTE_CAP + 512 = 66 048` (D8).
  `KASTELLAN_REQUIRE_GUARD=1` makes the *unconfigured* case fatal too.
- **`best_tau` returns NONE** — real captured content overlaps at every threshold, and that stratum
  was **catalogue-selected**, which is why **corpus growth from production is the cheap path**.
  Harvest it before designing another campaign.
- **`AuditSink::insert` is a provided method applying `truncate_payload` before delegating to
  `insert_stored`**, so no sink double can record a payload Postgres never stored
  [[audit-sink-doubles-hide-storage-transforms]]. **Absence and loss must not render identically.**
- **The stated mitigation for an issue can disarm the instrument built to check it** — the live probe
  passed having measured nothing under a *pinned* timeout, precisely what #612 tells a Metal operator
  to use. It now refuses a pin outright.
- **#624's thesis, proved on the host it was filed about:** one post-arc boot spread **4 765.7**
  against **1 450.4** tok/s — **3.29x inside a single boot** — making the derived timeout **3.4x too
  generous**. **`TimeoutBasis::Saturated` does NOT mean every sample stalled.**
- ⚠️ **#624 and #626 do NOT close [#612](https://github.com/hherb/kastellan/issues/612).** #624
  removed the *contention* error; #612 is that extrapolating from a ~1 KiB sample is non-linear **on
  Metal whatever the load** [[metal-prompt-processing-is-nonlinear]].

### Standing hazards that have each cost a session

Most are also memory notes (auto-loaded); kept here because they change the *first* move.

> ⚠️ **Clippy parity is a `rustup update`, not a property of the hosts.** CI pins nothing
> (`dtolnay/rust-toolchain@stable`) and both dev hosts float, so they drift silently. `rustc
> --version` on the host you are gating on. **2026-09-06: DGX on 1.98.0.**
> [[local-clippy-not-ci-parity-rust-version]]

> ⚠️ **A cached `cargo clippy` reports a full-workspace pass it never ran.** Exit code alone does not
> distinguish it — **count the `Checking` lines** against the *reverse-dependency set* of your
> change. Cold is ~217–303; a warm dir can report exit 0 having linted 4. And
> `cargo check`/`clippy --all-targets` do **not** warm the target dir for `cargo test` (metadata
> only, no linked binaries) — **run the sweep first, lint after.**

> ⚠️ **A private `CARGO_TARGET_DIR` does not build `examples/`,** so `email_channel_e2e`'s 6 tests
> fail with `fixture not built` at a perfectly green commit
> [[custom-cargo-target-dir-breaks-daemon-e2e]]. Read the failure text before believing a regression.

> ⚠️ **"Filed, not fixed: #N" in a PR body or commit message CLOSES #N.** GitHub matches the
> `fixed: #N` substring and has no notion of negation; it has cost three issues. Write **"deferred to
> #N"**, and before merging run
> `gh pr view <n> --json body --jq .body | grep -oiE '(close[sd]?|fix(e[sd])?|resolve[sd]?)[[:space:]:,]+#[0-9]+'`
> over the body *and* the commit message. [[pr-body-not-fixed-autocloses-issue]]

> ⚠️ **Squash-merge caveat:** every PR lands as one squash commit, so its *branch-tip* SHA (where the
> gate ran) is **not** an ancestor of `main`. Check content, not `merge-base`.

> ⚠️ **Freshly-linked executables can hang forever in `_dyld_start` on macOS**, so every daemon e2e
> fails with the daemon's stdout **and** stderr **completely empty** — which reads exactly like a
> code defect. **Newness, not size**, and `sample` alone does not prove it: check `uptime` and `%cpu`
> first, because contention looks identical. [[mac-fresh-large-binaries-hang-in-dyld]]

> ⚠️ **`kastellan-worker-egress-proxy` leaks on the Mac.** Five orphans in one sweep, four of them
> 1–7 days old, across three target dirs. Not investigated — flagged for whoever next touches
> sidecar lifecycle.

> ⚠️ **A `pgrep -f '<cmd>'` wait loop matches itself** and never exits, because the Bash tool's
> `zsh -c` wrapper puts the pattern in the waiter's own argv. Use `pgrep -x`, a log sentinel, or a
> background task. [[pgrep-wait-loops-match-themselves]]


## Read these first

1. [`docs/architecture.md`](../../architecture.md) — process model, cross-platform table
2. [`docs/threat-model.md`](../../threat-model.md) — the invariant, scenarios in scope, defence layers
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

**THEN, on the micro-VM path:** [#668](https://github.com/hherb/kastellan/issues/668) (repin a guest
kernel with Landlock), the standing posture item, which needs a kernel build rather than a code
change.

**A standing architecture item, and the frame for several open issues:**
[#678](https://github.com/hherb/kastellan/issues/678) — **retire truncation as the answer to "bigger
than the budget".** The key move: truncation does **three different jobs** and only one becomes
map-reduce — a *control that stops seeing its evidence* (the guard's 64 KiB `SCAN_BYTE_CAP`; the
reduce `p = max(p_i)` is strictly more sensitive than today), a *record that must be faithful*
(`truncate_payload` — **spill, never summarise: an audit row is testimony**), and a *resource guard*
(`MAX_RECORD_BYTES` — **these stay**, containment against a compromised worker). `core/src/handoff.rs`
already stashes oversized results **whole**, so only the reduce is missing. Likely subsumes
[#604](https://github.com/hherb/kastellan/issues/604) and
[#612](https://github.com/hherb/kastellan/issues/612) by removing their premise. ⚠️ **The polarity
inverts to fail-closed** — today a document past the cap is silently unscreened — which needs its own
test.

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
verifiable either side (this session's `tests-common/src/microvm/` split is the worked example).
Best first picks, each a pure test-lift: `core/src/channel/ask_message.rs` **956**,
`workers/mail/src/handler.rs` **670**, `sandbox/src/linux_firecracker/plan.rs` ~**1160**
(`cfg(linux)`, DGX-gated), `core/tests/guard_tier_e2e.rs` **1558**
([#639](https://github.com/hherb/kastellan/issues/639)). Clean seam visible:
`core/src/scheduler/asks.rs` **801**. Judgement first, not movement: `db/src/asks.rs` **1127**,
`db/graph.rs` **926**, `llm-router/src/config.rs` **843** — a small `mod tests` there means a split
is a production reorganisation. Also over cap, no seam called yet:
`core/src/scheduler/inner_loop.rs`, `core/src/channel/bus.rs`, `workers/matrix/src/sdk_live.rs`,
`llm-router/src/messages.rs`, `core/src/main.rs`.

**Standing deferrals (no owner; pick up when a consumer appears)** — listed only so nobody
re-derives them: egress [#242](https://github.com/hherb/kastellan/issues/242),
[#251](https://github.com/hherb/kastellan/issues/251),
[#304](https://github.com/hherb/kastellan/issues/304) (needs a controllable TLS origin),
[#260](https://github.com/hherb/kastellan/issues/260); micro-VM
[#381](https://github.com/hherb/kastellan/issues/381) and **true `jailer`** (a privileged-tier
`VmmConfinement::Jailer` sibling whose seam already exists in `confine.rs`); python-exec Phase 4
(curated-wheels RO dir — stdlib-only today, flipped by `KASTELLAN_PYTHON_EXEC_ENABLE=1`);
web-research polish, all opus-triaged DEFER; and an ANN index on `entities.embedding` once
cardinality warrants it.

**Generalizing net-worker-in-VM needs no new work** — 5c's `NetClientTransport` /
`spawn_net_transport` IS the reusable mechanism; a second consumer can adopt it directly.

---

## Load-bearing findings that still bind

- **The four faults (2026-08-02).** One real Matrix message, **four independent faults, only one a
  kastellan bug in the layer everyone suspected**, each masking the next. The durable lesson is the
  shape: a green stack with a silent output means look at every layer, and fix them one at a time so
  each fix's evidence is separable.
- **Egress / MITM traps — read before touching the proxy.** The proxy's MITM upstream trusts
  **webpki roots only**, so no hermetic self-signed origin is possible for a MITM'd worker's e2e;
  `extra_ca` is worker-side [[egress-proxy-upstream-trusts-webpki-only]]. A force-routed loopback
  endpoint needs an **IP SAN** [[macos-force-routed-loopback-needs-ip-san]]. A bare-host
  `Net::Allowlist` entry with no `:port` is an **all-port grant**
  [[bare-host-net-allowlist-is-all-port-grant]].
- **Process lessons that have each cost a re-run.** A truncated gate log is not a gate — keep the
  full sweep in a file under `$HOME` and parse `test result:` with a regex
  [[truncated-gate-log-is-not-a-gate]]. Mutation testing contaminates the git **index**; `git diff
  --stat` afterwards is the only proof index == tree [[mutation-testing-contaminates-the-index]], and
  revert by copying the file, never `git checkout` [[mutation-revert-never-git-checkout]]. Plan text
  is a defect source: subagents transcribe brief prose verbatim [[plan-text-is-a-defect-source]].
  A mutation proof counts only the mutants you tried; draw the inventory from the **changed**
  functions, not the tested ones [[mutation-proof-counts-only-mutants-you-tried]].
- **`sqlx::migrate!` embeds at compile time** — a new `db/migrations/*.sql` silently does not apply
  until `kastellan-db` is rebuilt (`touch db/src/lib.rs`) [[sqlx-migrate-embeds-at-compile-time]].

---

## Working state

### Test baseline (authoritative)

| Host | Commit | Result | clippy `-D warnings` | `[SKIP]` |
| --- | --- | --- | --- | --- |
| **DGX** (this branch after the #680 review round — **the gate that stands**) | **`4f268c14`** | **Full sweep:** `cargo test --workspace --no-fail-fast --locked -- --nocapture` **4142 / 0 / 57**, **177** suites, `TEST_EXIT=0`, **4 `[SKIP]`** (all the gliner tier, held) and **0 `[WARN]`**. **The delta reconciles exactly: +34** over the 4108 below, all in `kastellan-tests-common` (Linux **192 → 226**). **Linux gate** (the check CI does *not* run on this branch — `linux-check` last fired on `2411d241`, so it was run by hand): `cargo check --workspace --all-targets` exit 0, `clippy --workspace --all-targets -D warnings` exit 0, `cargo test -p kastellan-tests-common` **226 / 0**. ⚠️ **226 on Linux vs 228 on the Mac, and the 2 are pre-existing** — `serial.rs` is `cfg(target_os = "macos")`; the `microvm::` test set is **85 on both**, so nothing in this change compiles out on either host. **Firecracker gate** with `KASTELLAN_MICROVM_REQUIRE_E2E=1`: **10 / 0** across kv-demo + python-exec + web-fetch, **0 `[SKIP]`, 0 `[WARN]`** — and under REQUIRE a `Fresh`-with-caveats or `Indeterminate` verdict would have **panicked**, so this is positive evidence that both baked binaries in each image were actually compared, not that the check was skipped. **Live negative control:** appending one byte to `target/release/kastellan-microvm-init` turned the suite **red** with the full operator message naming `build-kv-demo-rootfs.sh` **and** `rebuild-all-rootfs.sh`; restoring the binary (digest re-verified identical to the baked copy) turned it green again | Mac: `--workspace --all-targets -D warnings` exit 0, **zero** warnings; `cargo doc` warnings **15 → 9**, none left in `microvm/` (the rest pre-existing, tracked by [#638](https://github.com/hherb/kastellan/issues/638)) | **0** `[SKIP]`, **0** `[WARN]` |
| **DGX** (this branch — **the gate that stands**) | **`685c9ba3`** | `cargo test --workspace --no-fail-fast --locked -- --nocapture` **4108 / 0 / 57**, **177** suites, `TEST_EXIT=0`. **The delta reconciles exactly: +33** over the 4075 below, all in `kastellan-tests-common` (161 → 194 lib tests: 13 freshness + 8 registry + 11 preflight + 2 `skip::warn_line`, less the 1 net of the movement-only split). **Firecracker: 29 / 0** across **14** suites (browser-driver 3, egress-channel 2, vmm-confinement 1, kv-demo 1, matrix 2, net-demo 1, python-exec 7 + hostdir 1 + warm-idle 4, web-fetch 2, web-research broker 1 / egress 1 / force-route 1, web-search 2) run with `KASTELLAN_MICROVM_REQUIRE_E2E=1` — **the first FC gate that DEMANDS a real run** — with **0 `[WARN]`**, **0** stale images and the 2 usual `KASTELLAN_MATRIX_FC_LIVE_E2E` opt-in `[SKIP]`s. ⚠️ **29/14, not the 28/13 recorded for #675** — that row enumerated one suite fewer (`web_research_vm_force_route_daemon_e2e`); the count is a suite-list difference, not a new test | `-p kastellan-tests-common --all-targets --locked -D warnings` exit 0 (Mac). ⚠️ Caught a `doc_lazy_continuation` on an orphaned doc block a text edit left behind | **4**, all the gliner tier — held. **0** `[WARN]` |
| **DGX** (#675, the micro-VM diagnostics cluster) | **`520f278d`** / tip **`a93fe60f`** | **4075 / 0 / 57**, 177 suites. **Firecracker: 28 / 0** across 13 suites, all 8 rootfs images rebuilt first | `--workspace --all-targets --locked -D warnings` exit 0 | **4** |
| **DGX** (#669 after its review round) | **`e35c3571`** | **4049 / 0 / 56**, 176 suites. **Firecracker: 21 / 0**, the first fully green run | exit 0 after force-touching core + sandbox. ⚠️ The first workspace clippy exited 0 having emitted **24** `Checking` lines in 6s — warm, not a gate | **4** |
Older rows (#663 `4040`, #656 `4009`, `4269ff7e` 3997/13, and back to 2950) are in the
[`archive/`](archive/) snapshots.

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
`systemd_user.rs` change is invisible to the Mac. `cargo test` on the Mac compiles **zero**
`systemd_user` tests, so a Mac-green run can be missing the test that pins a Linux fix entirely.
The mirror direction is just as real: Mac clippy compiles `cfg(target_os = "linux")` items out, so
an unused cfg-linux helper fails only the DGX `-D dead-code` gate.
[[cfg-linux-e2e-deadcode-dgx-clippy]] **This branch hit both again** — `worker_owned_paths` lives in
the cross-platform `cmdline` module but is called only from the Linux guest, so it needs the
`dead_code` allowance the Mac would otherwise fail on.

**Predict the count, then reconcile the delta exactly.** Every gate above was predicted from the
diff's new `#[test]` count and investigated when it missed — the cheapest available detector for "a
test I think I added is not being compiled". **Reconcile by diffing PER-SUITE counts, not test
names:** `--nocapture` interleaves output, so a `test … ok` name grep loses lines, and
`#[should_panic]` tests print `- should panic ... ok`, which a bare `… ok` grep reports missing.

⚠️ **A `[SKIP]` can hide a dead fixture for months.** The four gliner-relex venv-shim skips were not
"this host is unstaged" — the DGX's `.venv` was a **copy of the Mac's**, its `bin/python` pointing at
a path that cannot exist on Linux, and a venv is gitignored so nothing in the repo could tell you.
`readlink .venv/bin/python` before believing a skip, and prefer a `REQUIRE_*=1` knob that turns the
skip into a failure wherever one can be added.

⚠️ **A `[SKIP]` line is evidence, so nothing may fake one.** `grep -c '^\[SKIP\]'` over a
`--nocapture` run is how a green sweep is audited. A unit test that prints one inflates exactly the
number it protects. Every `[SKIP]` renders through the pure
[`tests_common::skip::skip_line`](../../../tests-common/src/skip.rs), so a test can pin the wording
**without emitting a line**. Assert on `skip_line`; call the `skip_if_*` wrappers only from real
fixtures.

**Two standing reading rules.** A green run with `[SKIP]` lines means tests *skipped*, not that the
sandbox contained anything — always re-check with `-- --nocapture`. And skip-as-pass counts as
passed, so counts stay comparable with or without `--nocapture`.

**Mac verification runs under a private `CARGO_TARGET_DIR`** (the IDE's rust-analyzer holds
`target/debug/.cargo-lock` — [[mac-cargo-buildlock-prefer-dgx]]), and **it must live under `$HOME`,
not `/tmp`**: macOS scrubbed a scratchpad target dir *mid-run* once, so a test binary vanished
between build and exec (`TEST_EXIT=101`) while every `test result:` line still said `ok`
[[dgx-run-logs-tmp-scrubbed]].

### Build & test

The cargo commands and the required one-time Linux host setup are in
[`CLAUDE.md`](../../../CLAUDE.md) § Build, test, run and § Linux host setup — not duplicated here.
Two things that file does not say:

**FC e2e gotchas (DGX) — read before running any Firecracker e2e:** rebuild the **release** launcher
(`cargo build --release -p kastellan-microvm-run`) AND `export PATH=$HOME/.local/bin:$PATH`
(firecracker is off the non-interactive ssh PATH). Since #667 the **second** is no longer silent —
`KASTELLAN_MICROVM_REQUIRE_E2E=1` turns every micro-VM **preflight** skip, the absent-firecracker
one included, into a panic naming itself — and a stale **image** now **fails** the run naming
`bash scripts/workers/microvm/rebuild-all-rootfs.sh`. ⚠️ **The stale release launcher is still
invisible: rebuild it by hand.** `kastellan-microvm-run` is baked into **no** rootfs image (checked:
zero hits across all eight build scripts), so the freshness gate structurally cannot see it — that
is the trap that already cost false bug report #362. ⚠️ "Every preflight skip" is also not every
skip: [#679](https://github.com/hherb/kastellan/issues/679) records the `skip_if_no_supervisor()` /
`skip_if_sandbox_unavailable()` helpers OR-ed beside it in 7 suites, which the knob does not reach.
**Use that knob whenever a Firecracker run is meant to be *evidence*.** `kastellan-core` won't
cross-compile on the Mac (`ring` C dep), so core e2e are compile+run on the DGX only. A VM worker's
`WorkerSpec.program` must be the **in-rootfs** `/usr/local/bin/kastellan-worker-<name>`, never the
host target-dir path [[vm-worker-in-rootfs-binary-path]]. Since #666, a failed boot leaves
`console.log` in the kept run dir and the launcher echoes a redacted tail to its own stderr —
**read that before theorising**; `KASTELLAN_MICROVM_KEEP_RUN_DIR=1` keeps it on a successful boot.


### The tree — 27 crates

Full layout in the root [`README.md`](../../../README.md) § Layout, and the load-bearing crates in
[`CLAUDE.md`](../../../CLAUDE.md) § Project shape. Not duplicated here — it drifts, and the README is
the one a fresh reader finds first.

### Integration-suite map

Only the rows that tell you *where to look when something goes red*; the full census is in the
[`archive/`](archive/) snapshots — a number that drifts, not a fact that binds.

| Suite | Tests | What's verified |
| ----- | ----- | --------------- |
| `sandbox` integration (`linux_smoke` / `macos_smoke` / `macos_container_smoke`) | 8 / 10 / 7+ | **real** jails: fs invisibility, net deny, relative-path reject, OOM-kill under MemoryMax, per-spawn `/tmp` tmpfs, fresh session leader — **and the Firecracker VMM jail actually launching** (#671), the one gate that catches a flag combination bwrap refuses at option-parse time |
| `core` Firecracker (14 suites, `#[ignore]`, DGX) | 29 | **real KVM**: round-trip, mem cap, net deny, host-dir share, warm idle, VMM confinement, egress + broker reverse channels, persistent store, browser-driver, matrix; W-2's in-guest privilege drop read from `/proc/self/status`; `/run` mode + relay-socket reachability from inside the guest. Run with `KASTELLAN_MICROVM_REQUIRE_E2E=1` to make it evidence (#667) |
| `core` (`shell_exec_e2e`, `python_exec_e2e`, `python_exec_container_e2e`) | 4 / 4 / 4 | **real** core→sandbox→worker round-trips under production policy; jail-contained socket attempt; per-spawn scratch; secret-scrub to `[redacted:]` |
| `core` (`egress_proxy_e2e`, `egress_force_routing_e2e`, `email_mitm_e2e`) | 3 / 4 / 2 | **real** sandboxed sidecar + CONNECT client; Linux-only no-direct-route; a hermetic MITM asserting the round-tripped event plus `tls_intercepted:true` |
| `core` (`injection_guard_e2e`, `secret_vault_e2e`, `guard_boot_row_e2e`) | 10 / 9 / 1 | **PG-required**: policy rows, privacy invariant, per-tool profiles, materialize/redeem, fail-closed redemption; a real daemon's stored guard boot row asserted equal to `boot_payload(..)` |
| `core` (`memory_recall_e2e`, `cli_ask_e2e`, `cli_memory_l3*`, `email_channel_e2e`) | 1 / 2 / 17 / 8 | three-lane RRF recall + 1-hop expansion; full prod chain against a queued mock LLM; L3 lifecycle; the hermetic channel loop incl. its two regressions |


## Key design decisions locked in

**Not restated here — they drift.** The hard constraints (AGPL-compatible deps only, cross-platform
first-class, Rust core with Python only inside sandboxed workers, one process + one OS sandbox per
worker with no unsandboxed escape hatch) are in [`CLAUDE.md`](../../../CLAUDE.md) § Hard constraints,
which a fresh session loads automatically. The rest — hybrid LLM with policy routing, OS-native
user-level supervisors (no k3s), JSON-RPC 2.0 over stdio, the operator→daemon channel being the
Postgres `tasks` queue, and fixed core tools with a human-approve gate on persisted skills — are in
[`docs/architecture.md`](../../architecture.md) and the ROADMAP entries that shipped them.

**The one worth repeating, because everything else is downstream of it:** worst-case compromise
reaches *at most* the agent's own OS user, its own Postgres role, its own scratch FS, and the
allowlisted endpoints for the *one* compromised tool. Nothing else.
([`docs/threat-model.md`](../../threat-model.md))


## Recently merged

Newest first. Older entries live in the [`archive/`](archive/) snapshots and in git history; the
substance of each is compressed under [Current state](#current-state) rather than repeated here.

- **[#680](https://github.com/hherb/kastellan/pull/680)** `fb560ab7` — a stale micro-VM rootfs image
  can no longer gate anything (#667): every FC e2e now compares the sha256 of each **baked** binary
  against the release build, and `KASTELLAN_MICROVM_REQUIRE_E2E=1` turns every micro-VM preflight
  skip into a panic.
- **[#681](https://github.com/hherb/kastellan/pull/681)** `aee2a7f0` — the Hermes Agent survey (docs
  only): four ROADMAP entries, the anchor index folded into #678 as slice (e).
- **[#675](https://github.com/hherb/kastellan/pull/675)** `f831b3d1` — the micro-VM diagnostics
  cluster: the guest console is captured and echoed redacted on a boot failure (#666), `EarlyExit`
  carries the worker's last words (#666 core half), the VMM jail gets a real-bwrap gate (#671),
  guest `/run` is mounted `mode=0755` (#672), and a failed relay-socket chown is fatal (#670).
- **[#669](https://github.com/hherb/kastellan/pull/669)** `4955a52c` — the Firecracker gate #660
  owed, plus the **three** defects it found (a refused VMM jail, a guest kernel with no Landlock
  against the audit's new fail-closed rule, root-owned relay sockets). 0/21 → 21/0.
- **[#663](https://github.com/hherb/kastellan/pull/663)** `9ace57ad` — the gliner-relex require knob
  (#653) and the one flag dialect (#654).
- **[#656](https://github.com/hherb/kastellan/pull/656)** `c03ec1a3` — the interpreter alias bind
  (#650), plus #661 and #662, the two defects that had made `main` spawn **no** worker under real bwrap.
- **[#660](https://github.com/hherb/kastellan/pull/660)** `62d98a00` — the second pre-release
  security audit: 29 fixes across containment, secrets, prompt and egress.
- **[#651](https://github.com/hherb/kastellan/pull/651)** `ef8144f8` — GHSA-xrqw-3rrv-vx5w, fixed
  properly (the advisory's stated one-command remedy lands on a still-vulnerable 5.6.2).

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
