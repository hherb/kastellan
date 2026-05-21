# MacosContainer Slice 2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add per-worker sandbox-backend selection so a `ToolEntry` can opt into `MacosContainer` instead of the per-OS default (Seatbelt on darwin, Bwrap on linux).

**Architecture:** New cfg-gated `SandboxBackendKind` enum + `SandboxBackends` struct in `hhagent_sandbox`; new optional `sandbox_backend` field on `ToolEntry`; lifecycle managers switch from `Arc<dyn SandboxBackend>` to `Arc<SandboxBackends>` and resolve per-call. No `gliner-relex` migration in this slice — that's Slice 2.5.

**Tech Stack:** Rust workspace (sandbox + core crates), tokio async, async-trait, Apple `container` 0.12.3 (darwin only, via `MacosContainer` from Slice 1).

**Spec:** [`docs/superpowers/specs/2026-05-21-macos-container-slice-2-design.md`](../specs/2026-05-21-macos-container-slice-2-design.md).

---

## File structure

**Modified:**
- `sandbox/src/lib.rs` (226 → ~330 LOC): add `SandboxBackendKind` + `SandboxBackends` + their tests
- `core/src/scheduler/tool_dispatch.rs` (748 LOC, already over-cap): add `sandbox_backend` field to `ToolEntry`; update `shell_exec_entry`; update inline `fake_entry` test fixture
- `core/src/workers/gliner_relex.rs`: add `sandbox_backend: None` to `gliner_relex_entry`
- `core/src/worker_lifecycle/manager.rs` (342 → ~390 LOC): `SingleUseLifecycle.sandbox` → `sandboxes: Arc<SandboxBackends>`; mirror for `IdleTimeoutLifecycle`; add counter-backend routing test
- `core/src/worker_lifecycle/composite.rs`: `CompositeLifecycle::new` + `with_backoff` switch to `Arc<SandboxBackends>`; update inline tests
- `core/src/worker_lifecycle/idle_timeout.rs`: `acquire_impl` signature flips from `sandbox: &dyn SandboxBackend` to receiving the already-resolved `Arc<dyn SandboxBackend>` from the caller
- `core/src/main.rs`: daemon swaps `Arc::from(hhagent_sandbox::default_backend())` → `Arc::new(SandboxBackends::default_for_current_os())`
- `core/tests/scheduler_step_dispatch_e2e.rs`: update `ToolEntry { ... }` literal + lifecycle-manager construction
- `core/tests/worker_lifecycle_idle_timeout_e2e.rs`: update `ToolEntry { ... }` literal + 5 lifecycle-manager constructions
- `core/tests/gliner_relex_e2e.rs`: update 3 lifecycle-manager constructions
- `core/tests/entity_extraction_e2e.rs`: update 2 `CompositeLifecycle::new(sandbox)` constructions
- `core/tests/memory_entity_link_e2e.rs`: update 1 `CompositeLifecycle::new(sandbox)` construction

**Created:**
- `core/tests/lifecycle_container_routing_e2e.rs` (~150 LOC): positive + negative integration smoke through `SingleUseLifecycle::acquire`

**Unchanged (intentional):**
- `sandbox/src/lib.rs::default_backend()`: kept for direct-spawn callers (`tests-common::sandbox::backend()`)
- `tests-common/src/sandbox.rs`: not used by daemon-backed tests; needs no update for this slice
- `sandbox/src/macos_container.rs`, `sandbox/src/macos_seatbelt.rs`, `sandbox/src/linux_bwrap.rs`: backend implementations unchanged

---

### Task 0: Create feature branch

**Files:** none (git only)

- [ ] **Step 1: Branch off main**

  ```bash
  cd /Users/hherb/src/hhagent
  source "$HOME/.cargo/env"
  git checkout main && git pull --ff-only
  git checkout -b feat/macos-container-backend-slice-2
  ```

- [ ] **Step 2: Sanity-check baseline build + tests**

  ```bash
  cargo test --workspace --no-fail-fast 2>&1 | grep "test result:" | awk '{for(i=1;i<=NF;i++){if($i=="passed;"){p+=$(i-1)}else if($i=="failed;"){f+=$(i-1)}else if($i=="ignored;"){ig+=$(i-1)}}} END {print "Aggregate — passed:"p" failed:"f" ignored:"ig}'
  ```

  Expected: `Aggregate — passed:901 failed:0 ignored:3` (on macOS) or close to it. If it diverges, stop and investigate before changing anything.

---

### Task 1: Add `SandboxBackendKind` enum to `hhagent_sandbox`

**Files:**
- Modify: `sandbox/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

  Inside the existing `#[cfg(test)] mod tests` block at the bottom of `sandbox/src/lib.rs` (around line 184), add:

  ```rust
      /// `SandboxBackendKind` is Copy + Eq so it can be threaded through
      /// per-call dispatch without lifetime gymnastics. Cfg-gating means
      /// the variant set is OS-specific by design — cross-OS mis-config
      /// is a compile-time error rather than a runtime surprise.
      #[test]
      fn sandbox_backend_kind_is_copy_and_eq() {
          // Compile-time pin: any variant satisfies Copy + Eq.
          #[cfg(target_os = "linux")]
          {
              let a = SandboxBackendKind::Bwrap;
              let b = a;
              assert_eq!(a, b);
          }
          #[cfg(target_os = "macos")]
          {
              let a = SandboxBackendKind::Seatbelt;
              let b = a;
              assert_eq!(a, b);
              let c = SandboxBackendKind::Container;
              assert_ne!(a, c);
          }
      }
  ```

- [ ] **Step 2: Run test to verify it fails**

  ```bash
  cargo test -p hhagent-sandbox sandbox_backend_kind_is_copy_and_eq 2>&1 | tail -10
  ```

  Expected: compile error like `error[E0433]: failed to resolve: use of undeclared type 'SandboxBackendKind'`.

- [ ] **Step 3: Add the enum**

  Insert just before the existing `pub trait SandboxBackend` definition (around line 140 of `sandbox/src/lib.rs`):

  ```rust
  /// Operator-facing identifier for selecting a specific sandbox backend
  /// per-worker. Cfg-gated per-OS so cross-OS mis-config (e.g. declaring
  /// `Container` on Linux) is a compile-time error rather than a runtime
  /// surprise.
  ///
  /// `None` on a `ToolEntry.sandbox_backend` means "use the per-OS
  /// default" — today darwin → `Seatbelt`, linux → `Bwrap`. Only opt in
  /// here when a worker has a concrete reason to diverge (e.g. needs
  /// memory enforcement on macOS, which `Seatbelt` can't provide).
  ///
  /// See `docs/superpowers/specs/2026-05-21-macos-container-slice-2-design.md`
  /// for the rationale behind OS-specific variant names vs an abstract
  /// `MicroVm` category.
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

- [ ] **Step 4: Run test to verify it passes**

  ```bash
  cargo test -p hhagent-sandbox sandbox_backend_kind_is_copy_and_eq 2>&1 | tail -5
  ```

  Expected: `test result: ok. 1 passed; 0 failed; ...`.

- [ ] **Step 5: Commit**

  ```bash
  git add sandbox/src/lib.rs
  git commit -m "feat(sandbox): add SandboxBackendKind enum (cfg-gated per-OS variants)"
  ```

---

### Task 2: Add `SandboxBackends` struct + `default_for_current_os()` + `resolve()`

**Files:**
- Modify: `sandbox/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

  Append to the `#[cfg(test)] mod tests` block in `sandbox/src/lib.rs`:

  ```rust
      /// `resolve(None)` returns the per-OS default backend. The test pins
      /// pointer identity against the struct's own per-OS default slot —
      /// if a future refactor swaps the default to a different slot, this
      /// trips deliberately.
      #[test]
      fn sandbox_backends_resolve_none_returns_per_os_default() {
          let sbs = SandboxBackends::default_for_current_os();
          let got = sbs.resolve(None);
          #[cfg(target_os = "linux")]
          assert!(Arc::ptr_eq(&got, &sbs.bwrap));
          #[cfg(target_os = "macos")]
          assert!(Arc::ptr_eq(&got, &sbs.seatbelt));
      }

      #[cfg(target_os = "macos")]
      #[test]
      fn sandbox_backends_resolve_some_seatbelt_on_darwin() {
          let sbs = SandboxBackends::default_for_current_os();
          let got = sbs.resolve(Some(SandboxBackendKind::Seatbelt));
          assert!(Arc::ptr_eq(&got, &sbs.seatbelt));
      }

      #[cfg(target_os = "macos")]
      #[test]
      fn sandbox_backends_resolve_some_container_on_darwin() {
          let sbs = SandboxBackends::default_for_current_os();
          let got = sbs.resolve(Some(SandboxBackendKind::Container));
          assert!(Arc::ptr_eq(&got, &sbs.container));
      }

      #[cfg(target_os = "linux")]
      #[test]
      fn sandbox_backends_resolve_some_bwrap_on_linux() {
          let sbs = SandboxBackends::default_for_current_os();
          let got = sbs.resolve(Some(SandboxBackendKind::Bwrap));
          assert!(Arc::ptr_eq(&got, &sbs.bwrap));
      }
  ```

  The `Arc::ptr_eq` calls force the test module to import `Arc`. Add to the top of the test mod (or import at module scope):

  ```rust
  use std::sync::Arc;
  ```

- [ ] **Step 2: Run tests to verify they fail**

  ```bash
  cargo test -p hhagent-sandbox sandbox_backends 2>&1 | tail -10
  ```

  Expected: compile errors (`SandboxBackends`, `default_for_current_os`, `resolve` not found).

- [ ] **Step 3: Implement the struct + constructor + resolver**

  Add after the `SandboxBackendKind` enum (or just after `default_backend()` to keep similar code adjacent — around line 165 of `sandbox/src/lib.rs`):

  ```rust
  /// Per-OS bundle of constructed sandbox backends, used by the lifecycle
  /// managers to resolve a per-worker [`SandboxBackendKind`] to a
  /// concrete `Arc<dyn SandboxBackend>`.
  ///
  /// Fields are cfg-gated to match `SandboxBackendKind` — every variant
  /// of the enum that exists at compile time has a backing field, so
  /// [`SandboxBackends::resolve`] is total (no runtime panic path for
  /// "unknown variant").
  ///
  /// Constructed once at daemon startup via
  /// [`SandboxBackends::default_for_current_os`] (cheap — backends hold
  /// no mutable state) and threaded through the lifecycle managers as
  /// `Arc<SandboxBackends>`. Tests build a custom instance directly via
  /// struct-literal syntax with their own counter / stub backends.
  ///
  /// The struct keeps `Clone` so callers that thread it through async
  /// boundaries can copy the `Arc`s cheaply.
  #[derive(Clone)]
  pub struct SandboxBackends {
      #[cfg(target_os = "linux")]
      pub bwrap: Arc<dyn SandboxBackend>,
      #[cfg(target_os = "macos")]
      pub seatbelt: Arc<dyn SandboxBackend>,
      #[cfg(target_os = "macos")]
      pub container: Arc<dyn SandboxBackend>,
  }

  impl SandboxBackends {
      /// Construct the per-OS default bundle. On Linux this is a single
      /// `LinuxBwrap`; on darwin it is `MacosSeatbelt` (the per-OS
      /// default) plus a `MacosContainer` for opt-in workers. Cheap —
      /// each backend is a unit struct with no I/O at construction.
      pub fn default_for_current_os() -> Self {
          #[cfg(target_os = "linux")]
          {
              Self {
                  bwrap: Arc::new(linux_bwrap::LinuxBwrap::new()),
              }
          }
          #[cfg(target_os = "macos")]
          {
              Self {
                  seatbelt: Arc::new(macos_seatbelt::MacosSeatbelt::new()),
                  container: Arc::new(macos_container::MacosContainer::new()),
              }
          }
          #[cfg(not(any(target_os = "linux", target_os = "macos")))]
          {
              compile_error!("SandboxBackends::default_for_current_os requires linux or macos");
          }
      }

      /// Resolve a per-worker [`SandboxBackendKind`] to a concrete
      /// backend.
      ///
      /// `None` returns the per-OS default (linux → `bwrap`, darwin →
      /// `seatbelt`). `Some(K)` returns the matching field.
      ///
      /// The returned `Arc` is a cheap refcount bump; callers store it
      /// for the lifetime of one `acquire` call (single-use lifecycle)
      /// or one warm-slot fill (idle-timeout lifecycle).
      pub fn resolve(&self, kind: Option<SandboxBackendKind>) -> Arc<dyn SandboxBackend> {
          match kind {
              None => {
                  #[cfg(target_os = "linux")]
                  {
                      Arc::clone(&self.bwrap)
                  }
                  #[cfg(target_os = "macos")]
                  {
                      Arc::clone(&self.seatbelt)
                  }
              }
              #[cfg(target_os = "linux")]
              Some(SandboxBackendKind::Bwrap) => Arc::clone(&self.bwrap),
              #[cfg(target_os = "macos")]
              Some(SandboxBackendKind::Seatbelt) => Arc::clone(&self.seatbelt),
              #[cfg(target_os = "macos")]
              Some(SandboxBackendKind::Container) => Arc::clone(&self.container),
          }
      }
  }
  ```

  Add the `Arc` import at the top of `sandbox/src/lib.rs` (with the existing imports near line 20):

  ```rust
  use std::sync::Arc;
  ```

- [ ] **Step 4: Run tests to verify they pass**

  ```bash
  cargo test -p hhagent-sandbox sandbox_backends 2>&1 | tail -10
  ```

  Expected: `test result: ok. N passed; 0 failed; ...` where N is 2 on linux or 3 on darwin (the cfg-gated tests).

- [ ] **Step 5: Run full sandbox-crate tests + workspace build**

  ```bash
  cargo test -p hhagent-sandbox 2>&1 | grep "test result:"
  cargo build --workspace 2>&1 | tail -3
  ```

  Expected: all sandbox tests pass; workspace builds clean.

- [ ] **Step 6: Commit**

  ```bash
  git add sandbox/src/lib.rs
  git commit -m "feat(sandbox): SandboxBackends bundle + resolve(kind) per-OS resolver"
  ```

---

### Task 3: Add `sandbox_backend: Option<SandboxBackendKind>` to `ToolEntry` + cascade

This task cascades through every `ToolEntry { ... }` struct literal in the workspace. The whole cascade lands in one commit so the workspace stays green at every commit boundary.

**Files:**
- Modify: `core/src/scheduler/tool_dispatch.rs`
- Modify: `core/src/workers/gliner_relex.rs`
- Modify: `core/src/worker_lifecycle/manager.rs` (inline test fixture)
- Modify: `core/src/worker_lifecycle/composite.rs` (inline test fixtures)
- Modify: `core/tests/scheduler_step_dispatch_e2e.rs`
- Modify: `core/tests/worker_lifecycle_idle_timeout_e2e.rs`

- [ ] **Step 1: Write the failing test (in `tool_dispatch.rs::tests`)**

  In `core/src/scheduler/tool_dispatch.rs`, find the existing inline `#[cfg(test)] mod tests` block (around line 565). Add:

  ```rust
      /// `shell_exec_entry` defaults `sandbox_backend` to `None` so the
      /// shell-exec worker stays on the per-OS default backend
      /// (Seatbelt on darwin, Bwrap on linux). A future explicit opt-in
      /// to `Some(SandboxBackendKind::Container)` would be a deliberate
      /// audit-trail change.
      #[test]
      fn shell_exec_entry_defaults_sandbox_backend_to_none() {
          let entry = shell_exec_entry(
              std::path::PathBuf::from("/usr/bin/true"),
              &["true".to_string()],
          );
          assert_eq!(entry.sandbox_backend, None);
      }
  ```

- [ ] **Step 2: Run test to verify it fails**

  ```bash
  cargo test -p hhagent-core shell_exec_entry_defaults_sandbox_backend_to_none 2>&1 | tail -10
  ```

  Expected: compile error like `no field 'sandbox_backend' on type 'ToolEntry'`.

- [ ] **Step 3: Add the field to `ToolEntry`**

  In `core/src/scheduler/tool_dispatch.rs` (line 93–111), update the struct:

  ```rust
  #[derive(Clone, Debug)]
  pub struct ToolEntry {
      pub binary: PathBuf,
      pub policy: SandboxPolicy,
      pub wall_clock_ms: Option<u64>,
      pub lifecycle: crate::worker_lifecycle::Lifecycle,
      /// Per-worker sandbox-backend opt-in. `None` (current default for
      /// every shipping tool) uses the per-OS default backend (Seatbelt
      /// on darwin, Bwrap on linux). `Some(K)` requests a specific
      /// backend, validated at compile time by the cfg-gated enum.
      ///
      /// Slice 2.5 will set `Some(SandboxBackendKind::Container)` on
      /// the `gliner-relex` manifest to opt that worker into macOS
      /// memory enforcement (Seatbelt has no memory primitive). All
      /// other workers stay on `None` until they have a concrete
      /// reason to diverge.
      pub sandbox_backend: Option<hhagent_sandbox::SandboxBackendKind>,
  }
  ```

- [ ] **Step 4: Update `shell_exec_entry` to default the field**

  In the same file (around line 180), update the `ToolEntry { ... }` literal returned by `shell_exec_entry`:

  ```rust
      ToolEntry {
          binary,
          policy,
          wall_clock_ms: Some(30_000),
          lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
          sandbox_backend: None,
      }
  ```

- [ ] **Step 5: Update the inline `fake_entry` test fixture**

  In the same file (around line 570), update:

  ```rust
      fn fake_entry() -> ToolEntry {
          ToolEntry {
              binary: PathBuf::from("/usr/local/bin/fake"),
              policy: SandboxPolicy {
                  ..SandboxPolicy::default()
              },
              wall_clock_ms: Some(5_000),
              lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
              sandbox_backend: None,
          }
      }
  ```

- [ ] **Step 6: Update `gliner_relex_entry`**

  In `core/src/workers/gliner_relex.rs` (around line 221), find the `ToolEntry { ... }` literal and add the field:

  ```rust
      ToolEntry {
          binary,
          policy,
          wall_clock_ms: None,
          lifecycle: crate::worker_lifecycle::Lifecycle::IdleTimeout { ... },
          sandbox_backend: None,
      }
  ```

  (Preserve the exact existing `lifecycle:` value; only add the new `sandbox_backend: None,` line.)

- [ ] **Step 7: Update the inline test fixtures in `manager.rs`**

  In `core/src/worker_lifecycle/manager.rs` (around line 323), update the `ToolEntry { ... }` in `idle_timeout_acquire_on_single_use_entry_returns_wiring_error`:

  ```rust
          let entry = crate::scheduler::tool_dispatch::ToolEntry {
              binary: std::path::PathBuf::from("/nope"),
              policy: hhagent_sandbox::SandboxPolicy::default(),
              wall_clock_ms: None,
              lifecycle: Lifecycle::SingleUse,
              sandbox_backend: None,
          };
  ```

- [ ] **Step 8: Update inline test fixtures in `composite.rs`**

  In `core/src/worker_lifecycle/composite.rs`, find both `dummy_single_use_entry` (around line 121) and `dummy_idle_timeout_entry` (around line 130). Each contains a `ToolEntry { ... }` — add `sandbox_backend: None,` to each.

- [ ] **Step 9: Update e2e test fixtures**

  In `core/tests/scheduler_step_dispatch_e2e.rs` (around line 382), add `sandbox_backend: None,` to the `ToolEntry { ... }` literal.

  In `core/tests/worker_lifecycle_idle_timeout_e2e.rs` (around line 94), find the `idle_timeout_entry` helper that builds `ToolEntry { ... }` at line 98. Add `sandbox_backend: None,`.

- [ ] **Step 10: Run the targeted test + workspace build**

  ```bash
  cargo build --workspace 2>&1 | tail -5
  cargo test -p hhagent-core shell_exec_entry_defaults_sandbox_backend_to_none 2>&1 | tail -5
  ```

  Expected: workspace builds clean (no `missing field` errors); targeted test passes.

- [ ] **Step 11: Run the full workspace test suite to confirm no regressions**

  ```bash
  cargo test --workspace --no-fail-fast 2>&1 | grep "test result:" | awk '{for(i=1;i<=NF;i++){if($i=="passed;"){p+=$(i-1)}else if($i=="failed;"){f+=$(i-1)}else if($i=="ignored;"){ig+=$(i-1)}}} END {print "Aggregate — passed:"p" failed:"f" ignored:"ig}'
  ```

  Expected: `Aggregate — passed:902 failed:0 ignored:3` (901 + 1 new test). If any test fails, investigate the missed field-update site.

- [ ] **Step 12: Commit**

  ```bash
  git add core/src/scheduler/tool_dispatch.rs \
          core/src/workers/gliner_relex.rs \
          core/src/worker_lifecycle/manager.rs \
          core/src/worker_lifecycle/composite.rs \
          core/tests/scheduler_step_dispatch_e2e.rs \
          core/tests/worker_lifecycle_idle_timeout_e2e.rs
  git commit -m "feat(core): ToolEntry.sandbox_backend field (additive, defaults to None)"
  ```

---

### Task 4: Lifecycle managers switch from `Arc<dyn SandboxBackend>` to `Arc<SandboxBackends>`

Another cascade — every lifecycle-manager constructor changes signature simultaneously. Single commit to keep the workspace green at each boundary.

**Files:**
- Modify: `core/src/worker_lifecycle/manager.rs` (SingleUseLifecycle + IdleTimeoutLifecycle struct fields, `new`, `with_backoff`, `acquire` impls)
- Modify: `core/src/worker_lifecycle/idle_timeout.rs` (`acquire_impl` still takes `&dyn SandboxBackend` — but the caller resolves first; no signature change needed there)
- Modify: `core/src/worker_lifecycle/composite.rs` (CompositeLifecycle constructors)
- Modify: `core/src/main.rs` (daemon construction)
- Modify: `core/tests/scheduler_step_dispatch_e2e.rs`
- Modify: `core/tests/worker_lifecycle_idle_timeout_e2e.rs`
- Modify: `core/tests/gliner_relex_e2e.rs`
- Modify: `core/tests/entity_extraction_e2e.rs`
- Modify: `core/tests/memory_entity_link_e2e.rs`

- [ ] **Step 1: Write the failing counter-backend routing test**

  In `core/src/worker_lifecycle/manager.rs`, find the `#[cfg(test)] mod tests` block (around line 302). Add:

  ```rust
      /// `SingleUseLifecycle::acquire` resolves `entry.sandbox_backend`
      /// against its `SandboxBackends` bundle and reaches *that*
      /// backend, not a hardcoded one. We verify by injecting two
      /// counter-backends and asserting only the per-entry-selected
      /// counter ticks.
      ///
      /// `SandboxBackends` fields are `pub`, so tests can build a custom
      /// instance directly with stub backends. No production constructor
      /// is exposed for this — the field-visible-to-callers shape is
      /// deliberate.
      #[cfg(target_os = "macos")]
      #[tokio::test]
      async fn single_use_lifecycle_acquire_routes_via_entry_sandbox_backend_kind() {
          use hhagent_sandbox::{SandboxBackend, SandboxBackends, SandboxBackendKind, SandboxError, SandboxPolicy};
          use std::sync::atomic::{AtomicU32, Ordering};

          struct CountingBackend {
              counter: Arc<AtomicU32>,
          }
          impl SandboxBackend for CountingBackend {
              fn spawn_under_policy(
                  &self,
                  _policy: &SandboxPolicy,
                  _program: &str,
                  _args: &[&str],
              ) -> Result<std::process::Child, SandboxError> {
                  self.counter.fetch_add(1, Ordering::Relaxed);
                  // Stub: never actually spawn — the routing assertion
                  // fires before any real I/O.
                  Err(SandboxError::Backend("counted, intentionally unspawned".to_string()))
              }
          }

          let seatbelt_calls = Arc::new(AtomicU32::new(0));
          let container_calls = Arc::new(AtomicU32::new(0));

          let sbs = Arc::new(SandboxBackends {
              seatbelt: Arc::new(CountingBackend { counter: Arc::clone(&seatbelt_calls) }),
              container: Arc::new(CountingBackend { counter: Arc::clone(&container_calls) }),
          });

          let mgr = SingleUseLifecycle::new(Arc::clone(&sbs));

          let entry_container = crate::scheduler::tool_dispatch::ToolEntry {
              binary: std::path::PathBuf::from("/dev/null"),
              policy: SandboxPolicy::default(),
              wall_clock_ms: None,
              lifecycle: Lifecycle::SingleUse,
              sandbox_backend: Some(SandboxBackendKind::Container),
          };
          // We expect Err (CountingBackend always errors), but the routing
          // assertion is the counter — it ticked because acquire resolved
          // to the container slot.
          let _ = mgr.acquire("test", &entry_container).await;
          assert_eq!(container_calls.load(Ordering::Relaxed), 1, "container backend should be called");
          assert_eq!(seatbelt_calls.load(Ordering::Relaxed), 0, "seatbelt backend should be untouched");

          let entry_default = crate::scheduler::tool_dispatch::ToolEntry {
              sandbox_backend: None,
              ..entry_container
          };
          let _ = mgr.acquire("test", &entry_default).await;
          assert_eq!(container_calls.load(Ordering::Relaxed), 1, "container backend should not be re-called");
          assert_eq!(seatbelt_calls.load(Ordering::Relaxed), 1, "seatbelt backend should be called for None");
      }
  ```

- [ ] **Step 2: Run test to verify it fails (compile-error)**

  ```bash
  cargo test -p hhagent-core single_use_lifecycle_acquire_routes_via_entry_sandbox_backend_kind 2>&1 | tail -10
  ```

  Expected: compile error — `SingleUseLifecycle::new` still expects `Arc<dyn SandboxBackend>`, not `Arc<SandboxBackends>`.

- [ ] **Step 3: Update `SingleUseLifecycle` struct + constructor + acquire**

  In `core/src/worker_lifecycle/manager.rs` (lines 180–219), replace the SingleUseLifecycle block:

  ```rust
  /// Single-use lifecycle: spawn one worker per acquire, terminate on drop.
  ///
  /// Production impl for slice 1. Behaviour is byte-equivalent to the spawn
  /// path that used to live inline in
  /// `scheduler::tool_dispatch::ToolHostStepDispatcher::dispatch_step`.
  ///
  /// Slice 2 (this slice): holds an `Arc<SandboxBackends>` bundle instead of
  /// a single `Arc<dyn SandboxBackend>`; resolves per-call via
  /// `entry.sandbox_backend`. Existing entries default to `None` so the
  /// per-OS default backend keeps being used (byte-equivalent).
  pub struct SingleUseLifecycle {
      sandboxes: Arc<hhagent_sandbox::SandboxBackends>,
  }

  impl SingleUseLifecycle {
      pub fn new(sandboxes: Arc<hhagent_sandbox::SandboxBackends>) -> Self {
          Self { sandboxes }
      }
  }

  #[async_trait]
  impl WorkerLifecycleManager for SingleUseLifecycle {
      async fn acquire(
          &self,
          _tool_name: &str,
          entry: &ToolEntry,
      ) -> Result<WorkerHandle, ToolHostError> {
          let policy = entry.policy.clone();
          let program = entry.binary.to_string_lossy().into_owned();
          let spec = WorkerSpec {
              policy: &policy,
              program: &program,
              args: &[],
              wall_clock_ms: entry.wall_clock_ms,
          };
          let backend = self.sandboxes.resolve(entry.sandbox_backend);
          let worker = spawn_worker(backend.as_ref(), &spec)?;
          Ok(WorkerHandle::single_use(worker))
      }
  }
  ```

- [ ] **Step 4: Update `IdleTimeoutLifecycle` struct + constructors + acquire**

  In the same file, replace the `IdleTimeoutLifecycle` block (lines 227–300):

  ```rust
  /// Idle-timeout lifecycle: warm-keep one worker per tool name; tear down
  /// post-completion when any of `idle_seconds` / `max_requests` /
  /// `max_age_seconds` fires.
  ///
  /// Slice 2 (this slice): holds an `Arc<SandboxBackends>` bundle instead
  /// of a single backend; resolves per-call via `entry.sandbox_backend`
  /// at slot-fill time. The warm cache remains backend-agnostic — the
  /// `WarmRegistry` is keyed by tool name, so two tools that select
  /// different backends still get separate slots.
  pub struct IdleTimeoutLifecycle {
      sandboxes: Arc<hhagent_sandbox::SandboxBackends>,
      backoff: super::idle_timeout::RestartBackoff,
      registry: super::idle_timeout::WarmRegistry,
  }

  impl IdleTimeoutLifecycle {
      pub fn new(sandboxes: Arc<hhagent_sandbox::SandboxBackends>) -> Self {
          Self::with_backoff(sandboxes, super::idle_timeout::RestartBackoff::default())
      }

      pub fn with_backoff(
          sandboxes: Arc<hhagent_sandbox::SandboxBackends>,
          backoff: super::idle_timeout::RestartBackoff,
      ) -> Self {
          Self {
              sandboxes,
              backoff,
              registry: super::idle_timeout::empty_registry(),
          }
      }

      #[doc(hidden)]
      pub async fn _test_slot_has_warm(&self, tool_name: &str) -> bool {
          let map = self.registry.lock().expect("warm-registry mutex poisoned");
          let Some(slot) = map.get(tool_name) else {
              return false;
          };
          let slot = Arc::clone(slot);
          drop(map);
          let state = slot.state.lock().await;
          state.warm.is_some()
      }

      #[doc(hidden)]
      pub async fn _test_slot_consecutive_restarts(&self, tool_name: &str) -> u32 {
          let map = self.registry.lock().expect("warm-registry mutex poisoned");
          let Some(slot) = map.get(tool_name) else {
              return 0;
          };
          let slot = Arc::clone(slot);
          drop(map);
          let state = slot.state.lock().await;
          state.consecutive_restarts
      }
  }

  #[async_trait]
  impl WorkerLifecycleManager for IdleTimeoutLifecycle {
      async fn acquire(
          &self,
          tool_name: &str,
          entry: &ToolEntry,
      ) -> Result<WorkerHandle, ToolHostError> {
          let backend = self.sandboxes.resolve(entry.sandbox_backend);
          super::idle_timeout::acquire_impl(
              backend.as_ref(),
              self.backoff,
              &self.registry,
              tool_name,
              entry,
          )
          .await
      }
  }
  ```

  Note: `acquire_impl` keeps its `&dyn SandboxBackend` signature — the caller resolves first and passes the already-resolved reference. No signature change in `idle_timeout.rs`.

- [ ] **Step 5: Update existing inline tests in `manager.rs`**

  Two existing tests construct `Arc<dyn SandboxBackend> = Arc::from(default_backend())` and pass it to the constructors. Replace with `Arc::new(SandboxBackends::default_for_current_os())`. Around lines 312 + 321:

  ```rust
      #[test]
      fn single_use_lifecycle_constructor_holds_the_sandbox_backend() {
          let sandboxes = Arc::new(hhagent_sandbox::SandboxBackends::default_for_current_os());
          let _mgr = SingleUseLifecycle::new(sandboxes);
      }

      #[tokio::test]
      async fn idle_timeout_acquire_on_single_use_entry_returns_wiring_error() {
          let sandboxes = Arc::new(hhagent_sandbox::SandboxBackends::default_for_current_os());
          let mgr = IdleTimeoutLifecycle::new(sandboxes);
          let entry = crate::scheduler::tool_dispatch::ToolEntry {
              binary: std::path::PathBuf::from("/nope"),
              policy: hhagent_sandbox::SandboxPolicy::default(),
              wall_clock_ms: None,
              lifecycle: Lifecycle::SingleUse,
              sandbox_backend: None,
          };
          let r = mgr.acquire("test-tool", &entry).await;
          assert!(r.is_err(), "must return Err on wiring bug");
      }
  ```

  Remove the now-unused `use hhagent_sandbox::SandboxBackend;` import at the top of `manager.rs` if it's only referenced by the test module (the `acquire` body uses `self.sandboxes.resolve` which returns `Arc<dyn SandboxBackend>`, so the import may still be needed for the spawn_worker call — check the imports list and remove only if it's now unused, otherwise leave it).

- [ ] **Step 6: Update `CompositeLifecycle` constructors**

  In `core/src/worker_lifecycle/composite.rs` (lines 56–74), update:

  ```rust
  impl CompositeLifecycle {
      pub fn new(sandboxes: Arc<hhagent_sandbox::SandboxBackends>) -> Self {
          Self {
              single_use: SingleUseLifecycle::new(Arc::clone(&sandboxes)),
              idle_timeout: IdleTimeoutLifecycle::new(sandboxes),
          }
      }

      pub fn with_backoff(
          sandboxes: Arc<hhagent_sandbox::SandboxBackends>,
          backoff: super::idle_timeout::RestartBackoff,
      ) -> Self {
          Self {
              single_use: SingleUseLifecycle::new(Arc::clone(&sandboxes)),
              idle_timeout: IdleTimeoutLifecycle::with_backoff(sandboxes, backoff),
          }
      }
  }
  ```

- [ ] **Step 7: Update inline composite tests**

  In `composite.rs` around lines 149 + 167:

  ```rust
      let sbs = Arc::new(hhagent_sandbox::SandboxBackends {
          #[cfg(target_os = "linux")]
          bwrap: Arc::new(NeverSpawnsBackend),
          #[cfg(target_os = "macos")]
          seatbelt: Arc::new(NeverSpawnsBackend),
          #[cfg(target_os = "macos")]
          container: Arc::new(NeverSpawnsBackend),
      });
      let composite = CompositeLifecycle::new(sbs);
  ```

  Apply at both call sites (the two `CompositeLifecycle::new(Arc::new(NeverSpawnsBackend))` lines).

- [ ] **Step 8: Update daemon `main.rs`**

  In `core/src/main.rs` find the line constructing the sandbox + CompositeLifecycle. There's currently:

  ```rust
  let sandbox = Arc::from(hhagent_sandbox::default_backend());
  // ...
  hhagent_core::worker_lifecycle::CompositeLifecycle::new(sandbox.clone()),
  ```

  Replace with:

  ```rust
  let sandboxes = Arc::new(hhagent_sandbox::SandboxBackends::default_for_current_os());
  // ...
  hhagent_core::worker_lifecycle::CompositeLifecycle::new(Arc::clone(&sandboxes)),
  ```

  (The `Arc::clone` may not be necessary if `sandboxes` is not used after this line — keep simple. If `sandbox.clone()` was the only consumer, just pass `sandboxes` directly.)

- [ ] **Step 9: Update e2e test constructors**

  In each of:
  - `core/tests/scheduler_step_dispatch_e2e.rs` (line 401: `SingleUseLifecycle::new(sandbox)`)
  - `core/tests/worker_lifecycle_idle_timeout_e2e.rs` (5 `IdleTimeoutLifecycle::new(sandbox)` callsites at lines 118, 158, 207, 253, 358)
  - `core/tests/gliner_relex_e2e.rs` (3 `IdleTimeoutLifecycle::new(sandbox)` callsites at lines 173, 244, 323)
  - `core/tests/entity_extraction_e2e.rs` (2 `CompositeLifecycle::new(sandbox)` callsites at lines 516, 568)
  - `core/tests/memory_entity_link_e2e.rs` (1 `CompositeLifecycle::new(sandbox)` callsite at line 487)

  Replace the `let sandbox: Arc<dyn SandboxBackend> = ...;` line with:

  ```rust
  let sandboxes = Arc::new(hhagent_sandbox::SandboxBackends::default_for_current_os());
  ```

  And the manager construction with the same variable name (the local `sandbox` rename to `sandboxes` is mechanical — search-replace `sandbox` → `sandboxes` within each test, being careful not to mangle the `tests_common::sandbox` import or unrelated identifiers).

  **Caveat:** in `tests_common::sandbox::backend()` returns `Box<dyn SandboxBackend>` — that helper is unused now for daemon-backed tests. If a test currently calls `tests_common::sandbox::backend()` and then `IdleTimeoutLifecycle::new(...)` on the result, the call to `backend()` becomes orphaned. Remove the orphaned `backend()` call site by line; the new `default_for_current_os()` constructs its own backends. If `tests_common::sandbox::backend()` is unused by any consumer after the sweep, leave it in place — direct-spawn tests outside this sweep still use it.

- [ ] **Step 10: Workspace build + targeted test**

  ```bash
  cargo build --workspace 2>&1 | tail -10
  cargo test -p hhagent-core single_use_lifecycle_acquire_routes_via_entry_sandbox_backend_kind 2>&1 | tail -10
  ```

  Expected: workspace builds; the routing test passes.

- [ ] **Step 11: Full workspace test suite**

  ```bash
  cargo test --workspace --no-fail-fast 2>&1 | grep "test result:" | awk '{for(i=1;i<=NF;i++){if($i=="passed;"){p+=$(i-1)}else if($i=="failed;"){f+=$(i-1)}else if($i=="ignored;"){ig+=$(i-1)}}} END {print "Aggregate — passed:"p" failed:"f" ignored:"ig}'
  ```

  Expected: `Aggregate — passed:903 failed:0 ignored:3` (902 + 1 new routing test).

- [ ] **Step 12: Commit**

  ```bash
  git add -A
  git commit -m "feat(core): lifecycle managers resolve per-call via SandboxBackends"
  ```

---

### Task 5: Integration smoke — alpine routing through `SingleUseLifecycle::acquire`

**Files:**
- Create: `core/tests/lifecycle_container_routing_e2e.rs`

- [ ] **Step 1: Write the integration test**

  Create `core/tests/lifecycle_container_routing_e2e.rs`:

  ```rust
  //! Integration smoke: prove `SingleUseLifecycle::acquire` actually routes
  //! through the entry-selected sandbox backend, end-to-end with a real
  //! Apple `container` invocation.
  //!
  //! Two-sided pin against the same `ToolEntry`-shape:
  //!   * positive: `sandbox_backend: Some(Container)` + alpine-only
  //!     `/sbin/apk` binary — container backend mounts alpine, apk
  //!     exists, spawn succeeds.
  //!   * negative: `sandbox_backend: None` (resolves to Seatbelt on
  //!     darwin) + the same `/sbin/apk` binary — Seatbelt runs on the
  //!     host, `/sbin/apk` doesn't exist on macOS, spawn fails.
  //!
  //! Together the two halves prove the selection bit actually changes
  //! which backend the lifecycle manager reaches.
  //!
  //! Skip-as-pass when `container --version` / `container system status` /
  //! the `alpine:3.20` image are missing.

  #![cfg(target_os = "macos")]

  use std::path::PathBuf;
  use std::sync::Arc;

  use hhagent_core::worker_lifecycle::{Lifecycle, SingleUseLifecycle, WorkerLifecycleManager};
  use hhagent_sandbox::{
      macos_container::MacosContainer, Net, Profile, SandboxBackendKind, SandboxBackends,
      SandboxPolicy,
  };

  /// Skip the test (via early-return) when Apple `container` isn't usable
  /// on this host. Returns `true` when the caller should skip.
  fn skip_if_no_container() -> bool {
      if let Err(e) = MacosContainer::probe() {
          eprintln!("\n[SKIP] container probe failed: {e}\n");
          return true;
      }
      // Image presence: cheap `container image list | grep` check.
      let listed = std::process::Command::new("container")
          .args(["image", "list"])
          .output();
      let has_image = matches!(listed, Ok(o) if String::from_utf8_lossy(&o.stdout).contains("alpine:3.20"));
      if !has_image {
          eprintln!("\n[SKIP] alpine:3.20 image not present; run `container image pull alpine:3.20`\n");
          return true;
      }
      false
  }

  fn minimal_policy() -> SandboxPolicy {
      SandboxPolicy {
          fs_read: vec![],
          fs_write: vec![],
          net: Net::Deny,
          cpu_ms: 5_000,
          mem_mb: 256,
          profile: Profile::WorkerStrict,
          cpu_quota_pct: None,
          tasks_max: None,
          env: vec![],
      }
  }

  /// Positive half: `sandbox_backend: Some(Container)` runs `/sbin/apk` inside
  /// the alpine container, which exists there.
  #[tokio::test]
  async fn single_use_lifecycle_routes_through_container_when_entry_opts_in() {
      if skip_if_no_container() {
          return;
      }

      let sbs = Arc::new(SandboxBackends::default_for_current_os());
      let mgr = SingleUseLifecycle::new(Arc::clone(&sbs));

      let entry = hhagent_core::scheduler::tool_dispatch::ToolEntry {
          binary: PathBuf::from("/sbin/apk"),
          policy: minimal_policy(),
          wall_clock_ms: Some(5_000),
          lifecycle: Lifecycle::SingleUse,
          sandbox_backend: Some(SandboxBackendKind::Container),
      };

      let result = mgr.acquire("apk-routing-positive", &entry).await;
      let mut handle = result.expect("acquire under Container backend must succeed; alpine has /sbin/apk");

      // Kill cleanly — apk would block on `apk --help` reading from stdin
      // (we never sent JSON-RPC). The kill is graceful from the host's
      // perspective; the worker exits via SIGKILL.
      let _ = handle.worker_mut().kill();
  }

  /// Negative half: `sandbox_backend: None` resolves to Seatbelt on darwin.
  /// `/sbin/apk` doesn't exist on macOS host, so the spawn fails.
  #[tokio::test]
  async fn single_use_lifecycle_defaults_to_seatbelt_and_fails_on_alpine_only_binary() {
      // No container-availability check needed — this path doesn't use container.
      let sbs = Arc::new(SandboxBackends::default_for_current_os());
      let mgr = SingleUseLifecycle::new(Arc::clone(&sbs));

      let entry = hhagent_core::scheduler::tool_dispatch::ToolEntry {
          binary: PathBuf::from("/sbin/apk"),
          policy: minimal_policy(),
          wall_clock_ms: Some(5_000),
          lifecycle: Lifecycle::SingleUse,
          sandbox_backend: None, // → Seatbelt on darwin
      };

      let result = mgr.acquire("apk-routing-negative", &entry).await;
      assert!(
          result.is_err(),
          "Seatbelt should fail to spawn /sbin/apk (macOS host has no apk)"
      );
  }
  ```

- [ ] **Step 2: Run the new tests**

  ```bash
  cargo test -p hhagent-core --test lifecycle_container_routing_e2e 2>&1 | tail -20
  ```

  Expected on macOS with container installed: 2 passed. On macOS without container: 1 passed (negative) + 1 `[SKIP]` printed (positive). On linux: tests are cfg-gated out — `running 0 tests`.

- [ ] **Step 3: Full workspace test sweep**

  ```bash
  cargo test --workspace --no-fail-fast 2>&1 | grep "test result:" | awk '{for(i=1;i<=NF;i++){if($i=="passed;"){p+=$(i-1)}else if($i=="failed;"){f+=$(i-1)}else if($i=="ignored;"){ig+=$(i-1)}}} END {print "Aggregate — passed:"p" failed:"f" ignored:"ig}'
  ```

  Expected: `Aggregate — passed:905 failed:0 ignored:3` on macOS (903 + 2 new integration tests).

- [ ] **Step 4: Commit**

  ```bash
  git add core/tests/lifecycle_container_routing_e2e.rs
  git commit -m "test(core): integration smoke for SingleUseLifecycle backend routing"
  ```

---

### Task 6: Update HANDOVER + ROADMAP, open PR

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

- [ ] **Step 1: Final workspace verification**

  ```bash
  cargo test --workspace --no-fail-fast 2>&1 | grep "test result:" | awk '{for(i=1;i<=NF;i++){if($i=="passed;"){p+=$(i-1)}else if($i=="failed;"){f+=$(i-1)}else if($i=="ignored;"){ig+=$(i-1)}}} END {print "Aggregate — passed:"p" failed:"f" ignored:"ig}'
  cargo clippy --workspace --lib --tests --no-deps 2>&1 | tail -10
  ```

  Expected: 905 passed / 0 failed / 3 ignored on macOS; clippy clean (modulo the pre-existing `mem_burner.rs` `uninit_vec` warning).

- [ ] **Step 2: Update HANDOVER.md**

  At the top of `docs/devel/handovers/HANDOVER.md`, update the header lines:
  - `Last updated:` → today's date + "Slice 2 shipped, PR pending" descriptor
  - `Last commit on main:` → keep current value until merge; add `Last commit on feat/macos-container-backend-slice-2:` line
  - `Session-end verification:` → 901 → 905 (or actual count) — Rust workspace on macOS

  Add a new "Recently completed (this session)" entry describing Slice 2's scope, with a similar structure to Slice 1's entry.

  Move Next-TODO item 18 (Slice 2) from "Next TODO (pick one)" to "Recently completed", and bump items 19+ down accordingly.

  Add a note that Issue #107 (PID-1 / --init) is now expected to land in Slice 2.5.

- [ ] **Step 3: Update ROADMAP.md**

  In `docs/devel/ROADMAP.md`, find the most recent shipped bullet (currently the Slice 1 / PR #106 entry around line 136). Add a new `[x]` bullet directly below it describing Slice 2's scope, with the merge commit placeholder (to be filled in after PR merge).

- [ ] **Step 4: Commit doc updates**

  ```bash
  git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
  git commit -m "docs(handover): Slice 2 shipped — per-worker sandbox backend selection"
  ```

- [ ] **Step 5: Push branch and open PR**

  ```bash
  git push -u origin feat/macos-container-backend-slice-2
  gh pr create --title "feat(sandbox): MacosContainer Slice 2 — per-worker backend selection" --body "$(cat <<'EOF'
  ## Summary
  - Add `SandboxBackendKind` (cfg-gated per-OS) + `SandboxBackends` resolver in `hhagent_sandbox`.
  - Add `sandbox_backend: Option<SandboxBackendKind>` to `ToolEntry`; existing entries default to `None` (per-OS default backend, byte-equivalent behaviour).
  - Lifecycle managers (`SingleUseLifecycle`, `IdleTimeoutLifecycle`, `CompositeLifecycle`) switch from `Arc<dyn SandboxBackend>` to `Arc<SandboxBackends>`; `acquire` resolves per call.
  - Daemon `main.rs` builds `SandboxBackends::default_for_current_os()` once at startup.
  - Integration smoke proves end-to-end routing via the alpine-only `/sbin/apk` two-sided pin (positive: `Some(Container)` succeeds; negative: `None` falls back to Seatbelt and fails to find `/sbin/apk`).

  Spec: `docs/superpowers/specs/2026-05-21-macos-container-slice-2-design.md`
  Plan: `docs/superpowers/plans/2026-05-22-macos-container-slice-2.md`

  ## Test plan
  - [x] `cargo test --workspace --no-fail-fast` on macOS at M3 Max — 905/0/3
  - [x] `cargo clippy --workspace --lib --tests --no-deps` clean
  - [x] Integration smoke passes (positive routes through Apple `container`; negative falls back to Seatbelt and fails as expected)
  - [ ] CI sweep on Linux — Bwrap path stays unchanged (cfg-gated)

  ## Out of scope (Slice 2.5)
  - `gliner-relex` migration to `Container` backend (needs Containerfile + image-build pipeline)
  - Issue #107 (`--init` / PID-1 signal-handling) — relevant only when a long-lived `IdleTimeoutLifecycle` worker migrates onto the container backend, which is Slice 2.5's scope.

  🤖 Generated with [Claude Code](https://claude.com/claude-code)
  EOF
  )"
  ```

  Note: PR creation only happens once human review-ready. The bot user should run this command **only with explicit operator approval per the system instructions** — surface the URL on completion.

---

## Self-review

**Spec coverage:**
- ✓ SandboxBackendKind enum (Task 1)
- ✓ SandboxBackends struct + default_for_current_os + resolve (Task 2)
- ✓ ToolEntry.sandbox_backend field (Task 3)
- ✓ Lifecycle managers switch (Task 4)
- ✓ Daemon main.rs swap (Task 4 step 8)
- ✓ Unit tests on enum + resolver (Task 1 + 2)
- ✓ Counter-backend routing test (Task 4 step 1)
- ✓ Integration smoke (Task 5)
- ✓ HANDOVER + ROADMAP (Task 6)
- ✓ Issue #107 deferred to Slice 2.5 (explicit out-of-scope callouts)

**Placeholder scan:** None — every step has the actual code or command.

**Type consistency:**
- `SandboxBackends` struct fields are `pub` and accessible from outside the crate. ✓
- `SandboxBackends::resolve` returns `Arc<dyn SandboxBackend>`. ✓
- Lifecycle managers take `Arc<SandboxBackends>` (not `Arc<dyn SandboxBackend>`). ✓
- `ToolEntry.sandbox_backend` is `Option<hhagent_sandbox::SandboxBackendKind>`. ✓

**Risks called out in spec are tracked here:**
- Cfg-gating ripples → tasks specify exact cfg attrs per step.
- Test-only constructor → resolved by exposing struct fields as `pub` instead of a separate test constructor.
- Counter-backend impl drift → test backend is minimal (~10 LOC stub).
- Issue #107 → explicitly out of scope, ROADMAP entry will note it.
- `tests-common::sandbox::backend()` divergence → unchanged in this slice; direct-spawn tests still use it.
