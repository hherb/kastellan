# kastellan — Session Handover

> Rolling working brief. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. Convention in
> [`README.md`](README.md); full historical detail in the [`archive/`](archive/)
> snapshots — most recently
> [`archive/handover_20260826_624_pre-prune.md`](archive/handover_20260826_624_pre-prune.md),
> which holds the verbose pre-prune version of everything summarised here,
> including the full #619, #615/#616/#618 and live-bring-up write-ups compressed below.

**Last updated:** 2026-09-05 (#669's review round: the chown set's only guard turned out to assert
nothing, and `/run` was never needed. Firecracker is now **21 / 0**) ·
**DGX RUNNING `9ace57ad`** (redeployed 2026-09-04, now current with `main`) (see [Merged work, compressed](#merged-work-compressed--the-guard-arc-and-the-2026-09-02-deploy)) ·
**`main` HEAD:** `9ace57ad` — [#663](https://github.com/hherb/kastellan/pull/663), which closed
[#653](https://github.com/hherb/kastellan/issues/653) and
[#654](https://github.com/hherb/kastellan/issues/654), on top of `c03ec1a3`
([#656](https://github.com/hherb/kastellan/pull/656): #650, #661, #662) and `62d98a00`
([#660](https://github.com/hherb/kastellan/pull/660), the 2026-09-02 security audit). ·
**OPEN BRANCH: `test/w2-microvm-uid-drop-gate`** — the Firecracker gate + its three fixes; see
[The two DGX gates](#the-two-dgx-gates-660-owed--run-and-the-firecracker-one-found-three-defects).

> ⚠️ **A gate booked as "pure verification, not code" is not evidence until it has RUN.** This file
> carried both #660 gates for two days as bookkeeping. The Firecracker one turned out to be three
> independent production defects, each masking the next, all reporting the same contentless
> `Protocol(EarlyExit)` — and **`0 of 21`** Firecracker tests had been passing the whole time.

> ✅ **`main` IS DEPLOYABLE AGAIN.** #656 merged 2026-09-03 carrying `4269ff7e` (#661, the bwrap
> `--disable-userns`/`--unshare-user` pair) and `f97991a6` (#662, python-exec's `socketpair` SIGSYS)
> — the two defects that made `main` `62d98a00` spawn **no** worker under real bwrap. The previous
> "do not run `scripts/upgrade_from_git.sh`" warning is **lifted**. The DGX itself still runs
> `121f22a2` and is now **three merges behind** (#660, #656, #663).

**Last gate: DGX over `test/w2-microvm-uid-drop-gate`, post-review-round — 4049 / 0 / 56, and the Firecracker suites are **21 / 0** for the first time (SearxNG brought back up). See [Test baseline](#test-baseline-authoritative).**
The `main` baseline it is measured against is DGX `f97991a6` (= #656's tip) — **4009 / 1 / 55**, 176
suites, 4 `[SKIP]`. The 1 was `scheduler_ask_expiry_e2e::an_unanswered_ask_expires_and_fails_its_task_without_a_restart`,
a 60-second poll that missed under full-workspace load and passed **2 / 2 in isolation**; flaky under
load, not a regression, untouched by either branch (last: #579). If it recurs, widen the poll deadline
at `core/tests/scheduler_ask_expiry_e2e.rs:193` rather than re-running until green.

> ⚠️ **A slow Mac cargo build is CONTENTION, not the `_dyld_start` wedge.** The two states are
> **not** distinguishable by `sample` alone — a thread that is never *scheduled* shows the same
> single `_dyld_start` frame, because `sample` reports where a stack is, not why it is not moving.
> What separates them is **load**: check `uptime` and the `%cpu` in `time` output first. A wedge
> burns no CPU *and never finishes*; contention burns little CPU and finishes (measured: load average
> 22.68, `4% cpu, 13:34.55 total`, then **115 / 0**). The hazard below is real and has cost a session;
> it is just not the first hypothesis for "slow".

---

## Current state

### The two DGX gates #660 owed — RUN, and the Firecracker one found three defects

Branch `test/w2-microvm-uid-drop-gate`, PR [#669](https://github.com/hherb/kastellan/pull/669). The gate was described in this file as
"pure verification, not code". It was not: **the Firecracker backend had been completely dead since
the audit merged**, in three independent ways, each masking the next, and every one of them
surfacing as the same contentless `Protocol(EarlyExit)`.

- **1. The VMM jail was refused by bwrap, so no micro-VM spawn ran at all.**
  `linux_firecracker::confine::build_vmm_jail_argv` pushed a bare `--disable-userns` beside
  `--unshare-all`. That is [#661](https://github.com/hherb/kastellan/issues/661) exactly — bwrap
  validates the pair at *option-parse* time (`bwrap: --disable-userns requires --unshare-user`,
  exit 1), so it is a refused jail, not a weaker one — **in a third producer the #661 fix missed**.
  The `USERNS_LOCKDOWN_FLAGS` const introduced to stop the probe and the spawn drifting had a doc
  comment reading "every jail passes", while this jail spelled the flags out by hand. Fixed by
  extending the const's use; pinned by a parity test that asserts **against the const**, never a
  copied literal. **Count the producers, and make the const the only spelling.**

- **2. The pinned guest kernel has no Landlock, and the audit made that fatal.**
  `CONFIG_SECURITY_LANDLOCK is not set` — established **without booting anything**, by unpacking
  the pinned `vmlinux`'s embedded IKCONFIG [[dgx-guest-kernel-config-inspection]]. Audit F2 turned
  an unenforceable ruleset from a silent downgrade into an error, so every VM worker refused to
  start during its own lockdown. Fixed the way that error message itself prescribes: the launch
  plan states the exception (`KASTELLAN_LANDLOCK_PROFILE=none` for the guest), as a **default and
  never an override**, so a caller keeps a profile it named and a future kernel is opted back in by
  deleting the injection. `docs/threat-model.md` claimed the VM worker "still installs its own
  Landlock + seccomp-bpf inside the guest"; half of that was never true on this path and is now
  corrected rather than quietly carried. Seccomp is unaffected (`CONFIG_SECCOMP_FILTER=y`).
  Restoring the layer = repinning a guest kernel: [#668](https://github.com/hherb/kastellan/issues/668).

- **3. The relay sockets stayed root-owned across W-2's privilege drop.**
  The guest init binds the egress (1025) and embed-broker (1026) relay UDSes in `/run` **as root**,
  then drops to the worker uid. `connect(2)` on an `AF_UNIX` socket needs **write** permission on
  the socket file, so every networked VM worker died on its first dial — 7 tests across net-demo,
  web-fetch, web-search and both web-research tiers, while the non-networked half passed. The guest
  reports it as `connect proxy uds: Permission denied (os error 13)`, which **names the proxy and
  not the cause**. The decision of which paths the worker must own is now the pure
  `cmdline::worker_owned_paths` (testable on any platform); the Linux-only `chown` loop just applies
  it. The relay entries are conditional on their relay being enabled, mirroring when `/run` is
  mounted at all — an unconditional chown would fail on every plain VM boot and train everyone to
  ignore the line.

- **W-2 is now proved from inside the guest, not by reading the code.** Every other assertion in the
  Firecracker suite passes just as happily with the worker running as uid 0, and **both** halves of
  the compatibility path are deliberately quiet (a pre-fix rootfs ignores the variable; a host that
  stops sending it leaves the worker root) — each says so only on the guest's stderr, which nothing
  drains. `microvm_worker_runs_unprivileged_with_no_new_privs` reads four facts from the running
  guest: `uid != 0`, `euid == uid` (a drop that moved only the real uid leaves `setuid(0)`
  reachable), `uid ==` the **host's** euid (a guest picking its own would also unstick the 0600
  ro-share files the host stages), and `NoNewPrivs == 1`.

- **Review round (2026-09-05) — the fix's own test was vacuous, and one of its grants was not
  needed.** `worker_owned_paths_keeps_rw_mountpoints_and_their_anchors` used the fixture
  `kastellan.mounts=rw:vdb:/data/scratch`, which is neither hex nor tab-separated, so
  `parse_mount_manifest` fell back to the DEFAULT empty manifest and the loop over `m.rw` never
  entered its body. **Deleting the entire RW/anchor block left all five tests green** — and that
  block is the *pre-existing* behaviour the refactor had just moved out of
  `drop_privileges_for_worker`, so it had no coverage at all. All five are now exact-set
  `assert_eq!`s over the whole returned `Vec`, which additionally kills the `egress || broker`
  collapse that every `contains` probe missed. **A non-hex cmdline fixture fails OPEN, silently:**
  it does not error, it yields an empty manifest. [[unreachable-success-path-proves-nothing]]

- **`/run` is out of the chown set.** It was justified as "the worker needs to traverse and write in
  it"; **measured**, a tmpfs mounted with no `mode=` comes up **1777** whatever the mounting umask,
  so both were already true. The chown only handed the worker ownership of a *sticky* directory —
  which is exactly what lets the owner unlink entries it does not own. `connect(2)` needs write on
  the socket **file**; the two socket entries are the whole fix. Proven live, not argued: with `/run`
  removed, all four egress suites AND the broker suite pass. Guest `/run` still coming up
  world-writable is [#672](https://github.com/hherb/kastellan/issues/672).

- **The Landlock opt-out is now pinned to both things it is a claim about.** Its own literal (a
  hand-copied mirror of the prelude's `LANDLOCK_PROFILE_ENV` that every other test interpolated on
  *both* sides, so a rename stayed green) and the guest-kernel sha256 (the injection is
  unconditional and the other tests assert it is *present*, so a pin bumped to a Landlock-capable
  kernel would leave the layer off forever, greener than before). It also **WARNs per spawn** now:
  `tool_host::warn_lockdown_overrides` inspects the derived policy *before* the backend runs, so the
  one production Landlock disable in the tree was the one that mechanism is blind to.

- **W-2 is now proved further in, and the extra assertions were mutation-tested.** The e2e read
  uid/euid/NoNewPrivs; it now also reads the **saved-set uid** (the field that actually makes
  `setuid(0)` unreachable — `euid == uid` does not, and the init's own post-drop self-check cannot
  see it), the gid, the supplementary set, and **`Seccomp: 2`**, the layer the Landlock opt-out's
  whole justification rests on and which nothing else in the tree observed. Deleting
  `setgroups` + `setgid` from the init and rebuilding the rootfs was measured: it turns `gid` red
  (0 vs 1000) and **every pre-existing assertion still passes**. ⚠️ The `groups` assertion is a
  regression guard, not a proof — guest PID 1 has no supplementary groups, so it holds either way,
  and it is annotated as such rather than counted.

- ⚠️ **A stale rootfs image gates nothing, and fails like a code regression.** Every image bakes its
  own copy of `kastellan-microvm-init` **and** the worker, so a guest-side change is invisible until
  that image is rebuilt. `kv_demo_firecracker_persistent_e2e` failed as "persistent store must
  survive a VM respawn" against a June image and passed unchanged after a rebuild. The whole W-2
  gate could have been run against stale images and reported green having tested none of it. Filed
  as [#667](https://github.com/hherb/kastellan/issues/667). Note the build scripts live in **two**
  directories (`scripts/workers/microvm/` but `scripts/workers/kv-demo/`).

- ⚠️ **The launcher discards firecracker's stdout/stderr to `/dev/null`, so nothing above is
  readable.** The guest console *is* firecracker's stdout (`console=ttyS0`), and every
  `microvm-init` diagnostic goes there; `--log-path` catches neither it nor anything firecracker
  writes before opening that file. The run dir self-cleans on graceful exit, taking `fc.log` with
  it. This is why three distinct defects all looked identical, and it cost most of the session.
  Filed as [#666](https://github.com/hherb/kastellan/issues/666).
  **The technique that did work**, if you need it before #666 lands: snapshot the per-spawn run dir
  from a tight poll loop while the test runs — and **keep re-copying**, because the dir is created
  *before* firecracker writes `fc.log`, so a copy-once poll captures an empty dir and lies about it.
  Then replay `fc.json` under a hand-built bwrap argv outside the test.

- **Gate result: 19 pass / 2 fail across the Firecracker suites, from 0 / 21.** Both failures are an
  **absent local SearxNG** on `127.0.0.1:8888` — `ss -ltn` shows nothing listening and the audited
  row reads `egress.allowed … connect_failed: Connection refused`, so they are the known
  stand-up-SearxNG gap, not code. Rootfs images for all seven workers were rebuilt first.
- **The second gate — live-Matrix — passes as far as a host can take it.** `cargo clippy -p
  kastellan-worker-matrix --all-targets --features live-matrix --locked -- -D warnings` exit 0, and
  its **27** tests pass. The scoping reads correctly: both sides of `peer_allowed` lowercase, and
  `room_is_two_party` counts invited-but-not-joined members and fails **closed** on a member-list
  error. ⚠️ **What is still owed is the LIVE DM ROUND-TRIP**, which needs a deployed daemon carrying
  #660's scoping — the property most at risk from it. The DGX is still on `121f22a2`.
- ⚠️ **BOTH `cfg` blindnesses fired again on this one branch, in opposite directions** — the same
  pair the #653 session recorded, so treat it as the rule and not the exception.
  (1) `worker_owned_paths` lives in the **cross-platform** `cmdline` module but is called only from
  the Linux guest path, so it needed the `dead_code` allowance every other item there carries —
  **the DGX is structurally blind to it** (proved by removing the attribute and watching Mac clippy
  fail). (2) The same refactor left `parse_mount_manifest` unused in `guest.rs`, which is
  `cfg(linux)` — **the Mac is blind to that one**, and the DGX `-D warnings` gate is what caught it.
  [[cfg-linux-e2e-deadcode-dgx-clippy]]

### #660 — the second pre-release security audit (2026-09-02), MERGED + its three defects fixed

Full write-up in [`docs/security-audit-2026-09-02.md`](../../security-audit-2026-09-02.md); every fix
in one ROADMAP line; the verbose version of this section in [`archive/handover_20260903_653_pre-prune.md`](archive/handover_20260903_653_pre-prune.md).
**MERGED `62d98a00`**, 29 fixes, 80 files. What still binds:

- **The four load-bearing fixes.** (H1) the dispatch chokepoint scrubs every redeemed secret out of
  the worker's `Ok` value **and** its `RpcError` — shell-exec's allowlist denial was handing a
  substituted `secret://` ref's plaintext to the planner and `audit_log`. (H2) agent-raised
  `l1_insight`s are screened by the strict catalogue at promotion (audited `l1.injection_blocked`)
  and at prompt assembly. (H3) every per-spawn `/tmp` dir is minted with
  `kastellan_sandbox::private_dir::create_private_dir` (exclusive `mkdir` 0700, owner-verified) and
  secret files with `O_EXCL` 0600 — **a pre-planted name from another uid FAILS THE SPAWN CLOSED;
  that is the contract, do not "fix" it back to `create_dir_all`**. (H4) seccomp admits `clone` only
  without `CLONE_NEW*` and answers `ENOSYS` to `clone3`.
- **Three lockdown behaviours are FAIL-CLOSED and will bite a careless fixture:** a missing
  `KASTELLAN_SECCOMP_PROFILE` is an error (`none` is the explicit opt-out), an unenforceable Landlock
  ruleset is an error (`KASTELLAN_LANDLOCK_PROFILE=none` opts out), and a corrupt `kastellan.env=`
  guest token refuses the VM boot. A new probe invocation must set both `none`s to exercise rlimit alone.
- **Every networked stdio worker builds its handler INSIDE `serve_stdio_with`.** Landlock is
  per-thread; a tokio/reqwest runtime built in `from_env()` ran unrestricted on the threads that parse
  the network. Brokers build transport after `lock_down`; Matrix restricts each runtime thread in
  `on_thread_start`. **Keep that order for any new worker.**
- **CodeQL reads NAMES.** Five `rust/cleartext-logging` alerts fired on the guest's privilege drop
  merely for interpolating a numeric `uid`. Keep identifier- and credential-like names out of log and
  panic text. And the **live-matrix clippy job** catches lints the default-feature workspace clippy
  never compiles — run
  `cargo clippy -p kastellan-worker-matrix --all-targets --features live-matrix --locked -- -D warnings`
  before pushing anything touching `sdk_live.rs`.
- **Its three real-bwrap defects are FIXED on `main` via #656** — [#661](https://github.com/hherb/kastellan/issues/661)
  (`--disable-userns` beside `--unshare-all` alone is refused at bwrap's option-parse time, because
  `--unshare-all` sets only the try-flag; the probe spelled the flag out, so it passed while **every**
  spawn died — 66 failures across the 23 sandbox-spawning suites, and no skip guard can see a
  parse-time failure). [#662](https://github.com/hherb/kastellan/issues/662) — **any `pre_exec` closure
  forces Rust std off `posix_spawn` onto its fork path, which opens a `socketpair` the `strict` profile
  pins out**; reach for `process_group`/std attrs instead. Plus three `secret_vault_e2e` tests that
  asserted the pre-H1 plaintext echo. ⚠️ **A core e2e does not rebuild a worker package** — `cargo test
  -p kastellan-core --test python_exec_e2e` runs the *stale* worker; build the worker package first.
- **Cargo.lock moved:** `h2` 0.4.15→0.4.16 (RUSTSEC-2026-0258), `rustls-webpki` a direct proxy dep,
  python-exec's `libc` a runtime dep. `workers/browser-driver/requirements.lock` is tracked and
  hash-pinned; the next rootfs rebuild picks up playwright 1.62.0 (was 1.60.0) — **a bump to gate**.
  **Migration 0025** narrows the runtime role's UPDATE on `pairing_codes` to `(consumed_at, consumed_by)`.
- **DGX gates still OWED** (the audit box had no bwrap/Landlock/KVM/unprivileged PG): the **Firecracker
  e2e** (guest init drops to the daemon's euid, chowns RW mounts `nosuid,nodev`; run dirs 0700; images
  0600) and the **live-Matrix path** (`--features live-matrix`; invites from outside
  `KASTELLAN_MATRIX_PEERS` declined, two-party rooms only — verify a DM still round-trips). The
  real-bwrap leg ran 2026-09-03 and is what found #661 + #662.
- **Deferred with a reason** (all in the audit doc): brokers not force-routed; the guard tier never
  sees bytes past 64 KiB / `fetch_handoff` slices; `secret://` refs not tool-bound; `Host:` ≠ CONNECT
  authority (fronting); `net_client` grants `bind`/`listen`; no email-replay freshness window; no
  gliner weights revision pin; macOS worker-side caps; force-routing opt-in.
  **Recommendation before release: flip force-routing to default-on.**

### #649 / #651 — the transformers advisory, compressed

Merged `ef8144f8`. Full prose in
[`archive/handover_20260902_649_pre-prune.md`](archive/handover_20260902_649_pre-prune.md).

- **The remedy an advisory states can be a no-op that exits 0.** `uv lock --upgrade-package
  transformers` reached **5.6.2 — still inside the vulnerable range** — because `gliner 0.2.27`
  capped it. Both floors moved in **`pyproject.toml`**, making the vulnerable range *unsatisfiable*.
  Now transformers **5.13.1**, gliner 0.2.28. New `python-lock-check` CI job runs
  `uv lock --check --offline`; it catches a **weakened floor**, not an advisory.
- **A skip-as-pass knob that only reads itself from inside the test is not a gate** —
  `conftest.py`'s `pytest_sessionfinish` guard, with `trylast=True` on
  `pytest_collection_modifyitems` load-bearing (`-k` deselection is itself such a hook).
- **A `[SKIP]` hid a dead fixture for months** — the DGX `.venv` was a macOS copy
  [[gitignored-venv-can-be-from-the-wrong-os]]. Its rebuild is what surfaced #650, and the
  can't-force-it half is what #653 (below) closes.

### #650 — the interpreter alias bind, MERGED (`c03ec1a3`, PR #656)

A **production** defect in a shared pure function reached by **two** workers. Full prose in [`archive/handover_20260903_653_pre-prune.md`](archive/handover_20260903_653_pre-prune.md).
`uv` lays a managed CPython out as `cpython-3.13.14-…/` with a minor-version **symlink alias**
`cpython-3.13-…` beside it, and the venv's `bin/python` — hence every console-script shebang — names
the **alias**. `resolve_interpreter_root` canonicalized, so only the `.14` directory bound and
`execve` returned **ENOENT for a file that is present and readable**. What still binds:

- **The function had two jobs and one return value.** Now `InterpreterRoot` with two named accessors:
  [`dep_walk_prefix`](../../../core/src/workers/interpreter_deps/root.rs) (canonical — `ldd`/`otool`
  output is canonical, so an alias here would misclassify the interpreter's own libraries) and
  `bind_paths` (canonical **plus** aliases). `interpreter_lib_dirs` takes the whole `InterpreterRoot`
  and picks for itself, so the transposition is not expressible. Same shape as #641/#643.
- **The admission rule is non-widening, and that is the load-bearing choice.** An alias binds only
  when it **canonicalizes to the canonical prefix**. Homebrew is the counter-example the tests pin:
  `/opt/hb/bin/python3.12` names the far larger `/opt/hb` and is **refused**; a prefix that does not
  canonicalize is refused too. **A containment fix must not widen containment.** The proof is taken at
  *resolve* time while `spawn_under_policy` re-resolves every `fs_read` source at *spawn* time, and an
  alias is by definition mutable state on the bind path — a residual, not a break, filed as
  [#659](https://github.com/hherb/kastellan/issues/659).
- ⚠️ **`Path::components()` strips INTERIOR `.` only** — it keeps a leading one on a relative path, so
  the `CurDir` arm is unreachable *because every production caller passes an absolute path*, not
  because `components()` normalizes it. The over-general version shipped as a comment.
  **Say why a line is unreachable, not that it is.** [[rust-path-components-normalizes-dot]]
- **15 mutants killed, and the inventory still stopped one layer short**: every root reachable from an
  entry-level test was `canonical_only`, where the two accessors agree, so **the pre-#650 line** passed
  the whole suite. [[mutation-proof-counts-only-mutants-you-tried]]
- **`browser_driver_e2e.rs` was a fourth hand-rolled copy** and would have silently missed the alias.
  **Count the call sites when a fix says "all N of them".**
- **The reusable debugging technique**: nothing drains a worker's piped stderr, so dump `bwrap_argv`
  out of `linux_bwrap::spawn_under_policy` and **replay it verbatim** — parse the `{:?}` form with
  `ast.literal_eval`, because `join(" ")` + `eval` mangles `KASTELLAN_LANDLOCK_RW=["/tmp"]` and hands
  you a *different*, wrong error. `journalctl -k | grep type=1326` rules out a seccomp kill in one command.
- Review follow-ups open, none blocking: [#657](https://github.com/hherb/kastellan/issues/657) (the
  module emits no diagnostic at all), [#658](https://github.com/hherb/kastellan/issues/658)
  (transposable same-typed probes; `ResolveCtx` was deliberately not given a `read_link` field —
  29 construction sites for one probe — and **the honest reason is cost, not purity**),
  [#659](https://github.com/hherb/kastellan/issues/659).

### #653 / #654 — the gliner-relex e2e require knob, and the flag dialect

**MERGED `9ace57ad`**, PR [#663](https://github.com/hherb/kastellan/pull/663). Both issues closed. The point is not the knob; it is that **#651's fixture bug was only findable because a
human happened to rebuild a venv**, and nothing in the tree could have demanded it.

- **`KASTELLAN_GLINER_RELEX_REQUIRE_E2E` now works on the Rust side**, meaning exactly what it means
  on the Python side: each unmet precondition becomes a **panic naming itself** instead of a `[SKIP]`.
  **Six** are covered, not the issue's five — the opt-in flag, sandbox, supervisor, venv shim, weights,
  **and the Postgres bring-up**. Without a cluster the test body never runs either, so leaving that one
  skip-only would have left the knob reporting green on the very premise it abolishes. The shared
  `pg_bin_dir_or_skip` / `skip_if_no_supervisor` stay skip-only for their **~70** other callers (234
  call sites across 69 files, counted — an earlier "~30" in this file and in three code comments was
  low by half); the decision is made in each suite's own `bring_up_pg` via
  `report_unmet(require_action(), &reason)`.
- **#654 was a real operator-facing skew, not a tidy-up.** The fixtures gated on `!= Some("1")` while
  production reads the same variable through `env_flag_enabled`, so a `kastellan.env` legitimately
  saying `KASTELLAN_GLINER_RELEX_ENABLE=true` — which **does** enable the daemon worker — produced a
  silent skip. Proved A/B: with `REQUIRE=1` and `ENABLE` unset the panic names the flag; with
  `ENABLE=true` it gets **past** the flag, which before this branch it did not. The `resolve.rs`
  comments that taught the strict rule (and are how the skew propagated into #651's Python) are fixed.
- **One new module, [`tests-common/src/gliner_e2e.rs`](../../../tests-common/src/gliner_e2e.rs)**,
  holding the whole host-mode cascade **and** the `GlinerRelexEnv` all three suites built identically.
  That folds in the last triplicated `resolve_worker_script`. Not tidiness: these are the copies that
  drifted in **both** #284 and #650, one keeping `interpreter_root: None` after the other two were fixed.
- **A five-agent review round found the knob had a hole, and the tests could not see it.** Both were
  fixed on this branch before merge; both were proved by execution rather than argued.
  - **The hole:** `memory_entity_link_e2e.rs` opened each of its six tests with a skip-only
    `skip_if_no_supervisor()`, *ahead* of everything require-aware — so `gliner_host_env`'s own
    supervisor check was unreachable in that file. Reproduced on the DGX:
    `XDG_RUNTIME_DIR=/nonexistent … REQUIRE_E2E=1 cargo test --test memory_entity_link_e2e`
    → **`6 passed`, zero models loaded, knob set**. The exact false green the knob exists to abolish,
    in the very suite this section names as the copy that drifted. Fixed structurally, not locally:
    the supervisor check moved *into* `bring_up_pg` beside the Postgres one, and
    `skip_if_no_supervisor` is no longer imported by either suite — if the file cannot name the
    skip-only helper, the ordering cannot come back. Same repro now panics naming the precondition.
  - **The blind spot:** the 12 new tests covered the pure leaves but nothing above them. Two mutants
    survived a full green run — `require_action()` returning a constant (the knob **permanently
    inert**, #653 silently reverted) and deleting `report_unmet`'s `eprint!` (every `[SKIP]` line
    gone, so `grep -c '^\[SKIP\]'` reports a *clean* run — strictly worse than the bug #653 fixed,
    because the audit workflow this tree relies on would then be actively lying). The cause was that
    `require_action`, `gliner_host_env`, `venv_shim_or_reason`, `workspace_root` and `report_unmet`'s
    Skip arm had **no test at all** — the mutant inventory had been drawn from the *tested* functions
    rather than the *changed* ones.
  - **What made them testable:** `report_unmet_to(action, reason, &mut dyn Write)` (so a test can
    prove the Skip arm *emits* the line, by reading bytes back from a `Vec<u8>` instead of adding a
    real `[SKIP]` to the count it protects), and splitting `gliner_host_env` into a pure
    `first_unmet_precondition(gate, flag, …four `FnOnce` probes)` plus a pure `host_env_from`. The
    `FnOnce` probes are what let a test `panic!()` inside an un-run probe to prove short-circuiting —
    the "we don't spawn `bwrap` unnecessarily" claim had been a doc comment only. **All six mutants
    now die**; `use_container_backend: false` is pinned for the first time.
- **The reusable pattern: `*_or_reason` siblings.** `sandbox_unavailable_reason`,
  `supervisor_unavailable_reason`, `weights_dir_or_reason`, `pg_bin_dir_or_reason`,
  `venv_shim_or_reason` return the reason **without rendering a verdict**; the `skip_if_*` forms are
  now thin wrappers that print `[SKIP]`. That is what lets one caller skip where another must fail.
  `skip_if_sandbox_unavailable` became `cfg`-free as a side effect.
- ⚠️ **BOTH `cfg` blindnesses fired in this one branch, in opposite directions, and each was invisible
  to one host.** (1) A mutant naming the **wrong shim binary SURVIVED** on the Mac because the name
  lived inside the `cfg(target_os = "linux")` arm — lifted out as `LOCKDOWN_SHIM_BIN`, after which it
  is killed on both hosts. (2) Five imports left used only by the **macOS-only container tier** were
  three unused-import warnings on the **DGX** and a clean Mac clippy — a `-D warnings` CI failure the
  Mac could not see (`0bf9f5e5`). **Neither host can gate the other's arm.**
  [[cfg-linux-e2e-deadcode-dgx-clippy]]
- **Scope kept tight on purpose:** the macOS **container** tier (container CLI + image) stays
  skip-only and stays in `gliner_relex_e2e.rs`. It is a separate, much heavier opt-in; folding it in
  would make the knob unusable on a Mac with the venv staged but no image.
- ⚠️ **A unit test faked a `[SKIP]` line, and the DGX gate is what caught it** — the first sweep
  reported **5** where four were real, because the test pinning the skip arm called `report_unmet`,
  which *prints*. Every `[SKIP]` now renders through the pure `tests_common::skip::skip_line`, so the
  wording can be pinned without emitting anything. **Assert on `skip_line`; call the `skip_if_*`
  wrappers only from real fixtures.** See [Test baseline](#test-baseline-authoritative).
- **12 new tests, all in `kastellan-tests-common` — which CI runs on every PR** (`cargo test -p
  kastellan-tests-common`), so unlike the e2es they are a real gate. 12 mutants tried, 12 killed.
- **[#510](https://github.com/hherb/kastellan/issues/510) was already CLOSED** (2026-08-03,
  `fba4102c`) — this file listed it as open work for a month. Its `REQUIRE_USER_MANAGER=1` idea is
  the same shape as this knob if anyone wants the supervisor half.

### Merged work, compressed — the guard arc and the 2026-09-02 deploy

Full prose in [`archive/handover_20260902_650_pre-prune.md`](archive/handover_20260902_650_pre-prune.md)
and the snapshots before it. Only the findings that still bind:

- **Reading the live guard rows.** `fastest_tok_per_s` is **absent** from the installed binary and
  that is CORRECT (the durable wire key stayed `tok_per_s`); grep `slowest_tok_per_s` /
  `measured_samples` with a **substring** match. **Per-dispatch guard records are a `guard`
  SUB-OBJECT**, so `WHERE action LIKE 'guard%'` finds only the five boot rows and reads as "never
  screened anything" — the honest query is `WHERE payload ? 'guard'`. The whole-fail-open query is
  `state NOT IN ('clear','block')`, **not** `error_kind IS NULL`; `TimeoutBasis::Operator` carries a
  `PinBand`, so use `LIKE 'operator%'`. The `kastellan.env` clobber ritual is **RETIRED** on the DGX.
- **#624's thesis proved on the host it was filed about**: one post-arc boot spread **4 765.7** against
  **1 450.4** tok/s — **3.29x inside a single boot** — where both pre-arc single-sample boots sat at
  this boot's floor, making the derived timeout **3.4x too generous** every time. **`TimeoutBasis::Saturated`
  does NOT mean every sample stalled.** **Not yet observed:** #626's retry on a genuinely stalled backend.
- **#641/#642/#643 were one failure mode at three layers — a same-typed neighbour transposable in
  silence.** #642: a character-identical `validate_service_name` behind each platform `cfg` meant
  **neither host ever ran the other's** — the third, fourth **and fifth** copy;
  [#646](https://github.com/hherb/kastellan/issues/646) records two still hand-rolled, and the shared
  predicate is *stricter*, so tightening `bring_up_pg_cluster`'s call sites without an audit turns
  passing tests into panics. #641: **deleting beat newtyping**, ⚠️ but `DaemonSpec::new` now reads the
  environment and a test daemon's unit suffix no longer matches its sibling PG cluster's — restore with
  a `.suffix()` setter, **not** by reverting [[issue-as-filed-can-carry-a-regression]]. #643: **a swap
  silences rather than inverts.** Also open: [#644](https://github.com/hherb/kastellan/issues/644).
- **Three rules that each cost a session.** A blind `sed` would have broken production:
  `\btok_per_s\b` does not match inside `slowest_tok_per_s` but **does** match `"tok_per_s"` — the
  reporting vocabulary is **frozen at `tok_per_s`**. **Making a distinction representable is not the
  same as making the wrong side of it unreachable** (#634's first fix was itself a regression: a bare
  `Verbatim` narrowed a variable a `strip_suffix`+append pair had been *normalising*). **When a fix's
  value lives in a fold, pin the fold's *inputs*.** And a budget relation belongs in a compile-time
  assertion **beside the constants** [[cfg-test-const-assert-is-not-a-release-guard]].

> ⚠️ **#624 and #626 do NOT close [#612](https://github.com/hherb/kastellan/issues/612), and merging
> them is the mistake to avoid.** #624 removed the *contention* error; #612 is that extrapolating
> from a ~1 KiB sample is non-linear **on Metal whatever the load**
> [[metal-prompt-processing-is-nonlinear]]. Both point at the same remedy: measure from the `ms` /
> `body_byte_len` the guard rows carry since #616. ⚠️ **#614's merge wrongly CLOSED #612 and #615**
> via "Filed, **not fixed**: #N" — see [Standing hazards](#standing-hazards-that-have-each-cost-a-session).

### The guard tier itself — what still binds

- **D10 — the tier is ADVISORY defence-in-depth, NOT a gate.** 65% recall (36/55) at FP-0; 6/6 on
  bare imperatives but **5/8 missed** on narrative framing. **Nothing downstream may relax on it.**
- **τ = 0.79552656 is a REQUIRED operator input with no default**, and **five misconfigurations STOP
  THE DAEMON** (D6): half-configured keys, τ outside `(0.0, 1.0]`, a pinned timeout of 0, an
  unreachable `/props`, and a context below `SCAN_BYTE_CAP + 512 = 66 048` (D8).
  `KASTELLAN_REQUIRE_GUARD=1` makes the *unconfigured* case fatal too.
- **Measurement 3 ([#606](https://github.com/hherb/kastellan/pull/606))** — 133 cases, FP-0 on both
  hosts. `best_tau` returns **NONE**: real captured content overlaps at every threshold. Its
  security-prose stratum was **catalogue-selected**, which is why **corpus growth from production is
  now the cheap path** — harvest it before designing another campaign. `RouterConfig` lost its `Eq`
  derive (`guard_tau: Option<f32>` can hold a NaN).
- **`AuditSink::insert` is a provided method applying `truncate_payload` before delegating to
  `insert_stored`**, so no sink double can record a payload Postgres never stored
  [[audit-sink-doubles-hide-storage-transforms]]. Round one kept half the defect by dropping an
  unaffordable preserved key *silently* — **absence and loss must not render identically**.
- **The stated mitigation for an issue can disarm the instrument built to check it** — the live
  probe passed having measured nothing under a *pinned* timeout, precisely what #612 tells a Metal
  operator to use. It now refuses a pin outright.
- **The other four `screen` call sites** (`fetch_screen`, `inner_loop/summary`, `channel/ingest`,
  `recall_assembly/pg_builder`) keep catalogue-only behaviour, as does the core-initiated
  `gliner-relex` dispatch. Widening is a separate slice with its own blast radius.
- **[#585](https://github.com/hherb/kastellan/pull/585) `f90631da`** — two findings overturned the
  feasibility study and must not be re-derived from it: its `0.45–0.70` band holds exactly one
  reachable value, and `observation replay` is plan-level and cannot score a document-level tier.
  *A mock that does not return what it was sent tests only your own canned response.*
- **[#579](https://github.com/hherb/kastellan/pull/579) `bb937df7`** — D16's peer-scoped `EXISTS`
  inside the guarded UPDATE (**the nonce is a BEARER token — reading, not guessing, was the real
  threat**). Its five-agent review found eight things nine per-task reviews and 3522 tests had
  missed, all on the **argument-passing seams between layers**.
  **[#578](https://github.com/hherb/kastellan/pull/578) `af3e7e66`** — **D11** (`asks.resume_state`,
  migration 0024), because a resumed task otherwise re-executed steps it had already run.
- **[#572](https://github.com/hherb/kastellan/pull/572)/[#573](https://github.com/hherb/kastellan/pull/573)** —
  **a mutation score is only as good as the mutation set**: a reviewer's own 15 mutations left **11
  surviving** with all 113 tests green.
  **[#569](https://github.com/hherb/kastellan/pull/569)** — runtime + quantisation **PINNED**:
  llama.cpp + `Shieldstral-1.0-3B-Q8_0` on both hosts, so one fitted τ transfers.

### Standing hazards that have each cost a session

> ⚠️ **Clippy parity is a `rustup update`, not a property of the hosts — check it, don't assume it.**
> CI pins nothing (`dtolnay/rust-toolchain@stable`) and both dev hosts float on the same `stable`
> channel, so they drift out of parity silently simply by not being updated. That is what bit #573:
> clippy-clean on both hosts, then a CI failure on a lint the older toolchain did not have.
> `rustc --version` on the host you are gating on, compare against `rustup check`, `rustup update
> stable` if behind. **2026-08-31: both hosts on 1.98.0 = CI parity** (from 1.96.0), and the bump
> surfaced **zero** new lints on either — but that is a fact about this tree at this pair of
> versions, not a reason to skip the check. `rust-version = "1.78"` is the MSRV and constrains none
> of this.

> ⚠️ **A cached `cargo clippy` reports a full-workspace pass it never ran.** Exit code alone does not distinguish it — **count the `Checking` lines**. Honest from a cold `CARGO_TARGET_DIR` is ~217–303; a warm dir can report exit 0 having linted 4. Count against the *reverse-dependency set*, not against 27, or a correct incremental lint reads as a failure.

> ⚠️ **`cargo check`/`clippy --all-targets` do NOT warm the target dir for `cargo test`** — they emit metadata-only artifacts, no linked binaries. A full sweep after a lint-only leg pays a cold link (11m on the Mac vs 29s on the DGX). **Run the sweep first, lint after.**

> ⚠️ **A private `CARGO_TARGET_DIR` does not build `examples/`,** so `email_channel_e2e`'s 6 tests fail with `fixture not built` at a perfectly green commit. Fix: `cargo build -p kastellan-core --example fake_email_worker`. Same family as the daemon-e2e breakage a custom target dir causes ([[custom-cargo-target-dir-breaks-daemon-e2e]]) — read the failure text before believing a regression.

> ⚠️ **"Filed, not fixed: #N" in a PR body or commit message CLOSES #N.** GitHub matches the `fixed: #N` substring and has no notion of negation. It has cost three issues: #539 (2026-08-11, noticed), then **#612 and #615 together** (2026-08-24, unnoticed until the next session reconciled this file against `gh issue list`). Write **"deferred to #N"** or **"#N — filed, unfixed"**, and before merging run `gh pr view <n> --json body --jq .body | grep -oiE '(close[sd]?|fix(e[sd])?|resolve[sd]?)[[:space:]:,]+#[0-9]+'` over the body *and* the commit message.

> ⚠️ **Squash-merge caveat:** every PR lands as one squash commit, so its *branch-tip* SHA (where the gate ran) is **not** an ancestor of `main`. Check content, not `merge-base`.

> ⚠️ **Freshly-linked executables can hang forever in `_dyld_start` on macOS**, so every daemon e2e fails with the daemon's stdout **and** stderr **completely empty** — which reads exactly like a code defect. **Newness, not size:** the 40 MB daemon first, 13 KB `build-script-build` binaries later (wedging a cold `cargo clippy`), while anything already assessed kept running. Not the target dir and not signing — hanging, old and fresh-test binaries are all identically `adhoc,linker-signed`. **A warm `CARGO_TARGET_DIR` still works**, so `check` and `clippy --all-targets` remain available; a cold one is what wedges. Distinct from [[custom-cargo-target-dir-breaks-daemon-e2e]], which a `cargo build --workspace` fixes and this does not. [[mac-fresh-large-binaries-hang-in-dyld]]
>
> ⚠️ **The `sample` signature alone does NOT prove it — that mistake cost a wrong diagnosis in five documents (2026-09-02).** A thread merely never *scheduled* shows the same single `_dyld_start` frame, because `sample` reports where a stack is, not why it is not moving. At **load average 22.68** with another project running 16 `rustc` processes, a `kastellan-supervisor --lib` run took **13m34s wall at 4% cpu** and then **passed**. **Check `uptime` and `%cpu` first:** a wedge burns no CPU *and never finishes*; contention burns little CPU and finishes.

> ⚠️ **`kastellan-worker-egress-proxy` leaks on the Mac.** Five orphans were live in one sweep, four of them 1–7 days old, across three target dirs. Test runs are not reaping them. Not investigated — flagged for whoever next touches sidecar lifecycle.

---

## Read these first

1. [`docs/architecture.md`](../../architecture.md) — process model, cross-platform table
2. [`docs/threat-model.md`](../../threat-model.md) — the invariant, scenarios in scope, defence layers
3. [`docs/devel/ROADMAP.md`](../ROADMAP.md) — the master sequenced TODO with commit hashes for shipped items
4. The design plan (outside the repo) — `~/.claude/plans/i-d-like-to-design-logical-starlight.md`
5. Memory notes (auto-loaded) — `~/.claude/projects/-Users-hherb-src-kastellan/memory/MEMORY.md`
6. [`archive/handover_20260803_pre-prune.md`](archive/handover_20260803_pre-prune.md) — the full prose for everything this file now summarises

---

## Next TODO

> Only *open* work is listed. Shipped items move to [Recently merged](#recently-merged) or the ROADMAP.

**FIRST: the DGX redeploy, and the one gate half that needs it.** The Firecracker and
live-Matrix gates #660 owed have now RUN (PR [#669](https://github.com/hherb/kastellan/pull/669));
only one piece is left and it is blocked on a deploy, not on code:

1. ✅ **The DGX redeploy is DONE** (2026-09-04): `scripts/upgrade_from_git.sh` took it from
   `121f22a2` to **`9ace57ad`**, so #660 + #656 + #663 are all live. `installed 15 binaries`,
   `services: active active active`, `✅ Matrix channel is up`, and
   `channel bus running {channel:matrix, attempts:1}` on the fresh boot. **Both env files diffed
   IDENTICAL** across the install and force-routing is still baked into the generated unit — the
   env-clobber ritual stays retired [[dgx-deploy-env-clobber-and-missing-workers]].
2. ⏳ **STILL OWED: the live Matrix DM round-trip.** Everything a host can check passes
   (`--features live-matrix` clippy clean, 27 tests, scoping reads correctly), but the property most
   at risk from #660's invite/two-party scoping is that a **normal DM still round-trips**, and only
   a real message shows that. **Send the bot a DM from `@horst` and confirm it answers.** If it does
   not, the scoping is the first suspect — read `channel.boot_failed` audit rows and the declined
   log line naming `KASTELLAN_MATRIX_PEERS`, not the restart count
   [[channel-boot-one-shot-fixed]]. ⚠️ The Matrix worker's *guest/VM* mode is unaffected by this
   branch — the live channel runs under bwrap, not in a VM.
3. **Then re-check the two SearxNG-blocked Firecracker tests** if you want 21/21:
   `scripts/web-search/setup-searxng.sh`, then `KASTELLAN_WEB_SEARCH_ENDPOINT` + the `web-search`
   `tool_allowlists` row. Nothing about them is a code defect —
   [[web-research-e2e-endpoint-must-be-allowlisted]].

**Before touching the micro-VM backend again, read [#666](https://github.com/hherb/kastellan/issues/666)
and [#667](https://github.com/hherb/kastellan/issues/667).** They are the two reasons three
production defects hid in it for two days: nothing drains the guest's diagnostics, and a stale
rootfs image gates nothing while failing like a code regression. Fixing #666 first would make any
further micro-VM work dramatically cheaper. [#668](https://github.com/hherb/kastellan/issues/668)
is the standing posture item (repin a guest kernel that has Landlock).

**THEN, cheap and now overdue:** [#655](https://github.com/hherb/kastellan/issues/655) — `main` has
**no required status checks**, so clippy, the matrix build and the new `python-lock-check` gate can
all go red and still merge. That is a repo-settings change, not code.

**THEN: the guard arc's remaining work is one item and it is the one that matters:**
[#612](https://github.com/hherb/kastellan/issues/612), a design call rather than a patch — **#616
unblocked its favoured option**, so it is now reachable rather than merely filed. Read the
measurement in the issue before proposing a fix; every cheap one is closed off there. Beside it,
both cheap: [#639](https://github.com/hherb/kastellan/issues/639) (split `guard_tier_e2e.rs`, 1558
lines, also [#622](https://github.com/hherb/kastellan/issues/622)'s cheapest option — the probe half
would then fit a CI gate with no Postgres service container) and
[#638](https://github.com/hherb/kastellan/issues/638) (214 rustdoc warnings, 67 of them broken
intra-doc links, in a tree that treats doc comments as the design record).

**Next up — operator's choice, each roughly one session.** Full issue text is authoritative; these
are the gotchas that are *not* in the issues.

- **[#560](https://github.com/hherb/kastellan/issues/560) — the planner fabricates a 16-hex
  `message_id`.** Do **not** close it by rewriting the parameter description: #536 already did
  exactly that ("not a placeholder"), deployed 2026-08-09, and both later runs still fabricated. The
  lead worth measuring: with keys stripped by `extract_scannable_text`, `"20973"` reaches the planner
  as a bare line among subjects and dates, with nothing marking it as *the id*
  [[tool-output-reaches-planner-key-stripped]] [[opaque-ids-are-unusable-tool-params]].
- **[#550](https://github.com/hherb/kastellan/issues/550) — the *generated* `kastellan.env` gets no
  end-to-end check.** #531 verifies the optional overlay most hosts do not have and skips the
  required file every host does. **The naive fix is wrong** — the overlay legitimately overrides
  `kastellan.env` keys, so per-file comparison false-positives; it must compare the *folded*
  environment, which `fold_env_files` already computes for launchd.
- **[#551](https://github.com/hherb/kastellan/issues/551) — no path directive escapes systemd's `%`
  specifier.** Pre-existing and workspace-wide (`ExecStart=`, `Environment=`, not just
  `EnvironmentFile=`). Measure first, then either escape `%%` or reject at install.
- **[#548](https://github.com/hherb/kastellan/issues/548) — PG e2e tests install units into the
  operator's *real* `~/.config/systemd/user/`.** Not a teardown bug — `PgCluster`'s `Drop` guards are
  correct and simply cannot run on SIGKILL — so the fix is about blast radius. **Confirmed still
  accruing 2026-09-01**, and it is a slow leak rather than one historical accident: the DGX carries
  units from two *different* tests, 2026-06-21 and a `failed`
  `kastellan-test-seccli-1-726614-…`. `systemctl --user list-units --type=service --all | grep -i
  kastellan` shows them. ⚠️ **#641 removed the shared suffix between a test daemon's unit and its
  sibling PG cluster**, so a sweep can no longer correlate the two; if that matters, restore it with
  a `.suffix()` setter rather than by reverting the constructor.
- **[#519](https://github.com/hherb/kastellan/issues/519), [#554](https://github.com/hherb/kastellan/issues/554),
  [#534](https://github.com/hherb/kastellan/issues/534)** — see
  [Open follow-up issues](#open-follow-up-issues-filed-but-not-picked); each is a design call, and
  #554 needs a live DGX gate because it narrows what a deployed worker may do.
- **[#564](https://github.com/hherb/kastellan/issues/564)** — slices 1a, 1b and 2 are all MERGED.
  What remains under that heading is non-blocking.
- **Email channel — slices 2 and 3.** Slice 1 (gated inbound) MERGED, #503 closed its MITM gap. Spec
  `docs/superpowers/specs/2026-07-28-email-fallback-channel-design.md`. **Slice 2** = SMTP outbound
  (`lettre`, MIT-verified) + full round trip; today `EmailChannel::send` refuses and every refusal is
  audited `channel.reply_undelivered`. **Slice 3** = DGX deploy + live tier; **restart
  `localmail-serve` (+ `localmail-daemon`) on the DGX first**. Its deploy blocker is gone (`41b21f36`
  installs `kastellan-worker-email-in`).
- **A Mac daemon deployment is a deliberate decision, not a task.** The tier boots fine there (91.4 s
  derived, `n_ctx` 66 048) but #612 means it fails open on large documents. Decide #612 first, or
  deploy with a pinned timeout and say so.
- **Live guard-host facts** (verified 2026-08-23): the DGX guard server is `llama-server …
  Shieldstral-1.0-3B-Q8_0.gguf --alias shieldstral --port 8081 -c 131072 -ngl 99`; `/props` reports
  the per-request context at `default_generation_settings.n_ctx` with **no top-level `n_ctx`**.
  Restart it with **at least `-c 66048`** or the daemon refuses to boot. The three guard keys live in
  `~/.config/kastellan/kastellan.env.local`, which `install` never rewrites.
- **Corpus growth is now cheap, and that is new.** D5's per-dispatch `p` is live and survives on
  large documents since the audit-cap fix, so production is finally a score source with no catalogue
  selection in it. Harvest it before designing another capture campaign.
- **Deferred with a reason, not forgotten:** macOS Seatbelt-loopback verification of mail tier 1a
  (needs a Mac run with working launchd-PG); **Telegram inbound** (still rejected as primary — no bot
  E2E, centralized, ban risk); **MITM-of-browser** via a proper NSS trust-store import, **not**
  `--ignore-certificate-errors-*`, since production must not be loosened to make a test pass.


- **File-split backlog (Item 9b)** — **`wc -l` before picking; the numbers drift and any list here
  is a pointer, not a census.** The rule the tree follows: **split BEFORE the change that grows a
  file**, in a movement-only commit whose `#[test]` name set is verifiable either side, so the
  movement diff is reviewable on its own. Folding a move in afterwards is the worst of both.
  **#650's `interpreter_deps` split (this session) is the worked example to copy**; `timeout.rs`,
  `tier/boot.rs` → `tier/probe.rs` and `boot_supervisor/tests.rs` are earlier ones, and
  `boot_report/tests.rs` (686) is the counter-example.
  - **Best first picks, each a pure test-lift** (production code untouched, count verifiable either
    side): `core/src/channel/ask_message.rs` **956** (~330 production),
    `workers/mail/src/handler.rs` **670**, `sandbox/src/linux_firecracker/plan.rs` ~**1160**
    (`cfg(linux)`, so DGX-gated), and `core/tests/guard_tier_e2e.rs` **1558**
    ([#639](https://github.com/hherb/kastellan/issues/639)), whose ~200-line multi-request HTTP mock
    lifts to `tests/guard_tier_e2e/{main,mock}.rs`.
  - **Clean seam already visible:** `core/src/scheduler/asks.rs` **801** — its pure half
    (`resolution_choice` / `decide` / `ask_deadline_seconds` / the resume-state codec) separates from
    its async half.
  - **Judgement first, not movement:** `tests-common/src/daemon/spec/tests.rs` **599** — its
    production half was split in #645, but the five `LlmEndpoint` cases mostly assert *through* a
    built `DaemonSpec`, so decide whether they belong with the type or with the spec **before**
    splitting. Same for `db/src/asks.rs` **1127**, `db/graph.rs` **926** (design-gated Item 23b) and
    `llm-router/src/config.rs` **843**, where a small `mod tests` means a split is a production
    reorganisation.
  - **Also over cap, no seam called yet:** `core/src/scheduler/inner_loop.rs`, `core/src/channel/bus.rs`,
    `workers/matrix/src/sdk_live.rs` (live-matrix-gated → DGX), `llm-router/src/messages.rs`,
    `core/src/main.rs` (next lift: the bring-up block), plus the over-cap *test* files
    `gliner_relex/tests.rs`, `python_exec/tests.rs`, `inner_loop/tests.rs`, `scheduler/audit/tests.rs`
    and `cassandra/types/tests.rs`.

**Standing deferrals (no owner; pick up when a consumer appears):**

- **Egress** — [#242](https://github.com/hherb/kastellan/issues/242) tunnel idle/resolve timeouts (folds in the missing read idle-deadlines on `copy_bidirectional` + `peek_first_byte`); [#251](https://github.com/hherb/kastellan/issues/251) stale-scratch crash-sweep (needs cross-platform pid-liveness); [#304](https://github.com/hherb/kastellan/issues/304) real-sandbox cert-pin enforcement e2e (needs a controllable TLS origin); [#260](https://github.com/hherb/kastellan/issues/260) literal-IP HTTPS origins requiring an IP-SAN cert under MITM; transparent gzip/brotli if an origin refuses `Accept-Encoding: identity`; `pg_decision_sink` back-pressure decoupling before high-rate load.
- **True `jailer`** (root chroot + dedicated-uid drop) stays deferred to a privileged-tier `VmmConfinement::Jailer` sibling (seam already in `confine.rs`). **Generalizing net-worker-in-VM** needs no new work: 5c's `NetClientTransport`/`spawn_net_transport` IS the reusable mechanism; a 2nd consumer can adopt it directly.
- **5c/5b minors** — `spawn_net_transport`'s fail-closed-path doc-comment is subtly worded; DGX `net_demo_firecracker_egress_e2e` leaves `cpu_ms` at default (unused by the FC backend); [#381](https://github.com/hherb/kastellan/issues/381) (`size_mib` resize + mkfs↔flock TOCTOU); the `respawns_on_death_and_serves_again` unbounded-retry test wants a deadline guard.
- **python-exec Phase 4** — curated-wheels RO dir if/when skills demand third-party packages (stdlib-only today); tiered delegation policy (ROADMAP). Operator flip: `KASTELLAN_PYTHON_EXEC_ENABLE=1`.
- **web-search / web-research** — stand up a local SearxNG (`scripts/web-search/setup-searxng.sh`), set `KASTELLAN_WEB_SEARCH_ENDPOINT` + the `web-search` `tool_allowlists` row, run the `#[ignore]` `web_search_e2e::real_search_against_searxng`. web-research polish (all opus-triaged DEFER): `http.rs` trait doc stale; `search_err_to_rpc` gives a "search"-worded error on an *embed* misconfig; `embed_note` conflates three conditions under first-wins, so a benign cap note can mask a genuine embed failure (severity-rank it: failure > cap).
- **Entity-embedding** — an ANN index (ivfflat/hnsw) on `entities.embedding` once cardinality warrants it (sequential cosine scan today); a batch-embed seam behind the `Embedder` trait if embed latency becomes a recall-path cost.
- **handoff-cache** (ROADMAP:129) — on-disk Workspace-backed store, only once a per-task `Workspace` is wired into the live scheduler (it isn't today).
- **Older** (ROADMAP:130) — core-side caller wiring for `insert_memory_light` (lands with the first high-frequency writer); per-namespace caps + oldest-eviction on `memories.metadata`; graph-lane degradation test ([#196](https://github.com/hherb/kastellan/issues/196)).
- **Test-infra / small** — a `KASTELLAN_REQUIRE_USER_MANAGER=1` knob for the supervisor smokes is the same shape as #653's `KASTELLAN_GLINER_RELEX_REQUIRE_E2E` and unfiled since #510 closed; [#134](https://github.com/hherb/kastellan/issues/134) `bring_up_pg_cluster` doc example or a real `_with_timeout` caller; [#104](https://github.com/hherb/kastellan/issues/104) systemic de-doubling of the `pid+nanos` suffix — **six** places, counted properly: `tests-common::unique_suffix`, three `TestRoot`s (`systemd_user`, `launchd_agents`, `atomic_write`), both supervisor smoke binaries, plus `atomic_write::tmp_path_for` and `install::run::staging_path` (#511 collapsed the two backend copies of the first into one, and added the last); [#353](https://github.com/hherb/kastellan/issues/353) route read-only `launchctl print` through `run_capped`. (The `KASTELLAN_GLINER_RELEX_REQUIRE_E2E=1` knob shipped for the **Python** half in #651 and the **Rust** half in #663.)
- **Operator actions (no code)** — recapture observation fixtures (`cargo test -p kastellan-core --test observation_capture -- --ignored --nocapture`); real-model relation-extraction validation (`KASTELLAN_GLINER_RELEX_ENABLE=1 cargo test … entity_extraction_e2e`); live-pin direct-VM + literal-loopback-embed hybrid ranking ([#456](https://github.com/hherb/kastellan/issues/456)); live-verify the #454 forced-synthesis path ([#455](https://github.com/hherb/kastellan/issues/455)).

---

## Load-bearing findings that still bind

Full prose in the [`archive/`](archive/) snapshots — most recently
[`archive/handover_20260831_626_pre-prune.md`](archive/handover_20260831_626_pre-prune.md).

- **The four faults (2026-08-02).** Driven end to end from one real Matrix message: **four
  independent faults, only one a kastellan bug in the layer everyone suspected**, each masking the
  next. The durable lesson is the shape, not the four — a green stack with a silent output means
  look at every layer, and fix them one at a time so each fix's evidence is separable.
- **The fail-open `data_ceiling` correction — CLOSED, kept for the shape.** A cap that silently
  dropped what it could not fit produced a row indistinguishable from one where the thing never
  happened. **Absence and loss must not render identically**; name the refusal.
- **Egress / MITM traps (from #491–#503) — read before touching the proxy.** The proxy's MITM
  upstream trusts **webpki roots only**, so no hermetic self-signed origin is possible for a MITM'd
  worker's e2e; `extra_ca` is worker-side, for transparent-tunnel workers
  [[egress-proxy-upstream-trusts-webpki-only]]. A force-routed loopback endpoint needs an **IP SAN**
  — a DNSName holding an IP literal never matches, and the symptom looks like a sandbox failure
  [[macos-force-routed-loopback-needs-ip-san]]. A bare-host `Net::Allowlist` entry with no `:port`
  is an **all-port grant** [[bare-host-net-allowlist-is-all-port-grant]].
- **Deployment facts (DGX).** `install` REGENERATES `kastellan.env` and silently reverts tuned
  values; re-add the four keys and repair the model tag afterwards
  [[dgx-deploy-env-clobber-and-missing-workers]]. Force-routing IS baked into the generated unit and
  survives install [[dgx-force-routing-deploy-facts]]. Daemon logs are in
  `~/.local/state/kastellan/*.out`, not the journal. `scripts/upgrade_from_git.sh` does the whole
  build+install+restart+verify and is hardcoded to `main`. A kernel upgrade can drop the NVIDIA
  module and silently put Ollama on CPU, which looks exactly like a router bug
  [[dgx-apt-upgrade-drops-nvidia-module]].
- **Process lessons that have each cost a re-run.** A truncated gate log is not a gate — keep the
  full sweep in a file under `$HOME` and parse `test result:` with a regex
  [[truncated-gate-log-is-not-a-gate]]. Mutation testing contaminates the git **index**; `git diff
  --stat` afterwards is the only proof index == tree [[mutation-testing-contaminates-the-index]], and
  revert by copying the file, never `git checkout` [[mutation-revert-never-git-checkout]]. A PR body
  saying "not fixed: #N" **auto-closes** #N [[pr-body-not-fixed-autocloses-issue]]. Plan text is a
  defect source: subagents transcribe brief prose verbatim [[plan-text-is-a-defect-source]].

## Working state

### Test baseline (authoritative)

| Host | Commit | Result | clippy `-D warnings` | `[SKIP]` |
| --- | --- | --- | --- | --- |
| **DGX** (#669 after the review round — **the gate that stands**) | **`e35c3571`** (the one later commit adds only a comment on the `groups` assertion — no test, no behaviour) | `cargo test --workspace --no-fail-fast --locked -- --nocapture` **4049 / 0 / 56**, **176** suites, `TEST_EXIT=0`. **+1** over the 4048 below — the new `the_landlock_opt_out_is_pinned_to_its_kernel_and_key`; the five `worker_owned_paths` tests were replaced 5-for-5 by exact-set assertions, so the count is unchanged there. **Firecracker: 21 / 0, zero `[SKIP]`** — the first fully green run. The 2 that failed in the row below were the absent local SearxNG; the `kastellan-searxng` container had been down 5 weeks and was restarted (`127.0.0.1:8888`, HTTP 200). All **7** rootfs images rebuilt first — mandatory, since this branch changes the guest init | `-p kastellan-core -p kastellan-sandbox -p kastellan-microvm-init -p kastellan-microvm-run --all-targets --locked -D warnings` exit 0 after force-touching core + sandbox, so the `cfg(linux)` e2e was really linted. ⚠️ The first workspace clippy exited 0 having emitted **24** `Checking` lines in 6s — warm, not a gate. Mac: native + `--target aarch64-unknown-linux-gnu` clippy on all three touched pure-Rust crates, both exit 0 | **4**, all the gliner tier — held. **0** `[WARN]` |
| **DGX** (#669, the Firecracker gate — **the gate that stands**) | **`b492966b`** (content-identical to the rebased tip `a5b148a1`; the last two commits are a `dead_code` attribute and an unused-import removal, neither of which adds a test) | `cargo test --workspace --no-fail-fast --locked -- --nocapture` **4048 / 0 / 56**, **176** suites, `TEST_EXIT=0`. **The delta reconciles exactly: +8** over the 4040 below — 5 `cmdline::worker_owned_paths` tests, 2 `plan.rs` Landlock tests, 1 `confine.rs` userns parity test — and **+1 ignored** (55→56), the new `#[ignore]` Firecracker e2e. Separately, the **Firecracker suites went 0 / 21 → 19 / 2**, the 2 being an absent local SearxNG | **cold** `--workspace --all-targets --locked -D warnings` from a fresh private target dir: exit 0, **345** `Checking`+`Compiling` lines, all **27** kastellan crates, **zero** warnings. rustc **1.98.0**. ⚠️ **The first cold run FAILED** on `unused import: parse_mount_manifest` in the `cfg(linux)` `guest.rs` — invisible to the Mac, and the reason to keep running this cold and on both hosts. Mac clippy `-p kastellan-microvm-init --all-targets -D warnings` exit 0, and its `dead_code` allowance was proved load-bearing by removing it | **4**, all the `KASTELLAN_GLINER_RELEX_ENABLE` tier — held. **0** `[WARN]` |
| **DGX** (#663 after the review round — **the gate that stands**) | **`fixall`, pre-commit** | `cargo test --workspace --no-fail-fast --locked -- --nocapture` **4040 / 0 / 55**, **176** suites, `TEST_EXIT=0`. **The delta reconciles exactly: +18** over the 4022 below — 15 new `gliner_e2e` tests + 3 new `skip.rs` tests. Run twice: once before and once after moving a `#[cfg(test)] mod tests` to end-of-file, identical both times, which is the evidence that code motion was behaviour-neutral | **cold** `--workspace --all-targets --locked -D warnings` from a fresh private target dir: exit 0, **345** `Checking`+`Compiling` lines, all **27** kastellan crates, **zero** warnings. rustc **1.98.0**. ⚠️ The *warm* run exited 0 having linted **3** crates and would have missed the real lint the cold one caught (`items_after_test_module`, from a `mod tests` inserted mid-file) — a warm clippy exit-0 is not a gate | **4**, all the `KASTELLAN_GLINER_RELEX_ENABLE` tier — the pre-branch count, held. **0** `[WARN]` lines, i.e. the new out-of-dialect warning stays silent when the knob is unset. The skip line now echoes the value read (`ENABLE=<unset>`), per #654 |
| **DGX** (#663, pre-review-round — superseded by the row above) | **`32295a7f`** | `cargo test --workspace --no-fail-fast --locked -- --nocapture` **4022 / 0 / 55**, **176** suites, `TEST_EXIT=0`. **The delta reconciles exactly: +12** over the 4010 total below (4009 passed + 1 load flake), and 12 is the `tests-common::gliner_e2e` test count. `scheduler_ask_expiry_e2e` passed this time, as expected of a flake | **cold** `--workspace --all-targets -D warnings` from a fresh private target dir: exit 0, **345** `Checking`+`Compiling` lines, all **27** kastellan crates, **zero** warnings — and **zero rustc warnings during the test build**, which is what proves the macOS-only imports are `cfg`-gated. rustc **1.98.0** | **4**, all `KASTELLAN_GLINER_RELEX_ENABLE` not truthy — the pre-branch count, restored. ⚠️ An earlier run of this gate reported **5**: the fifth was a `tests-common` unit test calling `report_unmet(Skip, ..)`, whose skip arm prints. Fixed in `32295a7f`; see the rule below |
| **DGX** (#656 at the three fixes — the gate that stands) | **`f97991a6`** | `cargo test --workspace --no-fail-fast -- --nocapture` **4009 / 1 / 55**, **176** suites, `TEST_EXIT=101`; total **4010** reconciles (see header). The 1: `scheduler_ask_expiry_e2e` — flaky under load, **2 / 2 in isolation** afterwards, untouched by this branch or #660. `python_exec_e2e` **5 / 5**, `cli_memory_l3py_run_daemon_e2e` **6 / 6**, `secret_vault_e2e` **11 / 11**, `linux_smoke` **8 / 8** | `--workspace --all-targets -D warnings` exit 0, zero warnings. rustc **1.98.0** | **4**, all `KASTELLAN_GLINER_RELEX_ENABLE != "1"` |
| **DGX** (after `4269ff7e`, the #661 fix) | **`4269ff7e`** | **3997 / 13 / 55**, 176 suites, `TEST_EXIT=101`; total **4010** (+1, the probe/spawn parity test). The 13: python-exec SIGSYS on `socketpair` ×10 ([#662](https://github.com/hherb/kastellan/issues/662)) and the 3 `secret_vault_e2e` pre-H1 assertions. `cargo test -p kastellan-sandbox -- --nocapture` **173 / 0** incl. `linux_smoke` **8 / 8**, zero `[SKIP]` | exit 0, incremental (4 crates), zero warnings | **4** (gliner) |

**Rows for `5659bc8a` (the bare #660 merge, 3943 / 66 — every spawn refused), `a990e8ec` / `757413c1` (#656's own gates), the #651 Python legs and the audit container are in [`archive/handover_20260903_653_pre-prune.md`](archive/handover_20260903_653_pre-prune.md).**

**The row the delta above is measured against:** DGX `f12ed26d` (tree-identical to `main`
`121f22a2`) — **3940 / 0 / 55**, 176 suites, `TEST_EXIT=0`; cold clippy exit 0 with **345**
`Checking`+`Compiling` lines over 330 distinct crates, all **27** kastellan crates, zero warnings,
rustc **1.98.0**; **8** `[SKIP]`, all gliner-relex. ⚠️ **Nothing between it and the row above was ever
gated on the DGX** — `5445dd68` (3937/3, the #649 branch tip) predated #651's own review round, and
`main` `ef8144f8` was never swept. The #650 gate is the first Linux run to cover main's current
content, which is why the reconciliation is +28 and not +18.

Older rows (`466ca7ff` DGX **3928**, `553ec6ff` 3921, `6764d272` 3910, `8d92c02b` 3910, `c0255cd7`
3909, `d3f8ed3f` 3908, `12809297` 3901, `33029e32` 3900, `020b0e53` Mac 3778, `b65e44ab` 3890,
`8cb8cfb7` 3854, `09c6231f` 3840/3718, and 3047 back to 2950) are in the [`archive/`](archive/)
snapshots.

**Both hosts are load-bearing, in opposite directions — always check both.** The two supervisor backends compile on one host each: a `launchd_agents.rs` change is invisible to the DGX and a `systemd_user.rs` change is invisible to the Mac, so the two hosts legitimately report different counts. `cargo test` on the Mac compiles **zero** `systemd_user` tests, so a Mac-green run can be missing the test that pins a Linux fix entirely (it was, in #530). The mirror direction is just as real: Mac clippy compiles `cfg(target_os = "linux")` items out, so an unused cfg-linux helper fails only the DGX `-D dead-code` gate. [[cfg-linux-e2e-deadcode-dgx-clippy]]

**This is why shared, `cfg`-free modules keep winning.** #458's gate predicted 3067 and landed 3069 — investigated rather than accepted, and the +2 was exactly two `env_file` tests **running on Linux for the first time**, having lived inside the macOS-only launchd backend. Same argument as #511's `atomic_write` fold.

**Predict the count, then reconcile the delta exactly.** Every gate above was predicted from the diff's new `#[test]` count and investigated when it missed — the cheapest available detector for "a test I think I added is not being compiled". **Reconcile by diffing PER-SUITE counts, not test names:** `--nocapture` interleaves output, so a `test … ok` name grep loses lines and invents "removed" tests, and `#[should_panic]` tests print `- should panic ... ok`, which a bare `… ok` grep reports missing.

⚠️ **A `[SKIP]` can hide a dead fixture for months.** The four gliner-relex venv-shim skips were not "this host is unstaged" — the DGX's `.venv` was a **copy of the Mac's**, its `bin/python` pointing at a path that cannot exist on Linux. A venv is gitignored, so nothing in the repo could tell you. `readlink .venv/bin/python` before believing a skip, and prefer a `REQUIRE_*=1` knob that turns the skip into a failure wherever one can be added.

**Mac verification runs under a private `CARGO_TARGET_DIR`** (the IDE's rust-analyzer holds `target/debug/.cargo-lock` — [[mac-cargo-buildlock-prefer-dgx]]), and **it must live under `$HOME`, not `/tmp`**: macOS scrubbed a scratchpad target dir *mid-run* once, so a test binary vanished between build and exec (`TEST_EXIT=101`) while every `test result:` line still said `ok` — [[dgx-run-logs-tmp-scrubbed]].

⚠️ **A `[SKIP]` line is evidence, so nothing may fake one.** `grep -c '^\[SKIP\]'` over a
`--nocapture` run is how a green sweep is audited for tests that reported success without executing
anything — this file and `CLAUDE.md` both tell the next session to read that count. A unit test that
prints one inflates exactly the number it protects, and #663's first DGX gate reported **5** where
four were real. Every `[SKIP]` now renders through the pure
[`tests_common::skip::skip_line`](../../../tests-common/src/skip.rs), so a test can pin the wording
**without emitting a line**. Assert on `skip_line`; call the `skip_if_*` wrappers only from real
fixtures.

**Two standing reading rules.** A green run with `[SKIP]` lines means tests *skipped*, not that the sandbox contained anything — always re-check with `-- --nocapture`. And skip-as-pass counts as passed, so counts stay comparable with or without `--nocapture`.


### Build & test

```sh
source "$HOME/.cargo/env"          # cargo isn't on the PATH for non-interactive shells
cargo build --workspace
cargo test --workspace             # authoritative counts in the table above
cargo test --workspace -- --nocapture   # required to verify [SKIP] lines
cargo clippy --workspace --all-targets -- -D warnings
./target/debug/kastellan           # the core daemon
```

**Required one-time Linux host setup (Ubuntu 24.04+):** `sudo scripts/linux/install-bwrap-apparmor-profile.sh` — without it every sandbox integration test skips silently. For the Firecracker backend: `sudo scripts/linux/install-firecracker-vsock.sh` (also a hard prerequisite for every `build-*-rootfs.sh`, which only *verify* the pinned guest kernel and never create one). macOS needs no setup.

**FC e2e gotchas (DGX) — read before running any Firecracker e2e:** rebuild the **release** launcher (`cargo build --release -p kastellan-microvm-run`) AND the affected rootfs (the init is baked in) AND `export PATH=$HOME/.local/bin:$PATH` (firecracker is off the non-interactive ssh PATH → the e2e silently skips-as-passes otherwise). `kastellan-core` won't cross-compile on the Mac (`ring` C dep), so core e2e are compile+run on the DGX only. `/var/lib/kastellan/microvm/` carries `vmlinux` + the four `*.ext4` images. `net_demo_firecracker_egress_e2e` also needs the egress-proxy binary built + a loopback origin (in-test); the CA rides `fs_read` under `/tmp` (a SHARE_ANCHOR) into the guest, and `KASTELLAN_NETDEMO_EXTRA_CA` must be in **both** `base_policy.env` and `NetTransportSpawn.extra_ca`. A VM worker's `WorkerSpec.program` must be the **in-rootfs** `/usr/local/bin/kastellan-worker-<name>`, never the host target-dir path ([[vm-worker-in-rootfs-binary-path]]).

### The tree — 27 crates

Full layout in the root [`README.md`](../../../README.md) § Layout, and the load-bearing crates in
[`CLAUDE.md`](../../../CLAUDE.md) § Project shape. Not duplicated here — it drifts, and the README is
the one a fresh reader finds first.


### Integration-suite map

| Suite | Tests | What's verified |
| ----- | ----- | --------------- |
| `sandbox` unit (linux / macos) | 16 / 14 | bwrap + cgroup argv builders; Seatbelt profile builder, path canonicalization, TinyScheme-injection rejection, mach-lookup guard |
| `sandbox` integration (`linux_smoke` / `macos_smoke` / `macos_container_smoke`) | 7 / 10 / 7+ | **real** jails: fs invisibility, net deny, relative-path reject, OOM-kill under MemoryMax, per-spawn `/tmp` tmpfs, fresh session leader, bind-mount-readonly |
| `core` (`shell_exec_e2e`, `python_exec_e2e`, `python_exec_container_e2e`) | 4 / 4 / 4 | **real** core→sandbox→worker round-trips under production policy; jail-contained socket attempt; per-spawn scratch; secret-scrub to `[redacted:]`; macOS micro-VM `mem_mb` cap + `Net::Deny` + >64 KiB params file channel |
| `core` (`web_fetch_e2e`, `web_search_e2e`) | 1+1 / 1+1 | **real** sandbox deny-paths (off-allowlist host denied; endpoint off-allowlist ⇒ worker refuses at startup); `#[ignore]` real-network tiers |
| `core` (`egress_proxy_e2e`, `egress_force_routing_e2e`) | 2+1 / 3+1 | **real** sandboxed sidecar + CONNECT client: allowed round-trip, 403, `decision_to_audit`, `ca.pem` export, 1:1 teardown, Linux-only no-direct-route, PG-gated `pg_decision_sink`→`audit_log` |
| `core` (`email_mitm_e2e`) | 2 | **real, hermetic, MITM**: force-routed `email-in` polls a self-signed HTTPS mock; asserts the **round-tripped event** plus `tls_intercepted:true`. Negative control pinned to `mitm_failed: origin TLS handshake`, not any `mitm_failed:` |
| `core` (`mail_e2e`, `mail_daemon_e2e`) | — | jailed `mail.search`, attachment delivery across the `fs_write` boundary, force-routing coupling; scripted planner advertises + dispatches `mail.*` (`#[ignore]` real-LLM tier) |
| `core` (`email_channel_e2e`) | 8 | hermetic channel loop incl. the header-order-bypass and skipped-id-cursor-wedge regressions |
| `core` (`injection_guard_e2e` / `_fixtures`, `secret_vault_e2e`) | 6 / 4 / 9 | **PG-required**: policy rows, privacy invariant, per-tool profiles (#142), materialize/redeem, fail-closed redemption, opaque-ref-not-plaintext |
| `core` (`memory_recall_e2e`, `cli_ask_e2e`, `cli_memory_l3*`) | 1 / 2 / 17 | three-lane RRF recall + 1-hop expansion; full prod chain against a queued mock LLM; L3 list/remove/approve/revoke/pin + operator `run` |
| `core` (`guard_boot_row_e2e`) | 1 | **PG-required, hermetic:** a real daemon boots a **configured** guard tier against a `/props`-only mock with `KASTELLAN_LLM_GUARD_TIMEOUT_MS` pinned above the ceiling, and the stored `policy / guard_tier.boot` row is asserted equal to `boot_payload(..)` plus five literals. Zero guard chat requests proves the pin skipped the probe; one `/props` proves the boot verified the context once |
| `core` (`handoff` unit + `handoff_dispatch_e2e`) | 19 + 3 | cache budget/eviction/purge; dispatcher-level `fetch` intercept |
| `db` unit + `postgres_e2e` | 71+ / 8+ | builders, SQL pins, secrets AES-GCM; probe idempotency, runtime-role REVOKE, audit NOTIFY, cascade + journalling |
| `llm-router` unit + integration | 41 + 8 | config, wire shapes, `compose_url`, `pick_backend`; hand-rolled TCP mock chat + embed chokepoints |
| `egress-proxy` unit | 37 | `decide`, real-UDS `handle_conn`, CA round-trip, leaf cache, `looks_like_tls`, hermetic two-leg TLS with only-CA worker trust |
| `prelude` unit + smoke | 21 | env/profile parse, BPF builds, landlock + seccomp smoke |
| `supervisor` unit + integration | 44–52 + 2–4 | unit/plist builders, name validation, driver round-trips (macOS serialised via a reentrant mutex) |
| `web-fetch` / `web-search` / `web-common` unit | 21 / 24 / 8 | extraction, redirect-drive caps, SearxNG parse, loopback/scheme truth tables, allowlist matcher |

Older rows (3668 back to 3327, covering the guard slice-1 arc, #587, #579 and #578) are in [`archive/handover_20260823_pre-prune.md`](archive/handover_20260823_pre-prune.md) § Test baseline, and in the archive snapshots before it.

---

## Key design decisions locked in

- **Vendor-neutral, AGPL-compatible deps only.** Apache-2.0 / MIT / BSD / MPL / LGPL / (A)GPL fine; CDDL, BUSL, SSPL, Elastic and "source-available" are blocked.
- **Cross-platform first-class.** Linux (DGX Spark primary) + macOS. No Linux-only code without a macOS counterpart of equivalent guarantee.
- **Rust core, Python only inside sandboxed workers.** No PyO3, no in-process Python; the core never executes untrusted code in-process.
- **One process per worker, one OS sandbox per worker.** No "spawn unsandboxed" escape hatch in `tool_host` — don't add one.
- **Hybrid LLM with policy routing.** Local-first over OpenAI-compatible HTTP; Frontier only via the Phase-5 policy gate, through the egress proxy.
- **Single-host deployment via OS-native user-level supervisors** (`systemd --user` / launchd). No k3s.
- **Fixed core tools, sandbox-bound agent-authored Python.** Named/persisted skills get a human-approve gate (the L3 arc).
- **JSON-RPC 2.0 over stdio** — MCP-stdio compatible, so a richer MCP client can be swapped in without moving the trust boundary.
- **The operator→daemon command channel is the Postgres `tasks` queue + `LISTEN/NOTIFY`**, not a new IPC socket. `ask` and `memory l3 run` both ride it (#179 Opt-3).
- **Threat-model invariant:** worst-case compromise reaches *at most* the agent's own OS user, its own Postgres role, its own scratch FS, and the allowlisted endpoints for the *one* compromised tool. Nothing else.

---

## Recently merged

Newest first. Older entries live in the [`archive/`](archive/) snapshots and in git history; the
substance of each is compressed under [Current state](#current-state) rather than repeated here.

- **`test/w2-microvm-uid-drop-gate`** ([#669](https://github.com/hherb/kastellan/pull/669), OPEN)
  — the Firecracker gate #660 owed, plus the **three** defects it found: the VMM jail's bare
  `--disable-userns` (#661's third producer — every micro-VM spawn refused by bwrap at option-parse
  time), the guest kernel having no `CONFIG_SECURITY_LANDLOCK` against the audit's new fail-closed
  rule (every VM worker refused to start), and W-2's privilege drop leaving the egress/broker relay
  sockets root-owned (every *networked* VM worker got `EACCES` on its first dial). Plus the first
  test that proves W-2 from inside the running guest, and the threat-model correction that stops it
  claiming a Landlock layer the Firecracker path never had. 0/21 → 19/21. Deferred with issues:
  [#666](https://github.com/hherb/kastellan/issues/666),
  [#667](https://github.com/hherb/kastellan/issues/667),
  [#668](https://github.com/hherb/kastellan/issues/668).
- **`9ace57ad`** ([#663](https://github.com/hherb/kastellan/pull/663))
  — #653 + #654: `KASTELLAN_GLINER_RELEX_REQUIRE_E2E` on the Rust side over **six** preconditions, the
  #459 flag dialect at the fixture call sites, and the whole host-mode cascade folded into the new
  `tests-common::gliner_e2e` (the last triplicated `resolve_worker_script` with it). Then a five-agent
  review round closed a **hole in the knob itself** (`memory_entity_link_e2e` skipped green under
  `REQUIRE=1` on a supervisor-less host — reproduced, then fixed by moving the check into
  `bring_up_pg` and un-importing the skip-only helper) and the **blind spot that hid it** (two mutants
  survived a green run; `report_unmet_to` + a pure `first_unmet_precondition`/`host_env_from` split
  now kill six of six). +18 tests, all in the crate CI actually runs. Deferred out of scope:
  [#664](https://github.com/hherb/kastellan/issues/664) (no Rust session-finish backstop) and
  [#665](https://github.com/hherb/kastellan/issues/665) (the same #654 dialect skew in `python_exec`
  and `browser_driver`).
- **`c03ec1a3`** ([#656](https://github.com/hherb/kastellan/pull/656)) — #650, the interpreter alias
  bind: `InterpreterRoot` with `dep_walk_prefix` + `bind_paths`, two new pure modules, and a fourth
  hand-rolled copy of the resolution cascade in `browser_driver_e2e.rs` folded into the production
  resolver. Preceded by a movement-only lift of `resolve_interpreter_root` into
  `interpreter_deps::root` (23 test names identical either side). Also carried the three real-bwrap
  fixes `main`'s #660 needed: `4269ff7e` (#661), `f97991a6` (#662), `407918e8` (three test-only
  `secret_vault_e2e` assertions).
- **`62d98a00`** ([#660](https://github.com/hherb/kastellan/pull/660)) — the 2026-09-02 pre-release
  security audit: 29 fixes, 80 files. Two DGX gates still owed (Firecracker, live-Matrix).
- **`ef8144f8`** ([#651](https://github.com/hherb/kastellan/pull/651)) — #649, the transformers
  advisory, plus the interpreter-bind fixture fix, a repeatable real-model load test, and a
  `uv lock --check` CI job. Exposed [#650](https://github.com/hherb/kastellan/issues/650).
- **`c5972572`** ([#652](https://github.com/hherb/kastellan/pull/652)) — docs-only: the openworker
  re-survey at `fb1bfc62`.
- **`e5cb6bfc`** ([#648](https://github.com/hherb/kastellan/pull/648)) — docs-only: the DGX redeploy.
- **`121f22a2`** ([#645](https://github.com/hherb/kastellan/pull/645)) — #641 + #642 + #643 + the
  `LlmEndpoint` split.
- **`466ca7ff`** ([#640](https://github.com/hherb/kastellan/pull/640)) — #632 + #634.
- **`44e0f38d`** ([#637](https://github.com/hherb/kastellan/pull/637)) — #626, the saturating first
  sample. **`d3f8ed3f`** ([#635](https://github.com/hherb/kastellan/pull/635)) — #633, the configured
  boot-row seam. **`8040ca83`** ([#631](https://github.com/hherb/kastellan/pull/631)) — #627,
  `boot_report` as a pure module. **`4aee83ad`** ([#625](https://github.com/hherb/kastellan/pull/625))
  — #624, three probe samples, keep the fastest. **`3bd45a36`**
  ([#623](https://github.com/hherb/kastellan/pull/623)) — the connect-timeout fold. **`e258ad3c`**
  ([#619](https://github.com/hherb/kastellan/pull/619)) — `guard.error_kind` as a closed
  discriminant.
- **`8736f559`** ([#607](https://github.com/hherb/kastellan/pull/607)) — the guard tier WIRED and
  running live on the DGX. **`bb937df7`** ([#579](https://github.com/hherb/kastellan/pull/579)) /
  **`af3e7e66`** ([#578](https://github.com/hherb/kastellan/pull/578)) — #564 slices 2 and 1b.


### Earlier history

One bullet per session, newest first, in [`archive/handover_20260803_pre-prune.md`](archive/handover_20260803_pre-prune.md) § "Earlier history" — covering the Firecracker micro-VM slices 1–5c, the python-exec warm/idle arc, the Matrix worker hardening + live-channel arc, the planner-feedback arc (#337–#340), the entity/L1-embedding arc, the L3 skill arc, the egress-proxy slices #1–#4, the comms/channel-bus slices, the crates.io 0.1.0 release and the hhagent→kastellan rename. Older snapshots: [`20260727`](archive/handover_20260727_pre-prune.md), [`20260719`](archive/handover_20260719_pre-prune.md), [`20260629`](archive/handover_20260629_pre-prune.md), [`20260615`](archive/handover_20260615_pre-prune.md), [`20260611`](archive/handover_20260611_pre-prune.md), [`20260605`](archive/handover_20260605_pre-prune.md), [`20260529`](archive/handover_20260529_pre-prune.md), [`20260510`](archive/handover_20260510_pre-prune.md).

---

## Open follow-up issues (filed but not picked)

Beyond those under [Next TODO](#next-todo). Only currently-open issues; closed-issue detail lives in
the [`archive/`](archive/) snapshots and git history. **The one-line summaries here are pointers —
read the issue before acting, since several carry measurements that close off the obvious fix.**

**Sandbox:** #650, #661 and #662 are all **fixed and on `main`**. Nothing blocks a green DGX today.
Open from #650's review round: [#657](https://github.com/hherb/kastellan/issues/657) (the
`interpreter_deps` cascade emits no diagnostic — a healthy venv and a broken one look identical),
[#658](https://github.com/hherb/kastellan/issues/658) (transposable same-typed probes),
[#659](https://github.com/hherb/kastellan/issues/659) (the alias non-widening proof is taken at
resolve time, re-resolved at spawn time).

**From the #640 review (fixed in #645 as #641/#642/#643):**
[#644](https://github.com/hherb/kastellan/issues/644) — a duplicate `ServiceSpec.env` key renders as
a duplicate launchd plist dict key, whose resolution the format does not define. `tests-common` is
safe (it collapses last-wins); this is the general case for every *other* producer.
[#646](https://github.com/hherb/kastellan/issues/646) — the two name-cap copies #642 undercounted.

**Guard model / measurement:** [#605](https://github.com/hherb/kastellan/issues/605) (the
`PROVISIONAL` banner is unconditional — until it lands no report can say a τ is fitted);
[#602](https://github.com/hherb/kastellan/issues/602) (an empty body pinned as the page — fail-**open**
under `--record`); [#601](https://github.com/hherb/kastellan/issues/601) (`capture` admits under
`Relaxed`, `calibrate` excludes under `Strict`; quantified **inert** for this run but still wrong);
[#603](https://github.com/hherb/kastellan/issues/603) (the URL inside the hash);
[#599](https://github.com/hherb/kastellan/issues/599)/[#600](https://github.com/hherb/kastellan/issues/600);
[#608](https://github.com/hherb/kastellan/issues/608)–[#611](https://github.com/hherb/kastellan/issues/611);
[#597](https://github.com/hherb/kastellan/issues/597) (the two hosts hold different *projectors*;
inert while the tier runs `vision:false`).
**#604 is addressed, not closed** — D8 makes the 400 unreachable on a correctly sized host; it does
not make it unrepresentable.
[#617](https://github.com/hherb/kastellan/issues/617) is the big one of that family: `req` is lost
wholesale above the 4 KiB cap, and for `shell.exec` `req.argv` **is** the audited act. The allowlist
is the wrong tool (unbounded); a bounded **producer-side** summary is the right one, which makes it a
change in every tool's dispatch path rather than in `db::audit`.

**Audit / observability:** [#628](https://github.com/hherb/kastellan/issues/628) — causal structure
lives in payload prose, not columns: `task_id` is a payload key in 58 places across 19 files,
enforced nowhere, with no grouping key for one plan iteration and no link from a row to the row that
caused it. [#629](https://github.com/hherb/kastellan/issues/629) — `MemoryLayer::L4` is declared with
no writer.

**Channel / scheduler:** [#588](https://github.com/hherb/kastellan/issues/588) (the shared
`live_ask_for_claimant!` bind contract is doc-only; both binds are `text`, so a transposition
type-checks and returns zero rows — fail-*closed*, hence deferred);
[#515](https://github.com/hherb/kastellan/issues/515) (an unreachable PG can delay daemon shutdown by
sqlx's 30 s pool-acquire timeout; fix is a 5 s `tokio::time::timeout`);
[#501](https://github.com/hherb/kastellan/issues/501) (no long-lived channel sidecar gets
leak-scanner fingerprints, **and the proxy fails open**, so it looks scanned);
[#497](https://github.com/hherb/kastellan/issues/497) (unify the per-family `ChannelBus` instances);
[#334](https://github.com/hherb/kastellan/issues/334); [#332](https://github.com/hherb/kastellan/issues/332)/[#328](https://github.com/hherb/kastellan/issues/328);
[#330](https://github.com/hherb/kastellan/issues/330).

**Tools / planner:** [#537](https://github.com/hherb/kastellan/issues/537)/[#538](https://github.com/hherb/kastellan/issues/538)
(`mail.search`'s `filters.account_ids`/`folder_ids` bypass the `LocalmailId` widening entirely —
**measure live before deciding the fix**); [#534](https://github.com/hherb/kastellan/issues/534)
(give `ToolParam` a type — smaller than #527 assumed: 38 literals across 10 files, one pure renderer,
two design calls to settle first); [#438](https://github.com/hherb/kastellan/issues/438);
[#277](https://github.com/hherb/kastellan/issues/277); [#485](https://github.com/hherb/kastellan/issues/485)/[#484](https://github.com/hherb/kastellan/issues/484).

**Sandbox / VM / egress:** [#519](https://github.com/hherb/kastellan/issues/519) (`microvm-run` is
resolved from `$PATH`, not exe-relative, so it is **not deployable**);
[#554](https://github.com/hherb/kastellan/issues/554) (`tool_allowlists` enforcement is kind-blind);
[#396](https://github.com/hherb/kastellan/issues/396); [#378](https://github.com/hherb/kastellan/issues/378)/[#372](https://github.com/hherb/kastellan/issues/372)/[#356](https://github.com/hherb/kastellan/issues/356);
[#407](https://github.com/hherb/kastellan/issues/407); [#426](https://github.com/hherb/kastellan/issues/426);
[#286](https://github.com/hherb/kastellan/issues/286); [#243](https://github.com/hherb/kastellan/issues/243);
[#298](https://github.com/hherb/kastellan/issues/298); [#317](https://github.com/hherb/kastellan/issues/317).
**The jail has no NSS, and that is documented rather than fixed** (closed by
[#546](https://github.com/hherb/kastellan/pull/546)): any command resolving a user or group name
— `ls -l`, `id`, `whoami`, a bare `python3` — dies by SIGSYS on `socket(2)` inside a `WorkerStrict`
worker. The kill is now *loud*; the commands still cannot run.

**Test-infra / hygiene:** [#535](https://github.com/hherb/kastellan/issues/535); [#442](https://github.com/hherb/kastellan/issues/442);
[#134](https://github.com/hherb/kastellan/issues/134); [#104](https://github.com/hherb/kastellan/issues/104)
(**six** `pid+nanos` suffix copies, counted properly); [#353](https://github.com/hherb/kastellan/issues/353);
[#130](https://github.com/hherb/kastellan/issues/130); [#196](https://github.com/hherb/kastellan/issues/196).
Long-tail: [#3](https://github.com/hherb/kastellan/issues/3), [#4](https://github.com/hherb/kastellan/issues/4),
[#8](https://github.com/hherb/kastellan/issues/8), [#13](https://github.com/hherb/kastellan/issues/13),
[#14](https://github.com/hherb/kastellan/issues/14), [#20](https://github.com/hherb/kastellan/issues/20),
[#21](https://github.com/hherb/kastellan/issues/21), [#24](https://github.com/hherb/kastellan/issues/24).


## Design notes for parked work

**Option P — entity↔memory linkage + graph lane (Phase 1 cont.).** The `memory_entities` join table shipped and the production caller wiring is DONE (2026-05-19 Slice F, PR #91): `RouterAgent::formulate_plan` populates `seed_entity_ids` from `entity_extractor.extract(&ctx.instruction)` each iteration, and `main.rs` wires the real `GlinerRelexExtractor`. **The remaining parked work is the quarantine review gate, not the wiring:** freshly-extracted entities default `quarantine=TRUE` and `graph_search` filters `quarantine=FALSE`, so seed entities surface no memories until an operator un-quarantines them ([#40](https://github.com/hherb/kastellan/issues/40) tracks the policy question). Secondary: `entities.embedding` is NULL for all entities; populating it would seed an entity-similarity lane (the column already exists).

## Open questions parked for later

1. Embedding model on-device — bge-m3 vs nomic-embed-text vs ColBERT (Phase 1).
2. ~~Channel approval~~ **Resolved 2026-05-06:** pairing flow with WebAuthn-or-OTP fallback.
3. ~~Egress proxy separate worker vs in-process~~ **Resolved 2026-05-06:** separate worker, leak scanner co-located.
4. Skill review workflow for *named* agent-authored Python (Phase 4) — trust enum + per-level capability ceiling; the L3 arc is the first concrete implementation for templated tool-call skills.
5. Worker keep-alive vs spawn-per-call — idle-timeout lifecycle shipped for GLiNER-Relex; revisit for other workers when latency matters.
6. ~~Worker binary discovery / install convention~~ **Resolved 2026-06-20** (`kastellan-cli install`, PR #316). Residual: an FHS `libexec` / multi-user layout if packaging ever wants it.

## Inspirations / things to read before each milestone

Two adjacent OpenClaw-derived projects ship AGPL-compatible code worth reading before a new milestone:

- **ZeroClaw** ([`zeroclaw-labs/zeroclaw`](https://github.com/zeroclaw-labs/zeroclaw), 100 % Rust) — [`crates/zeroclaw-runtime/src/security/`](https://github.com/zeroclaw-labs/zeroclaw/tree/main/crates/zeroclaw-runtime/src/security) has working `bubblewrap.rs`, `landlock.rs`, `seatbelt.rs`, `pairing.rs`, `webauthn.rs`, `leak_detector.rs`. **Don't copy its in-process tool model** — tools run as in-process Rust traits with the OS sandbox around the whole runtime, a weaker boundary than process-per-worker.
- **IronClaw** ([`nearai/ironclaw`](https://github.com/nearai/ironclaw)) — read its dispatcher chokepoint pattern (`ToolDispatcher::dispatch()` as the single audit/safety funnel for every action). Drawbacks: WASM-as-boundary is software-only containment; the Postgres+libSQL dual backend is overkill at our stage.

The *defining* architectural difference: kastellan enforces **one OS process + one bwrap/Seatbelt jail per worker**. Both reference projects retreated from that. Don't.

**openworker** ([`andrewyng/openworker`](https://github.com/andrewyng/openworker), **MIT** — so
nothing needs clean-room reimplementation) and its engine
[`aisuite`](https://github.com/andrewyng/aisuite), surveyed 2026-08-14 and **re-surveyed 2026-09-02**
at `fb1bfc62`. Full write-up:
[`docs/devel/notes/2026-09-02-openworker-resurvey.md`](../notes/2026-09-02-openworker-resurvey.md).
**Read it for consent ergonomics, never for containment** — it has no OS sandbox at all, and
`permissions.py` says so itself ("not a determined adversary (that needs the OS sandbox)"), so taking
its security architecture would be a regression. What it has done far more work on than we have is
everything around **an agent that runs while nobody is watching**, which is our default posture and
its edge case. Five ROADMAP entries came out of the first survey (the ask channel
[#564](https://github.com/hherb/kastellan/issues/564); declared tool risk + operator overrides;
target-bound standing grants; auto-compaction; `SKILL.md` progressive disclosure); the re-survey adds
four Phase-5 entries from their August oversight work, the load-bearing one being a **layered
oversight corpus with two answer keys per row** (`expected_current` vs `expected_secure`) so a test
*"cannot bless an identified vulnerability just because it matches today's behaviour"* — ours is
single-key and output-side only, and `cassandra::review` has no corpus at all. Two things we already
do **better**, so don't re-import: their `artifact_store` dehydration is a weaker `handoff.rs`, and
their shell-metacharacter rejection exists only because `run_shell` takes a command *string* —
`shell-exec` takes an argv array and never invokes a shell. One finding worth acting on
independently: `kastellan_runtime` holds INSERT/DELETE on `tool_allowlists` (migration 0009,
deliberate — the CLI writes under it), so **the daemon's own role can widen its own argv allowlist**.
Not exploitable today, but a `kastellan_policy` role owning the policy tables, `SELECT`-only for
runtime, is 0002's split one level in.

**Headlong** ([`laude-institute/headlong`](https://github.com/laude-institute/headlong), Apache-2.0),
surveyed 2026-08-27; write-up in
[`docs/devel/notes/2026-08-27-headlong-borrowings.md`](../notes/2026-08-27-headlong-borrowings.md).
**Read it for memory, context and loop pacing; never for containment** — its own `SECURITY.md` says
the agent *"runs arbitrary bash on its box with its API keys"* on a *"dedicated and burnable"* box.
Four of its defining features are things kastellan exists to refuse. What it has done far more work
on is **an agent that has lived for months**:
[#628](https://github.com/hherb/kastellan/issues/628) (*"writers stamp exact links; readers must not
guess"*) and [#629](https://github.com/hherb/kastellan/issues/629) (the logarithmic rollup pyramid for
the declared-but-unwritten `MemoryLayer::L4`) came out of it. Three more are in the note, not filed:
its pacing table for whenever routines land (with three bugs already paid for, including a `setsid`
timer that *silently never ran on macOS*); *"liveness is a dispatcher guarantee, not a property code
paths must each preserve"*, which lands on our startup-only `crash_recovery::sweep_and_audit`; and
blob spilling as the optional second half of [#617](https://github.com/hherb/kastellan/issues/617).

---

## How to update this document at session end

**Header first, prose last.** The header is what the next session treats as authoritative; stale header fields mislead silently even when the prose is right.

1. **Bump the header before writing any prose:** `Last updated:` → today; `main` HEAD → `git log --oneline -1`; `Last gate:` → the passed/failed/ignored/`[SKIP]` counts from a fresh `cargo test --workspace`. Then fix **every test count embedded elsewhere that changed** — a fresh agent greps for them and trusts whatever it finds.
2. **Move the picked TODO into [Recently merged](#recently-merged)** with enough detail (file paths, why-not-X, gotchas, count delta) to start cold, and update the [Test baseline](#test-baseline-authoritative) table.
3. **Write a fresh [Next TODO](#next-todo)** with options sized for one session each — file paths, gotchas, verification step.
4. **Refresh [Working state](#working-state)** — anything new, anything that became real.
5. **Tick the matching ROADMAP items** with the commit hash.
6. **Commit both files together** with a `docs(handover): …` message.
7. **If a milestone shipped:** does `site/roadmap.html` (timeline + "Last updated" stamp, and the landing-page status numbers) need a one-line update? See `site/README.md`.

### Pruning convention

This file stays focused on **what the next session must act on**: current state, the last 2–3 sessions, and the next TODO. Prune when it grows past what a fresh session can absorb cold — judge by *reading weight*, not line count; the 2026-08-03 prune was triggered at 546 lines / ~73 k tokens.

1. **Snapshot first** — copy to `archive/handover_<YYYYMMDD>[_<slug>].md`. The archive is the audit trail: never edited after the fact, never deleted.
2. **Keep:** the header, "Read these first", "Working state" (current truth), the most recent 1–2 sessions, "Key design decisions", "Next TODO", open issues, open questions, "Inspirations", and this section.
3. **Compress everything else** into one bullet per session, or into the archive pointer if it is no longer load-bearing.
4. **Cross-link** every compressed bullet to the archive snapshot.
5. **Commit the prune separately** (`docs(handover): prune older sessions, archive pre-prune snapshot`) so the diff is reviewable.

The archive directory is the historical record; this file is the working brief.
