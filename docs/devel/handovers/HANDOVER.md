# hhagent — Session Handover

> Rolling document. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. See
> [`README.md`](README.md) for the convention.

**Last updated:** 2026-05-06
**Last commit:** `3210f70` (`Phase 0 hardening stage 1: worker-side Landlock + seccomp + bwrap probe fix`)
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
├── sandbox            hhagent-sandbox: SandboxPolicy + LinuxBwrap (probe fixed)
├── supervisor         hhagent-supervisor: stub (NotYetImplemented)
├── protocol           hhagent-protocol: JSON-RPC 2.0 over stdio (working)
├── workers/prelude      hhagent-worker-prelude: Landlock + seccomp lock_down (NEW, Phase 0 hardening)
└── workers/shell-exec   hhagent-worker-shell-exec: now uses prelude::serve_stdio
```

**`cargo test --workspace` is green: 36 tests, 0 skipped.**

| Suite | Tests | What's verified |
| ----- | ----- | --------------- |
| `protocol` unit | 3 | dispatch, parse-error fallback, method-not-found |
| `sandbox` unit | 6 | bwrap argv builder shape |
| `sandbox` integration (`linux_smoke`) | 6 | **real** bwrap: echo runs jailed, /etc/passwd & /home invisible, listed paths visible, net unreachable under `Net::Deny`, relative-path policy rejected |
| `core` unit | 4 | `derive_lockdown_env` adds correct env entries, doesn't overwrite caller-supplied env |
| `core` integration (`shell_exec_e2e`) | 3 | **real** core → bwrap+landlock+seccomp → shell-exec round-trip; non-allowlisted argv → POLICY_DENIED; unknown method → METHOD_NOT_FOUND |
| `prelude` unit | 8 | env-var parsing, profile parsing, BPF program builds, kill-list contains `unshare` |
| `prelude` integration (`landlock_smoke`) | 3 | write-to-non-allowlisted denied with EACCES; allowlisted scratch write works; `/usr` reads still work |
| `prelude` integration (`seccomp_smoke`) | 3 | `unshare(CLONE_NEWUSER)` and `mount(...)` killed with SIGSYS; `getpid()` survives |

**Critical bug fix this session:** `LinuxBwrap::probe()` was missing the
`/lib*` symlinks the dynamic linker needs, so `execvp /usr/bin/true: No such
file or directory` made every bwrap-dependent test silently `[SKIP]`. The
"18 tests, 0 skipped" claim from the previous handover was a false green —
in reality only 12 host-only tests were running. The probe now mirrors the
full `build_argv` mount layout, and `cargo test --workspace -- --nocapture`
shows zero `[SKIP]` lines.

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
`sudo scripts/linux/install-bwrap-apparmor-profile.sh`. macOS isn't ported
yet (Phase 0b).

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

The user is in parallel attempting **Option A (Phase 0b — macOS port)** on a Mac, so prefer Options B' or C below for the Linux-side work. Listed in increasing scope; pick whichever the user wants.

### Option A — Phase 0b: macOS port  *(parallel work — coordinate with user)*

The user has stated they will start this on a Mac in parallel. Implementation outline kept here for cross-checking:

- `sandbox/src/macos_seatbelt.rs` — `SandboxPolicy` → `.sb` (TinyScheme) profile, then `sandbox-exec -f <profile> <program> ...`
- `setrlimit` for CPU/mem/wallclock (shared with future bwrap CPU/mem path)
- Wire `default_backend()` to pick `MacosSeatbelt::new()` under `cfg(target_os = "macos")`
- Mirror `sandbox/tests/linux_smoke.rs` as `sandbox/tests/macos_smoke.rs` (cfg-gated)
- Worker-side: on macOS the `hhagent-worker-prelude::lock_down()` is already a no-op (Seatbelt enforces equivalent containment from the parent). Verify on first run.

**Gotchas:**
- `sandbox-exec` is private API but stable; document in `docs/threat-model.md`.
- Seatbelt has no `--clearenv`; pass empty environment via `Command::env_clear()` + `Command::envs(policy.env)` before exec.
- macOS doesn't have `/usr/bin/getent`; for the net-deny test use a tiny Rust helper that calls `TcpStream::connect`.
- For the e2e test, factor the worker binary path resolver into a helper that handles the `target/debug/<name>` (Linux) vs `target/debug/<name>` (macOS) cases — they're the same path on both today, but documentation will keep the next reader sane.

**Verification:** `cargo test --workspace` should be 36 + N tests green on macOS, 0 skips.

### Option B' — Phase 0 hardening: stage 2 (Linux)

**Why:** Stage 1 used a deny-list and ABI v1 to ship quickly. Stage 2 tightens both:

**What to build:**
1. **Allow-list seccomp profile** — replace `KILL_LIST` with an explicit `ALLOW_LIST` of ~150–200 syscalls per profile. `Profile::WorkerStrict` and `Profile::WorkerNetClient` diverge here (NetClient adds `socket`, `connect`, `bind`, `listen`, `accept4`, `setsockopt`, `getsockopt`, `getpeername`, `getsockname`, `recvfrom`, `sendto`, `recvmsg`, `sendmsg`, `shutdown` and the inotify/io_uring family if needed).
2. **Bump Landlock TARGET_ABI** from V1 to V6 (or current). Audit each new access right and decide whether to handle it: `Refer` (V2), `TruncateFile` (V3), `IoctlDev` (V5), per-file scope rights (V6). Lifts `PartiallyEnforced` → `FullyEnforced` in the report.
3. **Negative tests for stage 2**: `prelude/tests/seccomp_smoke.rs` should also assert that an in-list syscall like `socket(AF_INET, SOCK_STREAM, 0)` survives under `WorkerNetClient` and is killed under `WorkerStrict`.

**Gotchas:**
- Allow-list seccomp will absolutely break things on first try. Iterate with `strace -fc` to find the missing syscalls. tokio + serde_json + std use ~80 syscalls; the rest are libc malloc / dynamic linker.
- Landlock V2's `Refer` right is "rename across rule boundaries"; V3's `TruncateFile` is "truncate even with O_RDONLY"; they're both fine to handle (deny by default).

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
