# browser-driver Phase 2 — make the worker render (plan)

**Spec:** `docs/superpowers/specs/2026-06-12-browser-driver-worker-design.md` (§3.1
spike findings are GREEN on Mac Seatbelt + DGX bwrap). **ROADMAP:** 147.
**Branch:** `feat/browser-driver-phase2`. **Blocker resolved:** #263 — see Slice 1.

Slice #1 (scaffold) shipped in PR #262: host manifest, Python stdio server, pure
`extract_render_result`, a `NotImplementedError` renderer stub. Phase 2 drops the
stub and makes the worker actually render under the real OS jail.

## Operator decision on #263 (the force-routing collision)

`KASTELLAN_EGRESS_FORCE_ROUTING=1` is **ON by default** in the supervised
deployment (`supervisor/src/specs.rs:240`). `policy_net_is_force_routable`
matches **all** `Net::Allowlist`, so an enabled browser-driver would be
force-routed into a private netns + `CONNECT`-over-UDS — which a browser cannot
speak → silent network loss.

**Decision (operator, 2026-06-14):** exempt browser-driver from force-routing
**for development only** (nobody uses the agent yet), but make it
**impossible to run unconfined in production by accident**. Enforced in code, not
just docs:

- browser-driver is the one force-route-exempt worker (`BROWSER_DRIVER_TOOL`).
- When force-routing is **ON** (the supervised/production signal) and
  browser-driver is spawned, the daemon **refuses fail-closed** unless the
  operator has set the explicit insecure-dev override
  `KASTELLAN_BROWSER_DRIVER_INSECURE_DIRECT_NET=1`.
- With the override set, it runs direct-net (legacy host-netns path) but emits a
  **loud `tracing::warn!`** on every spawn: egress is NOT confined at the OS
  boundary; dev only; see #263.
- Egress slice #2 (UDS↔loopback-TCP shim + in-browser per-instance CA trust) is
  the production fix; until then the in-worker per-navigation + per-subresource
  allowlist interception is the only (software-only) egress control.

This satisfies the #263 acceptance: a force-routed deployment either renders
(only with the explicit dev override) or **fails closed with a clear error** —
never silently loses the network, and never silently runs a browser unconfined.

## Slices (each TDD; all workspace tests green before commit)

### Slice 1 — force-route exemption + production lockout (pure Rust, Mac-verifiable)
- `core/src/worker_lifecycle/force_route.rs`: pure `force_route_action(force_routing_active, net_force_routable, worker_name, browser_dev_override) -> ForceRouteAction { Sidecar | Direct | DirectInsecureDevExempt | RefuseProductionUnconfined }`.
- `ForceRoutingConfig` gains `browser_insecure_direct_net: bool`; `from_env` reads `KASTELLAN_BROWSER_DRIVER_INSECURE_DIRECT_NET` (reusing `env_flag_enabled`).
- `spawn_worker_maybe_forced` consumes the action: `Sidecar`→`spawn_forced_net_worker`; `Direct`→`spawn_worker` (+ warn if browser-driver); `DirectInsecureDevExempt`→warn loudly + `spawn_worker`; `RefuseProductionUnconfined`→`Err(ToolHostError::ForceRouteUnconfined)`.
- New `ToolHostError::ForceRouteUnconfined` variant with the clear message.
- Tests: the decision truth table (all 4 arms × browser/non-browser), the new config field, the refuse error path.

### Slice 2 — `Profile::BrowserClient` seccomp (Linux; Mac cross-clippy + unit-testable)
- `workers/prelude/src/seccomp_lock.rs`: new `Profile::BrowserClient` = `net_client` + 9 additions (`fallocate ftruncate getresgid getresuid inotify_add_watch inotify_init1 memfd_create pidfd_open restart_syscall`) + an **`io_uring_setup`/`io_uring_enter` → `Errno(EPERM)`** carve-out (NOT `Allow`, NOT the default `KillProcess`).
- The Errno carve-out needs a per-syscall action distinct from the global `match_action`. Implement via seccompiler's mechanism (verify the installed crate's API — likely a separate rules map / `SeccompFilter` action per the version; if the simple API can't mix actions, add a second narrow filter or use the per-rule action form).
- `Profile::parse` learns `"browser_client"`; doc the threat-model note.
- Tests: BPF builds; `socket` present; the 9 additions present; io_uring NOT in the Allow set; (Linux smoke) io_uring returns EPERM not SIGSYS — gated like existing seccomp_smoke.
- **Cap watch:** file is already 650 LOC; if the additions push it meaningfully, lift the additions into a sibling const module or split (bucket-b candidate).

### Slice 3 — Seatbelt browser-profile extension (macOS; unit-testable builder + on-host probe)
- `sandbox/src/macos_seatbelt*`: gate the `(allow ipc-posix-shm*)` + `(allow iokit-open)`/`(allow iokit-get-properties)` + `(allow mach-lookup)`/`(allow mach-register)` cluster to the browser-driver tool only (a per-tool profile knob, NOT a global widening — keep issue #1's deny-by-default for every other worker).
- Phase-2 hardening note: try narrowing `mach-lookup` to `(global-name …)`; deferred follow-up if the service set is large.
- Tests: profile builder emits the cluster only for the browser tool; base strict profile unchanged (the existing mach-lookup-deny assertion still holds for non-browser).

### Slice 4 — `render.py` real Playwright drive (Python TDD)
- `render.py`: `PlaywrightRenderer` with `.render(url, timeout_ms, wait_until)` — launch `chromium.launch(headless=True, args=["--no-sandbox","--disable-dev-shm-usage"])`, new context, **request interception** aborting off-allowlist subresources (self-enforce `KASTELLAN_BROWSER_DRIVER_ALLOWLIST` via `web-common`-shaped host:port matching, reimplemented in Python), `page.goto(url, wait_until, timeout)` → `page.content()` + nav response status/title → `extract_render_result`.
- Pure, browser-free TDD: the allowlist host:port matcher; the should-abort decision; the render orchestration against a fake page/context (duck-typed, mirrors GLiNER's fake-model tests).
- `__main__._build_renderer`: construct `PlaywrightRenderer` from the env allowlist (drop `NotImplementedError`).

### Slice 5 — manifest wiring (Rust; Mac-verifiable)
- `core/src/workers/browser_driver.rs`: select `Profile::BrowserClient`; add `fs_read` for the browser binary tree + fonts + Playwright cache (per spike §3.1, per-OS); set `fs_write=[scratch]` + `TMPDIR=<scratch>` env (the per-spawn scratch wiring — shares browser-driver/python-exec scratch pattern); keep `Net::Allowlist`, no `proxy_uds` (legacy path, per #263 decision).
- Tests: entry has `BrowserClient` profile; fs_read includes the browser/font anchors; TMPDIR env present.

### Slice 6 — install script + e2e
- `scripts/workers/browser-driver/install.sh`: **self-contained** venv (system `python3 -m venv` — NOT uv, per the spike packaging gotcha) + `pip install -e .` + `playwright install chromium`.
- `core/tests/browser_driver_e2e.rs`: skip-as-pass without browser/sandbox/venv; hermetic off-allowlist deny (mirror `web_search_e2e`); `#[ignore] real_render_of_loopback_page` (cross-platform Seatbelt + bwrap; the spike runners are the reference).

## Verification
- `cargo test --workspace` (Mac skip-as-pass) + `cargo clippy --workspace --all-targets -D warnings`.
- `cargo clippy -p kastellan-worker-prelude --target aarch64-unknown-linux-gnu` for the Linux-gated seccomp (pure-Rust crate — the only Mac-side Linux check).
- Python: `pytest` in `workers/browser-driver`.
- DGX native `cargo test` for the Linux seccomp smoke + real render (`ssh dgx '<cmd>'`).
- Operator: stage the venv (`install.sh`) on Mac + DGX, run the `#[ignore]` real-render e2e.

## Out of scope (slice ≥2 of egress / later)
Egress-proxy force-routing for the browser (the production fix for #263);
screenshot output; scripted interaction; warm-keep lifecycle; broadened
subresource allowlist policy; hoisting web-fetch's `dom_smoothie`.
