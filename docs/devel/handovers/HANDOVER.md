# hhagent — Session Handover

> Rolling document. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. See
> [`README.md`](README.md) for the convention.

**Last updated:** 2026-05-09
**Last commit:** _set on commit_ — `feat(supervisor): wire core into default_supervisor with typed core_service_spec helper + cross-platform e2e`
**Branch:** `main`

---

## Read these first

1. [`docs/architecture.md`](../../architecture.md) — high-level diagram, process model, cross-platform table
2. [`docs/threat-model.md`](../../threat-model.md) — invariant, scenarios in scope, defence-in-depth layers
3. [`docs/devel/ROADMAP.md`](../ROADMAP.md) — the master sequenced TODO list with commit hashes for shipped items
4. The design plan (outside the repo) — `~/.claude/plans/i-d-like-to-design-logical-starlight.md`
5. Memory notes (auto-loaded) — see `~/.claude/projects/-home-hherb-src-hhagent/memory/MEMORY.md`

## Working state (what's green right now)

```
hhagent (Rust workspace, 6 crates, AGPL-3.0)
├── core               hhagent-core: lib + bin (skeleton main); tool_host derives lockdown env + spawns watchdog; workspace = per-task scratch with RAII cleanup
├── sandbox            hhagent-sandbox: SandboxPolicy + LinuxBwrap (now wraps in systemd-run --scope cgroup) + MacosSeatbelt
├── supervisor         hhagent-supervisor: SystemdUser (Linux: real install/start/stop/status/uninstall via systemctl --user) + LaunchAgents (macOS: real lifecycle via launchctl bootstrap/bootout/print in gui/<uid> domain) + specs::core_service_spec (typed ServiceSpec for the agent core daemon) + default_probe (per-OS supervisor probe)
├── protocol           hhagent-protocol: JSON-RPC 2.0 over stdio (working)
├── workers/prelude      hhagent-worker-prelude: Linux-only Landlock + seccomp lock_down (no-op on macOS)
└── workers/shell-exec   hhagent-worker-shell-exec: uses prelude::serve_stdio
```

**`cargo test --workspace` on Linux: 105 tests, 0 skipped, 0 failed, 0 warnings.**
*macOS test count was 83 last session (2026-05-08); the macOS suite gains the same +8 supervisor unit tests (`specs::*`) and +1 cross-platform e2e (`core_service_install_start_observe_log_uninstall`) when next run there, projecting macOS to 92.*

| Suite | Tests | What's verified |
| ----- | ----- | --------------- |
| `protocol` unit | 3 | dispatch, parse-error fallback, method-not-found |
| `sandbox` unit (linux) | 16 | bwrap argv builder shape (6) + cgroup `systemd-run` argv builder shape: starts with `systemd-run`, uses `--user --scope --quiet --collect`, sets `MemoryMax`+`MemorySwapMax=0` from policy, omits both when `mem_mb=0`, defense-in-depth `CPUQuota=200%` + `TasksMax=64` defaults, ends with `--` separator, no inner-program leakage, 4 `-p` flags total (10) |
| `sandbox` unit (macos) | 13 | sandbox-exec profile builder shape + path canonicalization + on-host probe + TinyScheme-injection rejection + canonicalize error propagation |
| `sandbox` integration (`linux_smoke`) | 7 | **real** bwrap+cgroup: echo runs jailed, /etc/passwd & /home invisible, listed paths visible, net unreachable under `Net::Deny`, relative-path policy rejected, **mem_burner allocating 256 MiB under `MemoryMax=32M` is OOM-killed by the kernel** |
| `sandbox` integration (`macos_smoke`) | 8 | **real** sandbox-exec: scaffold marker, echo runs jailed, /etc/master.passwd invisible, /Users does not leak username, fs_read paths readable (canonicalize /etc symlinks), /dev/disk0 denied, relative-path policy rejected, network unreachable under `Net::Deny` |
| `core` unit | 16 | `derive_lockdown_env` adds correct env entries (4 tests); watchdog loop honours cancel, fires at deadline, exits early on cancel during sleep, guard's Drop sets cancel flag (4 tests); `is_valid_target_pid` rejects 0/1/u32::MAX/`i32::MAX+1` (1 test); workspace creates layout, drops wipes tree, `fs_write_paths` order, `extend_policy` appends, task-id validation, root auto-create, pre-existing dir refused (7 tests) |
| `core` integration (`shell_exec_e2e`) | 4 | **cross-platform real** core → bwrap+landlock+seccomp (Linux) / sandbox-exec (macOS) → shell-exec round-trip; non-allowlisted argv → POLICY_DENIED; unknown method → METHOD_NOT_FOUND; **workspace e2e**: `Workspace::extend_policy` wires `<root>/<task_id>/{in,out,tmp}` into the policy, sandboxed `cp` reads from `in/` and writes to `out/`, host reads back byte-for-byte, `Workspace::Drop` wipes the whole tree |
| `core` integration (`supervisor_e2e`) | 1 | **cross-platform real** `default_supervisor()` round-trip against the actual `hhagent` binary: build spec via `core_service_spec`, install into `~/.config/systemd/user/` (Linux) or `~/Library/LaunchAgents/` (macOS), pre-start status=Inactive, start, poll the redirected stdout file for the daemon's startup JSON line ("hhagent core starting" + `version` field), stop, uninstall, post-uninstall status=NotInstalled. RAII guard cleans up on panic. Unique `hhagent-supervisor-test-{pid}-{nanos}` name avoids clobbering a real installed `hhagent-core`. macOS holds the same intra-binary serial mutex as `launchd_agents_smoke.rs` |
| `prelude` unit | 11 | env-var parsing, profile parsing, BPF program builds (Strict + NetClient), unshare/mount/ptrace/bpf absent from allow-list under both profiles, socket present *only* in NetClient, essential syscalls present in BASE_ALLOW |
| `prelude` integration (`landlock_smoke`) | 4 | write-to-non-allowlisted denied with EACCES; allowlisted scratch write works; `/usr` reads still work; **v6 ABI yields `FullyEnforced` on this kernel** |
| `prelude` integration (`seccomp_smoke`) | 6 | `unshare(CLONE_NEWUSER)` and `mount(...)` killed with SIGSYS under both Strict and NetClient; `socket(AF_INET, SOCK_STREAM)` killed under Strict, survives under NetClient; `getpid()` survives |
| `supervisor` unit (linux) | 35 | `build_unit_file` shape (14 tests: section order, Description, ExecStart program+args, arg quoting + escape of `"`/`\`, Environment ordering, Environment value quoting, WorkingDirectory present/absent, log redirects, keep_alive Restart=on-failure, no-Restart when keep_alive=false, TimeoutStopSec always, [Install] WantedBy=default.target); `validate_service_name` (6 tests: typical names, empty, traversal, dot/dash prefix, overlong, whitespace+specials); driver against custom units_dir (7 tests: install writes file, rejects relative program, rejects invalid name, creates units_dir, uninstall removes file, uninstall idempotent, status NotInstalled when absent); `specs::core_service_spec` (8 tests: canonical name `hhagent-core`, caller-supplied program path flows through, args+env empty by default, no working_dir, keep_alive=false-for-now regression pin, log paths under log_dir with predictable filenames, stdout/stderr distinct) |
| `supervisor` unit (macos) | 43 | `build_plist` shape (14 tests: XML preamble + DOCTYPE, Label, ProgramArguments order, XML-escaping of `<`, `>`, `&`, `"`, `'` in args, EnvironmentVariables presence/order/omission-when-empty, WorkingDirectory present/absent, log redirects, RunAtLoad=true unconditional, KeepAlive=true/false mirror of spec, ExitTimeOut always, Label XML-escaped); `validate_service_name` (6 tests: typical names incl. reverse-DNS like `org.hhagent.core`, empty, traversal, dot/dash prefix, overlong, whitespace+specials); helpers (7 tests: `xml_escape` predefined entities + Unicode passthrough, `parse_print_state` indented/multi-word/absent, `is_no_such_service_error` phrases, `user_domain_target` `gui/<digits>` shape); driver against custom agents_dir (8 tests: install writes plist, rejects relative program, rejects invalid name, rejects relative working_dir, creates agents_dir, uninstall removes plist, uninstall idempotent, status NotInstalled when absent); `specs::core_service_spec` (8 tests: canonical name `hhagent-core`, caller-supplied program path flows through, args+env empty by default, no working_dir, keep_alive=false-for-now regression pin, log paths under log_dir with predictable filenames, stdout/stderr distinct — same suite runs on both OSes) |
| `supervisor` integration (`systemd_user_smoke`, linux) | 2 | **real** `systemctl --user` round-trip: install → daemon-reload → start → status=Active → stop → status=Inactive → uninstall → status=NotInstalled, with RAII cleanup guard so a panic does not leave residue in `~/.config/systemd/user/`; invalid name rejected before any systemctl call |
| `supervisor` integration (`launchd_agents_smoke`, macos) | 4 | **real** `launchctl bootstrap gui/<uid>` round-trip against `~/Library/LaunchAgents/`: install → start → status=Active → stop → status=Inactive → uninstall → status=NotInstalled; idempotent `start` after start (status-first check via `launchctl print`, no version-specific error-string parsing); idempotent `stop` against not-bootstrapped agent; invalid name rejected before any launchctl call. RAII guard cleans up plist file + `bootout` on panic; tests serialised with a static `Mutex` because the GUI launchd domain is a shared global resource. `[SKIP]` line on hosts where the GUI domain is unreachable (SSH-only sessions). |

Earlier-session note (kept for context): `LinuxBwrap::probe()` was once
missing the `/lib*` symlinks the dynamic linker needs, so
`execvp /usr/bin/true: No such file or directory` made every
bwrap-dependent test silently `[SKIP]`. Fixed in `3210f70` by mirroring
the full `build_argv` mount layout in the probe. Today's run shows zero
`[SKIP]` lines.

**Build & test:**
```sh
source "$HOME/.cargo/env"
cargo build --workspace          # produces ./target/debug/hhagent + workers
cargo test --workspace           # all green
./target/debug/hhagent           # runs the (skeleton) core daemon, emits one JSON log line
```

**Required one-time host setup (Ubuntu 24.04+ only):** the AppArmor profile
that lets `bwrap` create unprivileged user namespaces is already installed
on the user's DGX Spark. Other Linux hosts may need
`sudo scripts/linux/install-bwrap-apparmor-profile.sh`. macOS uses
`sandbox-exec` (no setup needed; ships with the OS).

## Recently completed (this session, 2026-05-09)

**Phase 0 cont. — wire core into the supervisor (typed `core_service_spec` + cross-OS `default_probe` + e2e against the real `hhagent` binary).**

Closed Option C4 from the previous handover. The supervisor crate now
ships a typed [`ServiceSpec`] builder for the agent core daemon and a
cross-OS supervisor probe; the core crate proves both supervisor
backends can host the real `hhagent` binary end-to-end without per-OS
branching in the test code.

- **New module `supervisor/src/specs.rs` (~150 lines, 8 unit tests):**
  pure `core_service_spec(binary: &Path, log_dir: &Path) -> ServiceSpec`
  + `pub const CORE_SERVICE_NAME: &str = "hhagent-core"`. Returned spec:
  `name = "hhagent-core"` (same string on both OSes — no reverse-DNS,
  the lib.rs `ServiceSpec.name` doc-comment explicitly allows this);
  `program = caller-supplied`; `args` empty (daemon takes no flags
  yet); `env` empty (daemon's `RUST_LOG` defaults to `"info"` via
  `unwrap_or_else` in `core/src/main.rs::main`); `working_dir = None`;
  `keep_alive = false` (today's daemon is a placeholder that emits one
  log line and exits 0 — `Restart=on-failure` would be a no-op on
  clean exit anyway; flip when the daemon becomes a long-running
  event loop, regression pin in
  `core_service_spec_keep_alive_is_false_for_now`); `stdout_log =
  log_dir/hhagent-core.out`, `stderr_log = log_dir/hhagent-core.err`.
  Pure: no I/O, no env probing — caller resolves both inputs.
- **New `supervisor::default_probe()` in `supervisor/src/lib.rs`:**
  cross-OS supervisor probe mirroring `default_supervisor()`. Linux →
  `systemd_user::probe()`, macOS → `launchd_agents::probe()`, other
  Unix → `SupervisorError::NotImplemented`. Lets cross-platform tests
  do a single skip-if-no-supervisor check without per-OS branching.
- **New `supervisor::specs` module export in `supervisor/src/lib.rs`:**
  `pub mod specs;` (not `cfg`-gated — pure spec builders compile on
  every OS, only the backends are platform-specific).
- **New `core/tests/supervisor_e2e.rs` (~190 lines, 1 test):**
  - `core_service_install_start_observe_log_uninstall` — full e2e
    against `default_supervisor()`: build spec via
    `core_service_spec`, override the name to a unique
    `hhagent-supervisor-test-{pid}-{nanos}` (avoids clobbering a real
    installed `hhagent-core` and lets concurrent test runs coexist),
    redirect stdout to a per-test log file under `temp_dir`, install,
    assert pre-start status=Inactive, start, **poll the redirected
    stdout file** (50 ms tick, 5 s budget) for the daemon's startup
    JSON line containing `"hhagent core starting"` and the
    `"version":` field, stop (must be safe even after the daemon's
    natural exit — pins the "stop is always idempotent" contract),
    uninstall, assert post-uninstall status=NotInstalled. RAII
    `ServiceGuard` runs `uninstall` on Drop so a panic mid-test
    doesn't leave residue. macOS path holds the same intra-binary
    `static OnceLock<Mutex<()>>` the launchd smoke test uses, so the
    GUI domain is never touched concurrently. `[SKIP]` line on hosts
    where `default_probe()` fails (headless Linux without
    `loginctl enable-linger` / SSH-only macOS).
  - **Why observe via the log file, not via the `Active` window?**
    Today's daemon is "log one line and exit 0", so the `Active`
    window is well under 50 ms — too short to catch reliably with a
    polling status check. The redirected stdout is the durable side
    effect that proves the daemon actually ran. When the daemon
    becomes a long-running event loop (and `core_service_spec`
    flips to `keep_alive=true`), this test should grow an assertion
    that `status` reaches `Active` *and* stays there for a few
    polls — currently filed as part of the "core daemon goes
    long-running" follow-up.

**Test count:** 96 → 105 on Linux (+8 unit `specs::*`, +1 integration).
0 skipped, 0 warnings. macOS projects to 92 by the same delta.

**Why `keep_alive=false` for now (and the regression test that pins it).**
Flipping `keep_alive=true` would translate to `Restart=on-failure`
(systemd) / `KeepAlive=true` (launchd). For today's "log line and
exit 0" daemon, neither restart trigger fires (exit 0 is success on
both platforms). Setting `true` would just be cargo-culted noise; the
right time to flip it is when the daemon body becomes a real
event loop where unexpected exit *should* trigger restart. The
`core_service_spec_keep_alive_is_false_for_now` unit test makes this a
deliberate, paired change — flipping the helper trips the test, so the
implementer is forced to update both at once.

**Why the same `specs::*` suite shows up under both OS rows in the
test table.** `specs.rs` is not `cfg`-gated (pure builders, no
platform deps), so the 8 tests compile and run in whichever supervisor
suite executes — Linux row goes 27 → 35, macOS row goes 35 → 43, but
the *underlying* tests are the same 8 functions. This is intentional:
the spec contract is platform-independent and any per-OS divergence
would be a bug.

---

## Recently completed (previous session, 2026-05-08)

**Phase 0 cont. — macOS service supervisor (`hhagent-supervisor::launchd_agents`).**

Cross-platform parity with the Linux `SystemdUser` backend. The supervisor
crate now ships real install/start/stop/status/uninstall on both
operating systems. `default_supervisor()` returns `LaunchAgents::new()`
on macOS and `SystemdUser::new()` on Linux; only "other Unix" still
falls through to the `NotYetImplemented` placeholder.

- **API touch-ups in `supervisor/src/lib.rs`:** module gate
  `#[cfg(target_os = "macos")] pub mod launchd_agents`; `default_supervisor`
  branches on three cases (Linux / macOS / other) instead of two; the
  `NotYetImplemented` placeholder is now correctly cfg-gated to
  *non*-Linux-*non*-macOS Unixes. The `ServiceSpec.name` doc-comment
  is updated to reflect that file basename = `<name>.plist` on macOS
  (not the previously-suggested `org.hhagent.<name>.plist` auto-prefix
  scheme). Trait + spec are otherwise unchanged.
- **New module `supervisor/src/launchd_agents.rs` (~700 lines, ~280
  of those in the test block):**
  - **Pure `build_plist(spec) -> String`** — emits a deterministic
    XML LaunchAgent in fixed key order: `Label`, `ProgramArguments`,
    `EnvironmentVariables` (only when non-empty, mirroring systemd's
    `--clean-env` shape), `WorkingDirectory` / `StandardOutPath` /
    `StandardErrorPath` (only when set), `RunAtLoad=true`
    (unconditional — see "Why RunAtLoad is always true" below),
    `KeepAlive` (mirrors `spec.keep_alive`), `ExitTimeOut=10`
    (matches systemd's `TimeoutStopSec=10` so behaviour is uniform
    across OSes). All free-form strings (`name`, args, env keys/values,
    paths) flow through `xml_escape` for the five predefined XML
    entities (`&`, `<`, `>`, `"`, `'`).
  - **Pure `validate_service_name(&str)` helper** — same character
    class as the Linux side (`[A-Za-z0-9._-]`, no leading `.` or `-`,
    max 200 chars, no `.`/`..`). Identical rule set on both backends
    so a single user-facing service name is portable to either OS
    without a "rename for macOS" step. Includes tests for typical
    reverse-DNS labels like `org.hhagent.core`.
  - **`LaunchAgents` driver** — `new()` resolves `~/Library/LaunchAgents/`
    from `$HOME`; `with_agents_dir(path)` is the test seam that lets
    unit tests exercise the file-writing half against a temp dir
    without touching the live GUI launchd domain. `install` validates
    the spec (program/working_dir/log paths must be absolute), creates
    the agents dir if missing, atomically writes `<name>.plist`
    (write-to-tmp + `fsync` + `rename`). Unlike the Linux side,
    `install` never calls `launchctl` — there is no separate
    "daemon-reload" step on macOS; `bootstrap` *is* the load step
    and it's invoked from `start`. `start` checks `is_loaded_in_domain`
    via `launchctl print <target>` exit code, returns Ok if already
    bootstrapped (idempotent), otherwise `launchctl bootstrap gui/<uid>
    <plist-path>`. `stop` runs `launchctl bootout gui/<uid>/<label>`
    and swallows the "no such service" error so re-stops are
    idempotent. `uninstall` is best-effort about `bootout` (skipped
    entirely for custom agents_dir to prevent name collisions with
    real installed agents) then removes the plist file. `status`
    short-circuits to `NotInstalled` when the file is missing,
    otherwise parses the `state = <word>` line out of `launchctl
    print` stdout (`running` → `Active`, anything else → `Inactive`,
    matching the Linux backend's liberal mapping).
  - **`probe()`** — `launchctl print-disabled gui/<uid>`; succeeds
    silently or returns `SupervisorError::Probe` with a hint
    explaining that the GUI domain needs an active console login
    (SSH-only sessions can't reach it).
  - **35 unit tests** — see suite table for the breakdown.
- **New `supervisor/tests/launchd_agents_smoke.rs` (~200 lines, 4 tests):**
  - `install_start_status_stop_uninstall_round_trip` — full
    real-launchctl path against `~/Library/LaunchAgents/` with a
    `TestAgentGuard` whose Drop calls `uninstall`. Service body is
    `/bin/sleep 30`; polls `status()` for the Active/Inactive
    transitions (no flaky sleeps).
  - `start_after_install_is_idempotent` — calls `start` twice,
    proving the status-first idempotency check works (avoids the
    parsing-version-specific-bootstrap-error trap discussed below).
  - `stop_when_not_started_is_idempotent` — calls `stop` against
    an agent that was installed but never started; `bootout`'s
    "no such service" error is swallowed, `stop` returns Ok.
  - `invalid_name_is_rejected_before_any_launchctl_call` — pure
    path, runs even on hosts where the GUI domain is unreachable.
  - **All four smoke tests share `~/Library/LaunchAgents/` and the
    GUI launchd domain — both global resources — so they're
    serialised with a `static OnceLock<Mutex<()>>` acquired at the
    top of each test.** Without this, parallel runs produced
    flakes where one test's mid-flight `bootstrap` interfered with
    another test's atomic plist write (the tmp file would vanish
    before rename). Cargo's default workspace-wide parallelism
    is otherwise preserved.

**Test count:** 96 → 96 on Linux (no Linux files touched), 44 → 83 on
macOS (+35 unit, +4 smoke). No existing test changed.

**Why RunAtLoad is always true.** `launchctl bootstrap` only runs the
program when `RunAtLoad=true`; with `RunAtLoad=false` the agent loads
into the domain but sits dormant waiting for a demand-driven trigger
that hhagent doesn't use. Our public API contract is "install + start
runs the program," so the builder pins `RunAtLoad=true` regardless of
what the caller might set on the spec. There's a unit test
(`build_plist_run_at_load_is_always_true`) that pins this invariant.

**Idempotent `start` via status-first, not error-parse.** First TDD
pass tried `match run_launchctl(&["bootstrap", ...]) { Err(Backend(msg))
if is_already_loaded_error(&msg) => Ok(()), ... }` with substring
matching for `"already loaded"` etc. macOS 26.4's actual response to a
double-bootstrap on this host is `"Bootstrap failed: 5: Input/output
error"` (exit 5 / EIO) — no "already loaded" anywhere in the message.
Apple's launchctl error strings vary across macOS versions and even
across error paths within a single version, so substring matching is
brittle. Replaced with `is_loaded_in_domain(target)` — runs `launchctl
print <target>` and checks the exit code (0 = bootstrapped, non-zero
= not in domain). Stable across versions because we don't parse the
verbose `print` output, just the exit code. Verified by the
`start_after_install_is_idempotent` smoke test.

**Why uninstall skips bootout for custom agents_dir.** When tests
construct `LaunchAgents::with_agents_dir(temp_dir)`, the unit-tested
`uninstall` path runs `bootout gui/<uid>/<name>` against the *live*
GUI domain even though the plist itself is in a temp dir. If a test
name happened to collide with a real installed agent, that would
silently bootout someone else's service. Fixed by checking
`is_default_agents_dir()` before any launchctl call — for custom
dirs, uninstall is purely a file removal. Mirrors the Linux backend's
"only daemon-reload when writing into the canonical dir" pattern.

**`hhagent-supervisor-test-` prefix discipline.** The smoke tests name
their plist `hhagent-supervisor-test-{pid}-{nanos}.plist` — uniquely
greppable so leftovers from a hard crash can be cleaned up with
`find ~/Library/LaunchAgents -name 'hhagent-supervisor-test-*'`.
Verified post-test: zero residue (`ls ~/Library/LaunchAgents/ | grep
hhagent` returns nothing; `launchctl print-disabled gui/$(id -u) |
grep hhagent` agrees).

---

## Recently completed (2026-05-10)

**Phase 0 cont. — Linux service supervisor scaffold (`hhagent-supervisor::systemd_user`).**

The supervisor crate previously held a `Supervisor` trait + `ServiceSpec`
struct + a `NotYetImplemented` placeholder; this session grew the trait
slightly and shipped a real Linux backend.

- **API additions in `supervisor/src/lib.rs`:** new `ServiceStatus` enum
  (`Active | Inactive | Failed | NotInstalled`), new `Supervisor::status`
  method, new structured `SupervisorError` variants
  (`InvalidName`, `Probe`, `Io`; existing `Backend`, `NotImplemented`).
  `default_supervisor()` now returns `SystemdUser::new()` on Linux and
  `NotYetImplemented` only on non-Linux. The trait remains `dyn`-safe.
- **New module `supervisor/src/systemd_user.rs` (~600 lines, well under
  the 500-line guideline because the test block accounts for ~280 of
  those):**
  - **Pure `build_unit_file(spec) -> String`** — emits a deterministic
    `[Unit] / [Service] / [Install]` unit file. Quotes ExecStart args
    and Environment values only when the token contains whitespace,
    `"`, `\`, or is empty; backslash-escapes `"` and `\`. Emits
    `Restart=on-failure` + `RestartSec=5` only when `keep_alive=true`,
    always emits `TimeoutStopSec=10` so test teardown can never hang.
    Mirrors the `linux_bwrap::build_argv` / `linux_cgroup::build_systemd_run_argv`
    pattern (pure, separately testable from the spawn path).
  - **Pure `validate_service_name(&str)` helper** — rejects empty,
    overlong (>200), `.`, `..`, names starting with `.` or `-`, and
    any character outside `[A-Za-z0-9._-]`. This is the path-traversal
    + systemd-grammar gate; called by `install`/`start`/`stop`/`uninstall`/`status`.
  - **`SystemdUser` driver** — `new()` resolves `~/.config/systemd/user/`
    from `$HOME`; `with_units_dir(path)` is the test seam that lets unit
    tests exercise the file-writing half against a temp dir without
    touching the live `--user` manager. `install` validates the spec
    (program/working_dir/log paths must be absolute), creates the units
    dir if missing, atomically writes `<name>.service` (write-to-tmp +
    `fsync` + `rename`), and runs `daemon-reload` *only* when writing
    into the canonical dir. `uninstall` is best-effort about
    `stop`/`disable` (so it's idempotent for never-started or
    never-installed units), removes the file, and reloads. `status`
    short-circuits to `NotInstalled` when the file is missing, otherwise
    parses `systemctl --user is-active` stdout (trusting stdout, not the
    exit code, because `is-active` exits non-zero for inactive units).
  - **`probe()`** — `systemctl --user show-environment`; succeed silently
    or return `SupervisorError::Probe` with a hint pointing at
    `loginctl enable-linger $USER` for headless hosts. Mirrors
    `sandbox::linux_cgroup::cgroup_probe`.
  - **27 unit tests** — see the suite table for the full breakdown.
- **New `supervisor/tests/systemd_user_smoke.rs` (~150 lines, 2 tests):**
  - `install_start_status_stop_uninstall_round_trip` exercises the full
    real-systemctl path against `~/.config/systemd/user/` with a
    `TestUnitGuard` whose `Drop` calls `uninstall` so a panic mid-test
    does not leave a stale unit file behind. Uses `/usr/bin/sleep 30`
    as the service body and polls `status()` for the Active/Inactive
    transitions (no flaky sleeps). Skips with a `[SKIP]` line on hosts
    where `probe()` fails.
  - `invalid_name_is_rejected_before_any_systemctl_call` — pure path,
    runs even on hosts without a user manager. Defensive proof that
    name validation runs before any side effect.

**Test count:** 67 → 96 (+27 unit, +2 smoke). No existing test changed.

**Atomic-write idiom — write_atomic:** the unit file is written via
write-to-tmp (`<path>.tmp`) → `fsync` → `rename`. Without this, a
concurrent `systemctl --user` invocation could (in theory) read a
half-written unit file during a race. The cost is one extra rename
syscall per install — negligible — and the observable state is now
binary: either the old contents or the new ones, never a torn read.

**Why no auto-`enable`:** `install` emits `[Install] WantedBy=default.target`
so a caller *can* `systemctl --user enable <name>.service` to make the
service start at session login, but `install` does not call `enable`
itself. Whether to enable is a policy decision per service (the core
daemon probably wants it; one-shot test units don't). When we ship the
first concrete `hhagent.service` we'll make that explicit.

**`hhagent-supervisor-test-` prefix discipline:** the smoke test names
its unit `hhagent-supervisor-test-{pid}-{nanos}.service` — uniquely
greppable so leftovers from a hard crash can be cleaned up with
`find ~/.config/systemd/user/ -name 'hhagent-supervisor-test-*'`. Verified
post-test: zero residue (`ls ~/.config/systemd/user/ | grep hhagent`
returns nothing; `systemctl --user list-units` agrees).

---

## Recently completed (2026-05-09)

**Phase 0 hardening — final item: cgroup v2 CPU/memory/tasks caps via `systemd-run --user --scope`.**

The Linux backend now wraps every `bwrap` invocation in `systemd-run
--user --scope --quiet --collect -p MemoryMax=Nm -p MemorySwapMax=0 -p
CPUQuota=200% -p TasksMax=64 -- bwrap ...`. systemd-run is the
**outer** process so the cgroup is in place *before* `bwrap` creates
the unshare-all namespace — the worker is born inside the cap, never
outside it. With `--scope` the wrapped command runs in the foreground
with stdio inherited (mandatory for JSON-RPC over stdio); `--service`
would have detached and broken the protocol layer.

- New module `sandbox/src/linux_cgroup.rs` (~300 lines, well under the
  500-line guideline). Pure `build_systemd_run_argv(&policy) ->
  Vec<String>` returning the argv up to and including the trailing
  `--` separator. Caller (`linux_bwrap::spawn_under_policy`) appends
  the bwrap argv directly after. 10 unit tests cover each property and
  the omit-when-`mem_mb=0` path.
- New `cgroup_probe()` runs `systemd-run --user --scope --quiet
  --collect /usr/bin/true`. `LinuxBwrap::probe()` now calls both the
  bwrap probe and the cgroup probe and only returns Ok when **all**
  containment layers are available — fail-closed defense-in-depth: a
  host without a live user systemd manager doesn't run sandbox tests
  in degraded mode, it skips them entirely (so green CI without
  containment is impossible).
- `LinuxBwrap::spawn_under_policy` composes the two argv builders:
  `Command::new("systemd-run")`, args from `build_systemd_run_argv`,
  then `bwrap` + the existing bwrap argv.
- New fixture `sandbox/tests/fixtures/mem_burner.rs` (~60 lines, no
  deps): allocates `--mb N` MiB of `Vec<u8>` and **writes one byte per
  4 KiB page** so the kernel actually faults the pages in (without
  the touch they'd stay copy-on-write zero pages and never count
  against `memory.max`). Built via a `[[bin]]` stanza in
  `sandbox/Cargo.toml` mirroring the existing `net_probe` pattern.
- New regression test
  `sandbox/tests/linux_smoke.rs::worker_with_low_mem_max_is_oom_killed`:
  spawns mem_burner under a `mem_mb=32` policy with
  `--mb 256` (an 8× overrun). The cgroup OOM killer SIGKILLs the
  inner process; the parent observes a non-success exit. This test is
  what would have caught the `MemorySwapMax=0` gap that caused the
  first iteration to fail.

**`MemorySwapMax=0` discovery (and why it must be paired with
`MemoryMax`).** First TDD pass set only `MemoryMax=32M`; mem_burner
allocated 256 MiB and exited cleanly. Diagnosis: this host has 15 GiB
of swap, and without `MemorySwapMax=0` the kernel pages overruns to
swap rather than killing the cgroup. That's not just a test
inconvenience — it means a runaway worker would burn host I/O for
many seconds, degrading the system, before any cap fired. Pairing
`MemorySwapMax=0` with `MemoryMax` makes the cap honest: the kernel
counts swap against the cgroup, so OOM fires the moment RSS hits the
limit. Documented in the linux_cgroup.rs module-level doc and tested
by `argv_pairs_memory_max_with_memory_swap_max_zero`.

**Defense-in-depth defaults (not yet policy-driven).** `CPUQuota=200%`
(at most 2 CPUs) and `TasksMax=64` (fork-bomb resistance) are
hardcoded. Tunable `cpu_quota_pct` / `tasks_max` / `setrlimit`-based
`cpu_ms` enforcement is filed as a follow-up GitHub issue rather than
shipped this session (would require a `SandboxPolicy` schema change
that would touch every test fixture).

`docs/threat-model.md` defense-in-depth table grows a "Resource caps"
row pointing at `linux_cgroup.rs`; the negative-tests-shipped list
gains the OOM-kill row.

Test count: 56 → 67 (+10 unit, +1 integration). No existing test
changed.

---

## Recently completed (2026-05-08)

**Phase 0 polish — workspace+worker integration test + seccomp BASE_ALLOW broadening.**

`core/tests/shell_exec_e2e.rs::workspace_dir_is_writable_during_call_and_wiped_on_drop`
exercises the full `Workspace` contract end-to-end against a real
sandboxed worker: stage a known string in `<ws>/in/source.txt`, build
a `SandboxPolicy`, call `Workspace::extend_policy(&mut policy)` (the
canonical wiring point), spawn shell-exec with `cp` allowlisted, copy
`in/ → out/` *inside* the jail, read the artifact back from the host,
drop the workspace, assert the whole task tree is gone. This is the
first test that proves the host (`policy.fs_write` → bwrap bind-mount)
and worker (`HHAGENT_LANDLOCK_RW` → Landlock allow-list) layers agree
on what the worker may write — they share `Workspace::fs_write_paths`
through `derive_lockdown_env`, but the e2e is what catches drift.

To make `cp` actually run inside the jail, three syscalls had to be
added to `BASE_ALLOW`:

- `copy_file_range`: GNU coreutils' bulk-copy fastpath; without it,
  `cp` dies with SIGSYS on its first byte.
- `sendfile`: copy_file_range's fallback for cross-fs / pre-5.3 copies.
- `fadvise64`: a kernel readahead hint coreutils calls before its
  first `read(2)`. No security surface (cannot affect anything outside
  the calling process).

All three copy *between two already-open file descriptors* and grant
no capability beyond what `openat` already does — net-zero on the
threat model. `libc 0.2` doesn't expose `SYS_sendfile` or
`SYS_fadvise64` on `aarch64`, so a small `cfg`-gated shim
(`SYS_SENDFILE` / `SYS_FADVISE64`) carries the kernel ABI numbers
explicitly. x86_64 still forwards to `libc::SYS_*`. Other arches
fail-closed at compile time, which is the right behaviour.

Test count: 55 → 56. No existing test changed.

---

**Phase 0 polish — per-task scratch workspace with RAII cleanup (`9333311`).**

`core::workspace::Workspace` is the canonical type for per-task scratch
space. Construction lays down `<root>/<task_id>/{in,out,tmp}`; drop
wipes `<root>/<task_id>` recursively. Single owner, single cleanup
path. Replaces the previous "caller authors `policy.fs_write` paths
ad-hoc per worker" pattern, which had no cleanup contract at all.

- `Workspace::new(task_id)` uses default root from
  `$HHAGENT_WORKSPACE_ROOT` or `~/.hhagent/workspace`. Tests use
  `Workspace::with_root(&temp_dir, task_id)` so they don't pollute
  global state and don't depend on env vars.
- `extend_policy(&mut policy)` is the canonical wiring point: it
  appends `[in, out, tmp]` to `policy.fs_write`, which then flows
  unchanged into the worker-side Landlock allow-list via
  `tool_host::derive_lockdown_env`. Host and worker layers can never
  disagree because both read the same paths.
- Task ids are validated against `[A-Za-z0-9_-]+` up front. Rejected
  ids never touch the filesystem (path-traversal class refused with
  `WorkspaceError::InvalidTaskId`).
- Pre-existing task dir is refused (`ErrorKind::AlreadyExists`) — we
  never inherit another task's state silently.
- 7 unit tests under `core/src/workspace.rs::tests` cover layout,
  drop, fs_write order, extend_policy, validation, root auto-create,
  and pre-existing-dir refusal.

---

**Phase 0 polish — wall-clock watchdog + kill(-1) fanout defense (`57edfb2`).**

Workers now have an optional wall-clock budget. `WorkerSpec` gains
`wall_clock_ms: Option<u64>`; `spawn_worker` returns a
`SupervisedWorker` that owns a watchdog thread which SIGKILLs the
worker once the deadline elapses. Cancellation is fast: dropping the
handle flips an `AtomicBool` the watchdog picks up on its 50 ms poll,
so a normal close never produces a kill on a reused PID.

**Bug fix — watchdog SIGKILL fanout (a.k.a. the "DGX display blackout").**

This had been logged in user memory as a driver issue
(`host_display_blackout.md` — "driver 580.142 + X11 + dual-display;
reproducible from cargo *in VS Code*, NOT idle/DPMS"). It was actually
*us*. Smoking-gun trace: an SSH session died mid-test on
`watchdog_loop_runs_until_deadline_when_not_cancelled` — the only
watchdog test that allows the deadline to elapse and therefore the only
one that fires the kill path.

Root cause in `core/src/tool_host.rs`:

```rust
const SAFE_FAKE_PID: u32 = u32::MAX;            // ← misnamed
fn send_sigkill(pid: u32) {
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL); }
}
```

`pid_t` is `i32`; `u32::MAX as i32 == -1`; `kill(-1, SIGKILL)` signals
*every* process the calling user can signal. Running that one test
SIGKILLed the user's X session, gnome-shell, and per-session sshd
children. Looked like a GPU driver crash; was a self-inflicted process
massacre.

**Fix is two-layered (both shipped, do not remove either):**

1. `is_valid_target_pid(pid: u32) -> bool` rejects `0`, `1`, and any
   value `> i32::MAX` *before* `kill(2)` — defensive guard with
   incident write-up in the `send_sigkill` doc comment so future
   readers can't miss the history.
2. `watchdog_loop` now takes an injected `kill: fn(u32)`. Production
   passes `send_sigkill`; tests pass a `noop_kill` that discards the
   PID. The dangerous test never reaches `kill(2)` at all.

New regression test `is_valid_target_pid_rejects_broadcast_values`
asserts the validator behaviour against the four worst PID values
(`0`, `1`, `u32::MAX`, `i32::MAX as u32 + 1`). The dangerous watchdog
test now runs cleanly on the DGX without disturbing the GUI session.

**`cargo test --workspace` after the fix: 55 passed, 0 failed, 1 ignored**
(doc-test).

---

**Phase 0 hardening — stage 2 (Linux): seccomp allow-list + Landlock v6.**

The handover's "Option B'" shipped end-to-end. Both layers are now
fail-closed and per-profile; both have negative tests proving the
distinguishing behavior.

- **seccomp: deny-list → per-profile allow-list.** `workers/prelude/src/seccomp_lock.rs`:
  - Replaced `KILL_LIST` with `BASE_ALLOW` (~110 syscalls common to x86_64
    + aarch64) plus `BASE_ALLOW_X86_64_LEGACY` (~19 syscalls for the
    open/stat/pipe/dup2/poll/select/fork legacy entry points that don't
    exist on aarch64) plus `NET_CLIENT_ADDITIONS` (~18 syscalls in the
    BSD-socket family).
  - `Profile::Strict` = `BASE_ALLOW` (+ legacy on x86_64). `Profile::NetClient` =
    same plus `NET_CLIENT_ADDITIONS`. Default action flipped to
    `KillProcess`; listed syscalls get `Allow`.
  - The catastrophic syscall set (`unshare`, `setns`, `mount`,
    `umount2`, `pivot_root`, `move_mount`, `open_tree`, `bpf`,
    `ptrace`, `kexec_*`, `init_module`, …) is killed automatically by
    *not* being in either allow-list — verified by the unit test
    `unshare_is_not_in_allow_list`.
  - Base set was derived empirically from `strace -fc` of a real
    `shell_exec_e2e` round-trip plus the standard tokio/std runtime
    requirements (`futex`, `rseq`, `clone3`, `epoll_*`, `rt_sigreturn`).
    The shell-exec e2e passed first try under the new allow-list — no
    `strace` iteration needed.

- **Landlock: ABI v1 → v6.** `workers/prelude/src/landlock_lock.rs`:
  - `TARGET_ABI` bumped to `ABI::V6` (Linux 6.12+). The user's host on
    6.17 reports kernel ABI 7; the crate caps to V6 and proceeds.
  - All four new restricted accesses are now handled: `Refer` (v2),
    `Truncate` (v3), `IoctlDev` (v5), and the v6 `Scope` rights
    (`AbstractUnixSocket`, `Signal`). Refer + Truncate are granted on
    RW scratch dirs; IoctlDev is granted on `/dev` only (libc/dyld
    probe terminal-ness with `TCGETS`-style ioctls); Scope rights are
    handled but no rules — the kernel restricts both globally for the
    worker.
  - **Bug fix discovered by the new `FullyEnforced` test:** the kernel
    rejects directory-only rights like `ReadDir` on file-typed
    `PathBeneath` rules; the `landlock` crate silently strips them but
    flips the ruleset's compat state to `Partial`, downgrading the
    eventual report to `PartiallyEnforced`. `add_path_rule` now
    `stat`s the path and intersects with `AccessFs::from_file(V6)` for
    files, leaving `from_all(V6)` for directories. With this in
    place, `LandlockReport::FullyEnforced` is now reported on every
    run — verified by `v6_abi_yields_fully_enforced_on_modern_kernel`.

- **New tests (+7 over the previous 36):**
  - `prelude` unit (+3): `build_bpf_net_client_succeeds`,
    `socket_is_only_in_net_client_profile`, `essentials_are_in_base_allow_list`
    (replaces the now-stale `kill_list_contains_unshare`).
  - `seccomp_smoke` (+3): `socket_is_killed_under_strict`,
    `socket_survives_under_net_client`, `unshare_is_killed_under_net_client`.
  - `landlock_smoke` (+1): `v6_abi_yields_fully_enforced_on_modern_kernel`.

- **New probe subcommand:** `lockdown-probe seccomp-socket` attempts
  `socket(AF_INET, SOCK_STREAM, 0)` and reports survival vs SIGSYS.
  Used by both the kill-under-Strict and survives-under-NetClient
  integration tests.

Total tests after stage 2 on Linux: 43 passed, 0 skipped, 0 failed.
macOS side untouched (the prelude crate is `cfg(target_os = "linux")`-gated).

## Recently completed (2026-05-07)

**Phase 0b — macOS Seatbelt sandbox backend:**

- New module `sandbox/src/macos_seatbelt.rs`: pure `build_profile(policy)` returning a TinyScheme `.sb` profile, `MacosSeatbelt::probe()` mirroring the Linux probe pattern, `spawn_under_policy()` with up-front absolute-path validation, path canonicalization (so `/etc`-style platform symlinks resolve to `/private/etc/...`), `env_clear()` + per-policy env, and `process_group(0)` for `--new-session` parity. 11 unit tests cover the version+deny-default header, always-on dyld/libsystem allows, the explicit `/dev` allowlist, fs_read/fs_write rules, Net::Allowlist lifting the network deny, the canonicalize-with-fallback helper, the relative-path rejection, and the on-host probe.
- New `sandbox/tests/macos_smoke.rs` (8 tests): scaffold marker, echo-runs-jailed, /etc/master.passwd invisible, /Users does not leak username, fs_read becomes readable (exercising the canonicalize fix for /etc symlinks), relative-path rejection, /dev/disk0 denied, network unreachable under Net::Deny.
- New `sandbox/tests/fixtures/net_probe.rs` (12 LoC standalone bin): replaces the missing `/usr/bin/getent` on macOS for the network-deny test. Built into `target/debug/net_probe` via a `[[bin]]` stanza in `sandbox/Cargo.toml`.
- `sandbox/src/lib.rs`: `default_backend()` now returns `MacosSeatbelt` on `cfg(target_os = "macos")`. The `NotYetImplemented` fallback survives behind `cfg(not(any(target_os = "linux", target_os = "macos")))`. The orphan `SandboxError::NotImplemented` variant got a `#[allow(dead_code)]` and a one-line doc comment so future readers know it's reserved.
- `core/tests/shell_exec_e2e.rs` is now cross-platform: per-OS `skip_if_sandbox_unavailable()` and `backend()` helpers, and a `cfg`-gated `ECHO_PATH` (Linux: `/usr/bin/echo`, macOS: `/bin/echo` — verified empirically since `/usr/bin/echo` doesn't exist on this macOS 26.4 host). The same three round-trip tests run on both Linux and macOS.
- `docs/threat-model.md`: explicit paragraph on `sandbox-exec` being Apple-marked private API + the macos_smoke row in "negative tests already shipped".
- Two empirical broadenings vs the design doc — both committed transparently:
  - `build_profile` needed `(allow file-read* (literal "/"))` and `(allow mach-lookup)` to launch real binaries on macOS 26.4 ARM64. Without the literal `/` rule, `/bin/echo` aborts with SIGABRT before dyld even runs (SIP-related path-walk requirement).
  - `spawn_under_policy` canonicalizes `policy.fs_read` / `policy.fs_write` so `/etc/...` paths resolve to `/private/etc/...` before being emitted in the Seatbelt profile.

Total tests after Phase 0b on macOS: 29 passed, 0 skipped, 0 failed.

Linux side is unchanged (the macOS module is cfg-gated out). The Linux user should run `cargo test --workspace` on their Linux box to confirm the prior 36 tests still pass.

**Code-review hardening pass (same session):** addressed feedback from a
post-Phase-0b review of the macOS backend.

- `spawn_under_policy` now rejects policy paths containing TinyScheme-special
  characters (`"`, `\`, `(`, `)`, newline, NUL) before the profile is built —
  forecloses an injection class even though every caller is trusted core code
  today. New unit test `policy_paths_with_tinyscheme_specials_are_rejected_by_spawn`.
- `canonicalize_policy_paths` now returns `Result<SandboxPolicy, SandboxError>`
  and only falls back for `NotFound`. `PermissionDenied` (and any other
  `io::Error`) propagates so we don't silently emit a non-functional Seatbelt
  rule. New unit test `canonicalize_policy_paths_propagates_non_notfound_errors`
  uses `chmod 0o000` on a temp dir with an RAII guard for cleanup.
- `host_users_dir_is_invisible_when_not_in_policy` now asserts `!status.success()`
  primarily and only secondarily checks that `$USER` doesn't leak into stdout —
  no more host-specific hard-coded "hherb" string and no more vacuous-pass risk.
- `probe_succeeds_on_this_host` unit test now `[SKIP]`s on probe failure
  instead of panicking, matching the integration-test pattern (so an
  MDM-clipped Seatbelt host doesn't false-fail the suite).
- Dropped the unused `SandboxError::NotImplemented` variant — no constructor,
  no callers, can be re-added when a micro-VM backend lands.

**Filed as follow-up GitHub issues** (won't fit this session but flagged so they
don't get forgotten):

- [#1 — narrow `(allow mach-lookup)` to a `global-name` allowlist](https://github.com/hherb/hhagent/issues/1).
  The unrestricted Mach lookup is the largest concrete weakness in the macOS
  profile; capture the actual service set per worker and switch to an explicit
  allowlist.
- [#2 — evaluate `setpgid(0,0)` → `setsid()` for stronger session isolation](https://github.com/hherb/hhagent/issues/2).
  Today the worker is in its own process group but inherits the controlling
  terminal; `/dev/tty` is excluded from the profile but the asymmetry vs Linux
  `--new-session` is real.
- [#3 — drop `SYS_SENDFILE`/`SYS_FADVISE64` shim once libc exposes them on aarch64](https://github.com/hherb/hhagent/issues/3).
  Hygiene only; the shim in `workers/prelude/src/seccomp_lock.rs` carries the
  kernel ABI numbers explicitly so `BASE_ALLOW` compiles on `aarch64`.
- [#4 — bump Last-commit + test-count fields whenever a Recently-completed entry is added](https://github.com/hherb/hhagent/issues/4).
  This session started with HANDOVER 4 commits behind HEAD; the prose was
  updated but the header fields weren't. Promote the bump-the-header step
  to the top of the end-of-session checklist.
- [#5 — audit `BASE_ALLOW` against a fixture of common worker binaries](https://github.com/hherb/hhagent/issues/5).
  `BASE_ALLOW` was empirically derived from `echo`; the workspace e2e test
  surfaced a silent gap that broke `cp` (fixed in `50a06ec`). Build a
  coreutils fixture and audit before Phase 4 (`python-exec`) starts adding
  workers that exercise more of the syscall surface.

## Recently completed (2026-05-06)

**Phase 0 hardening — stage 1 (Landlock + seccomp + bwrap probe fix):**

- New crate `workers/prelude` (`hhagent-worker-prelude`):
  - `landlock_lock` module — applies a Landlock LSM filter from inside the worker. Targets ABI v1; RO+exec on `/usr`, `/lib`, `/lib64`, `/bin`, `/sbin`, `/etc/ld.so.cache`, `/dev`, `/proc`; RW from `HHAGENT_LANDLOCK_RW` env (JSON array of absolute paths). Graceful `KernelTooOld` fallback.
  - `seccomp_lock` module — installs a seccomp-bpf deny-list killing `unshare`, `setns`, `mount`, `umount2`, `pivot_root`, `init_module`, `finit_module`, `delete_module`, `ptrace`, `bpf`, `perf_event_open`, `kexec_load`, `kexec_file_load`, `reboot`, `swapon`, `swapoff`, `settimeofday`, `clock_settime`, `clock_adjtime`, `adjtimex`, `keyctl`, `add_key`, `request_key`, `personality` with `KillProcess`. Sets `PR_SET_NO_NEW_PRIVS` first.
  - `serve_stdio()` — drop-in wrapper around `hhagent_protocol::server::serve_stdio` that calls `lock_down()` first.
  - `lockdown_probe` test binary — subprocess fixture that integration tests fork off so the one-way filters don't poison sibling tests.
  - 8 unit tests (parsers, BPF builder), 3 landlock integration tests, 3 seccomp integration tests — all green, zero skips.
- `core/src/tool_host.rs`: `derive_lockdown_env()` injects `HHAGENT_LANDLOCK_RW` (from `policy.fs_write`) and `HHAGENT_SECCOMP_PROFILE` (from `policy.profile`) so callers cannot accidentally skip the worker-side layer. Caller-supplied env wins (useful for tests that want `seccomp=none`). 4 new unit tests.
- `workers/shell-exec/src/main.rs`: 1-line swap from `hhagent_protocol::server::serve_stdio` to `hhagent_worker_prelude::serve_stdio`. Existing 3 e2e tests still pass — this time **for real** (see bug fix below).
- **Bug fix in `sandbox/src/linux_bwrap.rs`**: `LinuxBwrap::probe()` was launching `bwrap` without the `/lib*` symlinks the dynamic linker needs, so `execvp /usr/bin/true` returned `ENOENT` (interpreter unreachable) and the probe failed-closed. The skip-on-probe-failure pattern in the integration tests then turned that into `[SKIP]` lines that masqueraded as green. Probe now mirrors `build_argv`'s mount layout. **The previous handover's "18 tests, 0 skipped" was wrong** — only the 12 host-only tests were actually running.
- New deps (workspace): `landlock = "0.4"` (MIT OR Apache-2.0), `seccompiler = "0.5"` (Apache-2.0 OR BSD-3-Clause), both AGPL-compatible.
- Docs: `threat-model.md` defence-in-depth table now lists the worker-side Landlock+seccomp row with the parent-side bwrap/Seatbelt row; "negative tests already shipped" section added.

**Earlier sessions (kept here as build-sequence memory):**

- Initial scaffold (`140eec5`): workspace, three crate stubs, docs skeletons, AGPL-3.0
- Linux bwrap backend (`eae3df4`): real containment + AppArmor probe + install script
- Protocol crate, shell-exec worker, tool_host, end-to-end test (`f2411ec`)
- Created `docs/devel/ROADMAP.md` and this handover convention
- Studied two adjacent OpenClaw-derived projects (IronClaw, ZeroClaw); resolved parked Q2 (channel pairing flow) and Q3 (egress proxy as separate worker + leak scanner); added five concrete roadmap items; codified five architectural invariants in `docs/architecture.md`

## Key design decisions locked in

- **Vendor-neutral, AGPL-compatible deps only.** AGPL project; all third-party deps must be AGPL-compatible (Apache-2.0, MIT, BSD, MPL, LGPL, (A)GPL all fine).
- **Cross-platform first-class.** Linux (DGX Spark primary) + macOS (Apple Silicon and Intel). No Linux-only code without a macOS counterpart of equivalent guarantee.
- **Rust core, Python workers.** Rust for core (no eval/dynamic surface); Python only inside sandboxed tool workers. shell-exec is Rust because it's a thin execve wrapper — Python's first appearance will be `python-exec` in Phase 4 (or possibly `web-fetch` earlier).
- **Hybrid LLM with policy routing.** Local-first via OpenAI-compatible HTTP (vLLM/SGLang on Linux, llama.cpp/Ollama on macOS). Frontier (Claude/OpenAI) only via the Phase-5 policy gate, through the egress proxy.
- **Single-host deployment via OS-native user-level supervisor.** `systemd --user` (Linux) / `launchd` LaunchAgents (macOS). No k3s.
- **Fixed core tools, sandbox-bound agent-authored Python.** Critical workers are human-curated and shipped with the binary. Agent-authored code only runs inside `python-exec`'s strict sandbox; named/persisted skills get an optional human-approve gate (Phase 4).
- **JSON-RPC 2.0 over stdio.** MCP-stdio compatible. Lets us swap in a richer MCP client later without changing the trust boundary.

## Next TODO (pick one)

**Phase 0 hardening is complete on Linux; macOS Seatbelt and both
supervisor backends are real.** The first concrete service is now
wired (`core_service_spec` + e2e against the real `hhagent` binary,
shipped this session). Remaining Phase 0 work: Postgres bring-up,
turning the daemon into a real long-running event loop (so
`keep_alive=true` becomes meaningful), and the supervisor
"auto-restart with backoff on worker crash" item (currently partial
— `keep_alive=true` ⇒ unconditional `Restart=on-failure` on systemd,
no exponential backoff yet).

### Option A — Phase 0b: macOS port  *(SHIPPED 2026-05-07)*

### Option B' — Phase 0 hardening: stage 2  *(SHIPPED 2026-05-08)*

### Option D — Phase 0 polish: per-task scratch + wall-clock kill  *(SHIPPED 2026-05-08 — `9333311`, `57edfb2`)*

### Option E — cgroup v2 CPU/memory caps  *(SHIPPED 2026-05-09 — see "Recently completed")*

### Option F — workspace+worker e2e test  *(SHIPPED 2026-05-08 — see "Recently completed")*

### Option C1 — Linux supervisor scaffold  *(SHIPPED 2026-05-10 — see "Recently completed")*

### Option C3 — macOS LaunchAgent supervisor backend  *(SHIPPED 2026-05-08 — see "Recently completed")*

### Option C4 — wire core into the supervisor  *(SHIPPED 2026-05-09 — see "Recently completed")*

### Option C2 — Phase 0 cont.: Postgres bring-up (private user-instance)

(Now the headline next-pickup. Decided in 2026-05-10 session: a
**private per-user PG cluster** under `~/.local/share/hhagent/pg/`
managed by `hhagent-postgres.service`, never network-listen, peer
auth over UDS. Cleaner containment than coupling to a system PG.)

- `db/initdb.sh` (or a small Rust tool) that runs `initdb -D
  ~/.local/share/hhagent/pg/data --auth-host=reject --auth-local=peer`
  on first boot. Idempotent: skip if data dir already initialised.
- `hhagent-postgres.service` ServiceSpec → install via the new
  `SystemdUser` supervisor. ExecStart: `/usr/lib/postgresql/<ver>/bin/postgres
  -D ~/.local/share/hhagent/pg/data -k /run/user/<uid>/hhagent-pg`
  (UDS dir under XDG_RUNTIME_DIR so socket path is per-session).
  `Restart=on-failure` makes sense here — the DB is the system's spine.
- `db/migrations/0001_init.sql` — `audit_log`, `tasks`, `memories`,
  `entities`, `relations`, `secrets`. Use `sqlx-cli` for the migration
  runner; integrate into core startup.
- A small probe in `core` that connects over the UDS, runs migrations,
  emits an audit-log entry on bring-up.

**Gotchas:**
- The host has **no Postgres installed at all** today. Decide
  apt-install (system package, system binaries, user-instance data)
  vs Docker vs build-from-source. Most pragmatic: install
  `postgresql-17` (system package), but use only the *binaries* and
  initdb our own data dir.
- pg_search is AGPL-3.0 (good license fit) but verify the apt build is
  available for arm64 noble; if not, defer the BM25 work to Phase 1
  and start with plain text columns + pgvector.
- Apache AGE on Postgres 17 may need a recent build; check supported
  PG version. If not ready, defer graph traversal to Phase 1 too —
  Phase 0 only needs the schema + audit log.

### Option H — turn `core/src/main.rs` into a real long-running daemon (and flip `core_service_spec` to `keep_alive=true`)

Smallest natural follow-up to C4. The `core_service_install_start_observe_log_uninstall`
e2e proves the supervisor wiring works for the *current* placeholder
daemon (log one line, exit 0), but a real agent core needs to block
on a stop signal and respond to lifecycle events. Sketch:

- Add a tokio signal handler for SIGTERM/SIGINT in `core/src/main.rs`;
  block on `tokio::signal::ctrl_c()` (or the unix-specific
  `signal(SignalKind::terminate())`) before returning from `main`.
- Plumb a graceful-shutdown channel through to the (currently
  non-existent) scheduler loop so the future loop has a clean exit
  path. For now, the daemon does nothing useful between log line and
  signal — the body is a placeholder for the Phase 1 scheduler.
- Flip `supervisor::specs::core_service_spec` to set
  `keep_alive = true`. Update the `core_service_spec_keep_alive_is_false_for_now`
  unit test to assert `true` (rename appropriately).
- Update the e2e: poll for `status() == ServiceStatus::Active` after
  `start`, hold for ~500 ms to prove it's stable (not transient
  `activating`), then `stop` and poll for `Inactive`. Drop the
  log-file poll (no longer the durable signal — though it's still a
  fine sanity check that the daemon got far enough to log its
  startup line).
- Verify the systemd `Type=simple` shape still works (it does — a
  real long-running daemon is exactly what `Type=simple` expects).

This unlocks the Phase 0 supervisor "auto-restart with backoff"
follow-up: once the daemon can crash for real reasons,
`Restart=on-failure` becomes meaningfully testable.

### Option G — make `cpu_quota_pct`/`tasks_max` policy-driven + setrlimit-based `cpu_ms` enforcement  ([#6](https://github.com/hherb/hhagent/issues/6))

Smaller follow-up to Option E. Today the cgroup layer hardcodes
`CPUQuota=200%` and `TasksMax=64`; `policy.cpu_ms` is documented but
unenforced. To wire them up:

- Extend `SandboxPolicy` with `cpu_quota_pct: Option<u32>` and
  `tasks_max: Option<u64>` (both `#[serde(default)]` so existing
  serialized policies still parse). This will require updating every
  test fixture that constructs `SandboxPolicy` literally — consider
  adding a `Default` impl for `SandboxPolicy` first to avoid that
  churn.
- Plumb the new fields through `linux_cgroup::build_systemd_run_argv`
  (use the policy value when `Some`, the current hardcoded default
  otherwise).
- For `cpu_ms`, the natural enforcement is `setrlimit(RLIMIT_CPU)`
  from the worker prelude before `exec(2)` — cgroup v2 has no direct
  CPU-budget primitive. Add a new prelude function
  `apply_rlimits(policy)` and call it from `serve_stdio` before
  Landlock/seccomp lock_down (rlimit applies process-wide; ordering
  is harmless but document it).
- macOS parity: same `setrlimit` approach in the prelude; will work
  unchanged because rlimits are POSIX. The cgroup-shaped `mem_mb` cap
  on macOS still requires the future micro-VM backend or
  `RLIMIT_AS` (which has known false-positive risks for malloc-heavy
  workers — flag in the issue).

### Open follow-up issues (filed but not picked)

- [#1](https://github.com/hherb/hhagent/issues/1) — narrow macOS `(allow mach-lookup)` to a `global-name` allowlist
- [#2](https://github.com/hherb/hhagent/issues/2) — evaluate `setpgid` → `setsid` for stronger session isolation on macOS
- [#3](https://github.com/hherb/hhagent/issues/3) — drop `SYS_SENDFILE`/`SYS_FADVISE64` shim once libc exposes them on aarch64
- [#4](https://github.com/hherb/hhagent/issues/4) — bump Last-commit + test-count fields whenever a Recently-completed entry is added
- [#5](https://github.com/hherb/hhagent/issues/5) — audit `BASE_ALLOW` against a fixture of common worker binaries
- [#6](https://github.com/hherb/hhagent/issues/6) — tunable `cpu_quota_pct`/`tasks_max` policy fields + `setrlimit`-based `cpu_ms` enforcement (Option G above)

---

## Open questions parked for later

(From the design plan, restated here so they're surfaced when relevant.)

1. Embedding model on-device — bge-m3 vs nomic-embed-text vs ColBERT (Phase 1)
2. ~~Channel approval — passcode pairing vs static contact allowlist (Phase 2)~~ **Resolved 2026-05-06:** pairing flow with WebAuthn-or-OTP fallback, modeled on ZeroClaw's `security/{pairing,webauthn,otp}.rs`. Static contact allowlists rejected as user-hostile and forgeable. Implemented in Phase 2.
3. ~~Egress proxy as separate worker vs in-process in `tool_host`~~ **Resolved 2026-05-06:** separate worker, with the credential-leak scanner co-located so every byte that crosses the trust boundary is inspected once. Cross-references with both reference projects (IronClaw `safety::leak_detector`, ZeroClaw `security/leak_detector.rs`) — convergent design.
4. Skill review workflow for *named* agent-authored Python (Phase 4) — see new Phase 4 line items: trust enum + per-level capability ceiling.
5. Worker keep-alive vs spawn-per-call (currently spawn-per-call; revisit when latency matters)
6. Worker binary discovery in production (currently `target/debug/...` for tests; need a stable install location convention)

## Inspirations / things to read before each milestone

Two adjacent OpenClaw-derived projects ship code we can read (Apache-2.0/MIT, AGPL-compatible) before each new milestone — convergent prior art saves design time:

- **ZeroClaw** ([`zeroclaw-labs/zeroclaw`](https://github.com/zeroclaw-labs/zeroclaw), 100% Rust): read [`crates/zeroclaw-runtime/src/security/`](https://github.com/zeroclaw-labs/zeroclaw/tree/main/crates/zeroclaw-runtime/src/security) — has working `bubblewrap.rs`, `landlock.rs`, `seatbelt.rs`, `firejail.rs`, `pairing.rs`, `webauthn.rs`, `leak_detector.rs`, `workspace_boundary.rs`. Architectural drawback vs us: tools run as in-process Rust traits, OS sandbox wraps the runtime — weaker boundary than our process-per-worker. Don't copy the in-process tool model.
- **IronClaw** ([`nearai/ironclaw`](https://github.com/nearai/ironclaw)): read its dispatcher chokepoint pattern (`ToolDispatcher::dispatch()` is the single audit/safety-validation funnel for *every* action, regardless of caller). Drawbacks: WASM-as-boundary is software-only containment; Postgres+libSQL dual backend is overkill at our stage.

The *defining* architectural difference: hhagent enforces **one OS process + one bwrap/Seatbelt jail per worker**. Both reference projects retreated from that. Don't.

## How to update this document at session end

1. Bump the **Last updated** / **Last commit** / **Branch** fields at the top.
2. Move whatever was the previous "Next TODO" into "Recently completed (this session, YYYY-MM-DD)" if it shipped.
3. Write a fresh "Next TODO (pick one)" with options sized for one session each — include file paths, gotchas, and the verification step.
4. Refresh "Working state" — green-test count, anything new under stubs, anything that became real.
5. Tick the matching items off in [`../ROADMAP.md`](../ROADMAP.md) with the commit hash.
6. Commit both files together with a `docs(handover): ...` message.
