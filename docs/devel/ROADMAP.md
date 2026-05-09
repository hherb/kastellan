# hhagent — Development Roadmap

Sequenced feature list. Items are added whenever a feature decision is made
and ticked off as they ship. Order reflects expected build sequence — earlier
items unlock later ones.

> **How to update.** When we agree on a new feature, append it to the most
> appropriate phase (or a new phase) — placed at the position that respects
> dependencies. When an item ships, change `[ ]` → `[x]` and add the commit
> hash. Don't delete completed items; they document the build sequence.

---

## Phase 0 — Skeleton & First Sandboxed Worker (Linux)

- [x] Cargo workspace, AGPL-3.0 license, README, .gitignore — `140eec5`
- [x] `hhagent-core` (bin+lib stub) — `140eec5`
- [x] `hhagent-sandbox` crate skeleton (trait + policy struct) — `140eec5`
- [x] `hhagent-supervisor` crate skeleton — `140eec5`
- [x] Architecture & threat-model doc skeletons — `140eec5`
- [x] Linux bwrap backend (`linux_bwrap.rs`): unshare-all, FS bind, --clearenv, --setenv, die-with-parent, new-session, as-pid-1 — `eae3df4`, `f2411ec`
- [x] AppArmor `unprivileged_userns` workaround: `scripts/linux/install-bwrap-apparmor-profile.sh` + runtime `LinuxBwrap::probe()` — `eae3df4`
- [x] Sandbox negative tests: /etc/passwd invisible, /home invisible, listed paths visible, net unreachable under `Net::Deny`, relative paths rejected — `eae3df4`
- [x] `hhagent-protocol` crate: JSON-RPC 2.0 server/client over stdio (MCP-stdio compatible) — `f2411ec`
- [x] `workers/shell-exec`: argv allowlist, no shell interpretation, allowlist via `HHAGENT_SHELL_ALLOWLIST` env — `f2411ec`
- [x] `core::tool_host::spawn_worker`: spawn worker under sandbox, return connected protocol Client — `f2411ec`
- [x] End-to-end test: core → bwrap → shell-exec → JSON-RPC echo round trip + POLICY_DENIED + METHOD_NOT_FOUND — `f2411ec`

## Phase 0 hardening — Defence in depth (Linux)

- [x] Landlock LSM as second FS-allowlist layer inside the worker before exec (defence-in-depth on top of bwrap) — `3210f70` *stage 1*: targets ABI v1, RO+exec on `/usr`, `/lib*`, `/bin`, `/sbin`, `/etc/ld.so.cache`, `/dev`, `/proc`; RW from `HHAGENT_LANDLOCK_RW` env
- [x] seccomp-bpf syscall filter — `3210f70` *stage 1*: deny-list of catastrophic syscalls (`unshare`, `setns`, `mount`, `umount2`, `pivot_root`, `init_module`, `finit_module`, `delete_module`, `ptrace`, `bpf`, `perf_event_open`, `kexec_load`, `kexec_file_load`, `reboot`, `swapon`, `swapoff`, `settimeofday`, `clock_settime`, `clock_adjtime`, `adjtimex`, `keyctl`, `add_key`, `request_key`, `personality`); SIGSYS-kill action; same set for both Strict and NetClient profiles
- [x] **Bug fix**: `LinuxBwrap::probe()` was missing `/lib*` symlinks, causing all bwrap-dependent tests to silently skip with "false green"; probe now mirrors `build_argv` so a green probe means real containment — `3210f70`
- [x] **Worker prelude crate** (`workers/prelude`, `hhagent-worker-prelude`) — `serve_stdio` wrapper that calls `lock_down()` before serving; tested via subprocess `lockdown_probe` binary — `3210f70`
- [x] **`tool_host` derives lockdown env** — `derive_lockdown_env` injects `HHAGENT_LANDLOCK_RW` (from `policy.fs_write`) and `HHAGENT_SECCOMP_PROFILE` (from `policy.profile`) so callers cannot accidentally skip the worker-side layer — `3210f70`
- [x] *Stage 2*: migrate seccomp to per-profile **allow-list** replacing the current deny-list — `97d4465` (~110 syscalls in `BASE_ALLOW` + 19 x86_64-only legacy + 18 in `NET_CLIENT_ADDITIONS`; `Profile::Strict` kills `socket()` while `Profile::NetClient` permits it; verified by `socket_is_killed_under_strict` and `socket_survives_under_net_client`). *Subsequent broadening*: `copy_file_range`, `sendfile`, `fadvise64` added so GNU coreutils file I/O works inside the jail (`workspace_dir_is_writable_during_call_and_wiped_on_drop` is the regression).
- [x] *Stage 2*: bump Landlock TARGET_ABI from v1 to v6 and audit each new access right (`Refer`, `Truncate`, `IoctlDev`, `Scope::AbstractUnixSocket`, `Scope::Signal`) — `97d4465` (lifts `PartiallyEnforced` → `FullyEnforced` on this kernel; verified by `v6_abi_yields_fully_enforced_on_modern_kernel`; required a fix in `add_path_rule` to use `AccessFs::from_file` for file-typed paths so directory-only rights aren't silently stripped)
- [x] cgroup v2 CPU/memory caps via `systemd-run --user --scope` — `3cea642`. `sandbox/src/linux_cgroup.rs` builds the `systemd-run --user --scope --quiet --collect -p MemoryMax=Nm -p MemorySwapMax=0 -p CPUQuota=200% -p TasksMax=64 --` prefix; `LinuxBwrap::spawn_under_policy` now invokes systemd-run as the outer process so the cgroup is set up before `bwrap --unshare-all`. `LinuxBwrap::probe()` chains `cgroup_probe()` so a host without a live `systemd --user` manager fails closed instead of running degraded. Verified by `worker_with_low_mem_max_is_oom_killed` (mem_burner allocating 256 MiB under MemoryMax=32M is OOM-killed). `MemorySwapMax=0` is paired with MemoryMax so overrun cannot silently page to swap. Tunable `cpu_quota_pct` / `tasks_max` policy fields and `setrlimit`-based `cpu_ms` enforcement are filed as Option G in HANDOVER (smaller follow-up).
- [x] Per-worker scratch dir lifecycle (create on spawn, wipe on exit) — `9333311` (subsumed by the `Workspace` type below; `Workspace::Drop` recursively wipes `<root>/<task_id>`)
- [x] Promote per-worker scratch to a first-class `Workspace` type — canonical layout `<root>/<task_id>/{in,out,tmp}`, single owner, single cleanup path; `Workspace::extend_policy(&mut SandboxPolicy)` is the canonical wiring point so host (`policy.fs_write`) and worker-side Landlock (via `tool_host::derive_lockdown_env`) cannot disagree (cf. ZeroClaw `workspace_boundary.rs`) — `9333311`
- [x] Spawn timeout / wall-clock kill — `57edfb2` (`WorkerSpec.wall_clock_ms: Option<u64>`; `spawn_worker` returns `SupervisedWorker` with a 50 ms-poll watchdog thread; cancellation on Drop closes the reused-PID race; `is_valid_target_pid` defends against `kill(-1)` fanout)

## Phase 0b — macOS Port (Seatbelt)

> Done before adding more workers, to stop Linux-isms leaking through the codebase.

- [x] `macos_seatbelt.rs`: `SandboxPolicy` → `.sb` (TinyScheme) generator — `2fa46a2`. *Hardened*: removed unrestricted `(allow mach-lookup)` from the strict profile (issue #1) — every shipping worker (`hhagent-worker-shell-exec` + the four fixture binaries + every coreutils binary on macOS 26.4 ARM64) was empirically confirmed to start without it, and granting it would have exposed every registered launchd service (Apple Events broker, pasteboard, etc.) — the largest known asymmetry vs the threat-model invariant. Pinned by the new unit test `profile_does_not_grant_unrestricted_mach_lookup` and the smoke test `worker_cannot_look_up_arbitrary_mach_services` (the worker calls `bootstrap_look_up("com.apple.coreservices.appleevents")` and must exit non-zero).
- [x] `sandbox-exec` invocation (env_clear + per-policy env + **setsid** for true session isolation — issue #2) — `2fa46a2` originally with `setpgid(0,0)` via `process_group(0)`, *upgraded* to `setsid()` via a `pre_exec` hook so the worker is the leader of a fresh session (sid == pid), strictly stronger than the old setpgid form. Closes the parity gap with bwrap's `--new-session` and forecloses any `/dev/tty` covert channel even under future profile broadening. Pinned by the smoke test `worker_runs_in_its_own_session`.
- [ ] setrlimit for CPU/mem/wallclock — DEFERRED to supervisor work (parity with Linux's current state)
- [x] Network containment via `(deny network*)` + allowlist rules — `2fa46a2`
- [x] Mirror of all sandbox containment integration tests, passing on macOS — 8 tests, 0 skipped (`macos_smoke.rs`: scaffold marker, echo-runs-jailed, /etc/master.passwd invisible, /Users does not leak username, fs_read readable, /dev/disk0 denied, relative-path rejection, network unreachable) — `2fa46a2`
- [x] Mirror of all 3 e2e tests on macOS — 3 tests passing under cross-platform `shell_exec_e2e.rs` — `2fa46a2`

## Phase 0 cont. — Service supervisor

- [x] `hhagent-supervisor` Linux backend: `systemd --user` unit generator + `systemctl --user` driver — supervisor scaffold landed 2026-05-10. `supervisor/src/systemd_user.rs`: pure `build_unit_file(spec) -> String` + `validate_service_name` + `SystemdUser` driver (`install`/`start`/`stop`/`uninstall`/`status`) + `probe()` against the live `--user` manager. Unit-file write is atomic (write-to-tmp + fsync + rename). 27 unit tests + 2 smoke tests (real `systemctl --user` round-trip with RAII cleanup guard). `default_supervisor()` now returns `SystemdUser::new()` on Linux.
- [x] `hhagent-supervisor` macOS backend: LaunchAgent plist generator + `launchctl bootstrap` — landed 2026-05-08. `supervisor/src/launchd_agents.rs`: pure `build_plist(spec) -> String` (XML LaunchAgent format with `Label`, `ProgramArguments`, `EnvironmentVariables`, `WorkingDirectory`, log redirects, `RunAtLoad=true`, `KeepAlive`, `ExitTimeOut`) + `validate_service_name` mirroring the Linux rules + `LaunchAgents` driver (`install`/`start`/`stop`/`uninstall`/`status`) + `probe()` against the live `gui/<uid>` domain. Plist write is atomic (write-to-tmp + fsync + rename). 35 unit tests + 4 smoke tests (real `launchctl bootstrap` round-trip with RAII cleanup guard, plus idempotent `start`/`stop` and pre-launchctl name validation). `default_supervisor()` now returns `LaunchAgents::new()` on macOS.
- [x] First concrete service: typed `ServiceSpec` for the agent core daemon + cross-OS `default_probe()` + e2e against the real `hhagent` binary — landed 2026-05-09. New `supervisor/src/specs.rs` ships pure `core_service_spec(binary, log_dir) -> ServiceSpec` with the canonical name `hhagent-core`, empty args/env, no working_dir, `keep_alive=false` (regression-pinned to today's "log line and exit 0" daemon shape), and predictable log-file names. New `supervisor::default_probe()` mirrors `default_supervisor()` so cross-OS tests skip uniformly. New `core/tests/supervisor_e2e.rs::core_service_install_start_observe_log_uninstall` drives `default_supervisor()` end-to-end against the real `hhagent` binary: install into the canonical user dir, observe the daemon's startup JSON line in the redirected stdout file, stop, uninstall. RAII cleanup guard + unique per-process names so concurrent runs don't collide and a real installed `hhagent-core` is never clobbered. Linux: 96 → 105 tests (+8 unit `specs::*`, +1 integration). macOS projects to 92 by the same delta. Precursor to `hhagent.target` below — once Postgres + inference exist as services, the target item composes them with this one.
- [ ] `hhagent.target` that brings up Postgres, inference, core, workers
- [ ] Auto-restart with backoff on worker crash (partial: `keep_alive=true` in ServiceSpec → `Restart=on-failure RestartSec=5` (constant) in the systemd unit / `KeepAlive=true` in the macOS plist; the agent-core daemon now blocks on SIGTERM/SIGINT and `core_service_spec` is `keep_alive=true` so the policy is wired and meaningful — shipped 2026-05-09 in HANDOVER Option H. **Still TODO:** cross-platform exponential backoff. systemd 252+ has `RestartSteps`/`RestartMaxDelaySec`; macOS launchd's `KeepAlive=true` has no operator-controllable throttle, so this needs a per-OS shape.)

## Phase 0 cont. — Postgres bring-up

- [x] Local Postgres install via PGDG apt repo + user-level supervisor unit (Linux now; macOS via Homebrew uses the same `postgres_service_spec`) — landed 2026-05-09. `scripts/linux/install-postgres.sh` is the idempotent install path: adds the official `apt.postgresql.org` repo via `postgresql-common`'s `apt.postgresql.org.sh` helper (with manual fallback), installs `postgresql-18 postgresql-client-18 postgresql-18-pgvector`, and stops + disables the auto-created system `postgresql@18-main.service` so it can never collide with our user-instance. Verified: PG 18.3 binaries at `/usr/lib/postgresql/18/bin/`.
- [x] Localhost-only via UDS, peer auth, dedicated DB role — landed 2026-05-09. New `hhagent-db` crate ships `hhagent-db-init` (idempotent: skips initdb when `PG_VERSION` already present, still re-writes `postgresql.auto.conf` to correct config drift). The cluster is born locked-down: `initdb --auth-local=peer --auth-host=reject --data-checksums --username=hhagent`, then `<data_dir>/postgresql.auto.conf` pins `listen_addresses=''`, `unix_socket_directories='<data_dir>/sockets'`, `unix_socket_permissions=0700`, `password_encryption='scram-sha-256'`. The supervisor side is `supervisor::specs::postgres_service_spec(postgres_binary, data_dir, log_dir) -> ServiceSpec` (canonical name `hhagent-postgres`, args `["-D", data_dir]`, `keep_alive=true`). Verified end-to-end by `db/tests/postgres_e2e.rs::postgres_install_start_select_one_uninstall`: real PGDG initdb → install via `default_supervisor()` → start → wait Active + stable-Active window → wait UDS socket → `psql -h <socket_dir> -U <whoami> -At -c 'SELECT 1'` returns `1` → stop → wait Inactive → uninstall.
- [x] Extensions: `pgvector` only. `postgresql-18-pgvector 0.8.2` is installed by `scripts/linux/install-postgres.sh`; `CREATE EXTENSION IF NOT EXISTS vector` is now the first statement in `db/migrations/0001_init.sql` (landed 2026-05-09 with C2.2). **Full-text search uses native Postgres `tsvector`+GIN with `ts_rank`** — committed-to after a 2026-05-09 review (closed [#10](https://github.com/hherb/hhagent/issues/10) won't-fix). The `memories` table now has `tsv tsvector GENERATED ALWAYS AS (to_tsvector('simple', body)) STORED` + a GIN index. **Graph storage uses plain relational `entities`/`relations` tables behind a `Graph` trait in `db/src/graph.rs`** — also committed-to in the same review (closed [#9](https://github.com/hherb/hhagent/issues/9) won't-fix); the trait is now defined and a `PgGraph` impl exercises `upsert_entity`/`upsert_relation`/`get_entity`/`neighbors`/`path` against the real schema (recursive-CTE traversal with visited-set cycle guard).
- [x] `db/migrations/` skeleton: `memories`, `tasks`, `entities`, `relations`, `audit_log`, `secrets` — landed 2026-05-09 with C2.2 in `db/migrations/0001_init.sql`. Schema notes in HANDOVER's "Recently completed" entry. Embedding columns are `vector(1024)` (bge-m3 dim, locked in this session). HNSW ANN index on `memories.embedding` deferred until Phase 1's first batch ingest (HNSW build cost scales with row count).
- [x] `sqlx` migration runner integration in core startup — landed 2026-05-09 with C2.2. `sqlx::migrate!("./migrations")` macro embeds the migration set at compile time into `hhagent_db::MIGRATOR`. `core/src/main.rs::bring_up_database` runs `MIGRATOR.run(&mut conn)` after ensuring the application DB exists, then INSERTs an `audit_log` startup row. Fail-closed: any error short-circuits daemon startup with a non-zero exit (the supervisor sees the failure). Workspace-level sqlx feature picks: `runtime-tokio` (no TLS, UDS only), `postgres`, `migrate`, `macros` (for the migrate proc-macro), `json`, `time`. **Did not pick** `sqlx-cli` separately — the embedded macro is sufficient and avoids an extra binary in the deploy footprint.
- [ ] Secrets at rest: AES-256-GCM in the `secrets` table; key from OS keyring (libsecret / Keychain); decrypted only at the host boundary when injecting into a worker call, never logged, never sent to the LLM (cf. IronClaw `secrets/`, ZeroClaw `security/secrets.rs`). The `secrets` table column shape (ciphertext BYTEA + nonce BYTEA + aad BYTEA + key_id TEXT) is now pinned by `0001_init.sql` (landed 2026-05-09 with C2.2); the runtime encrypt/decrypt path is a later Phase 0 slice.

## Phase 0 cont. — Audit log

- [x] Non-superuser `hhagent_runtime` role + DB-layer `REVOKE UPDATE, DELETE, TRUNCATE ON audit_log` so append-only is enforced by Postgres, not just by application discipline — landed 2026-05-10 as Option L. New `db/migrations/0002_runtime_role.sql` creates the role (`NOSUPERUSER NOCREATEROLE NOCREATEDB NOLOGIN NOINHERIT`), grants the OS user (cluster bootstrap superuser) membership via `EXECUTE format('GRANT hhagent_runtime TO %I', current_user)`, and carves the GRANT/REVOKE shape: `audit_log` gets `SELECT, INSERT` only; the other five tables (`tasks`, `memories`, `entities`, `relations`, `secrets`) get full CRUD; sequences for all six get explicit `GRANT USAGE` (BIGSERIAL needs `nextval()`); `ALTER DEFAULT PRIVILEGES` covers future migrations' tables/sequences automatically. New `RUNTIME_ROLE` constant + `set_role_runtime_statement()` pure helper in `db/src/conn.rs` (paired regression-test pins). `db/src/probe.rs` now executes `SET ROLE hhagent_runtime` between `MIGRATOR.run` and the `audit_log` INSERT — bootstrap identity (= OS user, peer auth) is preserved for migrations that need superuser (CREATE EXTENSION, CREATE ROLE), and the runtime role applies to every application write thereafter. New integration test `runtime_role_audit_log_revoke_is_enforced` proves the contract end-to-end: role exists with the expected `pg_roles` flags, OS user is recorded in `pg_auth_members`, INSERT under `SET ROLE` succeeds, UPDATE + DELETE both fail with `permission denied`, full CRUD on `memories` works, final `audit_log` count is exactly 2 (probe row + test INSERT, no UPDATE rewrite, no DELETE leak). 151 → 154 tests on Linux (+2 db unit, +1 db integration). **Why `SET ROLE` over `pg_ident.conf` mapping**: keeps the change pure-SQL inside a sqlx migration, no need to mutate `pg_hba.conf`/`pg_ident.conf` post-`initdb`; the runtime-role privilege boundary is identical regardless of the entry route.
- [x] Append-only `audit_log` writer in core (every tool call, LLM call, channel I/O, memory write) — landed 2026-05-10 as Option I (dispatcher write-site). `core::tool_host::dispatch(&PgPool, &mut SupervisedWorker, tool, method, params)` is the single chokepoint every Phase 0+ tool call goes through; one row per call (`actor = "tool:<name>"`, `action = <method>`, payload `{req, result|err, ms}`). Audit insert is best-effort (a transient DB issue must not mask a successful worker result), but the DB-layer durability story is strong: the `audit_log` REVOKE shape from Option L makes rows tamper-resistant, and the new `0003_audit_log_notify.sql` trigger emits `pg_notify('audit_log_inserted', NEW.id)` so the JSONL mirror is woken synchronously on every commit. Daemon-scoped `db::pool::connect_runtime_pool` opens a pool whose `after_connect` hook runs `SET ROLE hhagent_runtime` on every dialed connection — closes issue #11 ahead of the Phase-1 timeline. **Channel I/O / LLM / memory writers** still need to plumb through `dispatch` when those tools materialise (Phase 2+ for channels, Phase 1 for memory), but the chokepoint pattern is the explicit contract now.
- [x] JSONL on-disk mirror under `~/.local/state/hhagent/audit-*.jsonl` (rotated) — landed 2026-05-10 as Option I. `core::audit_mirror::spawn_mirror(pool, state_dir)` spawns a long-lived tokio task that holds a dedicated `PgListener` on `audit_log_inserted` and replicates committed rows to date-named files (`audit-YYYY-MM-DD.jsonl`, UTC). Initial drain via `audit::fetch_since(0)` so the bring-up row lands on cold starts; periodic 5 s catch-up SELECT in batches of 256 closes any NOTIFY-loss window from listener reconnects. fsync per write — operator visibility beats throughput at Phase 0 scale. JSONL line shape: `{id, ts (RFC 3339), actor, action, payload}`, one row per line, single trailing newline. `MirrorHandle::shutdown` flips a cancellation watch and awaits the task before pool close so the final fsync always runs.
- [x] CLI viewer: `hhagent-cli audit tail` — landed 2026-05-10 as Option I. New `[[bin]] hhagent-cli` in core/Cargo.toml; `core/src/audit_tail.rs` ships pure helpers (`parse_audit_filename`, `find_audit_files`, `tail_loop`) and the CLI binary wraps them. Subcommand `audit tail` supports `--from-start` (replay every line of every existing file), `--no-follow` (cat mode), and `--state-dir PATH` (override default). Polls 250 ms — sub-second visibility without a directory-watch dep. SIGPIPE-style `BrokenPipe` maps to exit 0 (matches BSD `tail`). No DB connection required, so the viewer works against a daemon that has crashed mid-startup.

## Phase 0 cont. — LLM router stub

- [ ] OpenAI-compatible HTTP client (single sole egress for model calls)
- [ ] Local backend pointer (vLLM/SGLang on Linux, llama.cpp/Ollama on macOS)
- [ ] Frontier backend pointer (Anthropic, OpenAI) — *unwired* until Phase 5 policy gate

## Phase 1 — Memory & Loop

- [ ] **Dispatcher chokepoint invariant** documented in `docs/architecture.md`: every tool/channel/routine action enters core through `ToolHost::dispatch()` (or successor) — one audit-log write site, one policy-check site, no side doors. Add a compile-time test that `core::tool_host` is the only module that constructs `WorkerCommand`. (Pattern lifted from IronClaw `ToolDispatcher`; cheap now, painful to retrofit once channels and routines exist.)
- [ ] `memory::recall(query, modes, k)` — pgvector (semantic) + native `tsvector`/GIN with `ts_rank` (lexical) + relational graph traversal (`Graph::neighbors`/`path` over `entities`+`relations`) fused via Reciprocal Rank Fusion in SQL. Three independent score lists, fused per-call. No external graph DB or BM25 extension; if either becomes a measured bottleneck we swap behind the existing abstractions.
- [ ] Embedding worker (small local embedding model behind OpenAI HTTP)
- [ ] `scheduler` agent loop: tick → drain channel bus → next task → LLM call → tool calls → repeat
- [ ] `context_manager`: token-budget + task-completion + wall-clock reset triggers
- [ ] Reset snapshot writer (compact context → memory before reset)

## Phase 2 — Channels (read-only)

- [ ] IMAP inbound worker (sandbox: net allowlist = configured IMAP server only)
- [ ] Telegram inbound adapter (`grammers`, Rust)
- [ ] Channel-bus fan-in into core conversation queue
- [ ] DM pairing flow: short-lived pairing code (TOTP/HOTP) issued via a separate trusted channel; WebAuthn for browser/CLI pairings where available; pairings recorded in `audit_log` with revocation. Static contact allowlists rejected (forgeable). (Pattern: ZeroClaw `security/{pairing,webauthn,otp}.rs`.)

## Phase 3 — Channels outbound + browser + web

- [ ] Egress proxy (per-worker host allowlist, TLS pinning, audit logging)
- [ ] **Credential-leak scanner co-located in the egress proxy** — every outbound request body and inbound response body scanned for the SHA-256 prefix of every secret currently materialized for the calling worker; hits are blocked and audited. Scanning happens at the trust boundary, not inside the worker (which may itself be compromised). (Pattern: IronClaw `safety::leak_detector`, ZeroClaw `security/leak_detector.rs`.)
- [ ] Telegram outbound; Signal in/out (presage)
- [ ] SMTP outbound in mail worker
- [ ] `web-fetch` worker: HTTPS-only, host allowlist, body cap, redirect cap
- [ ] `web-search` worker (SearxNG default)
- [ ] `browser-driver` worker (Playwright headless, dedicated profile, scratch FS)

## Phase 4 — python-exec & agent-authored skills

- [ ] `python-exec` worker: scratch FS only, no net, hard CPU/mem/wallclock; curated stdlib bind
- [ ] Skill catalog (named/persisted Python skills) with optional human-approve gate
- [ ] **Skill trust enum** — `Untrusted | UserApproved | Pinned`, each level mapping to an explicit capability ceiling (which workers it may invoke, which net allowlists, which fs paths). Authorship and approval recorded in `audit_log`; promotion requires re-approval. (Pattern: IronClaw skill trust model — user-placed vs registry-installed.)
- [ ] Optional micro-VM backend for `python-exec` (Firecracker on Linux, Apple `container` on macOS)

## Phase 5 — Frontier escalation, hardening, audit UI

- [ ] Policy gate: per-tool, per-task, per-data-classification routing decision
- [ ] Frontier escalation through egress proxy (Anthropic / OpenAI)
- [ ] Read-only audit log viewer (CLI complete; web optional)
- [ ] 7-day adversarial soak test (prompt-injected channel content; no escapes in audit log)

---

## Cross-cutting / continuous

- [ ] Threat-model doc kept in sync with shipped backends
- [ ] Architecture doc kept in sync with shipped components
- [ ] License audit on every new dependency (AGPL-compatible only)
- [ ] CI: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test --workspace` — both Linux and macOS
