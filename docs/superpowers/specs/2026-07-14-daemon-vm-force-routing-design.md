# Daemon-side VM force-routing (#448) — design

**Date:** 2026-07-14
**Issue:** [#448](https://github.com/hherb/kastellan/issues/448) — *daemon: force-route VM `Net::Allowlist` workers through a host MITM sidecar*
**Builds on:** #445 (the `NetWorkerSpawn.sidecar_backend` seam + the live VM×broker hybrid e2e), #446 (the second vsock channel, port 1026, for the embed broker).

## Problem

PR #445 added the `sidecar_backend` field to `NetWorkerSpawn` (and its sibling
`NetTransportSpawn`) so an egress-proxy sidecar can run on the **host** while the
worker it fronts runs in a **Firecracker VM**. The sidecar is the real-network
egress boundary, so it must always run on the host — only the *worker* may be
VM-isolated. The #445 live e2e exercises this directly by hand-assembling a
`NetWorkerSpawn` with two distinct backends.

But the **daemon** never uses the seam. The cold-spawn chokepoint
`spawn_worker_maybe_forced` (in `core/src/worker_lifecycle/force_route.rs`) sets
`sidecar_backend: backend` — the *same* backend for both. So in the supervised
deployment, a VM `Net::Allowlist` worker (e.g. `web-research` with
`KASTELLAN_WEB_RESEARCH_USE_MICROVM=1` and force-routing on) is **not** yet
force-routed through a host MITM sidecar end-to-end: the seam enables it but the
daemon doesn't select a distinct host `sidecar_backend` for VM workers.

The same gap applies to the **embed broker**: `spawn_worker_with_optional_broker`
spawns the trusted embed-broker sidecar via `spawn_broker(cfg, broker_spec, backend)`
using the *worker* backend. For a VM worker that would try to run the broker
*inside the VM*, which is wrong — the broker is a trusted host-side sidecar the VM
reaches over vsock 1026.

## Goal

Make the #445 seam the **supervised default**: when a force-routed (or
broker-backed) worker resolves to a VM backend, its egress-proxy sidecar **and**
its embed broker run on the host, while the worker runs in the VM — through the
daemon's own cold-spawn path, not just a hand-assembled test. Prove it with a
live manager-level DGX e2e.

## Key insight — no "is-VM" signal needed

The issue text sketched threading a "this worker is a VM" signal (the entry's
`sandbox_backend`, or a resolved-kind field on `ToolEntry`) to the chokepoint so
it can branch `sidecar_backend = if is_vm { host } else { worker_backend }`.

That branch is unnecessary. The correct invariant is simply:

> **The egress sidecar and the embed broker always run on the host-default
> backend (`SandboxBackends::resolve(None, None)`). The worker runs on its own
> resolved backend.**

- For a **host worker**, its resolved backend *is* the host default
  (`resolve(entry.sandbox_backend)` where `sandbox_backend` is `None` or
  `Some(Bwrap)`/`Some(Seatbelt)` → the same `Arc` the sidecar gets). So passing
  `resolve(None, None)` as the sidecar backend is **byte-identical** — no branch,
  no VM knowledge required.
- For a **VM worker**, the worker backend is `resolve(Some(FirecrackerVm))` and
  the sidecar backend is `resolve(None, None) = bwrap` (host). Automatically
  correct.
- It even generalizes for free: a hypothetical future macOS `Container` net
  worker would get a host `Seatbelt` sidecar with the same rule.

So the design is a plumbing change, not a new decision surface: thread a host
`sidecar_backend` through the two chokepoint functions, and have the managers
resolve `resolve(None, None)` for it.

## Approaches considered

- **A — sidecar/broker always on the host-default backend (CHOSEN).** As above.
  Simplest, no new flag, byte-identical for host workers, generalizes.
- **B — thread a resolved-kind / `is_vm` bool through `ToolEntry`.** What the
  issue sketched. Works, but adds a signal + a branch that A makes redundant.
- **C — pass the whole `SandboxBackends` bundle into the chokepoint and resolve
  there.** Couples the pure routing chokepoint to the backend bundle; rejected to
  keep the chokepoint dependency-injected (it takes bare `&dyn SandboxBackend`s).

## Design

All changes are in `core`. **No `kastellan-sandbox` change** — the VM↔host
plumbing (vsock relay 1025 for egress, 1026 for the broker; VMM-jail binds of
both host UDSes) already shipped in #445/#446.

### 1. Chokepoint: `spawn_worker_maybe_forced` (`force_route.rs`)

Add a `sidecar_backend: &dyn SandboxBackend` parameter. Use it for the sidecar in
the `Sidecar` arm:

```rust
let params = NetWorkerSpawn {
    backend,                 // worker → its own backend (may be a VM)
    sidecar_backend,         // egress proxy → host (was: `sidecar_backend: backend`)
    ...
};
```

The `Direct` arm ignores `sidecar_backend` (no sidecar); it stays a byte-identical
`spawn_worker(backend, spec)`.

### 2. Chokepoint: `spawn_worker_with_optional_broker` (`force_route.rs`)

Add the same `sidecar_backend: &dyn SandboxBackend` parameter and use it in two
places:

- `spawn_broker(cfg, broker_spec, sidecar_backend)` — the broker is a trusted
  host sidecar (was: `backend`).
- Forward `sidecar_backend` to both `spawn_worker_maybe_forced` calls (the
  no-broker early-return and the post-broker route).

The worker itself is still spawned via the forwarded `backend`.

### 3. Managers resolve the host default (`manager.rs`, `idle_timeout.rs`)

Both facades already hold `Arc<SandboxBackends>` and resolve the worker backend
via `self.sandboxes.resolve(entry.sandbox_backend, entry.container_image.as_deref())`.
Add, right beside it:

```rust
let sidecar_backend = self.sandboxes.resolve(None, None); // host default
```

- **`SingleUseLifecycle::acquire`** — pass `sidecar_backend.as_ref()` to
  `spawn_worker_with_optional_broker`.
- **`IdleTimeoutLifecycle::acquire`** — resolve the host default in the facade,
  thread it as a new `sidecar_backend: &dyn SandboxBackend` parameter into
  `super::idle_timeout::acquire_impl`, which forwards it to the chokepoint.

Resolving `resolve(None, None)` unconditionally (even for `Net::Deny` / non-force
-routed workers, where it goes unused) is a cheap `Arc::clone` and keeps the two
facades uniform.

### Data flow (VM web-research worker, force-routing on)

```
acquire("web-research", vm_broker_entry)
  backend        = sandboxes.resolve(Some(FirecrackerVm))   → VM
  sidecar_backend= sandboxes.resolve(None, None)            → bwrap (host)
    └─ spawn_worker_with_optional_broker
         ├─ spawn_broker(embed, spec, sidecar_backend)      → broker on HOST (bwrap)
         │     (VM worker reaches it over vsock 1026)
         └─ spawn_worker_maybe_forced(..., sidecar_backend)
              ├─ egress sidecar spawn on sidecar_backend    → proxy on HOST (bwrap)
              │     (VM worker reaches it over vsock 1025)
              └─ worker spawn on backend                    → worker in VM
```

## Testing

### Mac unit tests (`core/src/worker_lifecycle/force_route/tests.rs`)

Add **recording backends** — `RecordingBackend { label, calls: Arc<Mutex<Vec<String>>> }`
whose `spawn_under_policy` records `label` and returns
`Err(SandboxError::Backend(label))` (so no real process spawns) — to assert
*which* backend each spawn hit. The existing `FailBackend` error-variant
discrimination (`Sandbox` = direct path, `Io` = forced path) is reused where the
identity of the backend doesn't matter.

Tests to add / adjust:

1. **`forced_sidecar_spawns_on_sidecar_backend_not_worker_backend`** — Sidecar
   path, distinct `backend`/`sidecar_backend`. The egress sidecar is spawned
   first, so the *host* recorder is hit and the *VM* recorder is not (the worker
   spawn is never reached because the fake sidecar spawn fails). Proves the
   egress sidecar uses `sidecar_backend`.
2. **`broker_spawns_on_sidecar_backend_not_worker_backend`** — broker path in
   `spawn_worker_with_optional_broker` with a `broker: Some(...)` spec + distinct
   backends. `spawn_broker` runs first, so the host recorder is hit. Proves the
   broker uses `sidecar_backend`.
3. **`host_worker_sidecar_backend_equals_worker_backend_is_byte_identical`** —
   pass the *same* backend for both (the host-worker case) and assert behaviour
   is unchanged from the pre-#448 single-backend call (existing Sidecar/Direct
   assertions still hold).
4. Update the existing `spawn_worker_maybe_forced` / `spawn_worker_with_optional_broker`
   call sites in the test module for the new parameter (pass the same
   `FailBackend` for both to preserve their current assertions).

These are Mac-verifiable (no VM), and pin the seam wiring + the byte-identical
host path.

### DGX manager-level e2e (`core/tests/web_research_vm_force_route_daemon_e2e.rs`, `#[ignore]`)

New live test, gated like the #445 broker e2e (real KVM + vsock + live SearxNG +
live Ollama + PG). It drives the **daemon's own manager**, not a hand-assembled
`NetWorkerSpawn`:

```rust
let sbs = Arc::new(SandboxBackends::default_for_current_os());
let force = ForceRoutingConfig::new(egress_proxy_bin, /tmp, pg_sink, None);
let brokers = BrokerConfigs::with(embed = BrokerConfig::new(Embed, broker_bin, /tmp));
let mgr = SingleUseLifecycle::with_force_routing(sbs, Some(Arc::new(force)), brokers);

let entry = web_research_firecracker_broker_entry(worker_in_guest, image_dir,
    &searx_endpoint, &embed_endpoint, None, &allowlist); // sandbox_backend = Some(FirecrackerVm)

let mut handle = mgr.acquire("web-research", &entry).await?;
let result = dispatch(&pool, &vault, handle.worker_mut(),
    "web-research", "web.research", json!({"query": "...", "max_sources": 2})).await?;

assert_eq!(result["ranking"], "hybrid");           // brokered embed over vsock 1026
// + assert the embed host never appears in the egress decision audit rows
//   (zero embed egress; the broker path carried the embed bytes).
```

This is **strictly stronger** than the #445 e2e: it exercises the manager's own
`resolve(Some(FirecrackerVm))` + `resolve(None, None)` selection and the two
chokepoint functions, proving the daemon force-routes a VM worker through a host
sidecar+broker without any hand-wiring.

Reuse the #445 broker e2e's fixtures where possible (rootfs `web-research.ext4`,
`firecracker_backend()`/`host_backend()` helpers, the SearxNG loopback carve-out,
the egress-decision printing sink).

## Verification

- **Mac (Seatbelt):** `cargo test -p kastellan-core worker_lifecycle::force_route`
  green; `cargo clippy --workspace --all-targets -D warnings` clean; full
  `cargo test --workspace` green (host path byte-identical — no behaviour change
  expected for existing suites).
- **DGX (native aarch64, real KVM+vsock+PG+SearxNG+Ollama):** the new
  `#[ignore]` manager-level e2e GREEN (`ranking == "hybrid"`, embed host absent
  from egress); full-workspace `cargo test` + `clippy --workspace --all-targets
  -D warnings` clean, 0 `[SKIP]` — new DGX baseline (was 2514/0/43; +unit tests
  +1 ignored e2e).
- FC e2e gotchas (from HANDOVER): rebuild the release launcher
  (`cargo build --release -p kastellan-microvm-run`) + the `web-research.ext4`
  rootfs; `export PATH=$HOME/.local/bin:$PATH` (firecracker is off the
  non-interactive ssh PATH); build the egress-proxy + embed-broker binaries.

## Scope boundaries (out of scope)

- No new env flag. VM force-routing is naturally opt-in via the existing
  `KASTELLAN_WEB_RESEARCH_USE_MICROVM=1` + `KASTELLAN_EGRESS_FORCE_ROUTING=1`.
- No `kastellan-sandbox` change (vsock relays + VMM-jail binds already shipped).
- macOS `Container` VM force-routing is not wired for any worker today (Container
  = gliner-relex = `Net::Deny`), but rule A covers it correctly if one appears —
  no extra work needed.
- `IdleTimeoutLifecycle` warm-reuse keeps the sidecar/broker a worker was first
  spawned with (unchanged); only the cold-spawn path is affected.

## Risk

Low. The host-worker path is byte-identical by construction (the sidecar backend
and worker backend resolve to the same `Arc`). The only new behaviour activates
for VM workers, which are opt-in and covered by the DGX e2e.
