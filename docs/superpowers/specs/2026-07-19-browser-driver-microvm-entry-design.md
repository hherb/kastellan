# browser-driver × Firecracker micro-VM — slice 2: the VM entry

**Date:** 2026-07-19
**Status:** implemented
**Arc:** browser-driver micro-VM (slice 1 = rootfs, PR #470; **slice 2 = this**;
slice 3 = live render through a real egress sidecar)
**Predecessor spec:** `2026-07-19-browser-driver-microvm-rootfs-design.md`
(its §10.4 "Carried forward to slices 2 and 3" is the direct input to this one)

## 1. Problem

Slice 1 built `browser-driver.ext4` and proved a Chromium inside it can reach a
stub proxy. But nothing in the daemon can *select* that rootfs: browser-driver
is the last single-use net worker with no micro-VM entry. web-fetch, web-search
and web-research all have one (`*_firecracker_entry` + a `USE_MICROVM` branch in
`resolve()`); browser-driver — the worker with by far the largest attack surface,
a whole Chromium — does not.

## 2. Goal

`KASTELLAN_BROWSER_DRIVER_USE_MICROVM=1` (on top of the existing `ENABLE` gate)
registers browser-driver as a Firecracker VM worker booting `browser-driver.ext4`,
with no host venv, interpreter or lockdown-exec shim required.

Non-goals: a real page render through a real sidecar (slice 3); any change to
the rootfs, `microvm-init`, or the Python worker.

## 3. Design

### 3.1 `browser_driver_firecracker_entry(binary, image_dir, allowlist)`

Mirrors `web_fetch_firecracker_entry`. Differences from the host-mode
`browser_driver_entry`, each load-bearing:

| Field | Host | VM | Why |
|---|---|---|---|
| `fs_read` | venv, interpreter root + lib dirs, `/etc` resolver files, shim, operator extras | `vec![]` | A VM shares no host paths in. Everything is inside the rootfs; no NIC and no local DNS (the proxy resolves host-side). |
| `lockdown_shim` | `Some(shim)`, fail-closed | `None` | The shim applies seccomp + Landlock to a bwrap-spawned Python venv worker (#281). In VM mode the boundary **is** the VM. The shim is not staged in the rootfs, so requiring it would be a boot failure, not hardening. |
| `KASTELLAN_LANDLOCK_RW` | `["/tmp"]` | absent | Honoured by the shim; with no shim there is nothing to honour. |
| `mem_mb` | 1024 | **2048** | Firecracker *enforces* this as total guest RAM, and it must cover Chromium **plus** the `/tmp` tmpfs that `--disable-dev-shm-usage` redirects shared memory into (slice-1 spec §6, §10.4). |
| `PLAYWRIGHT_BROWSERS_PATH` | `<venv>/browsers` | `/usr/local/lib/kastellan-browser-driver/browsers` | The in-rootfs tree (`Dockerfile.browser-driver`'s `ENV`); there is no host venv to anchor against. |
| `wall_clock_ms` | 45 000 | **90 000** | A cold VM boot precedes the Playwright Node driver and a Chromium cold start. |
| `sandbox_backend` | `None` | `FirecrackerVm` | — |
| `KASTELLAN_MICROVM_DIR` / `_ROOTFS` | — | set | Tell the backend which image to boot. `build_launch_plan` strips both before hex-encoding the guest env, so they cost no cmdline budget. |

Deliberately unchanged: `Net::Allowlist` mapped through
`web_fetch::allowlist_to_net_entries` (`host:443`, wildcard dot preserved — a
bare host would be an all-port grant at the proxy, the #469 lesson), the verbatim
rows in `KASTELLAN_BROWSER_DRIVER_ALLOWLIST` for the worker's own per-navigation
check (the dual-allowlist shape), `TMPDIR`/`HOME` at `/tmp`,
`tasks_max: 512`, `Profile::WorkerBrowserClient`, `SingleUse`, and
`proxy_uds: None` (force-routing sets it at spawn).

### 3.2 The `resolve()` branch

Unlike web-fetch's, this branch must short-circuit **the entire host-side
resolution**, not just binary discovery. Host mode resolves a venv, its
interpreter prefix and out-of-prefix lib dirs, then fail-closes with
`Misconfigured` when the lockdown-exec shim is missing
(`browser_driver.rs`, the Linux arm). On a VM-only deployment none of those
exists on the host, so a branch placed any later would make a correctly
configured deployment `Misconfigured`.

Both flags are read (`ENABLE && USE_MICROVM`):

* `USE_MICROVM=1` alone must never register a tool the operator has not enabled;
* with `ENABLE` off we fall through to `resolve_env`, which reports the accurate
  `Disabled` rather than a VM-flavoured variant of it.

`ENABLE_ENV` was extracted from a string literal inside `resolve_env` into a
const so both gates read the same name.

Both flags go through `ResolveCtx::flag_enabled`, i.e. the unified truthiness
dialect (`1|true|yes|on`) — #459 residual #2.

### 3.3 Binding the guest path (the §10.4 gap)

Slice 1's §10.4 recorded that `/usr/local/bin/kastellan-worker-browser-driver`
existed as **three unlinked copies** (the Dockerfile symlink, a test const, and a
literal in an assertion), so "a slice-2 author who types a different
`MICROVM_WORKER_BIN` gets a green pin and a boot loop" — *an instruction, not a
mechanism*.

Slice 2 closes it: `core/tests/browser_driver_firecracker_e2e.rs` no longer
hand-rolls a policy. Both tiers resolve the **production** entry through
`BrowserDriverManifest::resolve`, so the hermetic pin's equality assertion is now
`MICROVM_WORKER_BIN` (carried through `resolve()` → `build_launch_plan` → the
hex-encoded `kastellan.worker=` token) against the baked path. Two copies remain
— the Dockerfile symlink and the const — and the test binds them.

The residual honesty: the test still restates the literal. It cannot import the
const (private) nor read the Dockerfile, so this is a two-way pin rather than a
single source of truth. That is strictly better than slice 1's self-comparison,
and the failure is now loud and local instead of a wall-clock hang on the DGX.

## 4. Tests

Hermetic, Linux-gated (macOS compiles them out; the DGX
`clippy -p kastellan-core --all-targets -D warnings` gate is authoritative —
memory `cfg-linux-e2e-deadcode-dgx-clippy`):

1. `firecracker_entry_is_vm_backed_with_no_host_binds_or_shim` — the entry's
   shape: VM backend, empty `fs_read`/`fs_write`, no shim, no `LANDLOCK_RW`,
   `mem_mb == 2048`, `tasks_max == 512`, port-scoped allowlist + verbatim env
   rows, in-rootfs browsers path, image coordinates.
2. `resolve_uses_microvm_entry_without_any_host_venv_or_shim` — registers with
   `exists` returning **false** throughout (host mode would be `Misconfigured`),
   and the binary is the in-rootfs path.
3. `resolve_microvm_honors_image_dir_override_and_ignores_blank`.
4. `resolve_microvm_without_enable_stays_disabled` — the VM flag is a backend
   choice, never an implicit opt-in.
5. `resolve_microvm_flag_honors_unified_truthiness_dialect`.

Plus the two rewired e2e tiers (§3.3): the hermetic launch-plan pin (any Linux,
no KVM) and the `#[ignore]` live DGX tier.

## 5. Verification

DGX (native aarch64, real KVM + vsock + PG): full workspace `cargo test`,
`clippy --workspace --all-targets -D warnings`, and the live VM tier re-run
against the production entry. Mac: `cargo check -p kastellan-core --all-targets`
(the inverse of the cfg-linux lesson — no macOS-gated code in this diff).

## 6. Carried to slice 3

From slice 1 §10.4, still open:

* **Option D is proven for launch, not for a real render.** No real page has
  rendered inside the VM. `--disable-dev-shm-usage` puts Chromium's shared
  memory in the guest `/tmp` tmpfs, competing with the same 2048 MB budget, so a
  heavy page could OOM the VM with `test_disable_dev_shm_usage_is_pinned` green
  throughout. Slice 3 should render something substantial and re-check `mem_mb`
  and `wall_clock_ms` (both set here from reasoning, not measurement).
* `PlaywrightRenderer.__init__` still accepts a `launch_args` override that
  routes around the `DEFAULT_LAUNCH_ARGS` pin (production does not use it).
* Host/VM Playwright pin skew: the Dockerfile pins exact versions,
  `workers/browser-driver/pyproject.toml` floats `>=`. Bump together.
* Defence-in-depth (noted, not actioned): the rootfs ships a full Ubuntu
  userland (`bash`, `coreutils`, `python3`, `apt`, `dpkg`). Reach is unchanged
  (read-only rootfs, no NIC, no host FS), but stripping them is cheap.
