# hhagent — Session Handover

> Rolling document. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. See
> [`README.md`](README.md) for the convention.

**Last updated:** 2026-05-10
**Last commit:** `cfff583` (`feat(llm-router): OpenAI-compatible HTTP client + Backend/PolicyGate seam (Option J)`)
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
hhagent (Rust workspace, 8 crates, AGPL-3.0)
├── core               hhagent-core: lib + 2 bins (`hhagent` daemon + `hhagent-cli` audit-tail viewer). Daemon blocks on SIGTERM/SIGINT via tokio::signal::unix; main.rs now runs db::probe::run → connect_runtime_pool → spawn_mirror before wait_for_shutdown (fail-closed startup for probe + pool; mirror failures are logged but non-fatal). lib modules: tool_host (spawn_worker, dispatch chokepoint, lockdown-env derivation, wall-clock watchdog), workspace (per-task scratch with RAII cleanup), audit_mirror (PgListener-driven JSONL writer with daily rotation + fsync per write), audit_tail (`tail -f`-style follower used by `hhagent-cli audit tail`)
├── db                 hhagent-db: pure helpers (build_initdb_argv, build_postgresql_auto_conf, find_pg_bin_dir) + conn::ConnectSpec (UDS PgConnectOptions builder) + RUNTIME_ROLE/set_role_runtime_statement (drop-privilege helper) + probe::run (ensure DB → migrate as superuser → SET ROLE hhagent_runtime → audit row, fail-closed) + graph::{Graph trait, PgGraph} (relational entities/relations + recursive-CTE path()) + audit::{insert, fetch_by_id, fetch_since, truncate_payload} (pure 4 KiB SHA-256 envelope + async CRUD) + pool::connect_runtime_pool (PgPool with `after_connect` SET ROLE hhagent_runtime hook) + MIGRATOR (sqlx::migrate!() over migrations/0001_init.sql + 0002_runtime_role.sql + 0003_audit_log_notify.sql + 0004_secrets_aad_nonempty.sql) + secrets::{Router-shaped AES-256-GCM at-rest with OS keyring KeyProvider} + hhagent-db-init bin
├── llm-router         hhagent-llm-router: sole egress for LLM calls. `Router::send(&ChatRequest) -> Result<ChatResponse, RouterError>` over reqwest+rustls; `Backend::{Local, Frontier}` closed enum; `PolicyGate` trait with `DefaultLocalPolicy` always picking `Local` (Phase-5 seam). `RouterConfig::from_env` reads `HHAGENT_LLM_LOCAL_URL` / `HHAGENT_LLM_LOCAL_MODEL` / `HHAGENT_LLM_FRONTIER_URL` / `HHAGENT_LLM_FRONTIER_MODEL` / `HHAGENT_LLM_TIMEOUT_MS`. Per-OS default URL: vLLM/SGLang on Linux (:8000), Ollama on macOS (:11434). Frontier dispatch returns `RouterError::PolicyDeniedFrontier` until Phase 5
├── sandbox            hhagent-sandbox: SandboxPolicy + LinuxBwrap (wrapped in systemd-run --scope cgroup) + MacosSeatbelt
├── supervisor         hhagent-supervisor: SystemdUser (Linux) + LaunchAgents (macOS) + specs::{core_service_spec, postgres_service_spec} + default_probe (per-OS supervisor probe)
├── protocol           hhagent-protocol: JSON-RPC 2.0 over stdio (working)
├── workers/prelude      hhagent-worker-prelude: Linux-only Landlock + seccomp lock_down (no-op on macOS)
└── workers/shell-exec   hhagent-worker-shell-exec: uses prelude::serve_stdio
```

**`cargo test --workspace` on Linux: 224 tests passed, 0 failed, 0 `[SKIP]` lines, 0 warnings** (192 → 224, +32 from the LLM router stub in `cfff583`: 28 unit + 4 integration. Two pre-existing doctests in `hhagent-sandbox` and `hhagent-worker-prelude` are `ignored` (explicit `ignore` markers, not regressions from this session).
**macOS projection:** ~171 (was ~139; +32 from the same set — the LLM router slice is platform-neutral, no PG/keyring dep, the integration test binds to 127.0.0.1:0 which works identically on both OSes). Re-run on macOS to confirm.

| Suite | Tests | What's verified |
| ----- | ----- | --------------- |
| `protocol` unit | 3 | dispatch, parse-error fallback, method-not-found |
| `sandbox` unit (linux) | 16 | bwrap argv builder shape (6) + cgroup `systemd-run` argv builder shape: starts with `systemd-run`, uses `--user --scope --quiet --collect`, sets `MemoryMax`+`MemorySwapMax=0` from policy, omits both when `mem_mb=0`, defense-in-depth `CPUQuota=200%` + `TasksMax=64` defaults, ends with `--` separator, no inner-program leakage, 4 `-p` flags total (10) |
| `sandbox` unit (macos) | 14 | sandbox-exec profile builder shape + path canonicalization + on-host probe + TinyScheme-injection rejection + canonicalize error propagation + **strict profile does NOT contain unrestricted `(allow mach-lookup)`** (issue #1) |
| `sandbox` integration (`linux_smoke`) | 7 | **real** bwrap+cgroup: echo runs jailed, /etc/passwd & /home invisible, listed paths visible, net unreachable under `Net::Deny`, relative-path policy rejected, **mem_burner allocating 256 MiB under `MemoryMax=32M` is OOM-killed by the kernel** |
| `sandbox` integration (`macos_smoke`) | 10 | **real** sandbox-exec: scaffold marker, echo runs jailed, /etc/master.passwd invisible, /Users does not leak username, fs_read paths readable (canonicalize /etc symlinks), /dev/disk0 denied, relative-path policy rejected, network unreachable under `Net::Deny`, **worker is the leader of a fresh session — sid == pid via setsid (issue #2)**, **worker cannot `bootstrap_look_up` `com.apple.coreservices.appleevents` (issue #1)** |
| `core` unit | 26 | `derive_lockdown_env` adds correct env entries (4 tests); watchdog loop honours cancel, fires at deadline, exits early on cancel during sleep, guard's Drop sets cancel flag (4 tests); `is_valid_target_pid` rejects 0/1/u32::MAX/`i32::MAX+1` (1 test); workspace creates layout, drops wipes tree, `fs_write_paths` order, `extend_policy` appends, task-id validation, root auto-create, pre-existing dir refused (7 tests). **Option I additions (10):** `audit_mirror::audit_log_path_for` zero-pads month/day + handles 4-digit year (2 tests), `format_jsonl_line` ends with single \n + serialises every AuditRow field (2 tests), `default_state_dir` resolves under `$HOME/.local/state/hhagent` (1 test). `audit_tail::parse_audit_filename` accepts canonical shape + rejects every off-shape (no prefix/suffix/wrong digit count/non-numeric/invalid date) (2 tests), `find_audit_files` returns dates ascending + ignores non-matching files + handles missing dirs (2 tests), `tail_loop` from-start mode replays then exits (1 test) |
| `core` integration (`shell_exec_e2e`) | 4 | **cross-platform real** core → bwrap+landlock+seccomp (Linux) / sandbox-exec (macOS) → shell-exec round-trip; non-allowlisted argv → POLICY_DENIED; unknown method → METHOD_NOT_FOUND; **workspace e2e**: `Workspace::extend_policy` wires `<root>/<task_id>/{in,out,tmp}` into the policy, sandboxed `cp` reads from `in/` and writes to `out/`, host reads back byte-for-byte, `Workspace::Drop` wipes the whole tree |
| `core` integration (`audit_dispatch_e2e`) | 1 | **NEW Option I — cross-platform real** dispatcher chokepoint. Brings up a per-test PG cluster (initdb + `postgres_service_spec` + start + wait Active + wait socket), runs `db::probe::run` to apply 0001/0002/0003, opens a `pool::connect_runtime_pool` (which auto-`SET ROLE hhagent_runtime` on every dialed conn), spawns shell-exec under the platform sandbox, and exercises `tool_host::dispatch` twice: once with an allowlisted argv (`echo dispatch-ok`) → success path returns the worker's result and writes a row with `actor=tool:shell-exec`, `action=shell.exec`, payload `{req, result, ms}` (no `err`); once with `/bin/cat /etc/passwd` → POLICY_DENIED, dispatch propagates the error AND writes a row with payload `{req, err, ms}` (no `result`). Final assertion: exactly 3 rows in `audit_log` (bring-up + 2 dispatches) with the per-row payload-shape pins. Multi-thread tokio runtime is mandatory — `dispatch` uses `block_in_place` around the synchronous `Client::call`. Short temp-dir labels (`disp-d`, `disp-l`) keep the cluster socket path under the 108-byte sockaddr_un limit |
| `core` integration (`supervisor_e2e`) | 1 | **cross-platform real** end-to-end smoke for the daemon's hard PG dependency. Brings up a per-test PG cluster via `default_supervisor()` (initdb + `postgres_service_spec` + start + wait socket + 500 ms stable-Active recheck), then `core_service_spec` for the freshly-built `hhagent` binary with `HHAGENT_DATA_DIR` + `HHAGENT_STATE_DIR` + `USER` injected via `spec.env` (peer auth needs role==OS user; `HHAGENT_STATE_DIR` keeps the audit-mirror's JSONL out of the operator's `~/.local/state/`). Install → start → wait Active → hold 500 ms and re-check (catches probe failure that would loop under `Restart=on-failure`) → poll the redirected stdout for the daemon's `"database probe succeeded"` log line → connect via `psql -d hhagent` and assert `audit_log` has at least one `(actor='core', action='startup')` row → **NEW Option I**: poll the per-test state dir for an `audit-YYYY-MM-DD.jsonl` file containing the bring-up row within ≤ 5 s (proves the audit-mirror task spawned, listened, drained, and fsynced) and assert every line is valid JSON → stop core → wait Inactive → uninstall → status=NotInstalled. Two `ServiceGuard`s + four `PathGuard`s clean up PG service, core service, two data/log dirs, the core log dir, and the per-test state dir on panic. Unique `hhagent-supervisor-test-{pg,core}-{pid}-{nanos}` names so concurrent runs don't collide. macOS holds the same intra-binary serial mutex as `launchd_agents_smoke.rs` |
| `db` unit | 61 | `build_initdb_argv` (8) + `build_postgresql_auto_conf` (7) + `find_pg_bin_dir` (3) + `is_data_dir_initialized` (2) + `require_absolute` / `default_data_dir` / `default_socket_dir` (5) — same 23 as before. **C2.2 additions:** `conn::ConnectSpec` (9 tests: `default_for` resolves `<data>/sockets`+`$USER`+`hhagent`; fails closed with `EnvVarMissing("USER")` when `$USER` is unset or empty; `for_maintenance_db` swaps only the database field; `DEFAULT_APPLICATION_DB` pinned `"hhagent"`; `MAINTENANCE_DB` pinned `"postgres"`; `quote_ident` wraps + doubles `"` + handles empty); `graph::{Entity, Relation}` field-shape pins (2); `probe::ensure_database_exists` SQL shape pin (1: `CREATE DATABASE "hhagent" OWNER "alice"`). **Plus Option L additions (2):** `RUNTIME_ROLE` const pinned `"hhagent_runtime"`; `set_role_runtime_statement()` returns `SET ROLE "hhagent_runtime"` (identifier-quoted). **Plus Option I additions (6):** `audit::truncate_payload` — small payloads pass through (1), empty object passes through (1), boundary-inclusive non-truncation at exactly `PAYLOAD_MAX_BYTES = 4096` (1), oversize replaced with `{_truncated, sha256, len}` envelope with 64-char lowercase-hex digest (1), deterministic for same input (1), distinct fingerprints for distinct inputs at same length (1). **Plus secrets-at-rest additions (18):** AES-GCM round-trip recovers plaintext (1); decrypt fails under wrong key (1), wrong AAD (1), tampered ciphertext (1), tampered nonce (1); each `encrypt` call uses a fresh nonce — no determinism leak (1); `encrypt` rejects > `MAX_PLAINTEXT_LEN = 64 KiB` (1); AAD shape pin — starts with `AAD_DOMAIN = b"hhagent-secrets-v1"`, NUL-delimited, name embedded (1); AAD with `extra` appends after the second NUL (1); AAD is always non-empty by construction (closes #12 at the application layer) (1); `validate_name` rejects empty (1), oversize > `MAX_NAME_LEN = 256` (1), embedded NUL (1), other control bytes (1); accepts typical operator-friendly names (1); `MapKeyProvider` returns the registered key (1); unknown id is `KeyNotFound` (1); constants `KEY_LEN = 32` / `NONCE_LEN = 12` / `AAD_DOMAIN` / `KEY_SERVICE = "hhagent"` / `KEY_ACCOUNT = "secrets-v1"` are all pinned (1) |
| `db` integration (`postgres_e2e`) | 5 | **`postgres_install_start_select_one_uninstall`** (existing): supervisor lifecycle for `hhagent-postgres` + `psql SELECT 1` over UDS. **`probe_runs_migrations_and_graph_happy_path`** (existing C2.2): brings up a per-test PG cluster, runs `db::probe::run` *twice* (proves CREATE DATABASE + migration idempotency — second run is a no-op except the audit row), then connects with sqlx and exercises `PgGraph`: upsert two `person` entities (alice, bob), re-upsert alice (id stable under `ON CONFLICT (kind, name)`, attrs updated), upsert relation alice—knows—bob, `get_entity` round-trip with updated attrs, `neighbors` filtered + unfiltered both return `[bob]`, `path(alice, bob, 5)` returns `[alice, bob]`, `path(bob, alice, 5)` returns `None` (relations are directed), final `audit_log` count == 2 (one row per probe call, no spurious writes). Runtime ~2.1 s on the DGX Spark. **`runtime_role_audit_log_revoke_is_enforced`** (Option L): brings up a per-test PG cluster, runs the probe (which now applies `0001` + `0002` and switches to `SET ROLE hhagent_runtime` for its own audit insert), then connects on a fresh pool connection, asserts `pg_roles` rolsuper/rolcanlogin/rolinherit/rolcreaterole/rolcreatedb are all false, asserts the OS user is recorded in `pg_auth_members` for `hhagent_runtime`, holds an acquired connection out of the pool and runs `SET ROLE hhagent_runtime` on it, then proves: INSERT into `audit_log` succeeds; UPDATE on `audit_log` fails with `"permission denied"`; DELETE on `audit_log` fails with `"permission denied"`; full SELECT/INSERT/UPDATE/DELETE on `memories` succeeds (so the bulk CRUD GRANT block is wired); final `audit_log` count is exactly 2 (probe row + test INSERT, no UPDATE rewrite, no DELETE leak). Skips with `[SKIP]` when no PG / no supervisor. Runtime ~3.0 s on the DGX Spark. **`audit_helpers_pool_and_notify_round_trip`** (NEW Option I): brings up a per-test PG cluster, runs the probe (applies 0001 + 0002 + 0003), opens `pool::connect_runtime_pool` and proves UPDATE on `audit_log` via the pool fails with `"permission denied"` (negative-path proof that `after_connect` SET ROLE actually ran). Then attaches a `PgListener` on `audit_log_inserted` BEFORE the watched insert, calls `audit::insert(&pool, "tool:test", "call", json)`, asserts `tokio::time::timeout(2 s, listener.recv())` returns a notification on the right channel whose payload parses as the inserted row id, calls `audit::fetch_by_id` and confirms the row round-trips byte-for-byte. Finally, `audit::insert` with an 8 KiB payload + `fetch_by_id` returns the `_truncated` envelope (proves `truncate_payload` is wired into the insert path, not just an unused pure helper). Skips with `[SKIP]` when no PG / no supervisor. Runtime ~2.1 s on the DGX Spark. **`secrets_put_get_list_delete_round_trip`** (NEW secrets-at-rest): brings up a per-test PG cluster, runs the probe (applies 0001 + 0002 + 0003 + the new 0004 `secrets_aad_nonempty` migration), opens a runtime-role pool, then exercises every leaf of the `db::secrets` API end-to-end with a `MapKeyProvider`. Asserts: (1) `put` then `get` round-trips plaintext byte-for-byte, with the AAD column populated by the application so 0004's `CHECK (octet_length(aad) > 0)` passes; (2) `list` returns metadata only (name + key_id + timestamps, ORDER BY name ASC) — no ciphertext, no nonce, no AAD in the returned struct; (3) re-`put` of the same name UPSERTs (single row, new ciphertext + new nonce); (4) `delete` removes the row and is idempotent (`Ok(false)` on absent), and a subsequent `get` is `NotFound`; (5) `UPDATE secrets SET name = …` via the runtime-role pool (which holds UPDATE on `secrets`, just not on `audit_log` — the worst-case attacker surface from the threat model) is detected by `get` as `AadMismatch` because the stored AAD still binds to the *old* name; (6) flipping a byte of `secrets.ciphertext` via `set_byte(...) # 1` is detected by `get` as `DecryptFailed` (GCM auth tag mismatch); (7) a direct `INSERT INTO secrets … aad = ''::bytea` is rejected by 0004's CHECK constraint with `"secrets_aad_nonempty"` in the error message. Skips with `[SKIP]` when no PG / no supervisor. Runtime ~2.1 s on the DGX Spark |
| `llm-router` unit | 28 | **NEW Option J.** `error::truncate_for_error` passes short strings through, appends `…[truncated]` marker when oversized (2); `ERROR_BODY_CAP` pinned at `1024` bytes (1). `messages::ChatRole` serialises `system`/`user`/`assistant`/`tool` lowercase + rejects unknown variants like `"developer"` — closed enum (2); `ChatMessage::{system,user,assistant}` constructors set the right role (1); `ChatRequest` serialises with `skip_serializing_if = Option::is_none` so `max_tokens`/`temperature` never leak as `null` on the wire — older llama.cpp builds reject null-valued optional fields (1); `ChatRequest` includes optional fields when set (1); `ChatResponse` decodes the canonical OpenAI-style envelope from vLLM 0.5+ with full `usage` block (1); decodes the minimal Ollama envelope without `id`/`usage`/`finish_reason` — `serde(default)` + `Option` survives missing fields (1). `backend::Backend` serialises lowercase tag (1), `as_tag()` matches the serde rename (1), round-trips (1). `config::default_local_url_for_os()` returns `http://127.0.0.1:8000/v1` on Linux + `http://127.0.0.1:11434/v1` on macOS (1); `DEFAULT_LOCAL_MODEL = "local-default"` + `DEFAULT_TIMEOUT_MS = 30_000` pinned (1); `RouterConfig::default()` shape (1); `RouterConfig::from_env` with no vars set equals `default()` (1); each env var override flows through individually + parses timeout as ms (1); empty-string env var treated as absent (1); non-numeric `HHAGENT_LLM_TIMEOUT_MS` rejected with operator-readable error (1). `policy::DefaultLocalPolicy` always picks `Backend::Local` regardless of model name / message count / sensitivity hints (1); compile-time `Send + Sync` pin so a future implementor capturing `Rc<_>` won't compile (1). `lib::compose_url` trims a single trailing slash from the base, inserts a slash when the path lacks one (2); `CHAT_COMPLETIONS_PATH = "/chat/completions"` pinned (1); `Router::new` succeeds with `RouterConfig::default()` (1); `Router::pick_backend` delegates to the policy (1); `Router::send` returns `RouterError::PolicyDeniedFrontier` synchronously when a test policy picks `Backend::Frontier` (1, async test) |
| `llm-router` integration (`local_backend_e2e`) | 4 | **NEW Option J.** `happy_path_round_trips_request_and_response` brings up a hand-rolled `tokio::net::TcpListener` mock on `127.0.0.1:0` that speaks just enough HTTP/1.1 to canned-respond a vLLM-shaped chat-completion (no `wiremock`/`httpmock` dev-dep — matches the `db/tests/postgres_e2e.rs` style of bringing fixtures up by hand). Asserts the router POSTs to `/chat/completions`, the request body decodes back as the original `ChatRequest` byte-for-byte (proves the `skip_serializing_if = Option::is_none` pin from `messages.rs` survives the round-trip), the response decodes as `ChatResponse` with full `usage` block. **`http_error_status_is_surfaced_with_truncated_body`**: backend returns 500 → `RouterError::HttpStatus { status: 500, body: <≤1 KiB capture> }`, body preserves operator-readable error text. **`decode_error_is_surfaced_when_response_is_not_chat_response`**: backend returns 200 + JSON that lacks `choices` → `RouterError::DecodeResponse { source, body }`, body captured for triage. **`router_send_routes_to_pick_backend_choice`**: a test-only `AlwaysFrontier` policy is wired into the router pointed at the mock; `Router::send` returns `RouterError::PolicyDeniedFrontier` AND no HTTP request reaches the mock (asserted via `oneshot::TryRecvError::Empty`). Defends the chokepoint: a future refactor that bypassed `policy.pick(&request)` would dial the local URL anyway and silently succeed — this test catches it |
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

## Recently completed (this session, 2026-05-10)

### Phase 0 cont. (Option J — LLM router stub: OpenAI-compatible HTTP client, local backend pointer, frontier-stub seam)

**Closed Option J from the previous handover's Next-TODO menu.** The
last application-layer plumbing required before Phase 1 is in place:
every future model call (memory recall ranking in Phase 1, the
scheduler's reasoning step, channel auto-reply drafting in Phase 2,
…) goes through `hhagent_llm_router::Router::send(&ChatRequest) ->
Result<ChatResponse, RouterError>`. Phase 0's `DefaultLocalPolicy`
always picks `Backend::Local`; the `Backend::Frontier` arm of
`Router::send` returns `RouterError::PolicyDeniedFrontier` by design
until the Phase-5 policy gate lands.

- **New top-level workspace crate `llm-router` (`hhagent-llm-router`,
  member #3 alongside `core` and `db`):** ~960 lines of Rust + ~340
  lines of integration test, 32 tests total (28 unit + 4 integration).
  License: AGPL-3.0-only (workspace inherit). Not a sub-folder of
  `core` — the user explicitly chose the new-crate boundary in the
  `/nextsession` design check at the start of the session, on the
  reasoning that the router is a self-contained subsystem (HTTP egress
  + backend selection + Phase-5 policy gate) with a stable typed
  surface, and the small Cargo.toml + `lib.rs` overhead is justified
  by the expectation of a Phase-5 grow-out (real policy gate + secret
  fetching + escalation telemetry).

- **`llm-router/src/messages.rs` (~190 lines, 7 unit tests):** the
  OpenAI-compatible wire shapes. `ChatRole` is a closed enum
  (`System | User | Assistant | Tool`) with `serde(rename_all =
  "lowercase")` so `"developer"` or any other unknown role fails to
  decode at compile-time review rather than silently round-tripping
  through a stringly-typed escape hatch. `ChatMessage` carries
  `{role, content: String}` — multimodal content parts deferred until
  a concrete consumer needs them (Phase 1+). `ChatRequest` carries
  `{model, messages, max_tokens?, temperature?}` with
  `skip_serializing_if = Option::is_none` on the optionals so the
  wire payload stays minimal — older `llama.cpp` builds with the
  `--api` server reject `null`-valued optional fields, and Ollama's
  OpenAI-compat front door is similarly strict. `ChatResponse`
  carries `{id?, model?, choices, usage?}` where every optional
  field is `serde(default)` so a minimal Ollama envelope (no `id`,
  no `usage`, no `finish_reason`) decodes without contortions.

- **`llm-router/src/backend.rs` (~80 lines, 3 unit tests):** the
  `Backend::{Local, Frontier}` closed enum. Wire-encoded as
  `"local"`/`"frontier"` for audit-log payloads (the future
  `actor='llm:router'` rows can be operator-queried via
  `payload->>'backend' = 'frontier'`). `as_tag(&self) -> &'static
  str` is the canonical accessor used in tracing spans and
  audit-log payloads; a paired test asserts it agrees with the
  serde rename so the two never drift. **Why an enum and not a
  `Box<dyn Backend>` trait:** Phase 0 has exactly two backends and
  the Phase-5 sketch keeps it that way; pattern-matching on the
  variant is what the audit-log payload writer wants to do.

- **`llm-router/src/config.rs` (~280 lines, 7 unit tests):**
  `RouterConfig::default()` resolves the local URL via
  `default_local_url_for_os()` (Linux: `http://127.0.0.1:8000/v1`,
  the canonical vLLM/SGLang OpenAI-compat port; macOS:
  `http://127.0.0.1:11434/v1`, the default Ollama port). Other
  Unixes fall back to the Linux default — better to have *something*
  than to require an env var. `from_env()` reads
  `HHAGENT_LLM_LOCAL_URL` / `HHAGENT_LLM_LOCAL_MODEL` /
  `HHAGENT_LLM_FRONTIER_URL` / `HHAGENT_LLM_FRONTIER_MODEL` /
  `HHAGENT_LLM_TIMEOUT_MS` with two operator-friendly behaviours: an
  *unset* var or an *empty-string* var (e.g. a stray `export
  HHAGENT_LLM_FRONTIER_URL=` in a shell profile) is treated as
  absent (the latter is the most common operator footgun); a
  non-numeric `HHAGENT_LLM_TIMEOUT_MS` is rejected with a typed
  `RouterError::Config` carrying both the env-var name and the
  invalid value. The frontier URL/model are deliberately *not*
  defaulted; setting them is purely a forward-compatible seam for
  Phase 5. **API keys are NOT read from env** by design — they
  belong in `db::secrets` (the AES-256-GCM-at-rest store shipped
  earlier this session) and will be fetched at dispatch time when
  the Phase-5 policy gate lands. Reading them from env at
  config-load time would defeat the whole keyring-wrapped at-rest
  encryption story.

- **`llm-router/src/policy.rs` (~80 lines, 2 unit tests):** the
  `PolicyGate` trait — `Send + Sync + Debug` so the trait object
  can cross tokio task boundaries inside the cloneable `Router` —
  with a single sync method `pick(&self, &ChatRequest) -> Backend`.
  `DefaultLocalPolicy` is the Phase-0 implementation; it
  unconditionally returns `Backend::Local` for every request. The
  trait is the explicit Phase-5 seam: the slice that introduces a
  real policy gate is a pure-Rust addition (drop-in
  `impl PolicyGate`, hand it to `Router::with_policy`), not a
  refactor that retro-threads a decision into already-wired call
  sites. **Why a trait and not a closure:** Phase 5's gate will
  read state (recent escalation count, secrets-keyring availability,
  the agent's current task) and emit traces / audit-log payloads;
  trait objects compose with that future state better than a
  single function pointer, and the cost today is one tiny `impl`
  block.

- **`llm-router/src/error.rs` (~80 lines, 3 unit tests):**
  `RouterError::{Config, Transport, HttpStatus, DecodeResponse,
  PolicyDeniedFrontier}`. `Transport` is `#[from] reqwest::Error`
  for ergonomic `?`. `HttpStatus` and `DecodeResponse` capture a
  truncated copy of the response body (cap 1 KiB via the pure
  helper `truncate_for_error`) so a hostile or oversized backend
  reply can't blow up our log lines / panic messages — operators
  still get the readable text of a typical
  `{"error": {"message": "..."}}` envelope but a multi-MiB HTML
  error page is bounded.

- **`llm-router/src/lib.rs` (~250 lines, 6 unit tests):** the
  `Router` type. Constructors: `Router::new(config)` (uses
  `DefaultLocalPolicy`), `Router::with_policy(config, policy)`
  (caller-supplied trait object, `Arc`-wrapped so the same gate
  can be reused across cloned routers without forcing it `Clone`).
  The `reqwest::Client` is built with `timeout = config.timeout`
  and `connect_timeout = min(config.timeout, 5 s)` so a dead
  local-backend port surfaces fast (connection refused on the
  OpenAI-compat URL is the most common operator-error signal in
  this neighbourhood). `Router::send(&request).await` calls
  `policy.pick(&request)` and either dispatches to
  `dispatch_local` (HTTP POST `<base>/chat/completions`, JSON
  body, decode `ChatResponse` from a 2xx body, surface
  `HttpStatus` on non-2xx, surface `DecodeResponse` on schema
  drift) or returns `PolicyDeniedFrontier` for `Backend::Frontier`.
  The pure helper `compose_url(base, path)` trims a single
  trailing `/` from the base before joining — pinned by unit
  tests so a future refactor that double-slashes the URL does so
  deliberately. `pick_backend(&request) -> Backend` is exposed as
  a public method for the future `actor='llm:router'` audit-log
  payload writer that wants to record the decision alongside the
  request, *not* a substitute for actually calling `send`.

- **`llm-router/tests/local_backend_e2e.rs` (~340 lines, 4
  integration tests):** the round-trip proof. Brings up a
  hand-rolled `tokio::net::TcpListener` on `127.0.0.1:0` (the
  kernel hands out an unused port; no port-collision race against
  other tests), spawns a one-shot accept task that parses the
  request as `<headers>\r\n\r\n<body>` with `Content-Length`, and
  responds with a hand-formatted HTTP/1.1 reply. **No
  `wiremock`/`httpmock`/`axum` dev-dep** — the workspace style
  (cf. `core/tests/audit_dispatch_e2e.rs` for PG, the sandbox
  smoke tests for bwrap) is to bring fixtures up by hand so the
  dependency footprint stays inspectable. Four tests:
  `happy_path_round_trips_request_and_response` (path =
  `/chat/completions`, body decodes back as `ChatRequest` byte-for-
  byte including the `skip_serializing_if = Option::is_none` pin
  on `max_tokens`/`temperature`, response decodes as
  `ChatResponse` with `usage`),
  `http_error_status_is_surfaced_with_truncated_body` (mock
  returns 500 → `RouterError::HttpStatus { 500, body }` with the
  operator-readable error text preserved),
  `decode_error_is_surfaced_when_response_is_not_chat_response`
  (200 OK + JSON that lacks `choices` → `RouterError::DecodeResponse`),
  `router_send_routes_to_pick_backend_choice` (an `AlwaysFrontier`
  test policy is wired in, asserting that **no HTTP request
  reaches the mock** when the policy denies — defends the
  chokepoint against a future refactor that accidentally bypasses
  `policy.pick`).

- **New deps (workspace):** `reqwest = { version = "0.12",
  default-features = false, features = ["rustls-tls", "json"] }`.
  License: MIT OR Apache-2.0 — AGPL-compatible. **Why
  `default-features = false`:** strips the `native-tls` /
  `openssl-sys` default and the blocking-client surface; we don't
  link against system OpenSSL anywhere in this workspace
  (matches `sqlx`'s `runtime-tokio` UDS-only choice). **Why
  `rustls-tls`:** pure-Rust TLS so a Phase-5 frontier HTTPS call
  works without dragging in `libssl-dev` at build time.
  **Why not `stream`:** Phase-0 stub returns a single completion;
  SSE streaming is a Phase-1+ concern. The transitive set
  (`hyper-util`, `rustls`, `webpki-roots`, etc.) is all
  MIT/Apache.

**Subtle bug found and fixed in the test scaffolding (kept as a
note for the next contributor who reaches for the same pattern).**
The hand-rolled HTTP/1.1 mock's `header_content_length` parser
originally used `?` on the second `splitn(2, ':')` token —
which means the *whole function* short-circuited to `None` on
the first colon-less line of the header block. The HTTP request
line (`POST /chat/completions HTTP/1.1`) has no colon, so the
parser never reached the actual `Content-Length: 109` line three
rows below. Symptom: the body came through as 0 bytes, and
`serde_json::from_str` panicked on EOF. Fix: replace `?` with
`else { continue }` so colon-less lines are skipped without
exiting the loop. Pinned by the diagnostic eprintln being
removed once the round-trip-pinning test passed.

**Why the new crate is a *crate* and not a `core::llm_router`
module.** The user picked the crate boundary at the
`/nextsession` design check. The reasoning we collected at the
time: (a) the router is a self-contained subsystem with a stable
typed surface (Phase 0's `Router::send` is the same shape Phase 5
will keep); (b) the Phase-5 grow-out adds a real policy gate that
will read state from `db::secrets`, emit telemetry, and gain its
own integration test surface — natural fit for a separate crate;
(c) `core` already holds the dispatcher chokepoint and the audit
infrastructure, so keeping the LLM-egress concern out of `core`
makes the dependency direction explicit (eventually `core`
imports `llm-router`, never the other way around).

**Why `Router::send` is async even though the policy gate is sync.**
The HTTP call is unavoidably async (reqwest's API surface is
async-native; the blocking surface is a Tokio thread-pool wrapper
under the hood and we'd be paying the thread-handoff cost for
nothing). The policy gate stays sync because today's decision is
local computation — a future async policy gate (one that consults
the keyring for a frontier API key, say) can do its async work
upfront in `Router::with_policy`'s constructor and cache the
result, or wrap the trait in an `async-trait`-style shim then.
Forcing every consumer to `.await` the policy lookup today would
buy nothing.

**Why we did NOT integrate `Router::send` into
`core::tool_host::dispatch` in this slice.** The HANDOVER said
"sole egress for model calls; Phase 5's policy gate slots in here
… Phase 1's memory recall and scheduler loop both depend on it."
This slice ships the *typed surface* (the contract every Phase-1+
consumer will dial). The dispatcher integration that fires an
`actor='llm:router'` audit row is a Phase-1 step — it requires a
concrete first consumer (memory recall is the most likely
candidate) to validate the integration shape, and bundling that
exploration into this slice would have widened scope unnecessarily.
The chokepoint pattern is already documented: when the first
LLM-using tool materialises, its dispatcher path will go through
`tool_host::dispatch(pool, llm_router_handle, "llm:router",
"chat", request)` (or a small variant) and the audit-log writer
will record the `Backend::as_tag()` selection alongside.

**Test count:** 192 → **224** on Linux (+28 unit + 4 integration;
0 skipped, 0 failed, 0 warnings). macOS projection: ~171 — every
new test is platform-neutral (no PG, no keyring, no sandbox; the
integration test binds to `127.0.0.1:0` which works identically
on both OSes). Re-run on macOS to confirm.

---

### Phase 0 cont. (secrets at rest — AES-256-GCM + OS-keyring wrapping key + `db::secrets` runtime + 0004 migration)

**Closed the last Phase 0 cont. line item under "Secrets at rest" in
ROADMAP.** The `secrets` table column shape was already pinned by
C2.2; the runtime that turns those columns into encrypt/decrypt
operations now exists. Plaintext for an API token, IMAP password, or
signing key lives only in agent-process memory and inside the OS
keyring; the Postgres row carries AES-256-GCM ciphertext + a fresh
random 12-byte nonce + AAD that binds the row to its name + a
`key_id` pointer back to the keyring entry that holds the wrapping
key.

- **`db/src/secrets.rs` (~520 lines, 18 unit tests):** the canonical
  shape for every secret read/write. Pure crypto helpers
  (`encrypt`, `decrypt`, `compute_aad`, `validate_name`) decoupled
  from any I/O so the AES-GCM contract is pinned by unit tests
  without a DB or a keyring. AAD layout:
  `b"hhagent-secrets-v1" || 0x00 || name.as_bytes() || 0x00 ||
  optional_extra` — domain-separated, NUL-delimited, name-bound.
  This is what gives us row-rename detection: `UPDATE secrets SET
  name = …` leaves the stored AAD pointing at the old name, so
  `get` either fails the prefix-match check (`AadMismatch`) or, if
  an attacker also UPDATEs the AAD column, fails the GCM auth tag
  (`DecryptFailed`) because the tag was computed under the original
  AAD. Public secret-getter returns `Zeroizing<Vec<u8>>` so a
  panic-unwind cannot leave plaintext on the stack; `SecretKey`
  (32-byte AES-256 key) is `Zeroizing<[u8; 32]>` for the same
  reason. Soft caps: `MAX_NAME_LEN = 256`, `MAX_PLAINTEXT_LEN =
  64 KiB`. `validate_name` rejects empty / oversize / NUL / other
  control bytes (NUL is the AAD separator; allowing it would let a
  crafted name push bytes into the "extra" half of AAD).

- **`KeyProvider` trait + two impls (in the same file):**
  `MapKeyProvider` is the test seam — production code never sees
  it. `OsKeyringProvider::ensure_initialized()` opens the
  `(hhagent, secrets-v1)` entry on first use; if no entry exists,
  it generates a fresh 32-byte key via `aead::OsRng` and writes it.
  The cached `key_bytes` field means the keyring lookup (and any
  unlock prompt) happens once at startup, not on every `get`. The
  trait is `Send + Sync` because `put`/`get` cross await points
  while holding a `&dyn KeyProvider`.

- **Async DB I/O (same file, ~150 lines):** `put`, `get`, `list`,
  `delete` all generic over `sqlx::Executor<'_, Database = Postgres>`,
  same shape as `db::audit::insert`/`fetch_by_id`. `put` UPSERTs by
  name (`ON CONFLICT (name) DO UPDATE SET ciphertext, nonce, aad,
  key_id, updated_at = now()`). `get` does a
  recompute-then-compare on AAD before passing to GCM, catching
  the swap case explicitly via `AadMismatch` instead of folding it
  into the generic `DecryptFailed` (operators want to know the
  difference: AAD mismatch means "row was renamed", `DecryptFailed`
  means "wrapping key is wrong or ciphertext was edited"). `list`
  selects only metadata columns (`name`, `key_id`, `created_at`,
  `updated_at`) so a debug-dump of the listing leaks nothing
  cryptographic. `delete` is idempotent (`Ok(false)` on absent
  name).

- **`db/migrations/0004_secrets_aad_nonempty.sql` (~30 lines):**
  drops the provisional `aad BYTEA NOT NULL DEFAULT ''::bytea` and
  adds `CHECK (octet_length(aad) > 0)`. Closes [#12](https://github.com/hherb/hhagent/issues/12).
  Belt-and-braces: the application layer is structurally
  incapable of producing an empty AAD (`compute_aad` always emits
  at least `AAD_DOMAIN.len() + 2` bytes), but the DB-layer CHECK
  catches a rogue `INSERT INTO secrets …` that bypassed
  `db::secrets::put`. Safe to apply on a populated cluster: at
  this point in the project's history nobody has written `secrets`
  rows yet, so no backfill is needed; a future migration on a
  populated table would have to ensure non-empty AAD first.

- **New deps (workspace):** `aes-gcm = { version = "0.10", features
  = ["zeroize"] }` (pure-Rust RustCrypto AEAD; the `getrandom`
  default brings in `aead::OsRng` for our nonce path; `zeroize`
  feature wires key+cipher state to wipe on drop), `zeroize = {
  version = "1", features = ["zeroize_derive"] }` (direct dep for
  `Zeroizing<Vec<u8>>` on the public boundary). All Apache-2.0 OR
  MIT — AGPL-compatible. **Per-target keyring deps in
  `db/Cargo.toml`:** Linux uses `keyring 3` with the
  `async-secret-service` + `tokio` + `crypto-rust` features —
  pure-Rust D-Bus via `zbus`, no `libdbus-1-dev` system-package
  requirement (we avoided the `sync-secret-service` /
  `dbus-secret-service` / `libdbus-sys` chain on purpose to keep
  the build hermetic across distros). macOS uses `apple-native`
  (Security.framework). The whole transitive set is MIT/Apache.

- **`db/tests/postgres_e2e.rs::secrets_put_get_list_delete_round_trip`
  (~280 lines, NEW):** the only new integration test. Brings up a
  per-test PG cluster (the canonical recipe shared with the
  audit-pool test), runs the probe so all four migrations apply,
  opens a runtime-role pool, then walks the seven assertions
  documented in the test table row above. Uses a `MapKeyProvider`
  with a hard-coded `[0x42u8; 32]` key so the test is hermetic
  (CI never touches libsecret / Keychain). The AAD-mismatch and
  ciphertext-tamper paths use the runtime-role pool's own UPDATE
  privilege — the threat model says a worst-case attacker has
  exactly that connection, so reproducing the tamper through the
  same connection that any compromised application code would
  hold is the right adversarial model. The 0004-CHECK assertion
  uses a direct `INSERT … aad = ''::bytea` (which `db::secrets::put`
  would never construct) to prove the database-layer guard is
  active. Skips cleanly with `[SKIP]` lines when PG / supervisor
  isn't available.

**Why pure-Rust D-Bus on Linux (`async-secret-service` + `zbus`)
instead of `sync-secret-service` + `libdbus-sys`.** The sync
backend pulls in `dbus-secret-service`, which depends on
`libdbus-sys`, which is a `pkg-config`-driven C-library binding
requiring `libdbus-1-dev` headers at build time. That breaks
`cargo build` on minimal containers, raises the bar for "clone +
cargo build works on a fresh DGX Spark", and gives us no security
benefit (it's the same D-Bus protocol either way). `zbus` is pure
Rust, MIT-licensed, AGPL-compatible. The keyring crate's `tokio`
feature integrates with our existing runtime so a blocking
`Entry::get_secret()` from inside an async task does not deadlock
the executor; we hit that exactly once per startup, in
`OsKeyringProvider::ensure_initialized`.

**Why not split the secrets module into multiple files.** It is
one file at ~520 lines (with tests) and naturally cohesive: pure
crypto helpers, the trait, both providers, async DB I/O, all
operate on the same small set of types (`SecretKey`, `Nonce`, the
AAD bytes). The 500-line target is a soft guideline; CLAUDE.md
calls for "consider refactoring where possible". A future
`secrets/{crypto.rs, key_provider.rs, keyring_provider.rs, db.rs,
mod.rs}` split is straightforward when a real second consumer
appears, but premature today.

**Why best-effort logging on `OsKeyringProvider` errors instead of
panicking.** A locked GNOME keyring or a missing D-Bus daemon
should surface as `SecretsError::Keyring(...)` so the caller can
choose to fall back (e.g. defer secret-using tools until the next
unlock) or fail closed. Panicking in the provider would prevent
the daemon from booting at all, which is the wrong default for a
single-user tool that may have non-secret-using subsystems.

**Why we did NOT integrate `secrets::get` into `tool_host::dispatch`
in this slice.** The HANDOVER's "decrypted only at host boundary
when injecting into a worker call" line stays as a Phase 1+ task.
This slice is the *boundary layer* — the typed surface that future
worker-secret injection will call into. Wiring the dispatcher to
fetch secrets requires a per-worker policy ("which secrets is this
tool allowed to ask for?") which doesn't exist yet, and adding
that lookup mechanism at the same time as the encrypt/decrypt path
would have bundled two distinct decisions into one slice. Filed as
implicit prep for the first tool that needs an outbound credential
(likely `web-fetch` in Phase 3 or the LLM router in Option J for
the Anthropic API key).

**Why issue #12 is closed at the application layer + DB layer
simultaneously.** The original ticket said "reject empty
`secrets.aad` in the runtime encrypt path; drop the schema's
`DEFAULT ''::bytea` once all call sites populate explicitly". The
runtime path is `db::secrets::put`, which always uses
`compute_aad(name, _)` — structurally non-empty. So the call-site
guarantee is in place. The schema DEFAULT had to go too, and a
DB-layer `CHECK` is the cheapest belt-and-braces against future
non-`db::secrets` insert paths (a debug query, a hand migration).
0004 does both in one step.

**Test count:** 172 → **191** on Linux (+18 unit secrets-crypto, +1
integration secrets full-lifecycle; 0 skipped, 0 failed, 0
warnings). macOS projects to ~138 once `brew install postgresql@18`
is done; the unit tests are platform-neutral, the integration test
`[SKIP]`s cleanly without PG.

---

### Phase 0 cont. (Option I — dispatcher chokepoint + audit_log NOTIFY trigger + JSONL mirror + `hhagent-cli audit tail`)

**Closed Option I from the previous handover's Next-TODO menu.** Every
Phase 0+ tool call now goes through a single `tool_host::dispatch`
chokepoint that writes one `audit_log` row per call. A long-lived
`audit_mirror` task (spawned by the daemon at startup) replicates
committed rows to `~/.local/state/hhagent/audit-YYYY-MM-DD.jsonl` with
fsync per write and daily UTC rotation; `hhagent-cli audit tail` reads
those files with no DB connection, so an operator can debug a daemon
that crashed mid-startup. The DB is the source of truth (the Phase 0
runtime-role `REVOKE UPDATE, DELETE` makes that durable); the JSONL
stream is the operator-visibility replica.

- **`db/migrations/0003_audit_log_notify.sql` (~70 lines):** AFTER
  INSERT trigger on `audit_log` that calls
  `pg_notify('audit_log_inserted', NEW.id::text)`. PL/pgSQL with
  `SET search_path = pg_catalog, public` so a future schema-name
  collision can't redirect `pg_notify`. Per-row trigger because
  Phase 0 throughput is one INSERT per tool call (NOTIFY granularity
  matches insertion granularity); per-statement would have lost the
  wake-up specificity. Payload = `id::text` rather than the full row
  for two reasons: (1) Postgres caps NOTIFY payloads at 8000 bytes
  by default, well below our worst-case truncated `audit_log.payload`
  + envelope; (2) the listener is in-process with the writer, so the
  extra SELECT is a sub-ms UDS round-trip — cheaper than encoding the
  row into the NOTIFY payload. Decoupling wake-up from data also
  lets the listener catch up on missed rows by ignoring the payload
  and querying `id > last_seen_id`.

- **`db/src/audit.rs` (~280 lines, 6 unit tests):** the canonical
  shape for every `audit_log` write/read. `truncate_payload(value)`
  is the pure 4 KiB cap: oversize JSON payloads are replaced with
  `{"_truncated": true, "sha256": "<64 hex>", "len": <bytes>}` so the
  table, WAL, and JSONL mirror stay bounded regardless of caller.
  Threshold is inclusive (`<= PAYLOAD_MAX_BYTES = 4096`), regression-
  pinned by a boundary test that constructs a string serialising to
  exactly 4096 bytes and asserts pass-through. SHA-256 via the new
  workspace dep `sha2 = "0.10"` (MIT OR Apache-2.0, AGPL-compatible);
  hex-formatted manually with `{:02x}` so a refactor that swaps the
  hash crate doesn't accidentally change the wire shape. Async I/O:
  `insert(executor, actor, action, payload) -> i64` returns the new
  row's id via `INSERT … RETURNING`; `fetch_by_id(executor, id)` and
  `fetch_since(executor, since, limit)` for the mirror's read paths.
  `executor` is generic (`E: sqlx::Executor<'_, Database = Postgres>`)
  so the same insert helper works against `&PgPool` (production
  dispatcher) and `&mut PgConnection` (deterministic test setup).
  `AuditRow` decodes all five columns (`id, ts, actor, action,
  payload`) into a single struct shared by the dispatcher's e2e
  assertions and the mirror's JSONL writer.

- **`db/src/pool.rs` (~110 lines, no unit tests — proven by the
  integration test instead):** `connect_runtime_pool(spec)` opens a
  `PgPool` with `PgPoolOptions::after_connect` running
  `set_role_runtime_statement()` on every dialed connection. So
  every pool checkout is *already* in the runtime role —
  application-level writes can never accidentally run as the
  bootstrap superuser, and the audit_log REVOKE shape from Option L
  applies to every dispatcher write by construction. Defaults:
  `max_connections = 4`, `acquire_timeout = 10 s`,
  `idle_timeout = 5 min`. `connect_runtime_pool_with_max` lets tests
  override the cap if they share a cluster across many pool
  instances. **Closes issue #11** ahead of the originally-planned
  Phase 1 timeline; the daemon's "today only one short-lived conn
  per probe" pattern is replaced with a real pool now that the
  dispatcher write site materialised.

- **`core/src/tool_host.rs::dispatch` (~80 lines):** the new chokepoint.
  Signature `dispatch(&PgPool, &mut SupervisedWorker, tool, method,
  params) -> Result<Value, ToolHostError>`. Snapshots `params` for
  the audit row, wraps the synchronous `Client::call` in
  `tokio::task::block_in_place`, measures elapsed ms, then
  best-effort writes one row to `audit_log` (`actor = "tool:<tool>"`,
  `action = <method>`, `payload = {"req", "result", "ms"}` on success
  or `{"req", "err", "ms"}` on failure). Audit failures are logged
  via `tracing::error!` but do **not** mask the worker's actual
  result — silently turning a successful tool call into an error
  because we couldn't log it would be a strictly worse failure mode
  than missing an audit row. Phase 1 will tighten this when the
  scheduler grows real durability requirements; for Phase 0 the DB
  is the source of truth via the trigger + REVOKE shape, and the
  audit-row best-effort behaviour is documented.

- **`core/src/audit_mirror.rs` (~370 lines, 5 unit tests):** the
  long-lived JSONL writer. `spawn_mirror(pool, state_dir)` spawns a
  tokio task that opens a `PgListener` on `audit_log_inserted` (its
  own dedicated connection, doesn't compete with pool slots), does
  an initial `fetch_since(0)` drain so the bring-up audit row lands
  in the JSONL file on cold starts, then enters a `tokio::select!`
  loop racing NOTIFY arrivals, a 5 s catch-up timer, and a
  cancellation watch. Each catch-up pulls in batches of 256 with
  `audit::fetch_since(last_seen_id, 256)` until it returns fewer
  rows than asked, so a multi-day backlog is bounded in memory. JSONL
  format: `{"id", "ts", "actor", "action", "payload"}` with `ts` as
  RFC 3339; daily UTC rotation keyed on `row.ts.date()` so a row
  inserted at exactly midnight UTC files itself in the right day
  regardless of host wallclock. Every line is followed by
  `File::sync_all` — operator visibility beats throughput at Phase 0
  scale (10s of audit rows/min, not 10k). NOTIFY drops are tolerated
  because the catch-up SELECT is the canonical fetch path; the
  listener's payload is treated as a wake-up signal only.
  `MirrorHandle::shutdown` flips the cancellation watch and awaits
  the task — the daemon's `main` calls it before closing the pool so
  the final `sync_all` always runs.

- **`core/src/audit_tail.rs` (~190 lines, 5 unit tests):**
  `tail -f`-style follower used by `hhagent-cli audit tail`. Pure
  helpers: `parse_audit_filename` (strict shape: `audit-YYYY-MM-DD.jsonl`,
  rejects every off-form including invalid dates like `Feb 30`),
  `find_audit_files` (returns `Vec<(Date, PathBuf)>` sorted ascending,
  silently skipping non-matching names so editor backups don't appear
  in the tail). The async `tail_loop(cfg, writer)` supports two
  modes: `from_start` (replay every line of every existing file in
  date order) and live (anchor at end of latest file). Polls every
  250 ms — fast enough for sub-second operator visibility, slow
  enough that an idle viewer doesn't busy-spin. Date roll-over is
  detected by polling `find_audit_files` for a newer-dated entry and
  flushing the previous file's tail before switching, so a midnight
  rotation doesn't drop the last few lines.

- **`core/src/bin/hhagent-cli.rs` (~140 lines):** new operator CLI
  binary. Today only one subcommand: `hhagent-cli audit tail
  [--from-start] [--no-follow] [--state-dir PATH]`. Hand-rolled argv
  parsing (no `clap` dep — the surface is too small to justify one).
  Resolves the state dir from `--state-dir` → `$HHAGENT_STATE_DIR` →
  `$HOME/.local/state/hhagent`, in that order. Maps SIGPIPE-style
  `BrokenPipe` to exit 0 (matches BSD `tail`'s "downstream `head`
  closed early is not an error" behaviour). Built into
  `target/debug/hhagent-cli` via a second `[[bin]]` stanza in
  `core/Cargo.toml`.

- **`core/src/main.rs` rewrite (~30 lines net):** after `probe::run`,
  the daemon now calls `connect_runtime_pool` (fail-closed — the
  dispatcher write site needs it) and `spawn_mirror`
  (best-effort — failure logs and continues, since the DB row is
  source of truth). On SIGTERM/SIGINT, it shuts down the mirror
  *before* closing the pool so the mirror's final `sync_all`
  observes an alive pool. Adds a third env-var seam:
  `HHAGENT_STATE_DIR` (parallel to `HHAGENT_DATA_DIR`) so the
  supervisor_e2e test can point the audit-mirror at a per-test
  tempdir without touching the operator's `~/.local/state/`.

- **Tests + e2e (+18 total):** 6 db unit (audit truncation), 1 db
  integration (`audit_helpers_pool_and_notify_round_trip` — full
  pool + NOTIFY + fetch_by_id + truncation round-trip in ~2.1 s on
  the DGX Spark), 5 core unit (audit_mirror date paths + JSONL
  formatting + state-dir resolution), 5 core unit (audit_tail
  filename parsing + dir listing + tail-loop replay), 1 core
  integration (`audit_dispatch_e2e::dispatch_writes_audit_row_for_success_and_failure`
  — both shapes of dispatch end-to-end with per-row payload-shape
  pins). The supervisor_e2e test gained a new ≤ 5 s assertion that
  the JSONL mirror picks up the bring-up audit row + every line in
  the JSONL file is valid JSON. Total: 154 → 172 on Linux, 0 skipped,
  0 warnings.

**Why we landed the PgPool now (vs. parking until Phase 1).** Issue
#11 originally filed the daemon-scoped pool for "Phase 1's concurrent
workload." Option I forced the question: the dispatcher write site
fires once per tool call and concurrent tool calls are inevitable as
soon as the scheduler lands, so opening connections per-dispatch
would be both slow (UDS dials are cheap but not free) and harder to
attach the SET ROLE hook to consistently. Building the pool with
`after_connect` SET ROLE means the runtime-role contract from Option L
is enforced by construction at every application write site —
no caller has to remember to drop privilege.

**Why best-effort audit writes vs. fail-closed.** The HANDOVER's
Option I sketch didn't commit either way. We chose best-effort: a
worker call that succeeded but couldn't log its audit row still
returns the worker's result. The reasoning: silently swapping a
successful tool call for an error because the audit insert hit a
transient cluster issue is a strictly worse failure mode than
missing a row in the operator-visibility log. The DB-layer durability
(append-only via REVOKE, NOTIFY → JSONL mirror) is the strong
guarantee; the dispatcher's audit write is the weak best-effort
mirror of "I tried to log this." Phase 1 may flip this once the
scheduler has a concrete contract for what audit-row durability
means to it (e.g. "no tool result enters the recall corpus until its
audit row is committed").

**Why the JSONL state dir is `$HOME/.local/state/hhagent` on both
OSes.** XDG-compliant on Linux; macOS doesn't follow XDG by default
but does support the path. We use the same on both OSes for the same
reason `default_data_dir` does — operator docs and scripts don't
need per-OS branches. The `HHAGENT_STATE_DIR` env override (parallel
to `HHAGENT_DATA_DIR`) is the test seam.

**Why `block_in_place` instead of converting `Client::call` async.**
The protocol crate's `Client` is synchronous (BufReader / Write over
piped stdio). Converting it to async would touch both client and
server (the worker prelude's `serve_stdio` is also sync) and is a
larger refactor than this slice should bundle. `block_in_place` runs
the sync call on the current async-runtime worker thread without
handing off — works with `&mut SupervisedWorker` and adds zero
allocations. Requirement: the caller's runtime must be multi-thread
(the daemon's `#[tokio::main]` already is; the dispatch e2e test
explicitly builds `Builder::new_multi_thread`). Phase 1 may revisit
when a real concurrent scheduler arrives — then the sync→async
conversion of the protocol crate becomes worth its weight.

**Why we did NOT add the "core::tool_host is the only constructor of
WorkerCommand" compile-time test.** HANDOVER's gotcha said "consider
sneaking it in here." The pre-existing `spawn_worker` is `pub` and
called directly by `core/tests/shell_exec_e2e.rs`; restricting
visibility breaks that test, and the supporting trait/marker scaffolding
to add a deny-on-construction guard is non-trivial. Filed for Phase 1
when the chokepoint contract becomes load-bearing on the scheduler
and there's a forcing function to do it cleanly.

**Test count:** 154 → **172** on Linux (+18). macOS projects to ~119 once
`brew install postgresql@18`'s done; both new PG-touching integration
tests `[SKIP]` cleanly without it.

---

### Phase 0 cont. (Option L — non-superuser runtime role + audit-log GRANT split, earlier on 2026-05-10)

**Closed Option L from the previous handover's Next-TODO menu.** The
audit_log table picked up its long-promised `REVOKE UPDATE, DELETE`
guarantee, and the daemon now drops privileges before every
application-level write. The contract is enforced at the database
layer, not by application discipline alone — a compromised
dispatcher (or LLM-issued SQL, or a future bug) running under
`hhagent_runtime` cannot tamper with prior audit rows even if the
caller wanted to. Phase 0 Option I (the dispatcher write-site) and
all later application paths inherit the same connection pattern.

- **`db/migrations/0002_runtime_role.sql` (~140 lines):** creates
  `hhagent_runtime` with `NOSUPERUSER NOCREATEROLE NOCREATEDB
  NOLOGIN NOINHERIT`, grants the OS user (= cluster bootstrap
  superuser) membership via `EXECUTE format('GRANT hhagent_runtime
  TO %I', current_user)` so `SET ROLE` works on every host
  regardless of the OS username, then carves the GRANT/REVOKE shape:
  `GRANT SELECT, INSERT ON audit_log` paired with
  `REVOKE UPDATE, DELETE, TRUNCATE ON audit_log FROM hhagent_runtime`
  (the contract pin from `0001_init.sql`'s comment block) and
  `REVOKE ALL ON audit_log FROM PUBLIC`. The other five tables
  (`tasks`, `memories`, `entities`, `relations`, `secrets`) get
  bulk `GRANT SELECT, INSERT, UPDATE, DELETE`. Sequences for all
  six tables get explicit `GRANT USAGE` because BIGSERIAL needs
  `nextval()`. `ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT …
  TO hhagent_runtime` covers future migrations' tables/sequences
  automatically (caveat noted inline: future insert-only tables
  still need their own explicit `REVOKE UPDATE, DELETE`). The
  `CREATE ROLE` is wrapped in a `DO $$ IF NOT EXISTS … END $$`
  block because Postgres has no `CREATE ROLE IF NOT EXISTS` syntax
  — keeps the migration safe to re-apply by hand in pathological
  recovery scenarios.

- **`db/src/conn.rs` additions (~40 lines, +2 unit tests):**
  `pub const RUNTIME_ROLE: &str = "hhagent_runtime"` (paired
  regression-test pin) and `pub fn set_role_runtime_statement() ->
  String` returning `SET ROLE "hhagent_runtime"` (identifier-quoted
  via the existing `quote_ident` helper, so a future role-rename to
  a name containing a reserved word or unusual character can't
  silently parse as a different statement). Pure functions — no
  I/O — same shape as the rest of `conn.rs`'s pin-set.

- **`db/src/probe.rs` change (~10 lines, doc-updated):** between
  `MIGRATOR.run` and the `audit_log` INSERT, the probe now executes
  `set_role_runtime_statement()` on the same connection. Because
  `0002_runtime_role.sql` is part of the embedded migration set, the
  role + grant are guaranteed to exist by the time SET ROLE runs.
  Module docstring updated to spell out the new pipeline (5 steps
  → 6 steps; SET ROLE is step 5). `audit_log` rows from this point
  on are inserted under `hhagent_runtime`, so the runtime-layer
  prohibition on UPDATE/DELETE applies to the probe's own writes
  too — defense-in-depth even on the bootstrap path.

- **`db/tests/postgres_e2e.rs::runtime_role_audit_log_revoke_is_enforced` (~150 lines, NEW):**
  full bring-up of a per-test PG cluster (mirrors the existing
  `probe_runs_migrations_and_graph_happy_path` setup), then runs the
  probe, then opens a sqlx pool and exercises both shapes of the
  contract:
  - **Role shape pin**: `SELECT rolname, rolcanlogin, rolsuper,
    rolinherit, rolcreaterole, rolcreatedb FROM pg_roles WHERE
    rolname = 'hhagent_runtime'` — all five booleans must be `false`.
  - **Membership pin**: `pg_auth_members` join asserts the OS user
    has been granted `hhagent_runtime` (so `SET ROLE` will succeed).
  - **Negative path**: hold a pool-acquired connection out, `SET
    ROLE hhagent_runtime`, then INSERT a row (succeeds), UPDATE
    (must fail, error message contains `"permission denied"`),
    DELETE (must fail, same message check). Substring match on
    `"permission denied"` is portable across PG major versions and
    survives the sqlx error-wrapper formatting.
  - **Positive path**: full CRUD on `memories` (proves the bulk
    GRANT block is in fact wired and `memories_id_seq` USAGE works).
  - **Final invariant**: `audit_log` row count is exactly 2 (probe
    row + test INSERT). Anything other than 2 means either a
    bookkeeping bug or — much worse — an UPDATE/DELETE that
    leaked through.

- **No change to `core/src/main.rs::bring_up_database`.** The
  daemon's bring-up path was already calling `probe::run`; adding
  SET ROLE *inside* the probe is the seam, so the daemon side
  needs no edits. The cross-OS supervisor e2e
  (`core_starts_runs_db_probe_writes_audit_row_and_shuts_down_cleanly`)
  continues to pass — it asserts the bring-up audit row is present,
  which is exactly what the SET-ROLE'd INSERT writes.

**Why `SET ROLE` instead of `pg_ident.conf` mapping.** HANDOVER's
Option-L sketch listed both routes and called pg_ident "cleaner"
conceptually. We went with SET ROLE for two reasons specific to
our codebase shape: (1) it's pure SQL and lives entirely in a
sqlx migration — no need to mutate `pg_hba.conf`/`pg_ident.conf`
inside the data dir post-`initdb`, which would require either
modifying `hhagent-db-init` (with awkward upgrade semantics) or
adding a non-SQL config-file step to the probe; (2) the runtime
role's privileges are bounded by the GRANTs regardless of how the
role was entered, so the threat-model story is identical. The
operational cost (one extra `SET ROLE` round-trip per connection)
is invisible against a UDS round-trip we'd be paying anyway.

**Why probe migrations as superuser, application writes as runtime.**
`MIGRATOR.run` executes whatever the migration files contain —
including `CREATE EXTENSION` (superuser-only), `CREATE ROLE`
(superuser-only), and any future migration that touches a
superuser-restricted catalog. Connecting as the runtime role for
*migrations* would deadlock the schema. The clean split: bootstrap
identity (= OS user under peer auth) for migrations, runtime role
for everything afterwards. The single shared connection makes the
role transition a one-statement event with no extra round-trips.

**Why the SET ROLE happens *inside* `probe::run` rather than at the
caller.** The probe IS the one place every daemon startup goes
through, and the audit_log INSERT IS the canonical first
application-level write. Keeping the privilege-drop adjacent to
the write that motivates it (vs. asking every caller of
`probe::run` to remember the SET ROLE step) means the runtime-role
guarantee is enforced by construction. Future call sites of the
graph helpers / audit_log writes will still need to drop privilege
themselves on their own connections — or, when Phase 1 introduces
a daemon-scoped `PgPool`, via sqlx's `PoolOptions::after_connect`
hook so every pool connection comes pre-SET-ROLE.

**Why we did not split per-worker roles yet** (e.g.
`hhagent_memory`, `hhagent_dispatcher`). The HANDOVER's Option-L
note said "GRANT all needed CRUD on the other tables; … audit each
subsystem's needs first". The audit revealed that today there's
exactly one application path — the daemon's audit_log INSERT —
which makes a per-worker split premature: every other write is a
Phase-1 future. The per-worker carving belongs in the migration
that introduces the first worker that needs *less* than full CRUD
(likely the embedding worker, which only needs SELECT + UPDATE on
`memories.embedding`). Today's bulk grant gives Phase 1 a clean
starting point; carving down is easier than carving up.

**Test count:** 151 → **154** on Linux (+2 db unit + 1 db
integration; 0 skipped, 0 failed, 0 warnings). macOS projection:
~101 — the new integration test `[SKIP]`s cleanly without
`brew install postgresql@18`, the unit tests are platform-neutral.

---

## Recently completed (previous session, 2026-05-09)

### Phase 0 cont. (Option C2.2 — schema + sqlx migrations + Graph trait + core probe + e2e)

**Closed the headline next-pickup item from the previous handover.** The
foundation that landed in C2 (a per-user PG cluster on a UDS, supervised
end-to-end) now has a schema, a migration runner integrated into the
agent-core daemon's startup, a typed graph abstraction, and a single
fail-closed probe path that connects → ensures the application DB →
runs migrations → emits a bring-up `audit_log` row.

- **`db/migrations/0001_init.sql` (~150 lines):** six tables + one
  extension. Tables:
  - **`audit_log`** — append-only landing zone for the dispatcher
    chokepoint (`core::tool_host::dispatch()`). Strictly monotonic
    `id BIGSERIAL`, `(actor, ts)` index. Append-only is application
    discipline today; a future migration adds
    `REVOKE UPDATE, DELETE ON audit_log FROM <runtime_role>` once
    that role is split out (see "Open follow-up issues" below).
  - **`tasks`** — scheduler queue. State machine pinned via a
    `CHECK (state IN ('pending', 'running', 'completed', 'failed',
    'cancelled'))` constraint rather than a Postgres ENUM (ENUMs
    require `ALTER TYPE … ADD VALUE` in its own transaction;
    CHECK is cheap to widen).
  - **`memories`** — recall corpus with three independent retrieval
    shapes (semantic via pgvector, lexical via generated `tsvector`
    + GIN, structured via JSONB metadata). Embedding column is
    `vector(1024)` (bge-m3 dim — locked in this session). HNSW
    index is *deferred* to Phase 1's first batch ingest because
    HNSW build cost scales with row count and building against an
    empty table just to grow it row-by-row is strictly worse.
  - **`entities`**, **`relations`** — graph nodes and edges.
    `UNIQUE (kind, name)` is the natural key on entities;
    `relations` allows multi-edges so two observations about the
    same triple coexist with timestamps. `ON DELETE CASCADE` keeps
    the graph internally consistent for recursive-CTE traversal.
  - **`secrets`** — column shape for AES-256-GCM ciphertext +
    12-byte nonce + AAD + key_id. The wrapping key lives in the OS
    keyring (libsecret on Linux, Keychain on macOS); only the
    column shape is pinned in this slice — the encrypt/decrypt
    runtime is a later Phase 0 slice.
  - **`vector` extension** loaded via `CREATE EXTENSION IF NOT
    EXISTS vector` (idempotent re-runs).
- **`db/src/conn.rs` (~240 lines, 9 unit tests):** `ConnectSpec` is the
  pure description of how to reach the per-user cluster.
  `default_for(&data_dir)` reads `$USER` for peer-auth identity,
  resolves `<data_dir>/sockets` for the UDS host, and pins the
  application database name to `"hhagent"`. Fails closed with
  `DbError::EnvVarMissing("USER")` when `$USER` is unset or empty —
  peer auth has no fallback identity so guessing would lead to a
  confusing connection failure or (worse) authenticating as the
  wrong role. `to_pg_connect_options()` materialises into the sqlx
  options struct. `for_maintenance_db()` swaps only the database
  field for the brief CREATE-DATABASE roundtrip in the probe.
  `quote_ident` is the canonical defense for any future DDL that
  pipes a less-trusted name into a CREATE statement (today's
  callers are constants only — belt-and-braces).
- **`db/src/probe.rs` (~150 lines, 1 unit test + 1 integration
  test):** `probe::run` is the single entry point the daemon calls
  on startup. Steps: connect to maintenance DB → check
  `pg_database` for `hhagent` → CREATE DATABASE if absent → reconnect
  to `hhagent` → `MIGRATOR.run(&mut conn)` → INSERT into `audit_log`.
  Fail-closed: any error short-circuits the daemon startup with `?`
  propagation, exits non-zero, the supervisor sees the failure, and
  the next restart attempt re-runs the probe. `ensure_database_exists`
  is split out as a pub helper so the create-branch can be exercised
  in isolation. Two short-lived connections (admin + app), no pool
  yet — the pool comes in Phase 1 when memory recall queries arrive.
- **`db/src/graph.rs` (~340 lines, 2 unit tests + happy-path
  integration test):** `Graph` trait + `PgGraph` impl. Uses
  async-fn-in-trait (Rust 1.75+) directly rather than `async-trait`
  to avoid the `Box<Pin<…>>` allocation per call. Operations:
  `upsert_entity` (`ON CONFLICT (kind, name) DO UPDATE` so re-upsert
  is id-stable, attrs replace whole-row), `upsert_relation` (multi-
  edges allowed; "upsert" here means "INSERT, returning id"),
  `get_entity`, `neighbors` (filtered + unfiltered SQL paths so the
  planner gets the predicate at parse time), `path` (recursive CTE
  with visited-set in the row to refuse re-entry on cycles, ORDER BY
  depth ASC LIMIT 1 picks the shortest path, then a second query
  expands ids into entities preserving walk order). Embeddings are
  *not* read or written in this slice — `entities.embedding` stays
  NULL for now; Phase 1 picks the encoding (pgvector crate vs
  text-cast) when the embedding worker lands.
- **`MIGRATOR` static (`db/src/lib.rs`):** `sqlx::migrate!("./migrations")`
  embeds the migration set at compile time, so a binary install
  doesn't need the source tree on disk. `MIGRATOR.run(&pool)` is
  what `probe::run` calls; sqlx tracks applied migrations in
  `_sqlx_migrations` so re-running on an up-to-date DB is a no-op.
- **`core::main::bring_up_database` (~30 lines, wired into `main.rs`
  before `wait_for_shutdown`):** the daemon's contract. Reads
  `HHAGENT_DATA_DIR` env (test-only override; production uses
  `default_data_dir()`), constructs the `ConnectSpec` from `$USER`,
  emits a structured tracing line with the resolved values, calls
  `probe::run` with `actor="core" action="startup" payload={"version": …}`,
  emits a `"database probe succeeded"` follow-up line. Any error
  bubbles up via `?` and exits non-zero.
- **sqlx feature picks (`Cargo.toml` workspace dep):** `runtime-tokio`
  (no TLS — UDS only), `postgres`, `migrate` (the `Migrator` type
  + `migrations/` runtime), `macros` (re-exports the `sqlx::migrate!()`
  proc-macro), `json` (JSONB ↔ `serde_json::Value` codec), `time`
  (TIMESTAMPTZ ↔ `time::OffsetDateTime`). Specifically *not* enabled:
  `query!` / `query_as!` (compile-time SQL validation requires
  `DATABASE_URL` at build time, which would tie CI to a running
  cluster). All non-macro forms (`sqlx::query`, `sqlx::query_as`)
  work fine.
- **`core/tests/supervisor_e2e.rs` rewrite:** test renamed to
  `core_starts_runs_db_probe_writes_audit_row_and_shuts_down_cleanly`
  to reflect the new contract. Brings up a per-test PG cluster
  (initdb + `postgres_service_spec` + start + wait socket + 500 ms
  stable-Active recheck) before installing the `hhagent` core
  service. Forwards `HHAGENT_DATA_DIR` and `USER` via `spec.env`
  so the daemon's probe targets the temp cluster (`USER` is needed
  because systemd `--user` units only inherit env vars listed in
  the unit file's `Environment=` lines). Asserts the
  `"database probe succeeded"` log line + the `audit_log` row count
  via psql. Two `ServiceGuard`s + three `PathGuard`s clean up on panic.
- **`db/tests/postgres_e2e.rs` extension:** new test
  `probe_runs_migrations_and_graph_happy_path` exercises probe
  idempotency + the `Graph` trait happy path against a real
  cluster (see "test table" entry above for the sequence).
- **`HHAGENT_DATA_DIR` env var override:** new optional env knob in
  `core::main::bring_up_database`. Production deployments leave it
  unset and use `default_data_dir()` → `~/.local/share/hhagent/pg/data`;
  tests inject a per-test temp dir so the operator's installed
  cluster is never touched. The doc-comment on `bring_up_database`
  makes the precedence explicit.

**Pre-existing Linux build break, fixed inline (`sandbox/tests/fixtures/mach_probe.rs`):**
the macOS-only Mach probe added in `326104b` (issue #1) used
`extern { static bootstrap_port; fn bootstrap_look_up; }` —
both libSystem-only symbols. `cargo build --workspace` failed on
Linux at the linker stage. The fix gates the body with
`#[cfg(target_os = "macos")]` and provides a stub `fn main()` for
non-macOS targets that prints a self-explanatory error and exits 1.
Cargo's `[[bin]]` table doesn't support per-target conditional
inclusion, so source-level cfg is the canonical pattern.

**Test count:** 138 → **151** on Linux (+12 db unit tests + 1 db integration
test + supervisor_e2e rewrite (still 1 test, contract upgraded);
0 skipped, 0 failed, 0 warnings). macOS projection: 99 once
`brew install postgresql@18`'s done; the new integration tests
`[SKIP]` cleanly without it.

**Why the probe lives in `hhagent-db` rather than `hhagent-core`.**
The probe's logic (connect → ensure DB → migrate → audit row) is
pure database orchestration with zero `core`-specific shape. Putting
it in `db` means the future memory worker (Phase 1) can call the
same function for its own bring-up without dragging the core crate
in. `core/src/main.rs` is a thin adapter: it resolves env-derived
defaults and supplies the `actor`/`action`/`payload` strings that
identify *who* is starting up.

**Why peer auth, role = OS user, application DB = `hhagent`.** These
three pin the smallest containment story. Peer auth on a UDS means
remote auth is structurally impossible (no listener); role = OS user
means a different OS user on the same host literally cannot
connect (peer rejects + 0700 socket dir); application DB =
`hhagent` keeps `postgres`/`template0`/`template1` for maintenance.
The cluster is born locked-down at `initdb` and stays that way
because every connection assumes this triple — there is no
"connect with password" code path to leak through.

**Why we did not split out a non-superuser runtime role yet.** The
HANDOVER's audit_log description called for `REVOKE UPDATE, DELETE
ON audit_log FROM <runtime_role>` once a non-superuser role is
split out. Today the daemon connects as the cluster superuser
(role == OS user, set up at `initdb` time). Adding a
`hhagent_runtime` role + `GRANT INSERT, SELECT ON audit_log` and
having the daemon connect as that role is a clean follow-up but
needs a careful audit of what each subsystem (memory worker,
graph writes, secret reads) actually requires before we GRANT.
Filed in "Open follow-up issues" below.

**Why no e2e test for the daemon's restart-loop on probe failure.**
The fail-closed contract is exercised by the existing supervisor
lifecycle pin (`Restart=on-failure RestartSec=5`) plus the new
e2e's "500 ms stable-Active recheck" — a probe that fails would
flip the daemon to Inactive within those 500 ms and the assertion
would trip with the stderr log dumped. A dedicated "probe fails →
daemon respawns → eventually succeeds" test would need to
*induce* a probe failure mid-test (e.g. tear PG down between the
daemon's connection attempts), which is an unbounded flake hazard
on a busy host. Filed for the future "exponential backoff"
hardening if and when that arrives.

**Why we opted for `sqlx` over `refinery` and over a hand-rolled
runner.** `refinery` is lighter on deps but has no async story for
sqlx-style query execution downstream — Phase 1 will need
`sqlx::query` for memory recall regardless, so adding sqlx now and
piggybacking the migration runner on the same crate is one tool
instead of two. A hand-rolled runner against `psql` would have
worked but trades binary cleanliness for source-tree-on-disk
deployment (we'd have to ship `migrations/*.sql` alongside the
daemon). `sqlx::migrate!()` macro embeds at compile time — same
shape as the workspace's existing fixture binaries.

### Post-review follow-ups (same session, after C2.2 review)

A round of self-review immediately after C2.2 turned up a handful of
small fixes (folded into a single follow-up commit) plus four parking
issues. None changed the test count (151 → 151, all green). Net diff:
~80 lines of polish.

- **`graph::path` collapsed to a single SQL statement.** The two-query
  variant ("get path ids" then "expand to entities") had a tiny race
  window against a concurrent `DELETE FROM entities` between the two
  queries — a half-deleted path could surface as
  `DbError::Query("path id N not found in entities")`. Replaced with
  one statement: a `hits` CTE picks the shortest path, then `unnest …
  WITH ORDINALITY JOIN entities` expands in path order. Snapshot
  consistency means FK CASCADE drops can't slip through.
- **`graph::decode_entity` helper.** Three near-identical column-decode
  blocks (`get_entity`, `neighbors`, `path`) collapsed into one
  `fn decode_entity(&PgRow) -> Result<Entity, DbError>`.
- **`db::env_lock` for unit tests that mutate `$USER` / `$HOME`.**
  `cargo test` runs unit tests in one binary across multiple threads;
  the three `default_for_*` tests (`conn.rs`) and the existing
  `default_data_dir_is_under_xdg_data_home` (`lib.rs`) now hold a
  shared `OnceLock<Mutex<()>>` for the duration of their env mutation.
  Pre-existing flake risk is now closed.
- **`probe::run` close-error logging.** Two `let _ = conn.close().await`
  sites swallowed the close result silently; now wrapped with
  `tracing::debug!` so a half-closed-socket symptom shows up in logs
  rather than only in packet captures.
- **Misleading "BFS-like via the planner" comment in `graph::path`
  rewritten** — execution order in the recursive term is irrelevant;
  the `ORDER BY depth ASC LIMIT 1` is what picks min-depth.
- **Parking issues filed for items deferred to later phases:**
  [#11](https://github.com/hherb/hhagent/issues/11) `PgPool` lifecycle
  (one daemon-scoped pool when concurrent workload lands in Phase 1);
  [#12](https://github.com/hherb/hhagent/issues/12) reject empty
  `secrets.aad` in the runtime encrypt path when it lands;
  [#13](https://github.com/hherb/hhagent/issues/13) migration numbering
  / rename-hygiene checklist (sqlx fingerprints version+slug, so a
  rename on a shipped migration silently breaks startup on existing
  clusters); [#14](https://github.com/hherb/hhagent/issues/14) brittle
  `wait_for_log_match("database probe succeeded")` in
  `core/tests/supervisor_e2e.rs` — promote to either a tracing constant
  in the daemon's public API or a dedicated readiness signal once
  Phase 1's scheduler grows real heartbeats.

---

## Recently completed (earlier sessions on 2026-05-09)

> **Two parallel work streams shipped earlier on 2026-05-09 from different machines.**
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

**Phase 0 is functionally complete.** The agent-core daemon comes
up fail-closed against a per-user, UDS-only Postgres cluster
managed by the same `default_supervisor()` that supervises the
daemon itself. Every application write runs under the non-
superuser `hhagent_runtime` role with a database-layer prohibition
on tampering with prior audit rows (Option L). Every Phase 0+ tool
call is funneled through `tool_host::dispatch`, which writes one
`audit_log` row per call (Option I); those rows are replicated to
a daily-rotated JSONL stream under `~/.local/state/hhagent/` by a
long-lived listener that wakes on `pg_notify`. Every secret at
rest is AES-256-GCM-encrypted under a keyring-wrapped key with
AAD-bound row identity. **As of this session,** there is also a
sole-egress LLM router (`hhagent-llm-router`) with an OpenAI-
compatible `Router::send` over reqwest+rustls, a per-OS local-
backend default (vLLM/SGLang on Linux, Ollama on macOS), and a
`PolicyGate` seam that Phase 5 plugs into; the frontier-dispatch
path is unwired by design.

This was the last must-ship Phase-0 line item. **Phase 1 is now
unblocked.** What's left as optional polish before declaring
Phase 0 done:

- **Phase 1 entry — Dispatcher chokepoint compile-time pin**
  (Option M below) — small, scoped, and the most natural bridge
  between Phase 0's "the chokepoint is documented" and Phase 1's
  "every Phase-1 consumer relies on the chokepoint pattern." Adds
  a `WorkerCommand` constructor seal so an `unsafe`-free outside-
  module construction fails to compile. **Suggested next pickup**
  if you want to start Phase 1 from a hardened base.
- **Phase 1 entry — `memory::recall` skeleton** (Option N below)
  — the first real consumer of the LLM router (for embedding-side
  reranking) and the first non-trivial sqlx query path. Bigger
  than Option M; pick this if you have a session's worth of focus
  to give it.
- **Cross-platform exponential restart backoff** (Option K below)
  — systemd 252+ has `RestartSteps`/`RestartMaxDelaySec`; macOS
  launchd's `KeepAlive=true` has no operator-controllable
  throttle, so this needs a per-OS shape. Filed but parked — no
  immediate need.

(The "LLM router HTTP-client stub" line item that was the
headline pickup in the previous handover shipped this session —
see "Recently completed" above.)

### Option M — Dispatcher chokepoint compile-time pin (Phase 1 entry, small)

The threat-model invariant (HANDOVER's *Architecture invariants*
section, repeated in `docs/threat-model.md`) says **every tool/
channel/routine action enters core through
`tool_host::dispatch()`**. Today this is policy, not enforcement —
a future contributor could `WorkerCommand::new(...)` from a
sibling module and silently bypass the audit-log write. The fix
is a constructor seal: `WorkerCommand` becomes a struct whose
fields are `pub(crate)` (or `pub(super)`), with `new` /
`with_args` / etc. moved to `tool_host`. Any sibling module that
tries to construct one fails to compile.

- **Files:** `core/src/tool_host.rs`, `core/src/lib.rs`. Maybe
  `core/src/sandbox/`-touching code if `WorkerCommand` is
  re-exported.
- **Verification:** add a doctest that constructs a
  `WorkerCommand` from outside `tool_host` and asserts it fails
  to compile (the standard "doctest as compile-fail pin" trick:
  `///` block with `compile_fail`).
- **Gotcha:** the existing `core/tests/shell_exec_e2e.rs`
  spawns workers directly — it pre-dates the dispatcher. Either
  route those tests through `tool_host::dispatch` (cleaner) or
  add a `pub(crate)` test-only `unsafe fn` constructor (uglier
  but bounded). Prefer the first.

### Option N — `memory::recall` skeleton (Phase 1 entry, larger)

The first real consumer of every Phase-0 piece: pgvector for
semantic, GIN-indexed `tsvector` for lexical, recursive-CTE
graph for relational neighbours, all fused via Reciprocal Rank
Fusion in SQL. The LLM router gets its first
`actor='llm:router'` audit-log row from this slice — likely the
embedding-call path, which routes through the local backend by
default.

- **Shape:** new `core::memory::recall(query: &str, modes:
  RecallModes, k: usize) -> Result<Vec<Memory>, _>`. Three
  independent score lists, fused per-call.
- **Files:** new `core/src/memory.rs` (or `core/src/memory/`),
  `db/src/memories.rs` (the typed sqlx query helpers).
- **Verification:** integration test against a per-test PG
  cluster (the canonical recipe is shared across
  `db/tests/postgres_e2e.rs` and `core/tests/audit_dispatch_e2e.rs`
  — issue #15 will eventually hoist it to a `tests-common` dev-
  dep crate). Seed three documents, query for one, assert it
  ranks first under each mode and under the fused score.
- **Gotcha:** the embedding worker doesn't exist yet. Phase 1's
  first instinct is "spin up a tiny model behind OpenAI HTTP";
  the cheap shortcut is to compute a deterministic test-only
  embedding (e.g. SHA-256-then-fold-to-1024-floats) so the
  integration test doesn't depend on a running model. Mark
  the helper `#[cfg(test)]` so it never ships.

### Option A — Phase 0b: macOS port  *(SHIPPED 2026-05-07)*

### Option B' — Phase 0 hardening: stage 2  *(SHIPPED 2026-05-08)*

### Option D — Phase 0 polish: per-task scratch + wall-clock kill  *(SHIPPED 2026-05-08 — `9333311`, `57edfb2`)*

### Option E — cgroup v2 CPU/memory caps  *(SHIPPED 2026-05-09 — see "Recently completed")*

### Option F — workspace+worker e2e test  *(SHIPPED 2026-05-08 — see "Recently completed")*

### Option C1 — Linux supervisor scaffold  *(SHIPPED 2026-05-10 — see "Recently completed")*

### Option C3 — macOS LaunchAgent supervisor backend  *(SHIPPED 2026-05-08 — see "Recently completed")*

### Option C4 — wire core into the supervisor  *(SHIPPED 2026-05-09 — see "Recently completed")*

### Option C2 — Phase 0 cont.: Postgres bring-up (foundation slice)  *(SHIPPED 2026-05-09 — see "Recently completed (earlier sessions)")*

### Option C2.2 — Phase 0 cont.: schema + migrations + Graph trait + core probe + e2e  *(SHIPPED 2026-05-09 — see "Recently completed (previous session)")*

### Option I — audit-log JSONL mirror + dispatcher write-site  *(SHIPPED 2026-05-10 — see "Recently completed (this session)")*

(Original pickup notes preserved below for context.) The `audit_log`
table now exists and the daemon writes one bring-up row to it; nothing
else writes yet, and there's no on-disk mirror an operator can
`tail -f`.

- **Wire `core::tool_host::dispatch()`** to insert into `audit_log` on
  every tool call: `actor` = the tool name, `action` = the JSON-RPC
  method, `payload` = `{"req": …, "result": …, "err": …, "ms": …}`
  with anything > 4 KiB truncated and replaced with a hash. Single
  insert per call, no batching at this stage — Phase 0 throughput
  doesn't justify it.
- **JSONL mirror writer:** small task spawned at daemon startup that
  watches `audit_log` (LISTEN/NOTIFY on a `audit_log_inserted`
  channel, with a `LASTVAL`-style fallback poll every 5 s) and
  appends each row as a JSON line to
  `~/.local/state/hhagent/audit-YYYY-MM-DD.jsonl`. Rotate on UTC
  date. `fsync` after each write — operator visibility beats
  throughput at this scale.
- **`hhagent-cli audit tail`** (new bin in core or a new tiny crate):
  reads the JSONL files in date order, follows the latest one. No
  DB connection needed for the viewer — operator can debug a daemon
  that crashed mid-startup without bringing the cluster up.
- **Test:** extend `core/tests/supervisor_e2e.rs` with one call to a
  fixture worker that should produce an `audit_log` insert, then
  assert the JSONL mirror picks it up within ≤ 1 s.

**Gotchas:**
- The dispatcher chokepoint invariant (every tool/channel/routine
  action enters core through `ToolHost::dispatch()`) is documented
  in HANDOVER but not enforced by a compile-time test. Phase 1
  ROADMAP's first item adds a "core::tool_host is the only
  constructor of WorkerCommand" pin; consider sneaking it in here.
- LISTEN/NOTIFY in sqlx requires a dedicated long-lived connection
  (the pool can't multiplex async notifications). One extra
  `PgConnection` is fine; document so a future "shrink the
  connection count" pass doesn't kill it.
- `~/.local/state/` is XDG-compliant on Linux; macOS doesn't follow
  XDG by default but does support the path. Use the same path on
  both OSes to keep operator docs simple (we already do this for
  the data dir).

### Option J — LLM router HTTP-client stub  *(SHIPPED 2026-05-10 — see "Recently completed (this session)")*

(Original pickup notes preserved below for context.) Same shape
as the audit-log mirror item: one new module in `core` (or a new
`hhagent-llm-router` crate, depending on where it grows to), an
OpenAI-compatible HTTP client over `reqwest` (or `hyper`), a
config knob pointing at a local backend (vLLM/SGLang on Linux,
llama.cpp/Ollama on macOS), and a *placeholder* for the Phase-5
policy gate that decides when to escalate to a frontier backend.
The escalation path is *unwired* in this slice — only the local-
backend call path needs to work end-to-end. Once Phase 1 wants
real model calls (memory recall + the scheduler loop), this is
the unblock.

### Option K — cross-platform exponential restart backoff

Currently `Restart=on-failure RestartSec=5` is a constant 5 s. systemd
252+ supports `RestartSteps` / `RestartMaxDelaySec` for true
exponential backoff. macOS launchd's `KeepAlive=true` has no
operator-controllable throttle (launchd uses an internal throttle
that's not configurable). The cross-platform shape: extend
`ServiceSpec` with `restart_backoff: Option<RestartBackoff>` (max
delay + step count); the systemd backend wires it into the unit
file, the macOS backend logs a warning at install time and falls
back to launchd's default. Filed but parked — no immediate need
since today's daemon doesn't crash routinely.

### Option L — non-superuser runtime role + audit-log GRANT split  *(SHIPPED 2026-05-10 — see "Recently completed (this session)")*

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
- [#11](https://github.com/hherb/hhagent/issues/11) — switch `core` to a daemon-scoped `PgPool` when Phase 1's concurrent workload lands (filed during C2.2 review)
- ~~[#12](https://github.com/hherb/hhagent/issues/12) — reject empty `secrets.aad` in the runtime encrypt path; drop the schema's `DEFAULT ''::bytea` once all call sites populate explicitly~~ **closed this session** — `db::secrets::put` always populates AAD via `compute_aad(name, _)` (structurally non-empty), migration `0004_secrets_aad_nonempty.sql` drops the DEFAULT and adds `CHECK (octet_length(aad) > 0)`
- [#13](https://github.com/hherb/hhagent/issues/13) — write a migration numbering / rename hygiene checklist; sqlx fingerprints version+slug, so a rename or edit on a shipped migration silently breaks startup on existing clusters (filed during C2.2 review)
- [#14](https://github.com/hherb/hhagent/issues/14) — replace the brittle `wait_for_log_match("database probe succeeded")` in `core/tests/supervisor_e2e.rs` with a constant in `hhagent-core`'s public API or a real readiness signal (filed during C2.2 review)
- [#15](https://github.com/hherb/hhagent/issues/15) — hoist the duplicated PG bring-up boilerplate (`unique_temp_root` + `initdb` + `postgresql.auto.conf` + supervisor install/start/wait) into a shared `tests-common` dev-dep crate; today it lives copy-pasted across `db/tests/postgres_e2e.rs`'s several integration tests and `core/tests/audit_dispatch_e2e.rs` (filed during the post-Option-I review in `553dcf8`)

(All Phase 0 follow-up issues filed in earlier sessions are still open: [#1](https://github.com/hherb/hhagent/issues/1)–[#6](https://github.com/hherb/hhagent/issues/6), [#8](https://github.com/hherb/hhagent/issues/8), and the C2.2-review issues [#11](https://github.com/hherb/hhagent/issues/11), [#13](https://github.com/hherb/hhagent/issues/13), [#14](https://github.com/hherb/hhagent/issues/14), plus [#15](https://github.com/hherb/hhagent/issues/15). [#12](https://github.com/hherb/hhagent/issues/12) is now closed by the secrets-at-rest slice. Both extension-deferral issues filed earlier are closed won't-fix — see below.)

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
