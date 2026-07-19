# Plan — browser-driver micro-VM entry (slice 2)

Spec: `docs/superpowers/specs/2026-07-19-browser-driver-microvm-entry-design.md`

Executed inline (controller-implements), iterated and gated on the DGX because
the whole diff is `cfg(target_os = "linux")` — the Mac compiles it out entirely
(memory: `cfg-linux-e2e-deadcode-dgx-clippy`, `mac-cargo-buildlock-prefer-dgx`).

## Task 1 — consts + the VM entry builder ✅

`core/src/workers/browser_driver.rs`:

* extract `ENABLE_ENV` from the string literal inside `resolve_env` (both gates
  must read the same name);
* add `cfg(linux)` consts `USE_MICROVM_ENV`, `MICROVM_WORKER_BIN`,
  `MICROVM_ROOTFS`, `MICROVM_BROWSERS_PATH`;
* add `browser_driver_firecracker_entry(binary, image_dir, allowlist)` per spec
  §3.1 — empty `fs_read`, no shim, no `LANDLOCK_RW`, `mem_mb: 2048`,
  `wall_clock_ms: 90_000`, `FirecrackerVm` backend, in-rootfs browsers path,
  image coordinates; allowlist mapped through `web_fetch::allowlist_to_net_entries`.

## Task 2 — the `resolve()` branch ✅

Short-circuits the **entire** host-side resolution (not just binary discovery as
in web-fetch), gated on `ENABLE && USE_MICROVM` via `ctx.flag_enabled` so the VM
flag is never an implicit opt-in and `ENABLE` off still reports `Disabled`.

## Task 3 — unit tests ✅

Five `cfg(linux)` tests in `core/src/workers/browser_driver/tests.rs` (spec §4).
Needed a `cfg(linux)` `outcome_label` helper: `Resolution` derives no `Debug`
(it holds a large `ToolEntry`), the same reason web-fetch's tests have one.

## Task 4 — rewire the e2e onto the production entry ✅

`core/tests/browser_driver_firecracker_e2e.rs`: delete the hand-rolled
`browser_driver_vm_policy()`; both tiers now resolve the real entry through
`BrowserDriverManifest::resolve` (`browser_driver_vm_entry`), with
`force_routed_policy` applying the one spawn-time mutation
(`proxy_uds`) that `build_launch_plan` requires. The live tier uses the entry's
own `wall_clock_ms` rather than a more generous test-only value, so a too-tight
manifest setting cannot be masked.

This is what closes spec §10.4's "an instruction, not a mechanism".

## Verification ✅

| Gate | Result |
|---|---|
| Mac `cargo check -p kastellan-core --all-targets` (isolated target dir) | clean |
| DGX `clippy -p kastellan-core --all-targets -D warnings` | clean |
| DGX `workers::browser_driver` lib | **22/0** (+5) |
| DGX hermetic e2e tier | **1/0** |
| DGX **live VM tier** (`--ignored`, real KVM + vsock + PG) | **GREEN** — CONNECT after 1.09 s, 0 `[SKIP]` |
| DGX full workspace `cargo test` + `clippy -D warnings` | see HANDOVER |

**Negative-case check (the discipline slice 1's tautological pin earned).** With
`MICROVM_WORKER_BIN` deliberately repointed at
`target/debug/kastellan-worker-browser-driver`, both the hermetic e2e pin and
`resolve_uses_microvm_entry_without_any_host_venv_or_shim` **FAIL loudly**
(e2e: "…is a HOST build-output path: PID1 will ENOENT, panic and boot-loop…").
The const was then restored and the suite re-run green. The pin is real.
