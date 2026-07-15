# Daemon-side VM force-routing (#448) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the #445 `NetWorkerSpawn.sidecar_backend` seam into the daemon cold-spawn chokepoint so a force-routed VM `Net::Allowlist` worker (web-research `USE_MICROVM`) runs its egress-proxy sidecar **and** its embed broker on the host while the worker runs in the VM.

**Architecture:** Thread a host `sidecar_backend: &dyn SandboxBackend` through the two chokepoint functions (`spawn_worker_maybe_forced`, `spawn_worker_with_optional_broker`) in `core/src/worker_lifecycle/force_route.rs`. The two lifecycle-manager facades resolve `SandboxBackends::resolve(None, None)` (the host default) as that sidecar backend. This is **byte-identical for host workers** (their backend *is* the host default) and automatically correct for VM workers. No new env flag; no "is-VM" branch. No `kastellan-sandbox` change (the vsock relays 1025/1026 + VMM-jail UDS binds already shipped in #445/#446).

**Tech Stack:** Rust (workspace, rustc 1.96), `kastellan-core`, `kastellan-sandbox`; tests via `cargo test`; macOS Seatbelt dev box + DGX Spark (aarch64) native Linux over `ssh dgx '<cmd>'` for real KVM+vsock+PG.

## Global Constraints

- **AGPL-3.0; AGPL-compatible deps only.** No new dependencies are needed for this change.
- **Cross-platform Linux + macOS first-class.** The change is OS-agnostic core wiring; the VM path is Linux-only (Firecracker), the host path works on both.
- **`spawn_worker_maybe_forced` and `spawn_worker_with_optional_broker` are `pub(crate)`** — only in-crate callers + the crate's own tests break on a signature change (no external API break).
- **Byte-identical host path is a hard requirement.** For any non-VM worker the resolved `sidecar_backend` equals the worker `backend` (same `Arc`), so behaviour must not change. Existing suites must stay green unchanged (except mechanical new-arg updates).
- **TDD:** write the failing test first, watch it fail, implement minimally, watch it pass, commit. Run all cargo commands in the **foreground** (no background jobs).
- **Source cargo env first:** every shell step begins from a shell where `source "$HOME/.cargo/env"` has run.
- **Commit granularity:** one commit per task; `git add` the specific files only (never `git add -A`).

---

### Task 1: Thread `sidecar_backend` through `spawn_worker_maybe_forced` (egress-sidecar seam)

**Files:**
- Modify: `core/src/worker_lifecycle/force_route.rs` (`spawn_worker_maybe_forced` signature + `NetWorkerSpawn` construction ~L227-268; the two internal calls from `spawn_worker_with_optional_broker` at ~L306 and ~L330)
- Test: `core/src/worker_lifecycle/force_route/tests.rs` (add a shared `RecordingBackend`; add one new test; update 4 existing call sites at L61, L78, L94, L138)

**Interfaces:**
- Produces: `pub(crate) fn spawn_worker_maybe_forced(force: Option<&ForceRoutingConfig>, backend: &dyn SandboxBackend, sidecar_backend: &dyn SandboxBackend, spec: &WorkerSpec<'_>, worker_name: &str) -> Result<SupervisedWorker, ToolHostError>` — the new 5-arg shape. `backend` spawns the worker; `sidecar_backend` spawns the egress-proxy sidecar.
- Produces (test-only, shared with Task 2): `struct RecordingBackend { label: &'static str, calls: Arc<Mutex<Vec<&'static str>>> }` whose `spawn_under_policy` pushes `label` to `calls` and returns `Err(SandboxError::Backend(label))`.

- [ ] **Step 1: Write the failing test + the shared RecordingBackend**

Add to the top of `core/src/worker_lifecycle/force_route/tests.rs` (after the existing `use` lines and `FailBackend`):

```rust
use std::sync::{Arc, Mutex};

/// A backend that records the label of each spawn attempt and always fails
/// (so no real child process is created). Two instances with distinct labels
/// let a test assert *which* backend a given spawn hit. Shared by the
/// egress-sidecar (Task 1) and broker (Task 2) seam tests.
struct RecordingBackend {
    label: &'static str,
    calls: Arc<Mutex<Vec<&'static str>>>,
}
impl SandboxBackend for RecordingBackend {
    fn spawn_under_policy(
        &self,
        _policy: &SandboxPolicy,
        _program: &str,
        _args: &[&str],
    ) -> Result<std::process::Child, SandboxError> {
        self.calls.lock().expect("recording mutex poisoned").push(self.label);
        Err(SandboxError::Backend(self.label.into()))
    }
}
```

Add the new test (Sidecar path spawns the egress sidecar on `sidecar_backend`, never touching the worker backend, because the sidecar spawn fails first):

```rust
#[test]
fn forced_egress_sidecar_spawns_on_sidecar_backend_not_worker_backend() {
    let policy = SandboxPolicy {
        net: Net::Allowlist(vec!["api.example.com:443".into()]),
        ..SandboxPolicy::default()
    };
    let scratch = tempfile::tempdir().expect("scratch root");
    let cfg = config_with(scratch.path().to_path_buf());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let worker_backend = RecordingBackend { label: "vm-worker", calls: Arc::clone(&calls) };
    let sidecar_backend = RecordingBackend { label: "host-sidecar", calls: Arc::clone(&calls) };

    let res = spawn_worker_maybe_forced(
        Some(&cfg),
        &worker_backend,
        &sidecar_backend,
        &spec_for(&policy),
        "web-fetch",
    );

    // Force-route path fails at the (recording) sidecar spawn → Io.
    assert!(matches!(res, Err(ToolHostError::Io(_))), "forced path maps sidecar failure to Io");
    let hit = calls.lock().unwrap().clone();
    assert_eq!(
        hit,
        vec!["host-sidecar"],
        "the egress sidecar must spawn on sidecar_backend (host); the worker backend must not be reached"
    );
}
```

- [ ] **Step 2: Run the test to verify it fails (compile error — wrong arity)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib worker_lifecycle::force_route::tests::forced_egress_sidecar_spawns_on_sidecar_backend_not_worker_backend 2>&1 | tail -20`
Expected: FAIL — compile error, `spawn_worker_maybe_forced` takes 4 arguments but 5 were supplied (the `sidecar_backend` param doesn't exist yet).

- [ ] **Step 3: Add the `sidecar_backend` parameter + wire it into `NetWorkerSpawn`**

In `core/src/worker_lifecycle/force_route.rs`, change the `spawn_worker_maybe_forced` signature and its doc to add `sidecar_backend`:

```rust
pub(crate) fn spawn_worker_maybe_forced(
    force: Option<&ForceRoutingConfig>,
    backend: &dyn SandboxBackend,
    sidecar_backend: &dyn SandboxBackend,
    spec: &WorkerSpec<'_>,
    worker_name: &str,
) -> Result<SupervisedWorker, ToolHostError> {
```

In the `ForceRouteAction::Sidecar` arm, replace the `sidecar_backend: backend` line in the `NetWorkerSpawn` literal:

```rust
            let params = crate::egress::net_worker::NetWorkerSpawn {
                backend,
                // The egress-proxy sidecar is the real-network egress boundary,
                // so it ALWAYS runs on the host default backend even when
                // `backend` is a VM. For host workers the caller passes the same
                // backend for both (byte-identical). (#448)
                sidecar_backend,
                proxy_bin: &cfg.proxy_bin,
                spec,
                allowlist: &allowlist,
                worker_name,
                secret_fingerprints: &[],
                cert_pins_json: pins_json.as_deref(),
                disable_mitm: disable_mitm_for(worker_name),
            };
```

Update the doc comment on `spawn_worker_maybe_forced` to mention `sidecar_backend` (add one line: "* `sidecar_backend` — the host backend the egress-proxy sidecar runs under; equals `backend` for host workers, differs (host vs VM) for VM workers.").

- [ ] **Step 4: Keep `spawn_worker_with_optional_broker`'s internal calls compiling (pass `backend` for now)**

In `spawn_worker_with_optional_broker`, both calls to `spawn_worker_maybe_forced` must pass a `sidecar_backend`. For this task pass `backend` (byte-identical — Task 3 makes the managers pass the real host default). Update the early-return call (~L306):

```rust
        return spawn_worker_maybe_forced(force, backend, backend, spec, worker_name);
```

and the post-broker call (~L330):

```rust
    let mut worker = spawn_worker_maybe_forced(force, backend, backend, &brokered_spec, worker_name)?;
```

- [ ] **Step 5: Update the 4 existing `spawn_worker_maybe_forced` call sites in tests**

In `core/src/worker_lifecycle/force_route/tests.rs`, add `&FailBackend` as the new third arg to each existing call (L61, L78, L94, L138). Example for L61:

```rust
    let res = spawn_worker_maybe_forced(None, &FailBackend, &FailBackend, &spec_for(&policy), "web-fetch");
```

Do the same for the calls in `some_config_allowlist_routes_through_forced_spawn` (L78), `some_config_deny_net_uses_plain_spawn_worker` (L94), and `browser_driver_force_routed_takes_sidecar_path` (L138) — insert `&FailBackend` as the third argument in each. Their assertions are unchanged (byte-identical: same backend for both).

- [ ] **Step 6: Run the new test + the whole force_route unit suite to verify green**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib worker_lifecycle::force_route 2>&1 | tail -20`
Expected: PASS — the new test passes (`calls == ["host-sidecar"]`) and all pre-existing force_route unit tests still pass.

- [ ] **Step 7: Clippy the crate**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -15`
Expected: clean (no warnings). NOTE: this will fail to compile until Task 3 fixes the manager call sites **only if** the managers call `spawn_worker_maybe_forced` directly — they do **not** (they call `spawn_worker_with_optional_broker`, whose signature is unchanged in Task 1). So the crate compiles and clippy is clean here.

- [ ] **Step 8: Commit**

```bash
git add core/src/worker_lifecycle/force_route.rs core/src/worker_lifecycle/force_route/tests.rs
git commit -m "feat(#448): egress sidecar runs on a host sidecar_backend in spawn_worker_maybe_forced

Thread a host sidecar_backend through the force-route chokepoint so a VM
worker's egress-proxy sidecar can run on the host. Byte-identical for host
workers (callers pass the same backend for both today). Recording-backend
unit test proves the sidecar spawns on sidecar_backend.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Thread `sidecar_backend` through `spawn_worker_with_optional_broker` (broker + forward)

**Files:**
- Modify: `core/src/worker_lifecycle/force_route.rs` (`spawn_worker_with_optional_broker` signature + the `spawn_broker` call at ~L318 + the two forwarded `spawn_worker_maybe_forced` calls)
- Modify: `core/src/worker_lifecycle/manager.rs` (`SingleUseLifecycle::acquire` call site, ~L274) and `core/src/worker_lifecycle/idle_timeout.rs` (`acquire_impl` call site, ~L494) — pass `backend`/`sandbox` for the new arg to keep them compiling (Task 3 refines)
- Test: `core/src/worker_lifecycle/force_route/tests.rs` (add one new test; update the 2 existing `spawn_worker_with_optional_broker` call sites at L431, L459)

**Interfaces:**
- Consumes: `RecordingBackend` (Task 1), `spawn_worker_maybe_forced` (Task 1's 5-arg shape).
- Produces: `pub(crate) fn spawn_worker_with_optional_broker(force: Option<&ForceRoutingConfig>, broker_configs: &BrokerConfigs, backend: &dyn SandboxBackend, sidecar_backend: &dyn SandboxBackend, spec: &WorkerSpec<'_>, broker: Option<&BrokerSpec>, worker_name: &str) -> Result<SupervisedWorker, ToolHostError>` — the new 7-arg shape. The embed broker AND the egress sidecar both spawn on `sidecar_backend`.

- [ ] **Step 1: Write the failing test (broker spawns on `sidecar_backend`)**

Add to `core/src/worker_lifecycle/force_route/tests.rs`:

```rust
#[test]
fn broker_spawns_on_sidecar_backend_not_worker_backend() {
    use crate::broker::{BrokerConfig, BrokerConfigs, BrokerKind, BrokerSpec};

    let policy = SandboxPolicy {
        net: Net::Allowlist(vec!["searx.example.org:443".into()]),
        ..SandboxPolicy::default()
    };
    let scratch = tempfile::tempdir().expect("broker scratch root");
    let broker_cfg = BrokerConfig::new(
        BrokerKind::Embed,
        PathBuf::from("/nonexistent/embed-broker"),
        scratch.path().to_path_buf(),
    );
    let broker_configs = BrokerConfigs { embed: Some(Arc::new(broker_cfg)), ..Default::default() };
    let broker_spec = BrokerSpec::embed("http://127.0.0.1:11434/v1/embeddings");

    let calls = Arc::new(Mutex::new(Vec::new()));
    let worker_backend = RecordingBackend { label: "vm-worker", calls: Arc::clone(&calls) };
    let sidecar_backend = RecordingBackend { label: "host-sidecar", calls: Arc::clone(&calls) };

    let res = spawn_worker_with_optional_broker(
        None, // no force-routing — the broker spawn is what we're testing
        &broker_configs,
        &worker_backend,
        &sidecar_backend,
        &spec_for(&policy),
        Some(&broker_spec),
        "web-research",
    );

    // The broker is spawned first and fails (recording backend) → Io.
    assert!(matches!(res, Err(ToolHostError::Io(_))), "broker spawn failure maps to Io");
    let hit = calls.lock().unwrap().clone();
    assert_eq!(
        hit,
        vec!["host-sidecar"],
        "the embed broker must spawn on sidecar_backend (host); the worker backend must not be reached"
    );
}
```

- [ ] **Step 2: Run the test to verify it fails (compile error — wrong arity)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib worker_lifecycle::force_route::tests::broker_spawns_on_sidecar_backend_not_worker_backend 2>&1 | tail -20`
Expected: FAIL — compile error, `spawn_worker_with_optional_broker` takes 6 arguments but 7 were supplied.

- [ ] **Step 3: Add the `sidecar_backend` parameter + use it for the broker and forwarded calls**

In `core/src/worker_lifecycle/force_route.rs`, change the signature (keep the existing `#[allow(clippy::too_many_arguments)]`):

```rust
#[allow(clippy::too_many_arguments)] // mirrors spawn_worker_maybe_forced + the broker configs
pub(crate) fn spawn_worker_with_optional_broker(
    force: Option<&ForceRoutingConfig>,
    broker_configs: &BrokerConfigs,
    backend: &dyn SandboxBackend,
    sidecar_backend: &dyn SandboxBackend,
    spec: &WorkerSpec<'_>,
    broker: Option<&BrokerSpec>,
    worker_name: &str,
) -> Result<SupervisedWorker, ToolHostError> {
```

Change the no-broker early return to forward `sidecar_backend`:

```rust
    let Some(broker_spec) = broker else {
        return spawn_worker_maybe_forced(force, backend, sidecar_backend, spec, worker_name);
    };
```

Change the broker spawn (was `spawn_broker(cfg, broker_spec, backend)`) to run on the host sidecar backend:

```rust
    // 1. Broker first (fail-closed on its Err). The embed broker is a trusted
    //    HOST sidecar the worker reaches over vsock 1026 in VM mode, so it runs
    //    on `sidecar_backend` — the host default — never inside a VM. (#448)
    let (sidecar, uds) = spawn_broker(cfg, broker_spec, sidecar_backend)?;
```

Change the post-broker route to forward `sidecar_backend`:

```rust
    let mut worker = spawn_worker_maybe_forced(force, backend, sidecar_backend, &brokered_spec, worker_name)?;
```

Update the doc comment on `spawn_worker_with_optional_broker` to note that both the broker and the egress sidecar run on `sidecar_backend`.

- [ ] **Step 4: Keep the two production call sites compiling (pass the worker backend for now)**

In `core/src/worker_lifecycle/manager.rs`, `SingleUseLifecycle::acquire` (~L274), add the new arg passing the same resolved `backend` (Task 3 replaces this with the host default):

```rust
        let worker = spawn_worker_with_optional_broker(
            self.force.as_deref(),
            &self.broker_configs,
            backend.as_ref(),
            backend.as_ref(),
            &spec,
            entry.broker.as_ref(),
            tool_name,
        )?;
```

In `core/src/worker_lifecycle/idle_timeout.rs`, `acquire_impl` (~L494), pass `sandbox` for both:

```rust
    let worker = spawn_worker_with_optional_broker(
        force,
        broker_configs,
        sandbox,
        sandbox,
        &spec,
        entry.broker.as_ref(),
        tool_name,
    )?
    .with_scratch(scratch);
```

- [ ] **Step 5: Update the 2 existing `spawn_worker_with_optional_broker` call sites in tests**

In `core/src/worker_lifecycle/force_route/tests.rs`, add `&FailBackend` as the new fourth arg to the two existing calls in `broker_requested_without_config_fails_closed_before_spawn` (~L431) and `no_broker_requested_is_passthrough` (~L459). Example (L431):

```rust
    let res = spawn_worker_with_optional_broker(
        None,
        &BrokerConfigs::default(),
        &FailBackend,
        &FailBackend,
        &spec,
        Some(&broker_spec),
        "web-research",
    );
```

Their assertions are unchanged (fail-closed / passthrough are backend-agnostic).

- [ ] **Step 6: Run the force_route unit suite to verify green**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib worker_lifecycle::force_route 2>&1 | tail -20`
Expected: PASS — the new broker test (`calls == ["host-sidecar"]`) plus all pre-existing force_route tests.

- [ ] **Step 7: Clippy the crate**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -15`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add core/src/worker_lifecycle/force_route.rs core/src/worker_lifecycle/force_route/tests.rs core/src/worker_lifecycle/manager.rs core/src/worker_lifecycle/idle_timeout.rs
git commit -m "feat(#448): embed broker runs on the host sidecar_backend too

spawn_worker_with_optional_broker now spawns both the embed broker and the
egress sidecar on sidecar_backend (the trusted host sidecars a VM worker
reaches over vsock 1025/1026). Call sites still pass the worker backend for
both (byte-identical); Task 3 makes the managers select the host default.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Managers resolve the host-default sidecar backend (delivers the behaviour)

**Files:**
- Modify: `core/src/worker_lifecycle/manager.rs` (`SingleUseLifecycle::acquire` ~L268-281; `IdleTimeoutLifecycle::acquire` ~L436-438)
- Modify: `core/src/worker_lifecycle/idle_timeout.rs` (`acquire_impl` signature ~L373 + its `spawn_worker_with_optional_broker` call ~L494)

**Interfaces:**
- Consumes: `spawn_worker_with_optional_broker` (Task 2's 7-arg shape); `SandboxBackends::resolve(None, None) -> Arc<dyn SandboxBackend>` (host default).
- Produces: `pub(crate) async fn acquire_impl(sandbox: &dyn SandboxBackend, sidecar_backend: &dyn SandboxBackend, backoff: RestartBackoff, registry: &WarmRegistry, tool_name: &str, entry: &ToolEntry, force: Option<&ForceRoutingConfig>, broker_configs: &BrokerConfigs, ...)` — the new `sidecar_backend` parameter inserted right after `sandbox`.

- [ ] **Step 1: `SingleUseLifecycle::acquire` — resolve the host default and pass it**

In `core/src/worker_lifecycle/manager.rs`, in `SingleUseLifecycle::acquire`, right after the existing `let backend = self.sandboxes.resolve(...)` block (~L268-270), add:

```rust
        // The egress sidecar + embed broker always run on the host default
        // backend (never inside a VM). For host workers this equals `backend`,
        // so the spawn is byte-identical; for a VM worker (sandbox_backend =
        // Some(FirecrackerVm)) it is the host bwrap/Seatbelt backend. (#448)
        let sidecar_backend = self.sandboxes.resolve(None, None);
```

Change the `spawn_worker_with_optional_broker` call to pass `sidecar_backend.as_ref()` as the fourth arg (replacing the temporary `backend.as_ref()` from Task 2):

```rust
        let worker = spawn_worker_with_optional_broker(
            self.force.as_deref(),
            &self.broker_configs,
            backend.as_ref(),
            sidecar_backend.as_ref(),
            &spec,
            entry.broker.as_ref(),
            tool_name,
        )?;
```

- [ ] **Step 2: `IdleTimeoutLifecycle::acquire` — resolve the host default and thread it into `acquire_impl`**

In `core/src/worker_lifecycle/manager.rs`, in `IdleTimeoutLifecycle::acquire`, after the existing `let backend = self.sandboxes.resolve(...)` (~L436-437), add:

```rust
        let sidecar_backend = self.sandboxes.resolve(None, None); // host default (#448)
```

Change the `super::idle_timeout::acquire_impl(...)` call (~L438) to pass `sidecar_backend.as_ref()` as the new second argument (immediately after the existing `backend.as_ref()`/`sandbox` argument). For example, if the current call is `acquire_impl(backend.as_ref(), backoff, registry, tool_name, entry, force, broker_configs, ...)`, it becomes:

```rust
        super::idle_timeout::acquire_impl(
            backend.as_ref(),
            sidecar_backend.as_ref(),
            /* remaining args unchanged */
        )
```

(Read the exact existing argument list at the call site and insert `sidecar_backend.as_ref()` right after the backend argument — do not reorder the others.)

- [ ] **Step 3: `acquire_impl` — accept and forward `sidecar_backend`**

In `core/src/worker_lifecycle/idle_timeout.rs`, add the `sidecar_backend` parameter to `acquire_impl` right after `sandbox` (~L373):

```rust
pub(crate) async fn acquire_impl(
    sandbox: &dyn SandboxBackend,
    sidecar_backend: &dyn SandboxBackend,
    backoff: RestartBackoff,
    registry: &WarmRegistry,
    tool_name: &str,
    entry: &ToolEntry,
    force: Option<&ForceRoutingConfig>,
    broker_configs: &BrokerConfigs,
    // ... remaining params unchanged
```

Change the `spawn_worker_with_optional_broker` call (~L494) to forward `sidecar_backend` (replacing the temporary second `sandbox` from Task 2):

```rust
    let worker = spawn_worker_with_optional_broker(
        force,
        broker_configs,
        sandbox,
        sidecar_backend,
        &spec,
        entry.broker.as_ref(),
        tool_name,
    )?
    .with_scratch(scratch);
```

Add a one-line doc note on `acquire_impl` explaining `sidecar_backend` is the host default that the egress sidecar/broker run on (worker uses `sandbox`).

- [ ] **Step 4: Build the crate + run the manager/lifecycle unit + hermetic tests**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib worker_lifecycle 2>&1 | tail -20`
Expected: PASS — all `worker_lifecycle` unit tests green (host path byte-identical: `resolve(None,None)` equals the worker backend for the non-VM entries these tests use).

- [ ] **Step 5: Confirm no host-path regression across the broader suite (Mac)**

Run: `source "$HOME/.cargo/env" && cargo build --workspace 2>&1 | tail -5 && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -15`
Expected: build exit 0; clippy clean. (Rationale: Task 3 is pure wiring — the managers select `resolve(None,None)` for the sidecar. Its VM behaviour is proven by the DGX e2e in Task 4; its byte-identical host behaviour is guarded by compile + clippy + the unchanged existing manager/host e2e suites.)

- [ ] **Step 6: Commit**

```bash
git add core/src/worker_lifecycle/manager.rs core/src/worker_lifecycle/idle_timeout.rs
git commit -m "feat(#448): managers run the egress sidecar + broker on the host default

Both lifecycle facades resolve SandboxBackends::resolve(None, None) as the
sidecar_backend and pass it to the chokepoint. Byte-identical for host workers
(their backend IS the host default); a VM worker (sandbox_backend =
Some(FirecrackerVm)) now runs its sidecar+broker on the host bwrap while the
worker boots in the VM — the daemon default for #448.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: DGX manager-level `#[ignore]` e2e (live VM force-route through the daemon)

**Files:**
- Create: `core/tests/web_research_vm_force_route_daemon_e2e.rs`

**Interfaces:**
- Consumes: `SingleUseLifecycle::with_force_routing`, `WorkerLifecycleManager::acquire`, `WorkerHandle::worker_mut`, `ForceRoutingConfig::new`, `DecisionSinkFactory`, `EgressAuditRow`, `BrokerConfigs`/`BrokerConfig`/`BrokerKind`, `web_research_firecracker_broker_entry`, `dispatch`, `Vault`, `SandboxBackends`, and the tests-common skip/PG/binary helpers.
- Produces: nothing consumed downstream (leaf e2e).

- [ ] **Step 1: Write the e2e (it is `#[ignore]`, so it only runs on the DGX)**

Create `core/tests/web_research_vm_force_route_daemon_e2e.rs`:

```rust
//! #448 — live manager-level proof that the DAEMON force-routes a VM
//! `Net::Allowlist` worker through a HOST egress sidecar + HOST embed broker.
//!
//! Unlike `web_research_firecracker_broker_e2e.rs` (which hand-assembles a
//! `NetWorkerSpawn` with two explicit backends), this drives the real
//! `SingleUseLifecycle::with_force_routing(...).acquire(...)` path: the manager
//! itself resolves the worker backend from `entry.sandbox_backend =
//! Some(FirecrackerVm)` and the sidecar/broker backend from
//! `SandboxBackends::resolve(None, None)` (host default). It is strictly
//! stronger — it proves the daemon's own resolution, not a test fixture's.
//!
//! DGX-only (`#[ignore]`): real KVM + vsock + web-research rootfs + egress
//! proxy + embed broker + live SearxNG + live Ollama (embeddinggemma). Asserts
//! `ranking == "hybrid"` (embed rode vsock 1026 to the host broker) AND the
//! embed host never appears in an egress decision (zero embed egress).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use kastellan_core::broker::{BrokerConfig, BrokerConfigs, BrokerKind};
use kastellan_core::egress::audit::EgressAuditRow;
use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::dispatch;
use kastellan_core::worker_lifecycle::force_route::{DecisionSinkFactory, ForceRoutingConfig};
use kastellan_core::worker_lifecycle::{SingleUseLifecycle, WorkerLifecycleManager};
use kastellan_core::workers::web_research::web_research_firecracker_broker_entry;
use kastellan_sandbox::SandboxBackends;
use kastellan_tests_common::{
    bring_up_pg_cluster, egress_proxy_bin_or_skip, pg_bin_dir_or_skip, skip_if_no_microvm,
    skip_if_no_supervisor, skip_if_sandbox_unavailable, unique_suffix, workspace_target_binary,
};

const DEFAULT_SEARX_ENDPOINT: &str = "http://127.0.0.1:8888/search";
const DEFAULT_EMBED_ENDPOINT: &str = "http://127.0.0.1:11434/v1/embeddings";

fn image_dir() -> String {
    std::env::var("KASTELLAN_MICROVM_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "/var/lib/kastellan/microvm".to_string())
}

fn url_host(endpoint: &str) -> String {
    url::Url::parse(endpoint)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_else(|| "127.0.0.1".to_string())
}

async fn probe_and_pool(conn_spec: &kastellan_db::conn::ConnectSpec) -> sqlx::PgPool {
    kastellan_db::probe::run(conn_spec).await.expect("probe run");
    kastellan_db::pool::connect_runtime_pool(conn_spec)
        .await
        .expect("connect runtime pool")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "DGX-only: real KVM + vsock + web-research rootfs + egress proxy + \
            embed broker + live SearxNG + live embeddinggemma. Drives the real \
            SingleUseLifecycle::acquire path for a VM web-research worker; \
            asserts hybrid ranking with the embed host absent from egress."]
async fn daemon_force_routes_vm_web_research_through_host_sidecar_and_broker() {
    if skip_if_no_microvm() || skip_if_no_supervisor() || skip_if_sandbox_unavailable() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return };
    let Some(proxy_bin) = egress_proxy_bin_or_skip() else { return };

    // VM worker runs from the rootfs-baked path; the broker is a host binary.
    let worker_in_guest = "/usr/local/bin/kastellan-worker-web-research";
    let broker_bin = workspace_target_binary("kastellan-worker-embed-broker");
    if !broker_bin.exists() {
        eprintln!("\n[SKIP] embed-broker binary not built; run cargo build --workspace\n");
        return;
    }

    let searx_endpoint = std::env::var("KASTELLAN_WEB_RESEARCH_ENDPOINT")
        .unwrap_or_else(|_| DEFAULT_SEARX_ENDPOINT.to_string());
    let embed_endpoint = std::env::var("KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT")
        .unwrap_or_else(|_| DEFAULT_EMBED_ENDPOINT.to_string());

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "vmfr-d",
        "vmfr-l",
        &format!("kastellan-supervisor-test-pg-vmfr-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    // Content allowlist: SearxNG endpoint host + one content host. NOT the embed
    // host — its only path is the broker over vsock 1026.
    let allowlist = vec![url_host(&searx_endpoint), "en.wikipedia.org".to_string()];

    // The VM broker-mode manifest entry (sandbox_backend = Some(FirecrackerVm),
    // broker = Some(Embed), embed host absent from Net::Allowlist).
    let entry = web_research_firecracker_broker_entry(
        PathBuf::from(worker_in_guest),
        image_dir(),
        &searx_endpoint,
        &embed_endpoint,
        None, // default embed model (embeddinggemma)
        &allowlist,
    );

    // Capture every egress decision so we can assert zero embed egress.
    let decisions: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_src = Arc::clone(&decisions);
    let make_sink: DecisionSinkFactory = Box::new(move || {
        let d = Arc::clone(&sink_src);
        Box::new(move |row: EgressAuditRow| {
            d.lock().unwrap().push(format!("{} {}", row.action, row.payload));
        })
    });
    let force = Arc::new(ForceRoutingConfig::new(
        proxy_bin,
        std::env::temp_dir(),
        make_sink,
        None, // no cert pins
    ));

    // Real host embed-broker config (scratch under /tmp so the VMM jail can bind
    // its UDS and the vsock-1026 relay can reach it).
    let broker_configs = BrokerConfigs {
        embed: Some(Arc::new(BrokerConfig::new(
            BrokerKind::Embed,
            broker_bin,
            std::env::temp_dir(),
        ))),
        ..Default::default()
    };

    // The real production manager. It resolves the worker backend from
    // entry.sandbox_backend (FirecrackerVm) AND the sidecar/broker backend from
    // resolve(None, None) (host bwrap) — the #448 behaviour under test.
    let sandboxes = Arc::new(SandboxBackends::default_for_current_os());
    let mgr = SingleUseLifecycle::with_force_routing(sandboxes, Some(force), broker_configs);

    let mut handle = mgr
        .acquire("web-research", &entry)
        .await
        .expect("acquire a force-routed VM web-research worker through the manager");

    let result = dispatch(
        &pool,
        &Vault::new(),
        handle.worker_mut(),
        "web-research",
        "web.research",
        serde_json::json!({"query": "rust programming language", "max_sources": 2}),
    )
    .await
    .expect("web.research round trip through the daemon-managed VM worker");

    // Print decisions for diagnosability on failure.
    for line in decisions.lock().unwrap().iter() {
        eprintln!("[egress-decision] {line}");
    }

    assert_eq!(
        result["ranking"], "hybrid",
        "expected hybrid ranking via the host broker over vsock 1026 (embed host absent from egress)"
    );

    // Zero embed egress: the embed host must never appear in an egress decision.
    let embed_host = url_host(&embed_endpoint);
    let leaked: Vec<_> = decisions
        .lock()
        .unwrap()
        .iter()
        .filter(|d| d.contains(&embed_host))
        .cloned()
        .collect();
    assert!(
        leaked.is_empty(),
        "embed host {embed_host} must be absent from egress decisions; leaked: {leaked:?}"
    );

    let _ = handle.worker_mut().kill();
}
```

- [ ] **Step 2: Verify it compiles + skip-passes on the Mac (no VM there)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --test web_research_vm_force_route_daemon_e2e -- --nocapture 2>&1 | tail -20`
Expected: the test is `#[ignore]` (not run by default) and the compile succeeds. Then confirm it skips cleanly when invoked: `cargo test -p kastellan-core --test web_research_vm_force_route_daemon_e2e -- --ignored --nocapture 2>&1 | tail -20` → prints a `[SKIP]` line (`skip_if_no_microvm`/no PG on the Mac) and exits 0. If any import path is wrong, fix it (e.g. `EgressAuditRow` / `DecisionSinkFactory` re-export paths) and re-run.

- [ ] **Step 3: Clippy the test crate**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -15`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add core/tests/web_research_vm_force_route_daemon_e2e.rs
git commit -m "test(#448): DGX manager-level e2e — daemon force-routes a VM worker

Drives the real SingleUseLifecycle::with_force_routing(...).acquire(...) path
for a VM web-research worker and asserts hybrid ranking with the embed host
absent from egress. Stronger than the #445 hand-assembled NetWorkerSpawn e2e:
it proves the daemon's own resolve(Some(FirecrackerVm)) + resolve(None,None)
selection. #[ignore] — runs on the DGX only.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: DGX live verification + full-workspace gate + baseline

**Files:** none (verification only; baselines recorded in HANDOVER at session end).

**Interfaces:** none.

- [ ] **Step 1: Push the branch so the DGX can fetch it (or rsync per local convention)**

Run: `git push -u origin feat/daemon-vm-force-routing 2>&1 | tail -5`
Expected: branch pushed.

- [ ] **Step 2: DGX — sync the branch + rebuild the workspace, release launcher, and web-research rootfs**

Run (single `ssh dgx '<cmd>'` invocations — the `Bash(ssh dgx *)` allow rule is a prefix match, so no flags before the hostname):

```bash
ssh dgx 'cd ~/src/kastellan && git fetch origin && git checkout feat/daemon-vm-force-routing && git reset --hard origin/feat/daemon-vm-force-routing && source ~/.cargo/env && cargo build --workspace 2>&1 | tail -5'
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo build --release -p kastellan-microvm-run 2>&1 | tail -3'
ssh dgx 'cd ~/src/kastellan && export PATH=$HOME/.local/bin:$PATH && bash scripts/workers/microvm/build-web-research-rootfs.sh 2>&1 | tail -5'
```
Expected: workspace + release launcher build clean; `web-research.ext4` (re)built under `/var/lib/kastellan/microvm/`.

- [ ] **Step 3: DGX — run the new manager-level e2e (real KVM + vsock + live SearxNG + Ollama)**

Run:
```bash
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && export PATH=$HOME/.local/bin:$PATH && setsid bash -lc "cargo test -p kastellan-core --test web_research_vm_force_route_daemon_e2e -- --ignored --nocapture > ~/vmfr-e2e.log 2>&1; echo DONE_EXIT=\$? >> ~/vmfr-e2e.log" </dev/null & echo launched'
```
Then poll: `ssh dgx 'tail -30 ~/vmfr-e2e.log; grep -c DONE_EXIT ~/vmfr-e2e.log'` until `DONE_EXIT=` appears.
Expected: `DONE_EXIT=0`, `test result: ok. 1 passed`, `[egress-decision]` lines showing SearxNG + wikipedia CONNECTs and NO `127.0.0.1:11434`, and `ranking == "hybrid"` (no assertion panic). (Logs go to `~` not `/tmp` — a workspace run scrubs `/tmp` mid-run.)

- [ ] **Step 4: DGX — full-workspace `cargo test` + clippy for the new baseline**

Run:
```bash
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && export PATH=$HOME/.local/bin:$PATH && setsid bash -lc "cargo test --workspace > ~/dgx-wf.log 2>&1; echo DONE_EXIT=\$? >> ~/dgx-wf.log" </dev/null & echo launched'
```
Poll `ssh dgx 'tail -20 ~/dgx-wf.log; grep DONE_EXIT ~/dgx-wf.log'` until done, then:
```bash
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -15'
```
Expected: `DONE_EXIT=0`; the aggregate `test result` line reads roughly **2514 + new unit tests (2) passed / 0 failed / 44 ignored** (was 2514/0/43; +2 passed unit tests, +1 ignored e2e), 0 `[SKIP]` beyond the gliner-relex gated ones; clippy clean. Record the exact numbers for HANDOVER.

- [ ] **Step 5: Mac — full-workspace confirmation (host path byte-identical)**

Run: `source "$HOME/.cargo/env" && cargo test --workspace 2>&1 | tail -15 && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -10`
Expected: green (use skip-as-pass for the whole workspace on the Mac per the standing PG-flake gotcha); clippy clean. The +2 new force_route unit tests pass; the new e2e is `#[ignore]` (skips).

- [ ] **Step 6: No commit** — verification only. (Session-end HANDOVER/ROADMAP updates + PR happen after this task, outside the plan, per the project's session checklist.)

---

## Self-Review

**1. Spec coverage:**
- Approach A (sidecar/broker on `resolve(None,None)`, no is-VM branch) → Tasks 1-3. ✓
- Both egress sidecar AND embed broker move to host backend → Task 1 (sidecar) + Task 2 (broker, `spawn_broker` at L318). ✓
- Managers resolve host default → Task 3 (both facades + `acquire_impl`). ✓
- Byte-identical host path → enforced in every task (callers pass same backend; existing tests keep assertions). ✓
- Mac recording-backend unit tests → Task 1 + Task 2. ✓
- DGX manager-level `#[ignore]` e2e driving `SingleUseLifecycle::acquire` → Task 4. ✓
- Verification (Mac + DGX full-workspace + clippy + baseline) → Task 5. ✓
- No new env flag → honored (nothing added). ✓
- No sandbox change → honored (all edits in `core`). ✓

**2. Placeholder scan:** No TBD/TODO/"handle edge cases"/"similar to Task N". All code blocks are complete. Task 3 Step 2 instructs reading the exact `acquire_impl` call-site arg list before inserting — this is a deliberate precision guard (the surrounding args are stable but long), not a placeholder; the inserted value (`sidecar_backend.as_ref()`) and position (right after the backend arg) are exact.

**3. Type consistency:** `sidecar_backend: &dyn SandboxBackend` is used consistently across Tasks 1-3. `spawn_worker_maybe_forced` = 5 args (Task 1); `spawn_worker_with_optional_broker` = 7 args (Task 2); `acquire_impl` gains `sidecar_backend` right after `sandbox` (Task 3). `RecordingBackend { label, calls }` defined in Task 1, reused in Task 2. `DecisionSinkFactory`, `EgressAuditRow`, `ForceRoutingConfig::new`, `BrokerConfigs { embed, search }`, `web_research_firecracker_broker_entry(binary, image_dir, endpoint, embed_endpoint, embed_model, allowlist)`, `dispatch(pool, vault, worker, tool, method, params)`, `handle.worker_mut()` — all match the signatures verified in the source.
