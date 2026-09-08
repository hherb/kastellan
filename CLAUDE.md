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
Rust workspace with 27 crates (full tree in the root `README.md` Layout
section). The load-bearing ones:

- `core` (`kastellan-core`): bin + lib. Agent loop + scheduler, three-lane memory, CASSANDRA oversight, audit log, the `tool_host` dispatcher chokepoint, channel bus (Matrix + gated email inbound), egress integration, secrets vault, installer; ships the `kastellan` daemon + `kastellan-cli`.
- `sandbox` (`kastellan-sandbox`): cross-platform sandbox abstraction. `SandboxPolicy` + `SandboxBackend` trait. Backends: Linux bwrap (+`systemd-run --scope` cgroup), macOS Seatbelt, opt-in Apple `container` micro-VM (macOS), opt-in Firecracker micro-VM (Linux; sha256-pinned guest kernel verified at every VM boot).
- `supervisor` (`kastellan-supervisor`): systemd --user / launchd unit generation + drivers; brings up the real `kastellan.target`.
- `protocol` (`kastellan-protocol`): JSON-RPC 2.0 server/client over stdio (MCP-stdio compatible). Sole IPC mechanism between core and workers.
- `db`, `llm-router`, `leak-scan`, `net-classify`, `tests-common`: Postgres layer + embedded migrations, the sole core-side LLM egress, the shared credential-leak scanner, the pure SSRF/denied-range predicate, and the shared dev-dep test harness.
- `workers/*`: 18 Rust workers/sidecars (prelude, shell-exec, web-common, web-fetch, web-search, web-research, mail, email-in, python-exec, egress-proxy, embed-broker, search-broker, matrix, matrix-wire, microvm-run, microvm-init, kv-demo, net-demo) plus two Python workers outside Cargo (gliner-relex, browser-driver).

## Hard constraints (do not violate)

- **AGPL-3.0 project; AGPL-compatible dependencies only.** Apache-2.0 / MIT / BSD / MPL / LGPL / (A)GPL all fine. Block any CDDL, BUSL, SSPL, Elastic License, or "source-available" dep — these are not compatible.
- **Cross-platform: Linux + macOS first-class.** No Linux-only or macOS-only code without a counterpart of equivalent guarantee. The sandbox layer is the canonical example: `linux_bwrap.rs` and `macos_seatbelt/` both implement `SandboxBackend` from the same `SandboxPolicy` struct.
- **No NVIDIA / DGX hard dependency.** Primary host is a DGX Spark, but the system must run on any Linux box and macOS.
- **Rust core, Python only inside sandboxed workers.** Don't introduce PyO3/in-process Python. Workers communicate over stdio JSON-RPC; the core never executes untrusted code in-process.
- **Every worker is sandboxed before it runs.** There is no "spawn unsandboxed" escape hatch in `tool_host`. Don't add one.

## Build, test, run

Cargo isn't on the default `PATH` for non-interactive shells; source the env first:

```sh
source "$HOME/.cargo/env"

cargo build --workspace                                    # builds core + workers
cargo test --workspace                                     # all tests (authoritative counts live in HANDOVER.md)
cargo test -p kastellan-sandbox                              # one crate
cargo test -p kastellan-sandbox --test linux_smoke           # one integration-test file
cargo test -p kastellan-sandbox argv_starts_with_bwrap       # one test by name substring
cargo test --workspace -- --nocapture                      # show stderr (useful when sandbox tests skip)

./target/debug/kastellan                                     # run the core daemon
```

**Release builds go through one script, always:** `bash scripts/build-release.sh`. Cargo unifies
features *per invocation*, so `cargo build --release -p <pkg>` and `--workspace` produce **different
bytes from identical source** — and the micro-VM rootfs images, the deploy path and the #667
freshness gate all read `target/release/`. A hand-run narrow `-p` release build leaves a reference no
image was ever built from, which is issue #682. The script also rebuilds `kastellan-worker-matrix`
with `live-matrix`, which a plain `--workspace` build gets wrong.

⚠️ **Run it LAST, and re-run it after any hand-run `cargo build --release`.** The rule is *one
producer*, not *no `-p`*: the script itself ends with a sanctioned
`-p kastellan-worker-matrix --features live-matrix` step, and both its invocations write
`target/release/kastellan-worker-matrix`. So a later bare `cargo build --release --workspace`
silently re-uplifts the non-featured artefact — measured at **0.32 s, no compilation, no output but
`Finished`** — leaving a Matrix worker that refuses to run and a `matrix.ext4` the freshness gate
calls stale. The script prints that binary's digest so the flip is visible. (Debug builds are
unaffected; this is only about `target/release/`.)

There's no `rustfmt` config yet; keep formatting consistent with what's already in the tree. Clippy IS enforced: CI runs `cargo clippy --workspace --all-targets -- -D warnings` and the tree is warning-clean — keep it that way.

## Linux host setup (Ubuntu 24.04+)

bwrap can't create unprivileged user namespaces by default
(`kernel.apparmor_restrict_unprivileged_userns=1`). Without the workaround,
all sandbox integration tests **skip silently with a `[SKIP]` line** rather
than fail — green CI without containment is a false positive.

Fix: `sudo scripts/linux/install-bwrap-apparmor-profile.sh` once. Same pattern Flatpak uses (`/etc/apparmor.d/flatpak`). After installing, `LinuxBwrap::probe()` returns `Ok` and integration tests exercise real bwrap.

Other Linux distros without AppArmor user-ns restrictions don't need this script.

For the optional Firecracker micro-VM backend (`KASTELLAN_<WORKER>_USE_MICROVM=1`,
e.g. `KASTELLAN_PYTHON_EXEC_USE_MICROVM=1`),
run the one-time privileged setup: `sudo scripts/linux/install-firecracker-vsock.sh`.
It does three things — grants the worker user the vsock device, provisions
`/var/lib/kastellan/microvm` as `root:<worker-group>` mode `1775`, and installs the
pinned guest kernel `root:root 0644`. Without it, `LinuxFirecracker::probe()` fails
closed and the worker stays on bwrap. `/dev/kvm` is usually already accessible; pass
`--kvm` if not.

Since #479 it is also a **hard prerequisite for every `build-*-rootfs.sh`**: builds only
*verify* the guest kernel (`require_guest_kernel`) and will never create one, because an
unprivileged build that can create it can create an **agent-owned** one in a
group-writable dir — silently voiding the ownership guarantee. Re-run the installer after
a pinned-kernel bump; it is idempotent and repairs older installs. For the documented
non-default layout (`KASTELLAN_MICROVM_DIR=~/.local/share/kastellan/microvm`, which root
does not manage and which therefore has **no** ownership protection) fetch the kernel
deliberately with `scripts/workers/microvm/fetch-guest-kernel.sh <dir>`.

## Architecture invariants worth knowing

- **Threat-model invariant:** worst-case compromise (LLM, tool, dep, agent-authored Python) reaches *at most* the agent's own OS user, its own Postgres role, its own scratch FS, and the explicitly allowlisted endpoints for the *one* tool that was compromised. Nothing else. See `docs/threat-model.md`.
- **One process per worker, one OS sandbox per worker.** Tool workers do not share a process or sandbox with each other or with the core. IPC is JSON-RPC 2.0 line-delimited over stdin/stdout (`kastellan-protocol`).
- **bwrap argv builder pattern.** `linux_bwrap::build_argv()` is a pure function that takes `SandboxPolicy` → `Vec<String>`; it's separately testable from the spawn. Always include `--unshare-all`, the `USERNS_LOCKDOWN_FLAGS` pair (`--unshare-user --disable-userns` — `--unshare-all` sets only bwrap's *try*-flag, and `--disable-userns` without a hard `--unshare-user` is refused at option-parse time, so a spawn missing it does not run at all), `--die-with-parent`, `--new-session`, `--as-pid-1`, `--clearenv`. Take the pair from the const; there are three bwrap argv producers (`linux_bwrap` probe + spawn, and the Firecracker VMM jail), and the two that spelled the pair out *incompletely* both broke outright (the worker spawn, #661; the VMM jail, #669) — while the probe's complete hand-spelled copy is exactly what hid the first one, by passing when every real spawn failed. The lesson is drift between copies, not spelling as such. Env vars come *only* from `policy.env` via `--setenv`. Network depends on `Net` + `proxy_uds`: **force-routed** `Net::Allowlist` **with** `proxy_uds` set (the default in the supervised deployment — `KASTELLAN_EGRESS_FORCE_ROUTING=1`, egress slice #2) → **private netns** (NO `--share-net`) + `--bind` the proxy UDS into the jail; the worker has no direct route and reaches the allowlist only via the egress proxy (which enforces host:port + SSRF). **Legacy** `Net::Allowlist` **without** `proxy_uds` → `--share-net` (host netns). `Net::ProxyEgress` (the proxy's own policy) keeps `--share-net`.
- **`SandboxPolicy.fs_read` paths must be absolute.** `LinuxBwrap::spawn_under_policy` rejects relative paths up front.
- **`SandboxBackend` is `dyn`-safe.** Don't add generic methods to it; add new strategies as new types implementing the trait.
- **Worker binaries are discovered `current_exe()`-relative** (`core::worker_manifest::discover_binary`): in the dev tree that resolves to workspace `target/debug/`, and `kastellan-cli install` copies all binaries into `~/.local/lib/kastellan/` so the same discovery works in a real deployment (no env override needed).
- **The agent core never speaks to Postgres or the LLM directly from a worker.** Memory access is core-only; LLM calls go through `llm-router` (the sole core-side model egress; the trusted `embed-broker`/`search-broker` sidecars are the deliberate worker-side exceptions).

## When tests "pass" but feel suspicious

The Linux sandbox integration tests use a `skip_if_no_userns()` early-return pattern (printed via `eprintln!` so it shows in `cargo test -- --nocapture`). A green run with `[SKIP]` lines means tests skipped, not that bwrap actually contained anything. Always re-check the `--nocapture` output if you suspect a false green.

## Memory & persistence (your own, not the agent's)

The user has a memory store under `~/.claude/projects/-home-hherb-src-kastellan/memory/`. Locked-in decisions (license, stack, cross-platform, LLM strategy, handover convention) are recorded there and auto-loaded into context. Don't re-ask the user about settled decisions — check the memory.
