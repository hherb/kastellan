# hhagent — Session Handover

> Rolling document. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. See
> [`README.md`](README.md) for the convention.

**Last updated:** 2026-05-09
**Last commit:** `326104b` (`feat(sandbox/macos): close issues #1 (mach-lookup) and #2 (setsid)`) — rebased on top of `f3fdb14` (Postgres bring-up foundation, Option C2 slice 1)
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
hhagent (Rust workspace, 7 crates, AGPL-3.0)
├── core               hhagent-core: lib + bin (long-running daemon blocking on SIGTERM/SIGINT via tokio::signal::unix; placeholder body until the Phase 1 scheduler lands); tool_host derives lockdown env + spawns watchdog; workspace = per-task scratch with RAII cleanup
├── db                 hhagent-db: pure helpers (build_initdb_argv, build_postgresql_auto_conf, find_pg_bin_dir) + hhagent-db-init bin (idempotent initdb against ~/.local/share/hhagent/pg/data, UDS-only listen, peer auth)
├── sandbox            hhagent-sandbox: SandboxPolicy + LinuxBwrap (now wraps in systemd-run --scope cgroup) + MacosSeatbelt
├── supervisor         hhagent-supervisor: SystemdUser (Linux) + LaunchAgents (macOS) + specs::{core_service_spec, postgres_service_spec} + default_probe (per-OS supervisor probe)
├── protocol           hhagent-protocol: JSON-RPC 2.0 over stdio (working)
├── workers/prelude      hhagent-worker-prelude: Linux-only Landlock + seccomp lock_down (no-op on macOS)
└── workers/shell-exec   hhagent-worker-shell-exec: uses prelude::serve_stdio
```

**`cargo test --workspace` on Linux: 138 tests, 0 skipped, 0 failed, 0 warnings.**
**`cargo test --workspace` on macOS: 86 tests, 0 skipped, 0 failed** *(after this session's macOS hardening; earlier-in-day Linux/db/supervisor work hasn't been re-run on macOS yet — projection ~126 there once `postgres_service_spec`'s +17 unit tests + the +23 db unit tests run, with `postgres_e2e` `[SKIP]`ping until `brew install postgresql@18`).*

| Suite | Tests | What's verified |
| ----- | ----- | --------------- |
| `protocol` unit | 3 | dispatch, parse-error fallback, method-not-found |
| `sandbox` unit (linux) | 16 | bwrap argv builder shape (6) + cgroup `systemd-run` argv builder shape: starts with `systemd-run`, uses `--user --scope --quiet --collect`, sets `MemoryMax`+`MemorySwapMax=0` from policy, omits both when `mem_mb=0`, defense-in-depth `CPUQuota=200%` + `TasksMax=64` defaults, ends with `--` separator, no inner-program leakage, 4 `-p` flags total (10) |
| `sandbox` unit (macos) | 14 | sandbox-exec profile builder shape + path canonicalization + on-host probe + TinyScheme-injection rejection + canonicalize error propagation + **strict profile does NOT contain unrestricted `(allow mach-lookup)`** (issue #1) |
| `sandbox` integration (`linux_smoke`) | 7 | **real** bwrap+cgroup: echo runs jailed, /etc/passwd & /home invisible, listed paths visible, net unreachable under `Net::Deny`, relative-path policy rejected, **mem_burner allocating 256 MiB under `MemoryMax=32M` is OOM-killed by the kernel** |
| `sandbox` integration (`macos_smoke`) | 10 | **real** sandbox-exec: scaffold marker, echo runs jailed, /etc/master.passwd invisible, /Users does not leak username, fs_read paths readable (canonicalize /etc symlinks), /dev/disk0 denied, relative-path policy rejected, network unreachable under `Net::Deny`, **worker is the leader of a fresh session — sid == pid via setsid (issue #2)**, **worker cannot `bootstrap_look_up` `com.apple.coreservices.appleevents` (issue #1)** |
| `core` unit | 16 | `derive_lockdown_env` adds correct env entries (4 tests); watchdog loop honours cancel, fires at deadline, exits early on cancel during sleep, guard's Drop sets cancel flag (4 tests); `is_valid_target_pid` rejects 0/1/u32::MAX/`i32::MAX+1` (1 test); workspace creates layout, drops wipes tree, `fs_write_paths` order, `extend_policy` appends, task-id validation, root auto-create, pre-existing dir refused (7 tests) |
| `core` integration (`shell_exec_e2e`) | 4 | **cross-platform real** core → bwrap+landlock+seccomp (Linux) / sandbox-exec (macOS) → shell-exec round-trip; non-allowlisted argv → POLICY_DENIED; unknown method → METHOD_NOT_FOUND; **workspace e2e**: `Workspace::extend_policy` wires `<root>/<task_id>/{in,out,tmp}` into the policy, sandboxed `cp` reads from `in/` and writes to `out/`, host reads back byte-for-byte, `Workspace::Drop` wipes the whole tree |
| `core` integration (`supervisor_e2e`) | 1 | **cross-platform real** `default_supervisor()` round-trip against the actual `hhagent` binary: build spec via `core_service_spec`, install into `~/.config/systemd/user/` (Linux) or `~/Library/LaunchAgents/` (macOS), pre-start status=Inactive, start, **poll status until Active** then **hold 500 ms and re-check** (rules out flapping/restart loops under the `keep_alive=true` ServiceSpec), sanity-check that the redirected stdout contains the daemon's startup JSON line (`"hhagent core starting"` + `version` field), stop, **poll status until Inactive** within 5 s (proves the daemon's tokio SIGTERM handler exited cleanly *before* `TimeoutStopSec=10` would have SIGKILLed it), uninstall, post-uninstall status=NotInstalled. RAII `ServiceGuard` + `LogDirGuard` clean up on panic. Unique `hhagent-supervisor-test-{pid}-{nanos}` name avoids clobbering a real installed `hhagent-core`. macOS holds the same intra-binary serial mutex as `launchd_agents_smoke.rs` |
| `db` unit | 23 | `build_initdb_argv` (8 tests: argv[0]=binary path, `--pgdata <data_dir>` always present, `--auth-local=peer` + `--auth-host=reject` always present, `--data-checksums` toggles cleanly, blank username falls back to `hhagent`, supplied username flows through verbatim); `build_postgresql_auto_conf` (7 tests: `listen_addresses=''` always emitted, `unix_socket_directories` flows through, `unix_socket_permissions = 0700` baked in, `log_destination=stderr` + `logging_collector=off` for supervisor capture, `max_connections`/`shared_buffers` clamp to ≥ 1, file starts with the don't-edit warning); `find_pg_bin_dir` + candidate ordering (3 tests: PG 18 first, empty list errors, picks first candidate with both `postgres`+`initdb` executable); `is_data_dir_initialized` (2 tests: false for empty, true when `PG_VERSION` regular file present); `require_absolute` + `default_data_dir` + `default_socket_dir` (5 tests: rejects relative, accepts absolute, default data dir under `$HOME/.local/share/hhagent/pg/data`, socket dir is `<data>/sockets`) |
| `db` integration (`postgres_e2e`) | 1 | **real** `find_pg_bin_dir` → `initdb` (PGDG `/usr/lib/postgresql/18/bin/initdb` on Linux, Homebrew on macOS) → write `postgresql.auto.conf` → `default_supervisor()` install → start → wait for `Active` (≤ 15 s) → wait for `<sockets>/.s.PGSQL.5432` to appear → hold 500 ms and re-check Active (no flapping under `Restart=on-failure`) → spawn `psql -h <socket_dir> -U <whoami> -At -c 'SELECT 1'` over the UDS (peer auth) → assert stdout trim equals `1` → stop → wait for `Inactive` → uninstall → status=NotInstalled. Per-test temp data dir + log dir under `std::env::temp_dir()`; both wiped by `PathGuard` Drop on success or panic. Unique `hhagent-supervisor-test-pg-{pid}-{nanos}` service name. Skips with `[SKIP]` when no Postgres binaries on host or supervisor probe fails. Runtime ~1.8 s on the DGX Spark |
| `prelude` unit | 11 | env-var parsing, profile parsing, BPF program builds (Strict + NetClient), unshare/mount/ptrace/bpf absent from allow-list under both profiles, socket present *only* in NetClient, essential syscalls present in BASE_ALLOW |
| `prelude` integration (`landlock_smoke`) | 4 | write-to-non-allowlisted denied with EACCES; allowlisted scratch write works; `/usr` reads still work; **v6 ABI yields `FullyEnforced` on this kernel** |
| `prelude` integration (`seccomp_smoke`) | 6 | `unshare(CLONE_NEWUSER)` and `mount(...)` killed with SIGSYS under both Strict and NetClient; `socket(AF_INET, SOCK_STREAM)` killed under Strict, survives under NetClient; `getpid()` survives |
| `supervisor` unit (linux) | 44 | `build_unit_file` shape (14 tests: section order, Description, ExecStart program+args, arg quoting + escape of `"`/`\`, Environment ordering, Environment value quoting, WorkingDirectory present/absent, log redirects, keep_alive Restart=on-failure, no-Restart when keep_alive=false, TimeoutStopSec always, [Install] WantedBy=default.target); `validate_service_name` (6 tests: typical names, empty, traversal, dot/dash prefix, overlong, whitespace+specials); driver against custom units_dir (7 tests: install writes file, rejects relative program, rejects invalid name, creates units_dir, uninstall removes file, uninstall idempotent, status NotInstalled when absent); `specs::core_service_spec` (8 tests: canonical name `hhagent-core`, caller-supplied program path flows through, args+env empty by default, no working_dir, keep_alive=true regression pin (flipped from false 2026-05-09 when the daemon became long-running), log paths under log_dir with predictable filenames, stdout/stderr distinct); `specs::postgres_service_spec` (8 tests: canonical name `hhagent-postgres`, caller-supplied program path flows through, args=`["-D", <data_dir>]` in order, env empty by default, no working_dir, keep_alive=true so a postgres crash respawns under `Restart=on-failure`, log paths under log_dir with predictable filenames, stdout/stderr distinct); `canonical_service_names_are_distinct` (1 test: `hhagent-core` ≠ `hhagent-postgres` so unit/agent files never collide) |
| `supervisor` unit (macos) | 52 | `build_plist` shape (14 tests: XML preamble + DOCTYPE, Label, ProgramArguments order, XML-escaping of `<`, `>`, `&`, `"`, `'` in args, EnvironmentVariables presence/order/omission-when-empty, WorkingDirectory present/absent, log redirects, RunAtLoad=true unconditional, KeepAlive=true/false mirror of spec, ExitTimeOut always, Label XML-escaped); `validate_service_name` (6 tests: typical names incl. reverse-DNS like `org.hhagent.core`, empty, traversal, dot/dash prefix, overlong, whitespace+specials); helpers (7 tests: `xml_escape` predefined entities + Unicode passthrough, `parse_print_state` indented/multi-word/absent, `is_no_such_service_error` phrases, `user_domain_target` `gui/<digits>` shape); driver against custom agents_dir (8 tests: install writes plist, rejects relative program, rejects invalid name, rejects relative working_dir, creates agents_dir, uninstall removes plist, uninstall idempotent, status NotInstalled when absent); `specs::*` (17 tests: 8 `core_service_spec` + 8 `postgres_service_spec` + 1 `canonical_service_names_are_distinct` — same suite runs on both OSes since `specs.rs` has no platform deps) |
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

> **Two parallel work streams shipped on 2026-05-09 from different machines.**
> The Linux work below (Postgres bring-up + supervisor wiring + native PG FTS
> + relational graph commitment) was implemented and committed first
> (`f3fdb14` and friends). The macOS hardening below (issues #1 + #2) was
> implemented in a parallel macOS session and rebased on top.

### Linux: Phase 0 cont. (Option C2 — Postgres bring-up, foundation slice)

**Install PG 18 binaries, idempotent `hhagent-db-init`, `postgres_service_spec`, full e2e against `default_supervisor()`.**

This is the first slice of HANDOVER's "headline next-pickup": a private
per-user PG cluster under `~/.local/share/hhagent/pg/data` managed by a
user-level supervisor unit, never network-listen, peer auth over UDS.
Foundation only — migrations, sqlx-cli, and the core probe land in a
follow-up session.

- **`scripts/linux/install-postgres.sh` (~140 lines):** idempotent PGDG
  setup. Installs `postgresql-common`, runs the upstream
  `apt.postgresql.org.sh` helper to add the signed repo (with manual
  `curl + sources.list.d` fallback for older `postgresql-common`),
  then `apt install postgresql-18 postgresql-client-18
  postgresql-18-pgvector`. Crucially also `systemctl stop` +
  `systemctl disable` the auto-created system-wide
  `postgresql@18-main.service` so it can never collide with our
  user-instance — Debian's postgresql package launches a system
  cluster on port 5432 by default; we want only the *binaries* on the
  system, with our cluster running under
  `~/.local/share/hhagent/pg/data` and listening on a UDS only.
- **New crate `hhagent-db` (~620 lines split across `lib.rs`, `bin/hhagent-db-init.rs`, `tests/postgres_e2e.rs`):**
  - **Pure functions in `lib.rs`** (23 unit tests):
    `build_initdb_argv(initdb_bin, &InitDbOptions) -> Vec<String>`
    pins `--auth-local=peer` + `--auth-host=reject` (so a future
    operator who re-enables TCP still gets refused at the auth
    layer — defense-in-depth) and `--data-checksums` by default.
    `build_postgresql_auto_conf(&PgConfigOptions) -> String` emits
    the file we drop into `<data_dir>/postgresql.auto.conf` after
    `initdb` (Postgres applies this file *after* `postgresql.conf`,
    so values here always win). The most important line is
    `listen_addresses = ''` (no TCP listener at all); also pins
    `unix_socket_directories = '<dir>'`,
    `unix_socket_permissions = 0700` (only the owning OS user can
    `connect()`), `password_encryption = 'scram-sha-256'`,
    `log_destination = 'stderr'` + `logging_collector = off` so the
    supervisor captures the stream.
    `find_pg_bin_dir(candidates)` probes a priority-ordered candidate
    list (PG 18 → 14, PGDG layout on Linux,
    `/opt/homebrew/opt/postgresql@<ver>/bin` and
    `/usr/local/opt/postgresql@<ver>/bin` on macOS) for a directory
    containing both executable `postgres` + `initdb`.
    `is_data_dir_initialized(data_dir)` checks for
    `<data_dir>/PG_VERSION` regular file — Postgres's canonical "this
    is a populated cluster" marker, the same one `pg_ctl` reads.
    Pure functions follow the same pattern as
    `sandbox::linux_bwrap::build_argv` and
    `supervisor::systemd_user::build_unit_file` (separately testable
    from any I/O).
  - **`bin/hhagent-db-init`** drives the helpers: parse argv (`--data-dir`,
    `--bin-dir`, `--username`, `--help`), resolve defaults
    (`$HOME/.local/share/hhagent/pg/data`, auto-detect bin dir,
    `hhagent` superuser), short-circuit if `PG_VERSION` already
    present (re-running is safe — it still re-writes
    `postgresql.auto.conf` so config drift is corrected), spawn
    `initdb` with the argv, create `<data_dir>/sockets` mode 0700,
    atomically write `postgresql.auto.conf` (write-to-tmp + fsync +
    rename — same idiom as `supervisor::systemd_user::install`).
    Verified end-to-end against a real PG 18.3 cluster in a temp dir;
    layout, PG_VERSION=18, postgresql.auto.conf, sockets/0700, and
    second-run idempotency all confirmed before the e2e was written.
- **New `supervisor::specs::postgres_service_spec` (+ `POSTGRES_SERVICE_NAME` const, +9 unit tests):**
  Pure ServiceSpec builder mirroring `core_service_spec`. Caller
  passes `postgres_binary`, `data_dir`, `log_dir`; helper returns
  `name = "hhagent-postgres"`, `args = ["-D", <data_dir>]` (the
  socket path comes from `postgresql.auto.conf` inside the data dir,
  so no `-k` flag at the supervisor layer), empty env, no
  working_dir, `keep_alive = true` (postgres is a long-running
  daemon; a crash should respawn under `Restart=on-failure`), and
  predictable log filenames `<name>.out`/`.err`. Same shape and
  reasoning as `core_service_spec`, paired regression-test pin.
- **New `db/tests/postgres_e2e.rs::postgres_install_start_select_one_uninstall` (~280 lines, 1 test):**
  Full real-world round-trip on Linux & macOS via
  `default_supervisor()`. Skips with `[SKIP]` when no Postgres
  binaries on host (so `cargo test --workspace` stays green on hosts
  without PG installed) or supervisor probe fails. Test flow:
  `find_pg_bin_dir` → `initdb` against a temp data dir using the
  pure helpers (peer auth, --data-checksums) → write
  `postgresql.auto.conf` (UDS-only) → build spec via
  `postgres_service_spec`, override name to
  `hhagent-supervisor-test-pg-{pid}-{nanos}` for collision-free
  parallel test runs → install → start → poll status until Active
  (≤ 15 s) → poll for `<sockets>/.s.PGSQL.5432` to appear → hold
  500 ms and re-check Active (rules out flapping under
  `Restart=on-failure`) → spawn `psql -h <socket_dir> -U <whoami> -At
  -c 'SELECT 1'` over the UDS (peer auth lines up because the test
  ran initdb with `--username=$(whoami)`) → assert stdout trim equals
  `1` → stop → poll until Inactive → uninstall → status=NotInstalled.
  RAII `ServiceGuard` and two `PathGuard`s (data dir, log dir) clean
  up even on panic. Runtime ~1.8 s on the DGX Spark.
- **Both extension-deferral issues dropped as won't-fix ([#9](https://github.com/hherb/hhagent/issues/9) Apache AGE, [#10](https://github.com/hherb/hhagent/issues/10) ParadeDB pg_search).**
  Both extensions were originally on the wishlist for this session
  ("install if available, defer if not"). After looking at what each
  actually buys for *our* use case versus the cost of tracking their
  PG 18 build availability, neither earns its keep:
  - **pg_search:** for ≤ ~1M memories at a few hundred writes/day,
    native `tsvector`+GIN with `ts_rank` is comparable to BM25 in
    recall quality, and the embedding (pgvector) dominates the
    lexical re-ranker anyway. Hybrid lex+vector via Reciprocal
    Rank Fusion is ~5 lines of SQL, not a dependency.
  - **Apache AGE:** for a personal-agent graph (low thousands of
    nodes, occasional 2-hop, almost never 5-hop), recursive CTEs
    handle variable-length paths fine. AGE's upstream lags new PG
    releases (PGDG only ships up to PG 16 today, RC-tagged), and its
    JSONB-backed storage fights natural Postgres indexing on the
    same columns as pgvector/tsvector. The Cypher language doesn't
    earn anything when our queries are agent-generated rather than
    human-written.
  - **What ships instead:** plain `entities` + `relations` tables
    in `0001_init.sql` (next session), plus a `Graph` trait in
    `db/src/graph.rs` so the rest of the codebase never writes
    graph SQL directly. All traversal lives behind
    `Graph::{neighbors, path, …}` — same chokepoint discipline as
    `tool_host::dispatch()` for tools. If we ever measure a real
    bottleneck (perf or expressiveness), we swap the impl, not the
    call sites — we're not painted into a corner.

**Test count:** 105 → **138** on Linux (+23 db unit, +1 db integration,
+9 supervisor specs, no skips, no warnings). macOS projects to ~115 with
PG installed via Homebrew, ~114 without (the e2e skips cleanly).

**Why postgres_service_spec carries no `-k` flag.** First TDD pass had
`args = ["-D", <data_dir>, "-k", <socket_dir>]` so the spec controlled
both. But that means the supervisor's view of the socket path can drift
from what's in `postgresql.auto.conf`, and clients (the future memory
worker, tests) read the UDS path from a *third* place. The single source
of truth has to be `postgresql.auto.conf` because that's what postgres
actually obeys — so the spec passes only `-D` and trusts the conf file.
Tests read the same `default_socket_dir(<data_dir>)` constant the conf
writer uses; production reads `<data_dir>/sockets` by the same convention.

**Why we picked `<data_dir>/sockets/` over `/run/user/<uid>/hhagent-pg/`
or `/tmp`.** Three reasons: (1) the data dir already has mode 0700
ownership by the cluster's OS user, so a sub-directory inherits the
right access shape; (2) it dodges the
`/run/user/<uid>` (Linux-only, depends on systemd) vs `/tmp` (macOS,
shared with anyone) split — same path on both OSes; (3) the cluster's
lifecycle owns it, so when the data dir is torn down the socket dir
goes with it. The `unix_socket_directories` config setting accepts a
list, so a future operator who wants an additional socket location
(e.g. a `/run/user/<uid>` symlink for a backwards-compat client) can
add it without removing ours.

**Why we disable the system `postgresql@18-main.service`.** Debian's
postgresql package post-install hooks call `pg_createcluster 18 main`,
which spins up a system-wide cluster on port 5432. We never want that
running — our auth, our data, our supervised lifecycle. Even though we
listen UDS-only and would not collide on the network port, the system
cluster competing for `pg_lsclusters` output, eating disk under
`/var/lib/postgresql/`, and showing up in `systemd` is operator
confusion we don't need. The install script stops + disables it.

---

## Recently completed (previous session, 2026-05-09)

**Phase 0 cont. (Option H) — turn `core/src/main.rs` into a real long-running daemon and flip `core_service_spec` to `keep_alive=true`.**

Closed Option H from the previous session's Next-TODO list. The
agent-core binary now blocks on SIGTERM/SIGINT instead of exiting
immediately, so `start` puts the supervisor unit in `Active` and it
stays there until `stop`. The `core_service_spec` ServiceSpec
helper flips to `keep_alive=true` to match — meaningful now that
the daemon body actually runs forever, where it would have been
cargo-culted noise on the previous "log line and exit 0" shape.

- **`core/src/main.rs` rewrite (~45 lines):** drops the `(skeleton)`
  suffix from the startup line ("hhagent core starting" is now the
  precise contract), then `await wait_for_shutdown()`. Helper uses
  `tokio::signal::unix::signal(SignalKind::terminate())` and
  `SignalKind::interrupt()` in a `tokio::select!` so either signal
  returns Ok and `main` logs a clean "hhagent core shutting down"
  line and exits 0. systemd treats exit-on-SIGTERM as success
  (so `Restart=on-failure` does *not* trigger an unwanted
  respawn); macOS launchd's `bootout` removes the agent from the
  domain entirely before `KeepAlive` would consider restarting.
  `tokio::signal::unix` is unix-only, which matches the rest of
  the workspace's Linux+macOS target set; if Windows support ever
  comes up, this is the natural place to add a `cfg(unix)` gate.
  No periodic work today — the signal future is the *only* thing
  that should ever wake the daemon, anything else would be a bug.
  This is the placeholder for the Phase 1 scheduler loop.
- **`supervisor/src/specs.rs`: `keep_alive` flipped `false` → `true`.**
  Doc-comment rewritten to explain the new semantics
  (`Restart=on-failure` on systemd, `KeepAlive=true` on launchd —
  both restart on *crash* but not on clean SIGTERM exit).
  Regression test renamed
  `core_service_spec_keep_alive_is_false_for_now` →
  `core_service_spec_keep_alive_is_true`; body asserts
  `spec.keep_alive == true`.
- **`core/tests/supervisor_e2e.rs` contract upgrade:** new
  `wait_for_status(predicate, timeout)` helper polls `sup.status`
  with the same 50 ms tick / 5 s budget as the existing
  `wait_for_log_match`. Test flow becomes: install → assert
  `Inactive` → start → wait until `Active` → **hold 500 ms and
  re-check** (rules out flapping under `Restart=on-failure` /
  `KeepAlive=true`) → sanity-check the redirected stdout for the
  daemon's startup JSON line → stop → wait until `Inactive`
  within 5 s → uninstall → assert `NotInstalled`. The Inactive
  poll after stop is the contract-pin for the daemon's signal
  handler: if `wait_for_shutdown` ever stops responding to
  SIGTERM, `systemctl --user stop` would eventually SIGKILL the
  daemon after `TimeoutStopSec=10`, which surfaces here as a
  timeout — a noisy failure rather than a silent one. The log-line
  poll demoted from primary signal to belt-and-suspenders sanity
  check. Test runtime grew ~600 ms (the explicit hold + the
  Inactive poll) but is still well under 1.5 s on this host.
- **Closes [#7](https://github.com/hherb/hhagent/issues/7).** With
  `(skeleton)` gone from the startup line, the substring
  `"hhagent core starting"` is now the precise startup contract —
  no further tightening needed until the daemon body changes
  again.

**Test count:** 105 → 105. No new tests, but the e2e contract is
materially stronger (status-based + stable-Active window + clean
shutdown). The unit test got a rename, not a delta. macOS
projection is unchanged at 92 tests.

**Why no exponential backoff yet.** The systemd unit emits
`Restart=on-failure RestartSec=5` (constant 5 s); systemd 252+
supports `RestartSteps` / `RestartMaxDelaySec` for true
exponential backoff but the macOS LaunchAgent `KeepAlive=true`
has no such knob (launchd uses an internal throttle that's not
operator-controllable). A cross-platform exponential-backoff
shape needs care; filed as the remaining "auto-restart with
backoff" item in ROADMAP rather than smuggled into this session.

---

## Recently completed (earlier in 2026-05-09 session)

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

**Follow-up hardening (`a6580a5`).** Two small fixes from a review of
`5d02a2f`, no test-count change (still 105 on Linux):
- New `LogDirGuard` in `core/tests/supervisor_e2e.rs` mirrors the
  existing `ServiceGuard` so a panic mid-test no longer leaks the
  per-test `temp_dir/hhagent-supervisor-e2e-…/` log dir alongside its
  (already-cleaned) supervisor unit. Drop order on success: log dir
  → service uninstall → macOS serial-mutex release (resource then
  lock — the right sequence).
- Cheap insurance assert that the constructed
  `hhagent-supervisor-test-{pid}-{nanos}` name stays inside both
  backends' `MAX_NAME_LEN=200`. Today's worst case is ~54 chars, so
  the assert trips well before `install` would, and the panic message
  tells the next person what to rework.

Two follow-ups from the same review filed but deferred:
- [#7](https://github.com/hherb/hhagent/issues/7) — tighten the daemon
  log-line substring match when the daemon body is rewritten (no-op
  until then; coupled to dropping `(skeleton)` from
  `core/src/main.rs`'s startup line, which is part of Option H).
- [#8](https://github.com/hherb/hhagent/issues/8) — collapse the
  `default_probe`/`default_supervisor` cfg-ladder duplication once a
  third entry point or backend OS appears.
### macOS: Seatbelt hardening — closed two open GitHub issues (#1 + #2)

Both issues had been filed during the post-Phase-0b code review on 2026-05-07
(see HANDOVER entry below). They are now closed in code, with negative tests
that pin the new behaviour against future regressions.

### Issue #2 — `setpgid(0,0)` → `setsid()`

`MacosSeatbelt::spawn_under_policy` previously called
`Command::process_group(0)` (which delegates to `setpgid(0, 0)`), giving
the worker its own process group but leaving it attached to the parent's
session and controlling terminal. Now switched to a `pre_exec` hook that
calls `libc::setsid()` directly between `fork(2)` and `execve(2)`. The
worker is the leader of a brand-new session (`sid == pid`) and has no
controlling terminal — any future open of `/dev/tty` fails with `ENXIO`
regardless of profile broadening.

- **`sandbox/src/macos_seatbelt.rs`**: removed `cmd.process_group(0)`,
  added an `unsafe { cmd.pre_exec(...) }` block that calls
  `libc::setsid()` and propagates `errno` via `io::Error::last_os_error()`.
  `setsid` is on the POSIX async-signal-safe list (signal-safety(7) on
  Linux, sigaction(2) on macOS), so it is a legal pre_exec operation.
  `setsid` implies `setpgid(0, 0)` in the new session, so dropping the
  old `process_group(0)` call is a strict subsumption — no behavioural
  loss.
- **`sandbox/tests/fixtures/sid_probe.rs`** (new, ~25 lines): a tiny
  Rust binary that prints `<pid> <sid>` and exits 0 (or 1 on syscall
  failure). Built into `target/debug/sid_probe` via a `[[bin]]` stanza
  in `sandbox/Cargo.toml`, mirroring the existing `net_probe` /
  `mem_burner` pattern.
- **`sandbox/tests/macos_smoke.rs::worker_runs_in_its_own_session`**
  (new, integration test): spawns `sid_probe` under the strict policy
  and parses `<pid> <sid>` from stdout. Asserts `sid == pid` (worker is
  a session leader) and `sid != parent_sid` (defence in depth). The
  `sid == pid` invariant is strictly stronger than a "different from
  parent" check — the only way to satisfy it is to have actually
  called `setsid()` in the child.
- **`sandbox/Cargo.toml`**: added `libc = { workspace = true }` as a
  direct dependency (the integration test's defence-in-depth check
  calls `libc::getsid(0)` to compare against the worker's reported sid).
- **Test count delta**: 83 → 84 (+1 smoke). All other tests unchanged.

### Issue #1 — narrow `(allow mach-lookup)` to a `global-name` allowlist

The original Phase 0b profile emitted an unrestricted `(allow mach-lookup)`
rule on the rationale "Python and libdispatch might need it." Empirical
finding this session: **none of our shipping workers need it on macOS
26.4 ARM64.** Verified by spawning each binary under a probe profile
with the rule replaced by `(deny mach-lookup)`:

- `hhagent-worker-shell-exec` → starts cleanly, prelude reports
  `lockdown SkippedNonLinux`.
- `sid_probe`, `net_probe`, `mach_probe` (all Rust) → exit 0.
- `/bin/echo`, `/bin/sh`, `/bin/cat`, `/bin/ls`, `/usr/bin/true` →
  exit 0.

The unrestricted rule was speculative, not load-bearing. Removed it from
`build_profile` entirely; kept the broader form in `probe()` where the
deliberately-permissive canary lives. When Phase 4 introduces
`python-exec`, the actual Mach service set CPython needs at startup
should be captured at that time and emitted as a *narrow*
`(allow mach-lookup (global-name "..."))` form — never as the broad
rule again.

- **`sandbox/src/macos_seatbelt.rs::build_profile`**: dropped the
  `(allow mach-lookup)` line. Replaced its inline rationale with a long
  comment describing the empirical methodology used to set the new
  baseline, the threat-model reason for denying (Mach bootstrap
  namespace is the back-end for every registered launchd service —
  pasteboard, Apple Events broker, distributed notifications, etc.,
  many of which bypass file/network rules entirely), and the contract
  for Phase 4 (`python-exec` may add a narrow allowlist; never re-add
  the unrestricted form).
- **`sandbox/src/macos_seatbelt.rs::tests`**:
  - `profile_emits_always_on_allows` — removed the
    `(allow mach-lookup)` needle from the assertion list; nothing else
    changed.
  - **new** `profile_does_not_grant_unrestricted_mach_lookup` —
    asserts the strict profile contains no `(allow mach-lookup)`
    substring and no whitespace-only-trailing variants. Pins the
    invariant against future refactors.
- **`sandbox/tests/fixtures/mach_probe.rs`** (new, ~50 lines): a tiny
  Rust fixture that calls `bootstrap_look_up(bootstrap_port,
  "com.apple.coreservices.appleevents", &mut port)` via `extern "C"`
  declarations against `libSystem`. Apple Events broker is a
  deliberately benign-but-non-essential service: present on every
  macOS host, but no shipping hhagent worker has any legitimate reason
  to talk to it (it's the back-end for AppleScript-driven cross-app
  automation — the canonical privilege-escalation surface). Built into
  `target/debug/mach_probe` via a `[[bin]]` stanza.
- **`sandbox/tests/macos_smoke.rs::worker_cannot_look_up_arbitrary_mach_services`**
  (new, integration test): spawns `mach_probe` under the strict policy
  and asserts non-zero exit + stderr containing `bootstrap_look_up
  failed`. With the old unrestricted rule, `mach_probe` outside the
  sandbox returns `port=2819`-ish; with the new profile it returns
  `kr=1100` (sandbox-imposed denial) — verified end-to-end.
- **Test count delta**: 84 → 86 (+1 unit, +1 smoke).

### Inline-comment update in build_profile's /dev block

The `/dev/tty` exclusion block previously cited `process_group(0)` as
the reason `/dev/tty` had to be denied at the profile level. After
issue #2 the worker is in a fresh session with no controlling terminal,
so `/dev/tty` is unusable (`ENXIO`) regardless of the profile rule. The
comment was rewritten to reflect this and to flag that the profile-level
deny remains valuable as defence in depth: any future broadening of
`/dev` would need to remember to re-deny `tty` explicitly.

### Threat-model + roadmap updates

- `docs/threat-model.md` "negative tests already shipped" gained two
  rows for the issue #1 + #2 smoke tests.
- `docs/devel/ROADMAP.md` Phase 0b section now annotates the two
  hardenings on the original sandbox-exec line items rather than adding
  new bullets — reflects that the issues were *closed* this session,
  not new scope.

**Total tests after this macOS session:** 86 on macOS (was 83). No existing
test changed; three new tests were added.

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
supervisor backends are real.** Two concrete services are now
wired and proven end-to-end against `default_supervisor()`:
`hhagent-core` (the agent-core daemon, blocks on SIGTERM/SIGINT) and
`hhagent-postgres` (the per-user PG cluster on a UDS). Remaining
Phase 0 work: schema + migrations + core probe (the rest of Option
C2 — see C2.2 below), the audit log writer + JSONL mirror, the
LLM-router HTTP client stub, and the supervisor "auto-restart with
backoff on worker crash" finishing-touch (`Restart=on-failure
RestartSec=5` is constant; cross-platform exponential backoff still
parked).

### Option A — Phase 0b: macOS port  *(SHIPPED 2026-05-07)*

### Option B' — Phase 0 hardening: stage 2  *(SHIPPED 2026-05-08)*

### Option D — Phase 0 polish: per-task scratch + wall-clock kill  *(SHIPPED 2026-05-08 — `9333311`, `57edfb2`)*

### Option E — cgroup v2 CPU/memory caps  *(SHIPPED 2026-05-09 — see "Recently completed")*

### Option F — workspace+worker e2e test  *(SHIPPED 2026-05-08 — see "Recently completed")*

### Option C1 — Linux supervisor scaffold  *(SHIPPED 2026-05-10 — see "Recently completed")*

### Option C3 — macOS LaunchAgent supervisor backend  *(SHIPPED 2026-05-08 — see "Recently completed")*

### Option C4 — wire core into the supervisor  *(SHIPPED 2026-05-09 — see "Recently completed")*

### Option C2 — Phase 0 cont.: Postgres bring-up (foundation slice)  *(SHIPPED 2026-05-09 — see "Recently completed (this session)")*

### Option C2.2 — Phase 0 cont.: schema + migrations + core probe (the rest of C2)

(Now the headline next-pickup. Foundation is in place: the cluster
runs under `default_supervisor()`, listens on a UDS only, accepts
peer auth — proven by `db/tests/postgres_e2e.rs::postgres_install_start_select_one_uninstall`.
What's missing is the *schema* and the migration runner that
`hhagent-core` will use on bring-up.)

- **`db/migrations/0001_init.sql`** — `audit_log`, `tasks`, `memories`,
  `entities`, `relations`, `secrets`. Schema notes:
  - `audit_log`: append-only (no UPDATE/DELETE GRANT), strictly
    monotonic `id BIGSERIAL`, `ts TIMESTAMPTZ DEFAULT now()`, `actor`,
    `action`, `payload JSONB`, indexes on `ts` and on `actor, ts`.
  - `memories`: text + `pgvector::vector` column for embeddings + a
    `tsvector` GENERATED column over the body with a GIN index for
    `ts_rank`-based lexical retrieval; choose embedding dimension up
    front (1024 for bge-m3 is the leading Phase 1 candidate but defer
    to Open Question #1).
  - `entities`: `(id, kind, name, attrs JSONB, embedding vector(N))`,
    `UNIQUE(kind, name)`, `kind+name` and `attrs` GIN indexes.
  - `relations`: `(id, src_id, dst_id, kind, attrs JSONB)` with
    cascading delete from `entities`, `(src_id, kind)` and
    `(dst_id, kind)` indexes. No graph extension; recursive CTEs
    handle variable-length traversal behind a `Graph` trait in
    `db/src/graph.rs` (see "Graph abstraction" below).
  - `secrets`: AES-256-GCM ciphertext columns; key from OS keyring
    (libsecret on Linux / Keychain on macOS) — *outside* this
    migration's scope, but pin the column shape now.
- **Graph abstraction in `db/src/graph.rs`** — `pub trait Graph` with
  `upsert_entity`, `upsert_relation`, `neighbors`, `path`, etc. All
  call sites in `core` go through this trait; no module outside `db`
  ever writes raw SQL against `entities`/`relations`. Same chokepoint
  discipline as `tool_host::dispatch()`. The trait makes a future
  swap to AGE/Neo4j/Memgraph mechanical (one impl, no call-site
  churn) if a real bottleneck ever appears.
- **Choose the migration runner.** Three viable options:
  - `sqlx-cli` + `sqlx::migrate!()` macro at startup (compile-time
    embedded migrations; what HANDOVER previously suggested).
  - `refinery` (similar shape, lighter dep tree).
  - Hand-rolled: just run the `.sql` file with `psql` on first
    bring-up and track applied migrations in a `_schema_migrations`
    table ourselves.
  Lean toward `sqlx-cli` for the MIT-OR-Apache-2.0 license, the
  `query!()` macro's compile-time SQL validation downstream, and the
  established `embedded::migrate!()` shape that doesn't require
  shipping the migrations alongside the binary.
- **Core probe in `core`** that, on startup:
  1. Resolves the cluster's socket dir (`<data_dir>/sockets`).
  2. Connects with the configured role (default `hhagent`).
  3. Runs `sqlx::migrate!()`.
  4. Emits an `audit_log` row on bring-up: actor=`core`,
     action=`startup`, payload=`{"version": …, "git": …}`.
  5. Fails closed if any of the above errors — the daemon should not
     reach the (future) scheduler loop without a working DB.

**Gotchas:**
- `core` does not currently depend on `hhagent-db`. Adding the dep is
  fine but the migration runner choice will pull in either `sqlx`
  (heavy: tokio + connection pool + macros) or `refinery` (lighter).
  Pick before wiring.
- Production install layout. `db/migrations/*.sql` lives in the
  source tree today; `sqlx::migrate!()` embeds them at compile time
  so a binary install doesn't need them on disk. Pin this shape now
  so we don't have to retrofit a "find migrations on disk" code path.
- The current `keep_alive=true` postgres ServiceSpec relies on
  Postgres exiting non-zero on crash, which it does. Smart shutdown
  (default SIGTERM behavior) waits for clients to disconnect; if the
  scheduler ever holds long-lived connections, `stop` could exceed
  `TimeoutStopSec=10`. Track this and consider `KillSignal=SIGINT`
  (fast shutdown) at the supervisor layer for postgres specifically.

### Option H — long-running daemon + `keep_alive=true`  *(SHIPPED 2026-05-09 — see "Recently completed (previous session)")*

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
- [#8](https://github.com/hherb/hhagent/issues/8) — collapse `default_probe` / `default_supervisor` cfg-ladder duplication once a third entry point or backend OS appears
(All Phase 0 follow-up issues filed in earlier sessions are still open: [#1](https://github.com/hherb/hhagent/issues/1)–[#6](https://github.com/hherb/hhagent/issues/6), [#8](https://github.com/hherb/hhagent/issues/8). Both extension-deferral issues filed at the start of this session are now closed won't-fix — see below.)

(Closed in this session, both as won't-fix after review: [#9](https://github.com/hherb/hhagent/issues/9) Apache AGE — relational `entities`/`relations` behind a `Graph` trait + recursive CTEs are sufficient for a personal-agent graph; AGE upstream lags PG releases and stores attributes in JSONB which fights pgvector/tsvector indexing. [#10](https://github.com/hherb/hhagent/issues/10) ParadeDB `pg_search` — native `tsvector`+GIN+`ts_rank` is comparable to BM25 at our corpus size; the embedding dominates the lexical re-ranker; RRF is ~5 lines of SQL.)

(Closed in earlier 2026-05-09 session: [#7](https://github.com/hherb/hhagent/issues/7) — daemon log-line substring is now precise after `(skeleton)` was dropped from the startup line.)

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
