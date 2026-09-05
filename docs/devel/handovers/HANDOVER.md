# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20260905_669_pre-prune.md`](archive/handover_20260905_669_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here.

**Last updated:** 2026-09-05 (the micro-VM diagnostics cluster: #666, #670, #671, #672) ·
**`main` HEAD:** `4955a52c` — [#669](https://github.com/hherb/kastellan/pull/669) MERGED, which is
the Firecracker gate #660 owed plus the three defects it found. ·
**OPEN BRANCH: `fix/666-670-671-672-microvm-diagnostics`** — see
[This session](#this-session-the-micro-vm-path-can-now-say-why-it-failed). ·
**DGX RUNNING `9ace57ad`** — now **one merge behind** `main` (missing #669's Firecracker fixes).

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

### This session: the micro-VM path can now say why it failed

Branch `fix/666-670-671-672-microvm-diagnostics`. Four issues, one theme, and each fix was **proved
to fail against un-hardened code** rather than argued.

- **[#666](https://github.com/hherb/kastellan/issues/666) — the guest console is captured.** The
  guest kernel boots `console=ttyS0` and firecracker presents that console as **its own stdout**,
  which the launcher sent to `/dev/null`; `--log-path` covers firecracker's own logs and nothing
  written before it opens that file. Both streams now go to `console.log` in the per-spawn run dir
  (0600, `create_new`, beside `fc.json`), and on a boot failure the launcher echoes a bounded,
  redacted tail to its **own stderr** — the one stream the backend pipes and the daemon drains.
  **Proved live:** with an unmountable rootfs the launcher now prints `micro-VM boot failed`, names
  the console, and carries the guest's `Kernel panic - not syncing: VFS: Unable to mount root fs`.
  Before, the caller got a bare `EarlyExit`.
- **Redaction is not optional here.** The kernel prints its command line at every boot and that line
  carries `kastellan.env=<hex>` — the worker's whole environment, secrets included (verified: it is
  on the console of every real boot). The **value** is replaced and the **key** kept, so its absence
  stays a signal.
- **#666, core half — `EarlyExit` now carries the worker's last words.** `spawn_worker` switched to
  the tail-retaining drainer and the dispatch path logs `format_early_exit_report` at **warn**.
  Log-only, deliberately: the tail is raw worker output and the chokepoint scrubs redeemed secrets
  out of everything reaching the planner (audit H1), so this changes the **level** — invisible at
  `debug` to visible — and nothing about where the bytes may go. `StderrTail` gained a
  drain-completion flag because without it the reader wins the race and reports "wrote NOTHING" for
  a worker that explained itself a millisecond later.
- **The promotion forced a security fix, and it is the one worth remembering.** Making untrusted
  worker bytes default-visible in an operator's terminal without neutralising them would be a
  widening dressed as a diagnostic improvement — a compromised worker is in scope, and an ESC (or
  the 8-bit CSI that does the same job) is an ANSI sequence executing in whatever tails the log.
  #544's character class already existed **privately** inside `prompt_assembly::assemble`; copying
  it would have been the #642/#661 mistake a third time, so it moved to
  [`core/src/untrusted_text.rs`](../../../core/src/untrusted_text.rs) with its reasoning and gained
  a second consumer.
- ⚠️ **And the fix's first version broke the thing it protected.** `\n` is *in* the neutralised class
  — it is precisely what would forge a row in a prompt — so neutralising the raw chunk replaced
  every newline with a space and `drain_reader`'s line split never fired again: one line, forever,
  silently. Caught by three pre-existing tests going red on the DGX. The split now runs on raw text
  and each line is neutralised in `push_trimmed`, after its endings are stripped. **A predicate
  correct for one renderer can be destructive in another; the shared class is right, the shared
  application point is not.**
- **[#671](https://github.com/hherb/kastellan/issues/671) — the VMM jail gets a real-bwrap gate.**
  #661 and #669 were one defect in two of the three bwrap argv producers and **both shipped green**,
  because the VMM jail's argv was executed by nothing in the suite. No content assertion can catch
  the class: every flag was spelled correctly and bwrap rejected the **combination** at option-parse
  time. The new test builds the production argv and runs `/bin/true` under it, asserting **exit
  status** rather than stderr wording (an option-parse refusal and a failed bind both exit 1 with a
  `bwrap:` line). It lives in `linux_smoke.rs` beside the worker-jail gates, which also keeps
  `skip_if_no_userns` to one copy. **It is not `#[ignore]`d** — it runs in the ordinary workspace
  sweep. **Proved to fail:** reintroducing the bare `--disable-userns` turns it red with
  `bwrap: --disable-userns requires --unshare-user`.
- **[#672](https://github.com/hherb/kastellan/issues/672) — guest `/run` is mounted `mode=0755`.**
  A tmpfs with no `mode=` comes up **1777** whatever the umask. Inside the VM that was near-harmless
  but it **masked** what #669 fixed: the world-writable default already granted what the removed
  `/run` chown was justified by, so the per-socket chown could have been deleted with nothing
  noticing. **Proved both ways from inside a running guest:** 1777 without the option, 755 with it.
- **[#670](https://github.com/hherb/kastellan/issues/670) — a failed relay-socket chown is now
  fatal.** `worker_owned_paths` returns `OwnedPath { path, role }` so the chown loop never
  re-derives which paths are sockets — a second place that knew was how the first version drifted.
  `connect(2)` on an `AF_UNIX` socket needs write permission on the socket **file**, so a socket the
  worker cannot own is a worker that dies on every dial, not a degraded one. Left warn-only in #669
  because a panicking PID 1 was illegible; **#666 is what makes it legible, which is why the two
  land together**. **Proved end to end:** forcing the chown to fail halts the guest
  (`Kernel panic … Attempted to kill init!`) and the console names the cause — the host still sees
  `EarlyExit`, but the reason is now readable.
- ⚠️ **Two findings that were measurements, not deductions, and both would have shipped inert:**
  - **`bwrap --clearenv` means the launcher has NO environment.** The `KASTELLAN_MICROVM_KEEP_RUN_DIR`
    opt-in was written as an env-var read; on the default confined path it could never be set, and
    the run dir came back holding only `teardown.done`. Every other launcher setting travels by argv
    for exactly this reason. The daemon now reads the variable in its own process and forwards
    `--keep-run-dir`.
  - **The release profile is `panic = "abort"`.** The launcher's boot-failure path relied on an RAII
    scopeguard, so in release **no destructor ran**: `fc.kill()` never fired and the run dir survived
    by accident rather than by the rule its doc comment described. Firecracker was left holding KVM
    and the vsock device except where the confined path's `--die-with-parent` + `--as-pid-1` jail
    happened to reap it — leaving the bare path unprotected. The failure path now does its teardown
    by hand. **Check the profile before trusting RAII on a failure path.**
- **The in-guest probe enumerates `/run` rather than naming a socket.** The first version read the
  path from `KASTELLAN_EGRESS_PROXY_UDS`; measured, `python.exec` does not hand its own environment
  to the code it runs. Enumerating asks the better question anyway — *every* socket the init bound
  must be reachable — and avoids a fourth copy of a constant that already exists in three places.
  Two guards precede the check (at least one socket; the worker is not root), because without them
  it is vacuously true. [[unreachable-success-path-proves-nothing]]

### #669 — the Firecracker gate, MERGED (`4955a52c`)

The micro-VM backend had been **entirely dead** since the 2026-09-02 audit merged, at **0 of 21**
Firecracker tests, in three independent ways each masking the next. Full prose in
[`archive/handover_20260905_669_pre-prune.md`](archive/handover_20260905_669_pre-prune.md). What
still binds:

- **Count the producers, and make the const the only spelling.** `build_vmm_jail_argv` was the
  **third** bwrap argv producer and #661's fix missed it. The `USERNS_LOCKDOWN_FLAGS` const's own
  doc said "every jail passes" while this jail spelled the flags out by hand. #671 (above) is the
  gate that would have caught it.
- **The pinned guest kernel has no Landlock** (`CONFIG_SECURITY_LANDLOCK is not set`, read out of
  the pinned `vmlinux`'s embedded IKCONFIG **without booting anything**
  [[dgx-guest-kernel-config-inspection]]), and the audit made an unenforceable ruleset fatal. The
  launch plan states the exception (`KASTELLAN_LANDLOCK_PROFILE=none`) as a **default that never
  overrides a caller**. Seccomp is unaffected. Repinning a Landlock-capable kernel is
  [#668](https://github.com/hherb/kastellan/issues/668).
- **W-2 is proved from inside the guest**, not by reading code: `uid != 0`, `euid == uid`, the
  **saved-set uid** (the field that actually makes `setuid(0)` unreachable, and the one the init's
  own post-drop check cannot see), `uid ==` the host's euid, gid, groups, and **`Seccomp: 2`** — the
  layer the Landlock opt-out's justification rests on and which nothing else observes. ⚠️ The
  `groups` assertion is a regression guard, **not** a proof: guest PID 1 has no supplementary
  groups, so it holds either way.
- ⚠️ **A stale rootfs image gates nothing, and fails like a code regression.** Every image bakes its
  own `kastellan-microvm-init` **and** worker, so a guest-side change is invisible until that image
  is rebuilt. The whole W-2 gate could have run green having tested none of it.
  [#667](https://github.com/hherb/kastellan/issues/667) — **still open, and still the cheapest
  remaining win on this path.** Build scripts live in **two** directories
  (`scripts/workers/microvm/` but `scripts/workers/kv-demo/`).
- **A non-hex `kastellan.mounts=` fixture fails OPEN, silently** — `parse_mount_manifest` falls back
  to an empty manifest, so a loop over `m.rw` never enters its body and the test asserts nothing.
  All five `worker_owned_paths` tests are exact-set `assert_eq!`s for this reason.
- **`/run` is out of the chown set** and re-adding it would be a regression: chowning a *sticky*
  directory is what lets the owner unlink entries it does not own. #672 (above) makes the mode a
  decision instead of a default.

### #660 — the second pre-release security audit (2026-09-02), MERGED (`62d98a00`)

29 fixes, 80 files. Full write-up in
[`docs/security-audit-2026-09-02.md`](../../security-audit-2026-09-02.md); every fix in one ROADMAP
line. What still binds:

- **The four load-bearing fixes.** (H1) the dispatch chokepoint scrubs every redeemed secret out of
  the worker's `Ok` value **and** its `RpcError`. (H2) agent-raised `l1_insight`s are screened by the
  strict catalogue at promotion (audited `l1.injection_blocked`) and at prompt assembly. (H3) every
  per-spawn `/tmp` dir is minted with `private_dir::create_private_dir` (exclusive `mkdir` 0700,
  owner-verified) and secret files with `O_EXCL` 0600 — **a pre-planted name from another uid FAILS
  THE SPAWN CLOSED; that is the contract, do not "fix" it back to `create_dir_all`**. (H4) seccomp
  admits `clone` only without `CLONE_NEW*` and answers `ENOSYS` to `clone3`.
- **Three lockdown behaviours are FAIL-CLOSED and will bite a careless fixture:** a missing
  `KASTELLAN_SECCOMP_PROFILE` is an error (`none` is the explicit opt-out), an unenforceable Landlock
  ruleset is an error, and a corrupt `kastellan.env=` guest token refuses the VM boot.
- **Every networked stdio worker builds its handler INSIDE `serve_stdio_with`.** Landlock is
  per-thread; a runtime built in `from_env()` ran unrestricted on the threads that parse the
  network. **Keep that order for any new worker.**
- **CodeQL reads NAMES** — five `rust/cleartext-logging` alerts fired merely for interpolating a
  numeric `uid`. Keep identifier- and credential-like names out of log and panic text. And the
  **live-matrix clippy job** catches lints the default-feature workspace clippy never compiles: run
  `cargo clippy -p kastellan-worker-matrix --all-targets --features live-matrix --locked -- -D
  warnings` before pushing anything touching `sdk_live.rs`.
- **Its three real-bwrap defects are fixed** — [#661](https://github.com/hherb/kastellan/issues/661)
  (a bare `--disable-userns` beside `--unshare-all` is refused at bwrap's *option-parse* time, so no
  skip guard can see it — 66 failures across 23 suites) and
  [#662](https://github.com/hherb/kastellan/issues/662) (**any `pre_exec` closure forces Rust std off
  `posix_spawn` onto its fork path, which opens a `socketpair` the `strict` profile pins out**; reach
  for `process_group`/std attrs instead). ⚠️ **A core e2e does not rebuild a worker package.**
- **Deferred with a reason** (all in the audit doc): brokers not force-routed; the guard tier never
  sees bytes past 64 KiB; `secret://` refs not tool-bound; `Host:` ≠ CONNECT authority; `net_client`
  grants `bind`/`listen`; no email-replay freshness window; no gliner weights revision pin; macOS
  worker-side caps. **Recommendation before release: flip force-routing to default-on.**

### Earlier merged arcs, compressed

Full prose in the [`archive/`](archive/) snapshots. Only what still binds:

- **#650 — the interpreter alias bind** (`c03ec1a3`). `uv` lays a managed CPython out with a
  minor-version **symlink alias**, and the venv's `bin/python` names the **alias**;
  `resolve_interpreter_root` canonicalized, so `execve` returned **ENOENT for a file that is present
  and readable**. The function had **two jobs and one return value** → `InterpreterRoot` with
  `dep_walk_prefix` (canonical) and `bind_paths` (canonical **plus** aliases). **The admission rule
  is non-widening and that is the load-bearing choice:** an alias binds only when it canonicalizes to
  the canonical prefix — **a containment fix must not widen containment**. ⚠️
  `Path::components()` strips **interior** `.` only [[rust-path-components-normalizes-dot]]. Open
  follow-ups: [#657](https://github.com/hherb/kastellan/issues/657),
  [#658](https://github.com/hherb/kastellan/issues/658),
  [#659](https://github.com/hherb/kastellan/issues/659).
- **#653 / #654 — the gliner-relex require knob** (`9ace57ad`). `KASTELLAN_GLINER_RELEX_REQUIRE_E2E`
  turns each unmet precondition into a **panic naming itself** instead of a `[SKIP]`. **The reusable
  pattern is the `*_or_reason` sibling:** return the reason **without rendering a verdict**, so one
  caller can skip where another must fail. #654 was a real operator-facing skew — fixtures gated on
  `!= Some("1")` while production reads `env_flag_enabled`, so a `kastellan.env` saying
  `ENABLE=true` produced a silent skip. Still open: [#664](https://github.com/hherb/kastellan/issues/664),
  [#665](https://github.com/hherb/kastellan/issues/665).
- **#649 / #651 — the transformers advisory** (`ef8144f8`). **The remedy an advisory states can be a
  no-op that exits 0:** `uv lock --upgrade-package transformers` reached **5.6.2, still inside the
  vulnerable range**, because `gliner 0.2.27` capped it. Both floors moved in **`pyproject.toml`**,
  making the vulnerable range unsatisfiable. New `python-lock-check` CI job runs `uv lock --check
  --offline` — it catches a **weakened floor**, not an advisory.

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

> ⚠️ **Clippy parity is a `rustup update`, not a property of the hosts — check it, don't assume it.**
> CI pins nothing (`dtolnay/rust-toolchain@stable`) and both dev hosts float on the same channel, so
> they drift out of parity silently. `rustc --version` on the host you are gating on. **2026-09-05:
> DGX on 1.98.0.** `rust-version = "1.78"` is the MSRV and constrains none of this.

> ⚠️ **A cached `cargo clippy` reports a full-workspace pass it never ran.** Exit code alone does not
> distinguish it — **count the `Checking` lines**, and count them against the *reverse-dependency
> set* of your change, not against 27, or a correct incremental lint reads as a failure. Cold is
> ~217–303; a warm dir can report exit 0 having linted 4.

> ⚠️ **`cargo check`/`clippy --all-targets` do NOT warm the target dir for `cargo test`** — they emit
> metadata-only artifacts, no linked binaries. **Run the sweep first, lint after.**

> ⚠️ **A private `CARGO_TARGET_DIR` does not build `examples/`,** so `email_channel_e2e`'s 6 tests
> fail with `fixture not built` at a perfectly green commit
> ([[custom-cargo-target-dir-breaks-daemon-e2e]]). Read the failure text before believing a regression.

> ⚠️ **"Filed, not fixed: #N" in a PR body or commit message CLOSES #N.** GitHub matches the
> `fixed: #N` substring and has no notion of negation. It has cost three issues. Write **"deferred to
> #N"**, and before merging run
> `gh pr view <n> --json body --jq .body | grep -oiE '(close[sd]?|fix(e[sd])?|resolve[sd]?)[[:space:]:,]+#[0-9]+'`
> over the body *and* the commit message. [[pr-body-not-fixed-autocloses-issue]]

> ⚠️ **Squash-merge caveat:** every PR lands as one squash commit, so its *branch-tip* SHA (where the
> gate ran) is **not** an ancestor of `main`. Check content, not `merge-base`.

> ⚠️ **Freshly-linked executables can hang forever in `_dyld_start` on macOS**, so every daemon e2e
> fails with the daemon's stdout **and** stderr **completely empty** — which reads exactly like a code
> defect. **Newness, not size.** A warm `CARGO_TARGET_DIR` still works; a cold one wedges. See the
> header warning for why `sample` alone does not prove it. [[mac-fresh-large-binaries-hang-in-dyld]]

> ⚠️ **`kastellan-worker-egress-proxy` leaks on the Mac.** Five orphans in one sweep, four of them
> 1–7 days old, across three target dirs. Not investigated — flagged for whoever next touches
> sidecar lifecycle.

> ⚠️ **A `pgrep -f '<cmd>'` wait loop matches itself** and never exits, because the Bash tool's
> `zsh -c` wrapper puts the pattern in the waiter's own argv. Use `pgrep -x`, a log sentinel, or a
> background task. [[pgrep-wait-loops-match-themselves]]

---

## Read these first

1. [`docs/architecture.md`](../../architecture.md) — process model, cross-platform table
2. [`docs/threat-model.md`](../../threat-model.md) — the invariant, scenarios in scope, defence layers
3. [`docs/devel/ROADMAP.md`](../ROADMAP.md) — the master sequenced TODO with commit hashes
4. Memory notes (auto-loaded) — `~/.claude/projects/-Users-hherb-src-kastellan/memory/MEMORY.md`
5. [`archive/`](archive/) — the full prose for everything this file summarises

---

## Next TODO

> Only *open* work is listed. Shipped items move to [Recently merged](#recently-merged) or the ROADMAP.

**FIRST — #660's gates are now all discharged; what follows is what the last one turned up.**

1. ✅ **The live Matrix DM round-trip is DISCHARGED** (2026-09-05, DGX at `9ace57ad`). Two DMs from
   `@horst` were received, planned and answered — tasks 185 and 186, both `channel.replied` with
   `peer: @horst:matrix.kastellan.dev`. #660's invite/two-party scoping does **not** break a normal
   DM, which was the property most at risk. That closes the last item #660 owed.
   **But the two answers were wrong in an interesting way**, and it is now
   [#677](https://github.com/hherb/kastellan/issues/677): task 186 spent three of six plan
   iterations on near-duplicate searches and a fourth on `shell.exec /usr/bin/ls` — the planner
   theorised that an email attachment might be a file in its **current working directory** — then
   blamed "the tool-step limit" for not reading the PDF, having never called
   `mail.get_attachment_text`, which task 185 had used successfully **four minutes earlier**. The
   two tasks reported **different booking references** for the same question, both with equal
   confidence. ⚠️ **Which answer was grounded could not be established**, because both large tool
   dispatches were audited `_truncated: true` with `req` and `result` dropped wholesale — that is
   [#617](https://github.com/hherb/kastellan/issues/617), and this is the first time it has blocked
   a real investigation rather than a hypothetical one. The evidence is on both issues.
2. **Redeploy the DGX**, which is two merges behind (`9ace57ad`; `main` is `4955a52c` and this
   branch is not merged yet). `scripts/upgrade_from_git.sh` does build+install+restart+verify and is
   hardcoded to `main`. A good install says `installed 15 binaries`; logs are in
   `~/.local/state/kastellan/*.out`, not the journal [[dgx-deploy-env-clobber-and-missing-workers]].

**THEN, on the micro-VM path — one issue left, and it is the one that turns a gate into a
formality:** [#667](https://github.com/hherb/kastellan/issues/667). Every rootfs image bakes its own
`kastellan-microvm-init` and worker, so a guest-side change is invisible until that image is
rebuilt, and the suites give no hint. The shape is settled in the issue: compare each image's mtime
against the init binary's in the shared `tests_common::microvm` helper and **fail** (honouring the
`REQUIRE_E2E` convention from #653) rather than skip. The second half — one `rebuild-all-rootfs.sh`
enumerating the eight scripts across **two** directories — is worth doing at the same time; this
session rebuilt them by hand-listing paths, twice.
[#668](https://github.com/hherb/kastellan/issues/668) (repin a guest kernel with Landlock) is the
standing posture item, and needs a kernel build rather than a code change.

**A standing architecture item, raised 2026-09-05 and now the frame for several open issues:**
[#678](https://github.com/hherb/kastellan/issues/678) — **retire truncation as the answer to "bigger
than the budget".** The key move is that truncation is doing **three different jobs** and only one
becomes map-reduce: a *control that stops seeing its evidence* (the guard's 64 KiB `SCAN_BYTE_CAP` —
map-reduce, and the reduce `p = max(p_i)` is strictly more sensitive than today, so nothing currently
blocked becomes clear); a *record that must be faithful* (`truncate_payload` — **spill, never
summarise: an audit row is testimony**, and the sha256 it already stores is the content-address key);
and a *resource guard* (`MAX_RECORD_BYTES` and friends — **these stay**, they are containment against
a compromised worker, not context management). `core/src/handoff.rs` already stashes oversized results
**whole**, so the body is not lost and no new storage is needed — only the reduce. Likely subsumes
[#604](https://github.com/hherb/kastellan/issues/604) and
[#612](https://github.com/hherb/kastellan/issues/612) by removing their premise rather than re-tuning
around it. ⚠️ **The polarity inverts to fail-closed** — today a document past the cap is silently
unscreened — which is the single most important behavioural difference and needs its own test.

**THEN, cheap and now long overdue:** [#655](https://github.com/hherb/kastellan/issues/655) — `main`
has **no required status checks**, so clippy, the matrix build and the `python-lock-check` gate can
all go red and still merge. A repo-settings change, not code.

**THEN the guard arc, whose remaining work is one item and it is the one that matters:**
[#612](https://github.com/hherb/kastellan/issues/612) — a design call rather than a patch, and #616
unblocked its favoured option (measure from the `ms` / `body_byte_len` the guard rows now carry).
Read the measurement in the issue first; every cheap fix is closed off there. Beside it, both cheap:
[#639](https://github.com/hherb/kastellan/issues/639) (split `guard_tier_e2e.rs`, 1558 lines, also
[#622](https://github.com/hherb/kastellan/issues/622)'s cheapest option) and
[#638](https://github.com/hherb/kastellan/issues/638) (214 rustdoc warnings, 67 broken intra-doc
links, in a tree that treats doc comments as the design record).

**Next up — operator's choice, each roughly one session.** Issue text is authoritative; these are the
gotchas that are *not* in the issues.

- **[#560](https://github.com/hherb/kastellan/issues/560) — the planner fabricates a 16-hex
  `message_id`.** Do **not** close it by rewriting the parameter description: #536 already did
  exactly that, deployed, and both later runs still fabricated. The lead worth measuring: with keys
  stripped by `extract_scannable_text`, `"20973"` reaches the planner as a bare line among subjects
  and dates, with nothing marking it as *the id*
  [[tool-output-reaches-planner-key-stripped]] [[opaque-ids-are-unusable-tool-params]].
- **[#550](https://github.com/hherb/kastellan/issues/550) — the *generated* `kastellan.env` gets no
  end-to-end check.** **The naive fix is wrong** — the overlay legitimately overrides `kastellan.env`
  keys, so per-file comparison false-positives; it must compare the *folded* environment, which
  `fold_env_files` already computes for launchd.
- **[#551](https://github.com/hherb/kastellan/issues/551) — no path directive escapes systemd's `%`
  specifier.** Pre-existing and workspace-wide. Measure first, then escape `%%` or reject at install.
- **[#548](https://github.com/hherb/kastellan/issues/548) — PG e2e tests install units into the
  operator's *real* `~/.config/systemd/user/`.** Not a teardown bug — `PgCluster`'s `Drop` guards are
  correct and cannot run on SIGKILL — so the fix is about blast radius. Confirmed still accruing.
  ⚠️ #641 removed the shared suffix between a test daemon's unit and its sibling PG cluster, so a
  sweep can no longer correlate the two; restore it with a `.suffix()` setter rather than by
  reverting the constructor [[issue-as-filed-can-carry-a-regression]].
- **[#519](https://github.com/hherb/kastellan/issues/519), [#554](https://github.com/hherb/kastellan/issues/554),
  [#534](https://github.com/hherb/kastellan/issues/534)** — each a design call; #554 needs a live DGX
  gate because it narrows what a deployed worker may do.
- **Mail credential expiry — [#673](https://github.com/hherb/kastellan/issues/673) +
  [#674](https://github.com/hherb/kastellan/issues/674).** Filed together: an upstream 401/403 is
  reported as `POLICY_DENIED`, so an expired localmail credential reads as a kastellan policy
  refusal, and nothing notices the expiry in the first place. Same family as everything above — a
  failure that names the wrong cause.
- **Email channel — slices 2 and 3.** Slice 1 (gated inbound) MERGED, #503 closed its MITM gap. Spec
  `docs/superpowers/specs/2026-07-28-email-fallback-channel-design.md`. **Slice 2** = SMTP outbound
  (`lettre`, MIT-verified) + full round trip; today `EmailChannel::send` refuses and every refusal is
  audited `channel.reply_undelivered`. **Slice 3** = DGX deploy + live tier; **restart
  `localmail-serve` (+ `localmail-daemon`) on the DGX first**.
- **A Mac daemon deployment is a deliberate decision, not a task.** The tier boots fine there
  (91.4 s derived, `n_ctx` 66 048) but #612 means it fails open on large documents. Decide #612
  first, or deploy with a pinned timeout and say so.
- **Live guard-host facts:** the DGX guard server is `llama-server … Shieldstral-1.0-3B-Q8_0.gguf
  --alias shieldstral --port 8081 -c 131072 -ngl 99`; `/props` reports the per-request context at
  `default_generation_settings.n_ctx` with **no top-level `n_ctx`**. Restart it with **at least
  `-c 66048`** or the daemon refuses to boot. The three guard keys live in
  `~/.config/kastellan/kastellan.env.local`, which `install` never rewrites.
- **Deferred with a reason, not forgotten:** macOS Seatbelt-loopback verification of mail tier 1a;
  **Telegram inbound** (still rejected as primary — no bot E2E, centralized, ban risk);
  **MITM-of-browser** via a proper NSS trust-store import, **not**
  `--ignore-certificate-errors-*`, since production must not be loosened to make a test pass.

**File-split backlog (Item 9b)** — **`wc -l` before picking; the numbers drift.** The rule the tree
follows: **split BEFORE the change that grows a file**, in a movement-only commit whose `#[test]`
name set is verifiable either side. Best first picks, each a pure test-lift:
`core/src/channel/ask_message.rs` **956**, `workers/mail/src/handler.rs` **670**,
`sandbox/src/linux_firecracker/plan.rs` ~**1160** (`cfg(linux)`, DGX-gated), and
`core/tests/guard_tier_e2e.rs` **1558** ([#639](https://github.com/hherb/kastellan/issues/639)).
Clean seam already visible: `core/src/scheduler/asks.rs` **801** — its pure half separates from its
async half. Judgement first, not movement: `tests-common/src/daemon/spec/tests.rs` **599**,
`db/src/asks.rs` **1127**, `db/graph.rs` **926**, `llm-router/src/config.rs` **843** — a small
`mod tests` there means a split is a production reorganisation. Also over cap, no seam called yet:
`core/src/scheduler/inner_loop.rs`, `core/src/channel/bus.rs`, `workers/matrix/src/sdk_live.rs`,
`llm-router/src/messages.rs`, `core/src/main.rs`.

**Standing deferrals (no owner; pick up when a consumer appears).** These live in the issue tracker
and are listed here only so nobody re-derives them: egress
[#242](https://github.com/hherb/kastellan/issues/242) (tunnel idle/resolve timeouts),
[#251](https://github.com/hherb/kastellan/issues/251) (stale-scratch crash-sweep),
[#304](https://github.com/hherb/kastellan/issues/304) (real-sandbox cert-pin e2e — needs a
controllable TLS origin), [#260](https://github.com/hherb/kastellan/issues/260) (literal-IP HTTPS
origins needing an IP-SAN cert under MITM); micro-VM
[#381](https://github.com/hherb/kastellan/issues/381) (`size_mib` resize + mkfs↔flock TOCTOU) and
**true `jailer`**, deferred to a privileged-tier `VmmConfinement::Jailer` sibling whose seam already
exists in `confine.rs`; python-exec Phase 4 (curated-wheels RO dir, if skills ever demand
third-party packages — stdlib-only today, flipped by `KASTELLAN_PYTHON_EXEC_ENABLE=1`);
web-research polish, all opus-triaged DEFER (`search_err_to_rpc` gives a "search"-worded error on an
*embed* misconfig; `embed_note` conflates three conditions under first-wins, so a benign cap note can
mask a genuine embed failure — severity-rank it); and an ANN index on `entities.embedding` once
cardinality warrants it (sequential cosine scan today).

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
| **DGX** (this branch — **the gate that stands**) | **`520f278d`**, re-verified at the tip **`a93fe60f`** | `cargo test --workspace --no-fail-fast --locked -- --nocapture` **4075 / 0 / 57**, **177** suites, `TEST_EXIT=0` at `520f278d`; the tip re-run is **4074 / 1**, the 1 being the `scheduler_ask_expiry_e2e` flake below ([#676](https://github.com/hherb/kastellan/issues/676)), **2 / 2 green in isolation**. **The delta reconciles exactly: +26** over the 4049 below — 3 `microvm-init` (role + tmpfs mode), 10 launcher `console`, 9 `core` lib (4 `untrusted_text` + 5 `worker_stderr`), 3 `sandbox` (1 real-bwrap VMM-jail gate + 2 launcher-flag/dialect) and 1 new suite — plus **+1 ignored**, the in-guest `/run` e2e. **Firecracker: 28 / 0** across **13** suites, with 2 `[SKIP]` that are the deliberate `KASTELLAN_MATRIX_FC_LIVE_E2E` opt-in. All 8 rootfs images rebuilt first — mandatory, since this branch changes the guest init | `--workspace --all-targets --locked -D warnings` exit 0. ⚠️ An earlier run of this branch FAILED on `too_many_arguments` after the keep-run-dir flag pushed `build_confined_spawn_argv` to 8 — fixed by `LauncherPaths`, not by an `allow` | **4**, all the gliner tier — held. **0** `[WARN]` |
| **DGX** (#669 after its review round) | **`e35c3571`** | **4049 / 0 / 56**, **176** suites, `TEST_EXIT=0`. **Firecracker: 21 / 0**, the first fully green run (SearxNG restarted after 5 weeks down) | `-p kastellan-core -p kastellan-sandbox -p kastellan-microvm-init -p kastellan-microvm-run --all-targets --locked -D warnings` exit 0 after force-touching core + sandbox. ⚠️ The first workspace clippy exited 0 having emitted **24** `Checking` lines in 6s — warm, not a gate | **4** |
| **DGX** (#669, the Firecracker gate) | **`b492966b`** | **4048 / 0 / 56**, 176 suites. Firecracker went **0 / 21 → 19 / 2**, the 2 an absent local SearxNG | **cold** `--workspace --all-targets --locked -D warnings`: exit 0, **345** `Checking`+`Compiling` lines, all **27** crates, zero warnings. rustc **1.98.0**. ⚠️ **The first cold run FAILED** on an unused import in the `cfg(linux)` `guest.rs` — invisible to the Mac | **4** |
| **DGX** (#663 after its review round) | **`fixall`, pre-commit** | **4040 / 0 / 55**, 176 suites | cold, exit 0, **345** lines, all 27 crates. ⚠️ The *warm* run exited 0 having linted **3** crates and would have missed the real lint the cold one caught | **4** |
| **DGX** (#656 at the three fixes) | **`f97991a6`** | **4009 / 1 / 55**, 176 suites, `TEST_EXIT=101`. The 1 was the same `scheduler_ask_expiry_e2e` flake — see the ⚠️ below, and **do not** apply the "widen the poll deadline" prescription this row used to carry | exit 0, zero warnings | **4** |

Older rows (`4269ff7e` 3997/13, `f12ed26d` 3940, `466ca7ff` 3928, and back to 2950) are in the
[`archive/`](archive/) snapshots.

⚠️ **`scheduler_ask_expiry_e2e` flakes under a full sweep, and this file's diagnosis of it was WRONG
for two gates.** It said "a 60-second poll that missed under full-workspace load — widen the poll
deadline at `scheduler_ask_expiry_e2e.rs:193`". The #675 gate's evidence says otherwise: the panic is
at **line 251**, not at either `await_state`, so both polls *succeeded* and the sweep did move the
task to `failed`; what failed is an **empty audit query**, and the next log line is
`claim_one error: … No such file or directory (os error 2)` — ENOENT on the per-test cluster's **unix
socket**, i.e. the database went away underneath the test. In isolation it runs **62.2 s and 62.5 s**
against a 20 s + 90 s budget, so it is nowhere near a deadline. Widening it would leave the test
green while the scheduler still loses its database mid-run — strictly worse than a visible flake.
Filed with the evidence as [#676](https://github.com/hherb/kastellan/issues/676); likely the same
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

```sh
source "$HOME/.cargo/env"          # cargo isn't on the PATH for non-interactive shells
cargo build --workspace
cargo test --workspace             # authoritative counts in the table above
cargo test --workspace -- --nocapture   # required to verify [SKIP] lines
cargo clippy --workspace --all-targets -- -D warnings
./target/debug/kastellan           # the core daemon
```

**Required one-time Linux host setup (Ubuntu 24.04+):**
`sudo scripts/linux/install-bwrap-apparmor-profile.sh` — without it every sandbox integration test
skips silently. For the Firecracker backend: `sudo scripts/linux/install-firecracker-vsock.sh` (also
a hard prerequisite for every `build-*-rootfs.sh`, which only *verify* the pinned guest kernel and
never create one). macOS needs no setup.

**FC e2e gotchas (DGX) — read before running any Firecracker e2e:** rebuild the **release** launcher
(`cargo build --release -p kastellan-microvm-run`) AND the affected rootfs (the init is baked in —
[#667](https://github.com/hherb/kastellan/issues/667)) AND `export PATH=$HOME/.local/bin:$PATH`
(firecracker is off the non-interactive ssh PATH → the e2e silently skips-as-passes otherwise).
`kastellan-core` won't cross-compile on the Mac (`ring` C dep), so core e2e are compile+run on the
DGX only. A VM worker's `WorkerSpec.program` must be the **in-rootfs**
`/usr/local/bin/kastellan-worker-<name>`, never the host target-dir path
[[vm-worker-in-rootfs-binary-path]]. Since #666, a failed boot leaves `console.log` in the kept run
dir and the launcher echoes a redacted tail to its own stderr — **read that before theorising**;
`KASTELLAN_MICROVM_KEEP_RUN_DIR=1` keeps it on a successful boot too.

### The tree — 27 crates

Full layout in the root [`README.md`](../../../README.md) § Layout, and the load-bearing crates in
[`CLAUDE.md`](../../../CLAUDE.md) § Project shape. Not duplicated here — it drifts, and the README is
the one a fresh reader finds first.

### Integration-suite map

Only the rows that tell you *where to look when something goes red*. The full census (db, llm-router,
egress-proxy, prelude, supervisor, web-* unit counts) is in the
[`archive/`](archive/) snapshots — it is a number that drifts, not a fact that binds.

| Suite | Tests | What's verified |
| ----- | ----- | --------------- |
| `sandbox` integration (`linux_smoke` / `macos_smoke` / `macos_container_smoke`) | 8 / 10 / 7+ | **real** jails: fs invisibility, net deny, relative-path reject, OOM-kill under MemoryMax, per-spawn `/tmp` tmpfs, fresh session leader — **and the Firecracker VMM jail actually launching** (#671), the one gate that catches a flag combination bwrap refuses at option-parse time |
| `core` Firecracker (13 suites, `#[ignore]`, DGX) | 28 | **real KVM**: round-trip, mem cap, net deny, host-dir share, warm idle, VMM confinement, egress + broker reverse channels, persistent store, browser-driver, matrix; W-2's in-guest privilege drop read from `/proc/self/status`; `/run` mode + relay-socket reachability from inside the guest |
| `core` (`worker_early_exit_diagnostic_e2e`) | 1 | a worker that dies before responding gets its own stderr into the daemon log at WARN (#666) — the wiring the pure tests cannot see |
| `core` (`shell_exec_e2e`, `python_exec_e2e`, `python_exec_container_e2e`) | 4 / 4 / 4 | **real** core→sandbox→worker round-trips under production policy; jail-contained socket attempt; per-spawn scratch; secret-scrub to `[redacted:]` |
| `core` (`egress_proxy_e2e`, `egress_force_routing_e2e`, `email_mitm_e2e`) | 3 / 4 / 2 | **real** sandboxed sidecar + CONNECT client; Linux-only no-direct-route; a hermetic MITM asserting the round-tripped event plus `tls_intercepted:true` |
| `core` (`injection_guard_e2e`, `secret_vault_e2e`, `guard_boot_row_e2e`) | 10 / 9 / 1 | **PG-required**: policy rows, privacy invariant, per-tool profiles, materialize/redeem, fail-closed redemption; a real daemon's stored guard boot row asserted equal to `boot_payload(..)` |
| `core` (`memory_recall_e2e`, `cli_ask_e2e`, `cli_memory_l3*`, `email_channel_e2e`) | 1 / 2 / 17 / 8 | three-lane RRF recall + 1-hop expansion; full prod chain against a queued mock LLM; L3 lifecycle; the hermetic channel loop incl. its two regressions |

---

## Key design decisions locked in

**Not restated here — they drift.** The hard constraints (AGPL-compatible deps only, cross-platform
first-class, Rust core with Python only inside sandboxed workers, one process + one OS sandbox per
worker with no unsandboxed escape hatch) are in [`CLAUDE.md`](../../../CLAUDE.md) § Hard constraints,
which is what a fresh session loads automatically. The rest — hybrid LLM with policy routing,
single-host deployment via OS-native user-level supervisors (no k3s), JSON-RPC 2.0 over stdio for
MCP-stdio compatibility, the operator→daemon channel being the Postgres `tasks` queue rather than a
new IPC socket, and fixed core tools with a human-approve gate on persisted skills — are recorded in
[`docs/architecture.md`](../../architecture.md) and the ROADMAP entries that shipped them.

**The one worth repeating, because everything else is downstream of it:** worst-case compromise
reaches *at most* the agent's own OS user, its own Postgres role, its own scratch FS, and the
allowlisted endpoints for the *one* compromised tool. Nothing else.
([`docs/threat-model.md`](../../threat-model.md))

---

## Recently merged

Newest first. Older entries live in the [`archive/`](archive/) snapshots and in git history; the
substance of each is compressed under [Current state](#current-state) rather than repeated here.

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
