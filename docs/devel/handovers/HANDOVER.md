# hhagent — Session Handover

> Rolling document. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. See
> [`README.md`](README.md) for the convention.

**Last updated:** 2026-05-08
**Last commit:** `97d4465` (`Phase 0 hardening stage 2 — seccomp allow-list + Landlock v6`)
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
├── core               hhagent-core: lib + bin (skeleton main); tool_host derives lockdown env
├── sandbox            hhagent-sandbox: SandboxPolicy + LinuxBwrap + MacosSeatbelt
├── supervisor         hhagent-supervisor: stub (NotYetImplemented)
├── protocol           hhagent-protocol: JSON-RPC 2.0 over stdio (working)
├── workers/prelude      hhagent-worker-prelude: Linux-only Landlock + seccomp lock_down (no-op on macOS)
└── workers/shell-exec   hhagent-worker-shell-exec: uses prelude::serve_stdio
```

**`cargo test --workspace` is green: 43 tests on Linux + 31 tests on macOS, 0 skipped on either.**

| Suite | Tests | What's verified |
| ----- | ----- | --------------- |
| `protocol` unit | 3 | dispatch, parse-error fallback, method-not-found |
| `sandbox` unit (linux) | 6 | bwrap argv builder shape |
| `sandbox` unit (macos) | 13 | sandbox-exec profile builder shape + path canonicalization + on-host probe + TinyScheme-injection rejection + canonicalize error propagation |
| `sandbox` integration (`linux_smoke`) | 6 | **real** bwrap: echo runs jailed, /etc/passwd & /home invisible, listed paths visible, net unreachable under `Net::Deny`, relative-path policy rejected |
| `sandbox` integration (`macos_smoke`) | 8 | **real** sandbox-exec: scaffold marker, echo runs jailed, /etc/master.passwd invisible, /Users does not leak username, fs_read paths readable (canonicalize /etc symlinks), /dev/disk0 denied, relative-path policy rejected, network unreachable under `Net::Deny` |
| `core` unit | 4 | (unchanged — `derive_lockdown_env` adds correct env entries, doesn't overwrite caller-supplied env) |
| `core` integration (`shell_exec_e2e`) | 3 | **cross-platform real** core → bwrap+landlock+seccomp (Linux) / sandbox-exec (macOS) → shell-exec round-trip; non-allowlisted argv → POLICY_DENIED; unknown method → METHOD_NOT_FOUND |
| `prelude` unit | 11 | env-var parsing, profile parsing, BPF program builds (Strict + NetClient), unshare/mount/ptrace/bpf absent from allow-list under both profiles, socket present *only* in NetClient, essential syscalls present in BASE_ALLOW |
| `prelude` integration (`landlock_smoke`) | 4 | write-to-non-allowlisted denied with EACCES; allowlisted scratch write works; `/usr` reads still work; **v6 ABI yields `FullyEnforced` on this kernel** |
| `prelude` integration (`seccomp_smoke`) | 6 | `unshare(CLONE_NEWUSER)` and `mount(...)` killed with SIGSYS under both Strict and NetClient; `socket(AF_INET, SOCK_STREAM)` killed under Strict, survives under NetClient; `getpid()` survives |

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

## Recently completed (this session, 2026-05-08)

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

## Recently completed (previous session, 2026-05-07)

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

## Recently completed (this session, 2026-05-06)

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

Phase 0 hardening (stages 1 and 2) is done on Linux. Phase 0b (macOS) is
done. The next session can pick from any of: closing Phase 0's
deployment story (Option C), starting Phase 1 (memory + agent loop), or
some smaller polish items.

### Option A — Phase 0b: macOS port  *(SHIPPED 2026-05-07)*

### Option B' — Phase 0 hardening: stage 2  *(SHIPPED 2026-05-08 — see "Recently completed")*

### Option C — Phase 0 cont.: supervisor + Postgres bring-up (Linux)

(Unchanged from previous handover — closes the Phase 0 deployment story.)

- `hhagent-supervisor::systemd::SystemdUser` writing unit files to `~/.config/systemd/user/` and shelling out to `systemctl --user`
- A first concrete unit: `hhagent.service` for the core, `hhagent-postgres.service` for the DB (or rely on system PG and just create a role+DB)
- `db/migrations/0001_init.sql` — `audit_log`, `tasks`, `memories`, `entities`, `relations`. Use `sqlx-cli` for migrations.

**Gotchas:**
- pg_search is AGPL-3.0 — perfect license-fit but verify build is fine on aarch64.
- Apache AGE on Postgres 17 may need a recent build; check supported PG version.

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
