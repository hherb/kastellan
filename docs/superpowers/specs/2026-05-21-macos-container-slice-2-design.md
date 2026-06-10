# MacosContainer Slice 2 — per-worker backend selection

**Date:** 2026-05-21
**Issue:** [#55](https://github.com/hherb/kastellan/issues/55) (parent)
**Predecessor slice:** [`2026-05-21-macos-container-spike-notes.md`](2026-05-21-macos-container-spike-notes.md) (spike) + PR #106 (Slice 1, merged at `cc0b0de`)
**Scope:** 0.5–1 session — pure plumbing, no new image-build pipeline.

## Context

Slice 1 (merged at `cc0b0de`) shipped `MacosContainer: SandboxBackend` as a sibling to `MacosSeatbelt`. The sibling is **opt-in per worker**, not the platform default: `kastellan_sandbox::default_backend()` on darwin still returns `MacosSeatbelt`. Slice 2 closes that opt-in seam — without it, the `MacosContainer` backend is dead code: nothing in the workspace can actually route a worker through it.

The driving use case is the Phase 4 macOS gap: `MacosSeatbelt` has no memory primitive (`mem_mb` is silently ignored on darwin), but `MacosContainer` does (`-m <N>M` with SIGKILL on overrun). Workers like `gliner-relex` (PyTorch — easily 600 MiB+ resident) and the future `python-exec` need real memory enforcement on macOS. Slice 1 proved `MacosContainer` works; Slice 2 lets specific workers opt in to it while leaving the lightweight `shell-exec` path on Seatbelt for the 50× spawn-latency advantage (~50 ms Seatbelt vs ~760 ms warm Container).

## Scope

### In scope

- New `SandboxBackendKind` enum in `kastellan_sandbox`, cfg-gated per-OS so mis-config is a compile-time error.
- New `SandboxBackends` struct + `default_for_current_os()` constructor + `resolve(kind) -> Arc<dyn SandboxBackend>` resolver.
- New `sandbox_backend: Option<SandboxBackendKind>` field on `ToolEntry`. `None` defaults to the per-OS backend (current behaviour).
- `SingleUseLifecycle` + `IdleTimeoutLifecycle` switch from holding `Arc<dyn SandboxBackend>` to holding `Arc<SandboxBackends>`; `acquire` resolves the entry's backend kind per call.
- Daemon `main.rs` builds `SandboxBackends::default_for_current_os()` once at startup and threads it into the lifecycle managers.
- Unit tests on the enum + resolver.
- Unit test on `ToolEntry` widening.
- Counter-backend test that pins the lifecycle-manager routing.
- End-to-end smoke test in `core/tests/` that spawns `alpine sh` through the full chain (ToolEntry → SingleUseLifecycle → MacosContainer → real `container run`), skip-as-pass when container/image are missing.

### Out of scope (deferred)

- **`gliner-relex` migration to the container backend.** Slice 2.5 owns this — needs a `Containerfile` + operator-runnable `container build` step + image-tag config on the manifest. Slice 2's smoke uses plain `alpine` to avoid the image-build dependency.
- **JSON-RPC end-to-end through the container.** No tiny JSON-RPC-speaking worker binary is cross-compiled in this slice. The smoke spawns `alpine sh -c 'echo ok'`, which proves the spawn-through-lifecycle chain reaches the container backend; the JSON-RPC layer was already proven separately by Slice 1's `macos_container_smoke.rs::echo_runs_inside_container`. Re-proving it would duplicate coverage.
- **Issue #107 (--init / PID-1 / signal-handling).** Per the issue thread, the concern is real only for long-lived `IdleTimeoutLifecycle` workers. Slice 2's smoke is short-lived (single `echo`); Slice 2.5 is the natural place to address it alongside `gliner-relex`'s migration.
- **Linux backend extension.** The `Bwrap` enum variant is defined for symmetry and so future work (e.g. an experimental Firecracker backend on Linux) has a precedent shape, but there is no behaviour change on Linux in this slice. `default_for_current_os()` on Linux returns a `SandboxBackends` holding only `Bwrap`.
- **Operator CLI for swapping a worker's backend at runtime.** Backends are declared in the `ToolEntry` literal (e.g. `shell_exec_entry`, future `gliner_relex_entry`), not configured by `kastellan-cli` at runtime. If runtime swapping becomes a real operator need, that's a separate slice.

## Design

### Types

#### `SandboxBackendKind` (new, in `kastellan_sandbox`)

```rust
/// Operator-facing identifier for selecting a specific sandbox backend
/// per-worker. Cfg-gated per-OS so cross-OS mis-config (e.g. declaring
/// `Container` on Linux) is a compile-time error rather than a runtime
/// surprise.
///
/// `None` on a `ToolEntry.sandbox_backend` means "use the per-OS default"
/// — today darwin → Seatbelt, linux → Bwrap. Only opt in here when a
/// worker has a concrete reason to diverge (e.g. needs memory enforcement
/// on macOS, which Seatbelt can't provide).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SandboxBackendKind {
    #[cfg(target_os = "linux")]
    Bwrap,
    #[cfg(target_os = "macos")]
    Seatbelt,
    #[cfg(target_os = "macos")]
    Container,
}
```

**Trade-off considered:** an abstract `MicroVm` variant (which would map to `Container` on darwin and a future `Firecracker` on Linux) was rejected for now — it conflates two backends with quite different startup latency + image-build stories, and "I want memory enforcement on macOS specifically" is the only concrete use case today. If a Linux micro-VM backend ships later and operators want a single declarative knob across platforms, that's a future enum variant.

#### `SandboxBackends` (new, in `kastellan_sandbox`)

```rust
/// Per-OS bundle of constructed sandbox backends, used by the lifecycle
/// managers to resolve a per-worker `SandboxBackendKind` to a concrete
/// `Arc<dyn SandboxBackend>`.
///
/// Fields are cfg-gated to match `SandboxBackendKind`. The struct is
/// constructed once at daemon startup via `default_for_current_os()`
/// (cheap — backends hold no mutable state) and threaded through the
/// lifecycle managers as `Arc<SandboxBackends>`.
pub struct SandboxBackends {
    #[cfg(target_os = "linux")]
    bwrap: Arc<dyn SandboxBackend>,
    #[cfg(target_os = "macos")]
    seatbelt: Arc<dyn SandboxBackend>,
    #[cfg(target_os = "macos")]
    container: Arc<dyn SandboxBackend>,
}

impl SandboxBackends {
    pub fn default_for_current_os() -> Self { ... }

    /// Resolve a per-worker backend kind to a concrete backend.
    /// `None` returns the per-OS default (linux → Bwrap, darwin → Seatbelt).
    pub fn resolve(&self, kind: Option<SandboxBackendKind>) -> Arc<dyn SandboxBackend> { ... }
}
```

Cfg-gating makes `resolve` total: every variant of `SandboxBackendKind` that exists at compile time has a backing field. No runtime panic path for "unknown variant."

For testability: `SandboxBackends::with_backends_for_test(...)` constructor under `#[cfg(test)]` (or behind a `pub(crate)` boundary depending on call-site needs) lets tests inject counter-backends to verify routing.

#### `ToolEntry` widening (in `core/src/scheduler/tool_dispatch.rs`)

Adds one field:

```rust
pub struct ToolEntry {
    pub binary: PathBuf,
    pub policy: SandboxPolicy,
    pub wall_clock_ms: Option<u64>,
    pub lifecycle: crate::worker_lifecycle::Lifecycle,
    /// Per-worker sandbox backend opt-in. `None` (current default for all
    /// shipping tools) uses the per-OS default; `Some(K)` requests a
    /// specific backend, validated at compile time by the cfg-gated enum.
    /// Slice 2.5 will set `Some(SandboxBackendKind::Container)` on the
    /// `gliner-relex` manifest to opt that worker into macOS memory
    /// enforcement.
    pub sandbox_backend: Option<SandboxBackendKind>,
}
```

`shell_exec_entry` (the only shipping constructor) defaults the new field to `None`. All existing tests + integration callers continue to work bit-for-bit.

### Data flow

```
daemon startup
└─► kastellan_sandbox::SandboxBackends::default_for_current_os()
    └─► Arc<SandboxBackends>
        └─► SingleUseLifecycle::new(Arc<SandboxBackends>)
        └─► IdleTimeoutLifecycle::new(Arc<SandboxBackends>)

step dispatch (one per JSON-RPC request)
└─► ToolHostStepDispatcher::dispatch_step
    └─► lifecycle_manager.acquire(tool_name, &entry)
        └─► sandbox_backends.resolve(entry.sandbox_backend)
        └─► Arc<dyn SandboxBackend>
        └─► spawn_worker(&*backend, &worker_spec)
```

The resolution happens **per acquire call**, not once at construction. This costs a `HashMap`-free struct-field lookup + `Arc::clone` — negligible (nanoseconds). The benefit: a future hot-reload of the registry (e.g. operator-CLI-driven manifest update) doesn't need to rebuild the lifecycle managers.

### Lifecycle-manager changes

`SingleUseLifecycle` and `IdleTimeoutLifecycle` are the only consumers of `Arc<dyn SandboxBackend>` today. Both store it as a struct field:

```rust
pub struct SingleUseLifecycle {
    sandbox: Arc<dyn SandboxBackend>,  // before
}

pub struct SingleUseLifecycle {
    sandboxes: Arc<SandboxBackends>,  // after
}
```

In each `acquire` impl, replace `self.sandbox.as_ref()` with `self.sandboxes.resolve(entry.sandbox_backend)` and dereference. The change is mechanical; the resolution lives inside the lifecycle managers (not in `tool_host::spawn_worker`) because that's where the per-entry knowledge already flows.

The `IdleTimeoutLifecycle` impl pulls warm workers from a per-tool-name slot cache. Backend resolution happens at first-spawn time; once a worker is warm, repeat acquires hit the cache without re-resolving. If a future operator changes a tool's `sandbox_backend` between daemon runs, the slot cache empties at startup so the new selection takes effect (the cache is in-process, not persistent — already true).

### Daemon main.rs change

One-line swap:

```rust
// before
let sandbox: Arc<dyn SandboxBackend> = Arc::from(kastellan_sandbox::default_backend());
let manager = SingleUseLifecycle::new(sandbox);

// after
let sandboxes = Arc::new(kastellan_sandbox::SandboxBackends::default_for_current_os());
let manager = SingleUseLifecycle::new(sandboxes);
```

`kastellan_sandbox::default_backend()` is kept (not deprecated, not removed) for direct-spawn callers like `tests-common::sandbox::backend()` that don't need per-entry selection.

## Tests

Test additions, in TDD order:

### Unit (sandbox crate)

1. **`sandbox_backends_default_for_current_os_returns_per_os_default_on_resolve_none`** — `SandboxBackends::default_for_current_os().resolve(None)` returns a backend; on darwin it's the Seatbelt slot; on linux it's the Bwrap slot. Pinned via type-id round-trip or via a probe call (whichever is cleaner).
2. **`sandbox_backends_resolve_returns_container_when_requested_on_darwin`** — darwin-gated; `resolve(Some(Container))` returns the container slot. Pin Arc-pointer identity to the `container` field.
3. **`sandbox_backends_resolve_returns_seatbelt_when_requested_on_darwin`** — darwin-gated; `resolve(Some(Seatbelt))` returns the seatbelt slot. Symmetric pin.
4. **`sandbox_backends_resolve_returns_bwrap_when_requested_on_linux`** — linux-gated; `resolve(Some(Bwrap))` returns the bwrap slot.

### Unit (core: ToolEntry widening)

5. **`tool_entry_sandbox_backend_defaults_to_none`** — `shell_exec_entry(...).sandbox_backend == None`. Catches anyone accidentally hard-coding the new field.

### Unit (core: lifecycle manager routing)

6. **`single_use_lifecycle_acquire_routes_via_entry_sandbox_backend_kind`** — constructs `SandboxBackends::with_backends_for_test(...)` from two counter-backends (Seatbelt-slot + Container-slot), builds a `ToolEntry { sandbox_backend: Some(Container), ... }`, calls `acquire`, asserts the container counter ticked + the seatbelt counter did not. Symmetric assertion for `sandbox_backend: None`.

### Integration smoke (new file: `core/tests/lifecycle_container_routing_e2e.rs`)

7. **`single_use_lifecycle_routes_through_real_container_when_entry_opts_in`** — darwin-gated; skip-as-pass when `container --version` / `container system status` / `alpine:3.20` are missing. End-to-end check that the *entry → resolve → spawn* chain wired through `SingleUseLifecycle::acquire` actually reaches `MacosContainer`. Two halves:

   - **Positive:** build a `ToolEntry { sandbox_backend: Some(Container), binary: PathBuf::from("/sbin/apk"), wall_clock_ms: Some(5_000), policy: <minimal>, lifecycle: SingleUse }`. `/sbin/apk` is alpine-specific (Alpine's package manager) — it exists inside the `alpine:3.20` image but is absent on macOS. Call `SingleUseLifecycle::new(Arc::new(SandboxBackends::default_for_current_os())).acquire("test", &entry).await`. The container backend wraps argv as `container run --rm -i alpine:3.20 /sbin/apk` — apk exists, spawn succeeds. Assert `acquire` returns `Ok`, then `worker_mut().kill()` for cleanup.
   - **Negative control (same test or sibling):** identical entry but with `sandbox_backend: None`, which resolves to `MacosSeatbelt`. The seatbelt-exec call is `sandbox-exec -p <profile> -- /sbin/apk` — `/sbin/apk` doesn't exist on the host, so `Command::spawn` fails. Assert `acquire` returns `Err`. Together the two halves prove the selection bit actually changes the resolved backend.

   `Client::from_child` is constructed by `spawn_worker` but doesn't read until the first `call()`, so wrapping a non-JSON-RPC `/sbin/apk` invocation works fine. The worker is killed before any RPC call.

### Test count delta

macOS 901 → ~908 (+7): 4 sandbox + 1 core widening + 1 lifecycle routing + 1 integration smoke.
Linux DGX ~897 → ~899 (+2): the `tool_entry_sandbox_backend_defaults_to_none` test + the Bwrap resolve test (both cross-platform under cfg gates).

## File-size watch

- `sandbox/src/lib.rs`: 239 → ~330 LOC with the new types + helpers + tests. Well under the 500-LOC soft cap.
- `core/src/scheduler/tool_dispatch.rs`: currently ~700 LOC (already over the cap, on the deferred-refactor list). +5–10 LOC for the new field + its doc comment is negligible relative to the existing breach; this slice doesn't trigger a refactor.
- `core/src/worker_lifecycle/manager.rs`: 343 → ~360 LOC. Under cap.
- New file `core/tests/lifecycle_container_routing_e2e.rs`: ~150 LOC. Under cap.

## Risks + mitigations

- **Cfg-gating ripples across modules.** The enum + struct fields + lifecycle-manager fields all need consistent `#[cfg(...)]` attributes. Mitigation: define the enum + struct in one module with co-located cfg gates; lifecycle managers only see the abstract `Arc<SandboxBackends>` and never reach inside, so their code doesn't need per-OS gates.
- **Test-only constructor leak.** `SandboxBackends::with_backends_for_test` exposed beyond `#[cfg(test)]` would let production code build inconsistent backends. Mitigation: gate with `#[cfg(test)]` if all callers are in the same crate; use `pub(crate)` + a `#[doc(hidden)]` warning if cross-crate-test access is needed.
- **Counter-backend impl drift.** A test backend that increments a counter on `spawn_under_policy` could mask real `SandboxBackend` trait shape changes if the trait grows new methods. Mitigation: keep the test backend minimal; rely on the trait's `Send + Sync` + one-method shape (current contract).
- **Issue #107 surface.** The Slice 2 smoke deliberately doesn't exercise long-lived workers, so PID-1 / signal-handling stays invisible. Mitigation: explicit "out of scope" callout in the spec; cross-reference Issue #107 in Slice 2.5's spec when it's written.
- **`tests-common::sandbox::backend()` divergence.** This helper returns a single `Box<dyn SandboxBackend>` for direct-spawn tests. Not updated in Slice 2 because no test today builds a `ToolEntry { sandbox_backend: Some(Container) }` and goes through the lifecycle manager for the existing direct-spawn coverage. Mitigation: documented (in code) that direct-spawn tests don't exercise per-entry selection; daemon-backed tests (which use the production `SandboxBackends`) do.

## Rollback

If Slice 2 turns out to be a wrong abstraction (e.g. operators report wanting runtime swapping after all), the rollback is: revert `feat/macos-container-backend-slice-2`. The `MacosContainer` backend (Slice 1) and `default_backend()` (today's single-backend selector) stay green untouched. No migrations, no data model changes; revert is a clean `git revert`.

## Open questions

None blocking. The smoke-test shape was clarified in the brainstorming round (routing + end-to-end alpine spawn through the lifecycle manager). Issue #107 (--init / PID-1) is explicitly deferred.

## Future slices (preview, not in this scope)

- **Slice 2.5 (1 session):** `workers/gliner-relex/Containerfile` (Python 3.12 + uv sync + weights mount) + operator-runnable `container build -t kastellan/gliner-relex:dev workers/gliner-relex/` step. Update `gliner_relex_entry()` to set `sandbox_backend: Some(SandboxBackendKind::Container)` + image tag. Re-run e2e on macOS; confirm canonical `Dr Smith --[treats]--> asthma (0.994)` output through the container. **This is where Issue #107 (PID-1 / signal-handling) must be addressed** before `IdleTimeoutLifecycle` keeps gliner-relex warm.
- **Slice 3 (deferred to Phase 4):** `python-exec` worker — defaults to container on macOS for sandboxed user-Python execution.
