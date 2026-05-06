# hhagent — Session Handover

> Rolling document. Updated at the end of every working session so the next
> session (likely a fresh Claude Code) can resume cold. See
> [`README.md`](README.md) for the convention.

**Last updated:** 2026-05-06
**Last commit:** `3051294` (`docs: add CLAUDE.md as agent operating manual`)
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
hhagent (Rust workspace, 5 crates, AGPL-3.0)
├── core              hhagent-core: lib + bin (skeleton main); has tool_host
├── sandbox           hhagent-sandbox: SandboxPolicy + LinuxBwrap working
├── supervisor        hhagent-supervisor: stub (NotYetImplemented)
├── protocol          hhagent-protocol: JSON-RPC 2.0 over stdio (working)
└── workers/shell-exec  hhagent-worker-shell-exec: argv allowlist (working)
```

**`cargo test --workspace` is green: 18 tests, 0 skipped.**

| Suite | Tests | What's verified |
| ----- | ----- | --------------- |
| `protocol` unit | 3 | dispatch, parse-error fallback, method-not-found |
| `sandbox` unit | 6 | bwrap argv builder shape |
| `sandbox` integration (`linux_smoke`) | 6 | real bwrap: echo runs jailed, /etc/passwd & /home invisible, listed paths visible, net unreachable under `Net::Deny`, relative-path policy rejected |
| `core` integration (`shell_exec_e2e`) | 3 | core → bwrap → shell-exec round-trip; non-allowlisted argv → POLICY_DENIED; unknown method → METHOD_NOT_FOUND |

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

- Initial scaffold (`140eec5`): workspace, three crate stubs, docs skeletons, AGPL-3.0
- Linux bwrap backend (`eae3df4`): real containment + AppArmor probe + install script
- Protocol crate, shell-exec worker, tool_host, end-to-end test (`f2411ec`)
- Created `docs/devel/ROADMAP.md` and this handover convention
- Studied two adjacent OpenClaw-derived projects (IronClaw, ZeroClaw); resolved parked Q2 (channel pairing flow) and Q3 (egress proxy as separate worker + leak scanner); added five concrete roadmap items (`Workspace` type, AES-256-GCM secrets, dispatcher chokepoint invariant + test, pairing flow, leak scanner, skill trust enum); codified five architectural invariants in `docs/architecture.md`

## Key design decisions locked in

- **Vendor-neutral, AGPL-compatible deps only.** AGPL project; all third-party deps must be AGPL-compatible (Apache-2.0, MIT, BSD, MPL, LGPL, (A)GPL all fine).
- **Cross-platform first-class.** Linux (DGX Spark primary) + macOS (Apple Silicon and Intel). No Linux-only code without a macOS counterpart of equivalent guarantee.
- **Rust core, Python workers.** Rust for core (no eval/dynamic surface); Python only inside sandboxed tool workers. shell-exec is Rust because it's a thin execve wrapper — Python's first appearance will be `python-exec` in Phase 4 (or possibly `web-fetch` earlier).
- **Hybrid LLM with policy routing.** Local-first via OpenAI-compatible HTTP (vLLM/SGLang on Linux, llama.cpp/Ollama on macOS). Frontier (Claude/OpenAI) only via the Phase-5 policy gate, through the egress proxy.
- **Single-host deployment via OS-native user-level supervisor.** `systemd --user` (Linux) / `launchd` LaunchAgents (macOS). No k3s.
- **Fixed core tools, sandbox-bound agent-authored Python.** Critical workers are human-curated and shipped with the binary. Agent-authored code only runs inside `python-exec`'s strict sandbox; named/persisted skills get an optional human-approve gate (Phase 4).
- **JSON-RPC 2.0 over stdio.** MCP-stdio compatible. Lets us swap in a richer MCP client later without changing the trust boundary.

## Next TODO (pick one)

The user's last guidance: *suggested stopping point after Phase 0 milestone*. Three reasonable next moves, listed in the order they're least painful to do later if you defer:

### Option A — Phase 0b: macOS port  ⭐ recommended

**Why now:** The plan flagged this explicitly. Every additional Linux-only line of code makes the macOS port harder. Right now there's only one Linux-specific module (`sandbox/src/linux_bwrap.rs`); a clean port keeps the cross-platform contract honest.

**What to build:**
- `sandbox/src/macos_seatbelt.rs` — `SandboxPolicy` → `.sb` (TinyScheme) profile, then `sandbox-exec -f <profile> <program> ...`
- `setrlimit` for CPU/mem/wallclock (bwrap had bwrap-specific CPU/mem TODOs anyway; do the rlimit path that both backends will share)
- Wire `default_backend()` to pick `MacosSeatbelt::new()` under `cfg(target_os = "macos")`
- Mirror `sandbox/tests/linux_smoke.rs` as `sandbox/tests/macos_smoke.rs` (cfg-gated). Same six containment assertions translated to Seatbelt semantics.
- Mirror `core/tests/shell_exec_e2e.rs` as `core/tests/shell_exec_e2e_macos.rs` (or, better, factor the e2e test to be platform-agnostic if `default_backend()` already picks the right one).

**Gotchas:**
- `sandbox-exec` is technically a private API but stable; document this in `docs/threat-model.md`.
- Seatbelt has no equivalent to `--clearenv`; pass empty environment via `Command::env_clear()` then `Command::envs(policy.env)` *before* exec. Update `core::tool_host` only if the abstraction needs it (probably it doesn't — bwrap handles env via `--setenv` argv flags; Seatbelt handles env via the parent's exec).
- Seatbelt network deny via `(deny network*)` works at syscall level; the test using `getent hosts` would fail closed. Verify on a real macOS box.
- macOS doesn't have `/usr/bin/getent`; use `nslookup` or write a tiny Rust helper that does `TcpStream::connect`.

**Verification:** `cargo test --workspace` should be 18 + N tests green on both Linux and macOS, with 0 skips.

### Option B — Phase 0 hardening: Landlock + seccomp on top of bwrap (Linux)

**Why:** Defence in depth. bwrap gets you most of the way; Landlock + seccomp close known bwrap gaps (e.g. `/proc/self/...` tricks) and align with what production agents do (Codex CLI uses Landlock+seccomp).

**What to build:**
- Add the `landlock` crate (BSD-3-Clause, AGPL-compatible) and apply `LandlockRuleset` from inside the worker process **before** it does any I/O
- Add `seccompiler` or `libseccomp` crate; ship a `Profile::WorkerStrict` syscall list and apply via `prctl(PR_SET_SECCOMP, ...)` after worker startup
- The application of these is in the *worker*, not bwrap argv; introduce a tiny `hhagent-worker-prelude` crate that workers `prelude::lock_down(profile)` at the top of `main()`. The shell-exec worker becomes the test bed.

**Gotchas:**
- Workers must apply these *after* loading shared libs and *before* any user-controlled input, otherwise the dynamic linker can't open libc.
- seccomp filters are easy to write too tight; start permissive and tighten incrementally with negative tests.

### Option C — Phase 0 cont.: supervisor + Postgres bring-up

**Why:** Closes out the Phase 0 deployment story so the agent can actually be left running.

**What to build:**
- `hhagent-supervisor::systemd::SystemdUser` writing unit files to `~/.config/systemd/user/` and shelling out to `systemctl --user`
- A first concrete unit: `hhagent.service` for the core, `hhagent-postgres.service` for the DB (or rely on system PG and just create a role+DB)
- `db/migrations/0001_init.sql` — `audit_log`, `tasks`, `memories`, `entities`, `relations`. Use `sqlx-cli` for migrations. Don't load `pgvector`/`pg_search`/`AGE` until they're actually queried (Phase 1).

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
