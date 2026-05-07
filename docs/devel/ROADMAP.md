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
- [ ] cgroup v2 CPU/memory caps via `systemd-run --user --scope`
- [x] Per-worker scratch dir lifecycle (create on spawn, wipe on exit) — `9333311` (subsumed by the `Workspace` type below; `Workspace::Drop` recursively wipes `<root>/<task_id>`)
- [x] Promote per-worker scratch to a first-class `Workspace` type — canonical layout `<root>/<task_id>/{in,out,tmp}`, single owner, single cleanup path; `Workspace::extend_policy(&mut SandboxPolicy)` is the canonical wiring point so host (`policy.fs_write`) and worker-side Landlock (via `tool_host::derive_lockdown_env`) cannot disagree (cf. ZeroClaw `workspace_boundary.rs`) — `9333311`
- [x] Spawn timeout / wall-clock kill — `57edfb2` (`WorkerSpec.wall_clock_ms: Option<u64>`; `spawn_worker` returns `SupervisedWorker` with a 50 ms-poll watchdog thread; cancellation on Drop closes the reused-PID race; `is_valid_target_pid` defends against `kill(-1)` fanout)

## Phase 0b — macOS Port (Seatbelt)

> Done before adding more workers, to stop Linux-isms leaking through the codebase.

- [x] `macos_seatbelt.rs`: `SandboxPolicy` → `.sb` (TinyScheme) generator — `2fa46a2`
- [x] `sandbox-exec` invocation (env_clear + per-policy env + setsid via process_group) — `2fa46a2`
- [ ] setrlimit for CPU/mem/wallclock — DEFERRED to supervisor work (parity with Linux's current state)
- [x] Network containment via `(deny network*)` + allowlist rules — `2fa46a2`
- [x] Mirror of all sandbox containment integration tests, passing on macOS — 8 tests, 0 skipped (`macos_smoke.rs`: scaffold marker, echo-runs-jailed, /etc/master.passwd invisible, /Users does not leak username, fs_read readable, /dev/disk0 denied, relative-path rejection, network unreachable) — `2fa46a2`
- [x] Mirror of all 3 e2e tests on macOS — 3 tests passing under cross-platform `shell_exec_e2e.rs` — `2fa46a2`

## Phase 0 cont. — Service supervisor

- [ ] `hhagent-supervisor` Linux backend: `systemd --user` unit generator + `systemctl --user` driver
- [ ] `hhagent-supervisor` macOS backend: LaunchAgent plist generator + `launchctl bootstrap`
- [ ] `hhagent.target` that brings up Postgres, inference, core, workers
- [ ] Auto-restart with backoff on worker crash

## Phase 0 cont. — Postgres bring-up

- [ ] Local Postgres install via systemd unit (Linux) / `pg_ctl` (macOS)
- [ ] Localhost-only via UDS, peer auth, dedicated DB role
- [ ] Extensions: `pgvector`, `pg_search` (ParadeDB BM25), `Apache AGE` graph
- [ ] `db/migrations/` skeleton: `memories`, `tasks`, `entities`, `relations`, `audit_log`, `secrets`
- [ ] `sqlx-cli` migration runner integration in core startup
- [ ] Secrets at rest: AES-256-GCM in the `secrets` table; key from OS keyring (libsecret / Keychain); decrypted only at the host boundary when injecting into a worker call, never logged, never sent to the LLM (cf. IronClaw `secrets/`, ZeroClaw `security/secrets.rs`)

## Phase 0 cont. — Audit log

- [ ] Append-only `audit_log` writer in core (every tool call, LLM call, channel I/O, memory write)
- [ ] JSONL on-disk mirror under `~/.local/state/hhagent/audit-*.jsonl` (rotated)
- [ ] CLI viewer: `hhagent-cli audit tail`

## Phase 0 cont. — LLM router stub

- [ ] OpenAI-compatible HTTP client (single sole egress for model calls)
- [ ] Local backend pointer (vLLM/SGLang on Linux, llama.cpp/Ollama on macOS)
- [ ] Frontier backend pointer (Anthropic, OpenAI) — *unwired* until Phase 5 policy gate

## Phase 1 — Memory & Loop

- [ ] **Dispatcher chokepoint invariant** documented in `docs/architecture.md`: every tool/channel/routine action enters core through `ToolHost::dispatch()` (or successor) — one audit-log write site, one policy-check site, no side doors. Add a compile-time test that `core::tool_host` is the only module that constructs `WorkerCommand`. (Pattern lifted from IronClaw `ToolDispatcher`; cheap now, painful to retrofit once channels and routines exist.)
- [ ] `memory::recall(query, modes, k)` — pgvector + BM25 + AGE traversal via Reciprocal Rank Fusion
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
