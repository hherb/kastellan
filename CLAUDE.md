# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Read first

**Always read `docs/devel/handovers/HANDOVER.md` before doing anything.** It is the
single source of truth for current state, what's green, what's stubbed, and the
next TODO with full context. The convention (read at start, update at end) is
documented in `docs/devel/handovers/README.md`. Skim
`docs/devel/ROADMAP.md` for the long-range view.

At the **end of every working session**, update both files (HANDOVER.md +
ROADMAP.md) and commit them — see the checklist at the bottom of HANDOVER.md.

## Project shape

A personal agentic system, security-first, vendor-neutral, AGPL-licensed.
Rust workspace with 5 crates today:

- `core` (`kastellan-core`): bin + lib. Owns the agent loop, memory, policy, LLM router, audit log, IPC. Currently has `tool_host`; everything else is stubbed.
- `sandbox` (`kastellan-sandbox`): cross-platform sandbox abstraction. `SandboxPolicy` + `SandboxBackend` trait. **Linux backend done** (`linux_bwrap.rs`); macOS Seatbelt backend is the next major work item.
- `supervisor` (`kastellan-supervisor`): systemd / launchd abstraction. Stub.
- `protocol` (`kastellan-protocol`): JSON-RPC 2.0 server/client over stdio (MCP-stdio compatible). Sole IPC mechanism between core and workers.
- `workers/shell-exec` (`kastellan-worker-shell-exec`): first jailed worker. Argv allowlist via `KASTELLAN_SHELL_ALLOWLIST` env, no shell interpretation.

## Hard constraints (do not violate)

- **AGPL-3.0 project; AGPL-compatible dependencies only.** Apache-2.0 / MIT / BSD / MPL / LGPL / (A)GPL all fine. Block any CDDL, BUSL, SSPL, Elastic License, or "source-available" dep — these are not compatible.
- **Cross-platform: Linux + macOS first-class.** No Linux-only or macOS-only code without a counterpart of equivalent guarantee. The sandbox layer is the canonical example: `linux_bwrap.rs` and (future) `macos_seatbelt.rs` both implement `SandboxBackend` from the same `SandboxPolicy` struct.
- **No NVIDIA / DGX hard dependency.** Primary host is a DGX Spark, but the system must run on any Linux box and macOS.
- **Rust core, Python only inside sandboxed workers.** Don't introduce PyO3/in-process Python. Workers communicate over stdio JSON-RPC; the core never executes untrusted code in-process.
- **Every worker is sandboxed before it runs.** There is no "spawn unsandboxed" escape hatch in `tool_host`. Don't add one.

## Build, test, run

Cargo isn't on the default `PATH` for non-interactive shells; source the env first:

```sh
source "$HOME/.cargo/env"

cargo build --workspace                                    # builds core + workers
cargo test --workspace                                     # all tests, currently 18 green
cargo test -p kastellan-sandbox                              # one crate
cargo test -p kastellan-sandbox --test linux_smoke           # one integration-test file
cargo test -p kastellan-sandbox argv_starts_with_bwrap       # one test by name substring
cargo test --workspace -- --nocapture                      # show stderr (useful when sandbox tests skip)

./target/debug/kastellan                                     # run the (skeleton) core daemon
```

There's no `cargo fmt` or `clippy` config yet; before adding either, decide on style. Until then, keep formatting consistent with what's already in the tree.

## Linux host setup (Ubuntu 24.04+)

bwrap can't create unprivileged user namespaces by default
(`kernel.apparmor_restrict_unprivileged_userns=1`). Without the workaround,
all sandbox integration tests **skip silently with a `[SKIP]` line** rather
than fail — green CI without containment is a false positive.

Fix: `sudo scripts/linux/install-bwrap-apparmor-profile.sh` once. Same pattern Flatpak uses (`/etc/apparmor.d/flatpak`). After installing, `LinuxBwrap::probe()` returns `Ok` and integration tests exercise real bwrap.

Other Linux distros without AppArmor user-ns restrictions don't need this script.

## Architecture invariants worth knowing

- **Threat-model invariant:** worst-case compromise (LLM, tool, dep, agent-authored Python) reaches *at most* the agent's own OS user, its own Postgres role, its own scratch FS, and the explicitly allowlisted endpoints for the *one* tool that was compromised. Nothing else. See `docs/threat-model.md`.
- **One process per worker, one OS sandbox per worker.** Tool workers do not share a process or sandbox with each other or with the core. IPC is JSON-RPC 2.0 line-delimited over stdin/stdout (`kastellan-protocol`).
- **bwrap argv builder pattern.** `linux_bwrap::build_argv()` is a pure function that takes `SandboxPolicy` → `Vec<String>`; it's separately testable from the spawn. Always include `--unshare-all`, `--die-with-parent`, `--new-session`, `--as-pid-1`, `--clearenv`. Env vars come *only* from `policy.env` via `--setenv`. Network depends on `Net` + `proxy_uds`: **force-routed** `Net::Allowlist` **with** `proxy_uds` set (the default in the supervised deployment — `KASTELLAN_EGRESS_FORCE_ROUTING=1`, egress slice #2) → **private netns** (NO `--share-net`) + `--bind` the proxy UDS into the jail; the worker has no direct route and reaches the allowlist only via the egress proxy (which enforces host:port + SSRF). **Legacy** `Net::Allowlist` **without** `proxy_uds` → `--share-net` (host netns). `Net::ProxyEgress` (the proxy's own policy) keeps `--share-net`.
- **`SandboxPolicy.fs_read` paths must be absolute.** `LinuxBwrap::spawn_under_policy` rejects relative paths up front.
- **`SandboxBackend` is `dyn`-safe.** Don't add generic methods to it; add new strategies as new types implementing the trait.
- **Worker binaries find themselves at runtime via `CARGO_MANIFEST_DIR` + workspace `target/debug/`** (see `core/tests/shell_exec_e2e.rs::worker_binary`). Production deployment will need a stable install location convention — flagged in HANDOVER.md "Open questions".
- **The agent core never speaks to Postgres or the LLM directly from a worker.** Memory access is core-only; LLM calls go through (the future) `llm_router`.

## When tests "pass" but feel suspicious

The Linux sandbox integration tests use a `skip_if_no_userns()` early-return pattern (printed via `eprintln!` so it shows in `cargo test -- --nocapture`). A green run with `[SKIP]` lines means tests skipped, not that bwrap actually contained anything. Always re-check the `--nocapture` output if you suspect a false green.

## Memory & persistence (your own, not the agent's)

The user has a memory store under `~/.claude/projects/-home-hherb-src-kastellan/memory/`. Locked-in decisions (license, stack, cross-platform, LLM strategy, handover convention) are recorded there and auto-loaded into context. Don't re-ask the user about settled decisions — check the memory.
