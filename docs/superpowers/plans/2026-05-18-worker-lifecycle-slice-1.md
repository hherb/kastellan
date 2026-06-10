# Worker Lifecycle Policy — Slice 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce a `WorkerLifecycleManager` abstraction over the spawn-per-request path so the production dispatcher routes through it. Slice 1 ships only the `single_use` runtime (byte-equivalent to today's shell-exec behaviour) plus the type-level definitions for `idle_timeout`. Slice 2 (the GLiNER-Relex prereq) fills in `idle_timeout` runtime.

**Architecture:** New module `core::worker_lifecycle` carries `Lifecycle` enum + `WorkerLifecycleManager` trait + `SingleUseLifecycle` impl + `IdleTimeoutLifecycle` stub (panics at `acquire`). `ToolEntry` gains a `lifecycle: Lifecycle` field defaulting to `SingleUse`. `ToolHostStepDispatcher` swaps its `sandbox: Arc<dyn SandboxBackend>` field for `lifecycle: Arc<dyn WorkerLifecycleManager>` and delegates spawning. The `kastellan-supervisor` crate (OS-unit installer for systemd/launchd) is untouched — its name collision with the spec's "supervisor" wording is purely conceptual.

**Tech Stack:** Rust 2021, `async_trait`, `tokio`, `kastellan_sandbox::SandboxBackend`, `kastellan_protocol` JSON-RPC, existing `tool_host::{spawn_worker, dispatch, SupervisedWorker, ToolHostError, WorkerSpec}`.

---

## Reading list (do this once, before Task 1)

The implementing engineer should skim these files cold:

1. `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md` — the design spec this plan implements.
2. `core/src/tool_host.rs` — the existing spawn-and-talk module. `spawn_worker`, `SupervisedWorker`, `WorkerSpec`, `ToolHostError`, `dispatch` are the surfaces this slice composes around.
3. `core/src/scheduler/tool_dispatch.rs` — the production caller. `ToolEntry`, `ToolHostStepDispatcher`, `shell_exec_entry`, and `dispatch_step` are what slice 1 modifies.
4. `core/src/main.rs:140-162` — the daemon's wiring of the dispatcher.
5. `CLAUDE.md` — the project's hard constraints (cross-platform, AGPL-compat, sandbox invariant, no in-process Python, no `cargo fmt`/`clippy` config yet — match existing formatting).

## File structure (decomposition lock-in)

**New files:**

- `core/src/worker_lifecycle/mod.rs` — module facade. Re-exports `Lifecycle`, `IdleTimeoutCaps`, `Contract`, `WorkerLifecycleManager`, `WorkerHandle`, `SingleUseLifecycle`, `IdleTimeoutLifecycle`.
- `core/src/worker_lifecycle/types.rs` — pure types: `Lifecycle` enum, `IdleTimeoutCaps` struct, `Contract` struct. No I/O, no spawn calls — unit-testable without a sandbox backend.
- `core/src/worker_lifecycle/manager.rs` — async trait + impls: `WorkerLifecycleManager`, `WorkerHandle`, `SingleUseLifecycle`, `IdleTimeoutLifecycle`.

**Modified files:**

- `core/src/lib.rs` — add `pub mod worker_lifecycle;` next to the existing `pub mod tool_host;` line.
- `core/src/scheduler/tool_dispatch.rs` — `ToolEntry` gains `lifecycle: Lifecycle`; `shell_exec_entry` sets it explicitly; `ToolHostStepDispatcher` swaps its `sandbox` field for a manager; `dispatch_step` rewires the spawn path; the unit-test `fake_entry()` and the e2e test's `broken-tool` literal both gain the new field.
- `core/src/main.rs` — instantiate `SingleUseLifecycle` and pass it to `ToolHostStepDispatcher::new`.
- `core/tests/scheduler_step_dispatch_e2e.rs` — its `broken-tool` `ToolEntry { … }` literal at lines 382-393 gains the `lifecycle: Lifecycle::SingleUse` field; its `ToolHostStepDispatcher::new` call at lines 399-403 changes signature.
- `docs/devel/handovers/HANDOVER.md` + `docs/devel/ROADMAP.md` — session-end updates per the project's read-at-start / update-at-end convention.

**Untouched (deliberately):**

- `supervisor/` — the OS-unit installer crate stays exactly as it is. Naming overlap with the spec's "supervisor" wording is irrelevant at the code level.
- `workers/shell-exec/` — no manifest file lands in slice 1. The lifecycle declaration for shell-exec is `Lifecycle::SingleUse` baked into the `shell_exec_entry()` helper. Worker-manifest plumbing is deferred (open question 1 in the spec).

## Test count baseline

Pre-slice baseline on `main`: **721 passed, 0 failed, 4 ignored, 0 [SKIP], 0 warnings**. Slice 1 adds unit tests for the new types and manager (estimated +12 to +18). After slice 1: ~ **735+**; existing integration tests (`scheduler_step_dispatch_e2e`, `cli_ask_e2e`) continue green by construction because shell-exec's runtime behaviour is byte-equivalent.

## Branch + worktree

Before Task 1, create the worktree (see "Worktree setup" appendix at the end). Branch name: `feat/worker-lifecycle-slice-1`.

---

## Task 1: Pure type definitions in `worker_lifecycle/types.rs`

**Files:**
- Create: `core/src/worker_lifecycle/types.rs`
- Create: `core/src/worker_lifecycle/mod.rs`
- Modify: `core/src/lib.rs` (add `pub mod worker_lifecycle;`)

- [ ] **Step 1: Write the failing tests first**

Create `core/src/worker_lifecycle/types.rs` with only the test module and no production code yet:

```rust
//! Pure-type definitions for the worker lifecycle policy.
//!
//! Spec: `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md`.
//!
//! Slice 1 ships:
//!   - `Lifecycle::SingleUse` — current shell-exec behaviour (spawn → one request → exit).
//!   - `Lifecycle::IdleTimeout { caps, contract }` — declarable shape only; the runtime path
//!     (`IdleTimeoutLifecycle::acquire`) panics until slice 2 fills it in.
//!
//! All types here are pure: no I/O, no clock, no spawn calls. The runtime layer lives in
//! `super::manager`.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_default_is_single_use() {
        // Default Lifecycle is `SingleUse` so a freshly-constructed `ToolEntry`
        // (or any other consumer using `..Default::default()`) gets the conservative
        // current-shell-exec semantics, not an inference-worker policy.
        let l: Lifecycle = Lifecycle::default();
        assert!(matches!(l, Lifecycle::SingleUse));
    }

    #[test]
    fn idle_timeout_caps_carries_four_named_durations() {
        // The four caps from the spec, exposed by name so a future consumer can read
        // each one without positional indexing. All four are required at construction.
        let caps = IdleTimeoutCaps {
            idle_seconds: 600,
            max_requests: 10_000,
            max_age_seconds: 86_400,
            grace_period_seconds: 5,
        };
        assert_eq!(caps.idle_seconds, 600);
        assert_eq!(caps.max_requests, 10_000);
        assert_eq!(caps.max_age_seconds, 86_400);
        assert_eq!(caps.grace_period_seconds, 5);
    }

    #[test]
    fn contract_stateless_true_is_the_only_v1_supported_value() {
        // Slice-1 / v1 of this spec only supports `stateless = true` workers under
        // `idle_timeout`. The field exists as a bool to keep the shape forward-compatible
        // with a future `stateless = false` worker that needs its own threat review.
        let c = Contract { stateless: true };
        assert!(c.stateless);
    }

    #[test]
    fn idle_timeout_variant_carries_caps_and_contract() {
        // Round-trip the IdleTimeout variant — the struct-style variant is what slice 2's
        // runtime will pattern-match on.
        let l = Lifecycle::IdleTimeout {
            caps: IdleTimeoutCaps {
                idle_seconds: 60,
                max_requests: 100,
                max_age_seconds: 3600,
                grace_period_seconds: 5,
            },
            contract: Contract { stateless: true },
        };
        match l {
            Lifecycle::IdleTimeout { caps, contract } => {
                assert_eq!(caps.idle_seconds, 60);
                assert!(contract.stateless);
            }
            _ => panic!("expected IdleTimeout variant"),
        }
    }

    #[test]
    fn idle_timeout_requires_stateless_contract_per_spec_v1() {
        // Construction-time validation: a `Lifecycle::IdleTimeout` carrying
        // `Contract { stateless: false }` violates the v1 invariant (spec §"The stateless
        // contract"). `Lifecycle::idle_timeout(caps, contract)` is the validated constructor
        // that rejects this combination; the struct-style literal stays available for tests
        // that want to construct an invalid value deliberately.
        let bad = Lifecycle::idle_timeout(
            IdleTimeoutCaps {
                idle_seconds: 60,
                max_requests: 100,
                max_age_seconds: 3600,
                grace_period_seconds: 5,
            },
            Contract { stateless: false },
        );
        assert!(bad.is_err(), "stateless=false under idle_timeout must be rejected in v1");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core worker_lifecycle::types 2>&1 | tail -30
```

Expected: compile error — `cannot find type Lifecycle in this scope` (production types not yet defined).

- [ ] **Step 3: Add the module declaration to `lib.rs`**

Add a single line to `core/src/lib.rs`, immediately below the existing `pub mod tool_host;` line:

```rust
pub mod worker_lifecycle;
```

- [ ] **Step 4: Create the `mod.rs` facade**

Create `core/src/worker_lifecycle/mod.rs`:

```rust
//! Worker lifecycle policy — slice 1 (single_use runtime + idle_timeout types).
//!
//! See `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md` for the
//! design contract and `docs/superpowers/plans/2026-05-18-worker-lifecycle-slice-1.md`
//! for the implementation plan this module realises.
//!
//! Public surface:
//!   - `Lifecycle`, `IdleTimeoutCaps`, `Contract` — pure value types declarable on a
//!     `ToolEntry`.
//!   - `WorkerLifecycleManager` — async trait that lends out `WorkerHandle`s.
//!   - `SingleUseLifecycle` — production impl for slice 1; spawns one process per
//!     acquire and tears down on handle drop. Behaviour byte-equivalent to today's
//!     `scheduler::tool_dispatch::dispatch_step` spawn path.
//!   - `IdleTimeoutLifecycle` — stub impl; `acquire()` panics with `unimplemented!()`
//!     until slice 2 implements warm-keeping. Declarable at the type level today so
//!     downstream code can name it.
//!   - `WorkerHandle` — `&mut`-able holder of a live `SupervisedWorker`. Drop semantics
//!     for slice 1 just drops the inner worker (today's behaviour).

pub mod manager;
pub mod types;

pub use manager::{IdleTimeoutLifecycle, SingleUseLifecycle, WorkerHandle, WorkerLifecycleManager};
pub use types::{Contract, IdleTimeoutCaps, Lifecycle};
```

- [ ] **Step 5: Add the production types alongside the existing test module**

Insert above the `#[cfg(test)] mod tests { … }` block in `core/src/worker_lifecycle/types.rs`:

```rust
/// Lifecycle policy declared on a `ToolEntry`.
///
/// `SingleUse` is the conservative default and matches today's shell-exec behaviour:
/// spawn a fresh sandboxed process per request, run one JSON-RPC call, exit. This is the
/// right policy for transient operations where per-request isolation is the security
/// model itself.
///
/// `IdleTimeout` is the warm-keeping policy for stateless inference workers with
/// non-trivial startup cost (GLiNER-Relex, sentiment, embedding, classification, OCR).
/// The supervisor holds a single live process per worker type and re-uses it across
/// requests; caps are evaluated post-completion only (never mid-flight).
///
/// **Slice 1 ships the `IdleTimeout` variant declarable but inert** —
/// `IdleTimeoutLifecycle::acquire` panics until slice 2 implements warm-keeping.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Lifecycle {
    /// Spawn a fresh process per request. Caps don't apply.
    #[default]
    SingleUse,
    /// Spawn on first request, stay alive, tear down post-completion when any of the
    /// caps fires.
    IdleTimeout {
        caps: IdleTimeoutCaps,
        contract: Contract,
    },
}

impl Lifecycle {
    /// Validated constructor for the `IdleTimeout` variant.
    ///
    /// Rejects `Contract { stateless: false }` because slice-1 / v1 of the spec only
    /// supports stateless workers under warm-keeping. A future `stateless = false`
    /// worker needs its own threat review (per spec §"The stateless contract") and
    /// will reach this constructor via a different path.
    ///
    /// The struct-style variant literal (`Lifecycle::IdleTimeout { caps, contract }`)
    /// remains accessible for tests that need to plant an invalid value deliberately.
    pub fn idle_timeout(
        caps: IdleTimeoutCaps,
        contract: Contract,
    ) -> Result<Self, LifecycleValidationError> {
        if !contract.stateless {
            return Err(LifecycleValidationError::StatelessRequiredForIdleTimeout);
        }
        Ok(Self::IdleTimeout { caps, contract })
    }
}

/// Construction-time validation errors for `Lifecycle`.
///
/// Distinct from a generic `String` error because callers (slice 2's manifest parser,
/// the worker-author's `WorkerManifest::validate`) will programmatically branch on the
/// reason. Slice 1 has only one variant; future variants slot in cleanly.
#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum LifecycleValidationError {
    #[error("idle_timeout lifecycle requires Contract { stateless = true } in spec v1")]
    StatelessRequiredForIdleTimeout,
}

/// Post-completion caps that bound a warm worker's lifetime.
///
/// All four are evaluated after a JSON-RPC response has been written — never mid-flight.
/// See spec §"Cap-check semantics" for the load-bearing invariant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdleTimeoutCaps {
    /// Tear down after this many seconds with no in-flight or queued request.
    pub idle_seconds: u64,
    /// Rotate after this many requests served cumulatively (slow-leak hygiene).
    pub max_requests: u64,
    /// Rotate after the process has been alive this many seconds (drift hygiene).
    pub max_age_seconds: u64,
    /// SIGTERM grace before SIGKILL during graceful shutdown.
    pub grace_period_seconds: u64,
}

/// Per-request statelessness contract declared by the worker author.
///
/// `stateless = true` is the only value v1 supports for `IdleTimeout` workers — see
/// spec §"The stateless contract" for what the worker author is asserting at code-review
/// time. The bool field shape is forward-compatible with a future `stateless = false`
/// path that ships with its own threat review.
///
/// For `SingleUse` workers this field is irrelevant — there is no "next request" in the
/// same process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Contract {
    pub stateless: bool,
}
```

- [ ] **Step 6: Run the tests to verify they pass**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core worker_lifecycle::types 2>&1 | tail -20
```

Expected: `test result: ok. 5 passed; 0 failed`.

- [ ] **Step 7: Verify the whole crate still builds and tests pass**

```sh
source "$HOME/.cargo/env"
cargo build -p kastellan-core 2>&1 | tail -10
cargo test -p kastellan-core --lib 2>&1 | tail -5
```

Expected: clean build + all `kastellan-core` library unit tests green (no integration / e2e tests yet — those run in Task 6).

- [ ] **Step 8: Commit**

```sh
git add core/src/lib.rs core/src/worker_lifecycle/mod.rs core/src/worker_lifecycle/types.rs
git commit -m "$(cat <<'EOF'
feat(core,worker_lifecycle): pure type definitions for slice 1

Introduces `core::worker_lifecycle` module carrying:
  * `Lifecycle` enum (`SingleUse` default + `IdleTimeout { caps, contract }`)
  * `IdleTimeoutCaps { idle_seconds, max_requests, max_age_seconds, grace_period_seconds }`
  * `Contract { stateless: bool }`
  * `Lifecycle::idle_timeout(caps, contract)` validated constructor
    (rejects stateless=false per spec v1)
  * `LifecycleValidationError` (one variant today; forward-compatible)

Pure types only — no spawn, no I/O. Runtime path lands in the next commit.

5 unit tests pinning Default, struct-variant fields, idle_timeout validation.

Refs: docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md
EOF
)"
```

---

## Task 2: `WorkerLifecycleManager` trait + `WorkerHandle` + `SingleUseLifecycle` + `IdleTimeoutLifecycle` stub

**Files:**
- Create: `core/src/worker_lifecycle/manager.rs`

- [ ] **Step 1: Write the failing tests first**

Create `core/src/worker_lifecycle/manager.rs` with only the test module and no production code yet. Tests use the existing `tests-common` infrastructure indirectly via `core::scheduler::tool_dispatch::shell_exec_entry`-like fixtures, but the manager unit tests stay pure (no real sandbox backend — they exercise the panic path on `IdleTimeoutLifecycle` and the constructor of `SingleUseLifecycle`).

```rust
//! Lifecycle manager: spawns workers, lends out `WorkerHandle`s.
//!
//! Slice 1 ships `SingleUseLifecycle` (production, byte-equivalent to today's
//! per-request spawn) and `IdleTimeoutLifecycle` (stub — `acquire` panics).

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker_lifecycle::types::{Contract, IdleTimeoutCaps, Lifecycle};
    use std::sync::Arc;

    // A no-op sandbox backend that lets us construct a `SingleUseLifecycle` without
    // pulling in `kastellan-sandbox`'s integration-test setup. We never call `acquire` on
    // it here — Task 6's existing `scheduler_step_dispatch_e2e` integration test is the
    // real spawn-path regression pin.
    struct NoopBackend;
    impl kastellan_sandbox::SandboxBackend for NoopBackend {
        fn probe(&self) -> Result<(), kastellan_sandbox::SandboxError> {
            Ok(())
        }
        fn spawn_under_policy(
            &self,
            _policy: &kastellan_sandbox::SandboxPolicy,
            _program: &str,
            _args: &[String],
        ) -> Result<std::process::Child, kastellan_sandbox::SandboxError> {
            unreachable!("test fixture: acquire never called against NoopBackend")
        }
    }

    #[test]
    fn single_use_lifecycle_constructor_holds_the_sandbox_backend() {
        let sandbox: Arc<dyn kastellan_sandbox::SandboxBackend> = Arc::new(NoopBackend);
        let _mgr = SingleUseLifecycle::new(sandbox);
        // The presence of a constructor that compiles is the assertion; the manager's
        // production path is exercised by `scheduler_step_dispatch_e2e` after slice 1's
        // wiring lands in Task 5.
    }

    #[tokio::test]
    #[should_panic(expected = "idle_timeout lifecycle runtime — slice 2")]
    async fn idle_timeout_lifecycle_acquire_panics_until_slice_2() {
        // The stub exists at the type level so downstream code (a future `WorkerManifest`
        // parser, slice 2's runtime) can refer to it without conditional compilation.
        // Runtime invocation is deliberately wired to `unimplemented!()` so a test that
        // accidentally routes idle-timeout traffic through slice 1's daemon trips loudly.
        let caps = IdleTimeoutCaps {
            idle_seconds: 60,
            max_requests: 100,
            max_age_seconds: 3600,
            grace_period_seconds: 5,
        };
        let contract = Contract { stateless: true };
        let lc = Lifecycle::idle_timeout(caps, contract).expect("valid lifecycle");
        let mgr = IdleTimeoutLifecycle::new();
        // We need a `ToolEntry` to call acquire — defer to a dummy. The acquire body
        // panics before reading any field of the entry, so the dummy is safe.
        let entry = crate::scheduler::tool_dispatch::ToolEntry {
            binary: std::path::PathBuf::from("/nope"),
            policy: kastellan_sandbox::SandboxPolicy::default(),
            wall_clock_ms: None,
            lifecycle: lc,
        };
        let _ = mgr.acquire(&entry).await;
    }

    #[test]
    fn worker_handle_exposes_worker_mut() {
        // Type-level pin: `WorkerHandle::worker_mut` returns `&mut SupervisedWorker`,
        // which is what `dispatch_step` will pass into `tool_host::dispatch`. The
        // assertion is the signature; no runtime invocation here.
        fn _shape_pin(h: &mut WorkerHandle) -> &mut crate::tool_host::SupervisedWorker {
            h.worker_mut()
        }
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core worker_lifecycle::manager 2>&1 | tail -30
```

Expected: compile errors — `SingleUseLifecycle`, `IdleTimeoutLifecycle`, `WorkerHandle`, the `lifecycle` field on `ToolEntry`, etc. are all unresolved. Task 2 fills in the manager surface; Task 3 adds the `lifecycle` field on `ToolEntry`. Until Task 3 the failing assertion will be on the unknown `lifecycle` field of `ToolEntry`. That's expected.

- [ ] **Step 3: Add the production types in `manager.rs`**

Insert above the `#[cfg(test)] mod tests { … }` block in `core/src/worker_lifecycle/manager.rs`:

```rust
use std::sync::Arc;

use async_trait::async_trait;
use kastellan_sandbox::SandboxBackend;

use crate::scheduler::tool_dispatch::ToolEntry;
use crate::tool_host::{spawn_worker, SupervisedWorker, ToolHostError, WorkerSpec};

/// Holder of an exclusively-owned, live `SupervisedWorker` lent out by a lifecycle
/// manager. The dispatcher calls `worker_mut()` to get the `&mut SupervisedWorker`
/// that `tool_host::dispatch` wants.
///
/// **Slice 1 Drop semantics:** the default `Drop` drops the inner `SupervisedWorker`,
/// whose own `Drop` closes stdio + cancels the watchdog. For `SingleUseLifecycle` this
/// is exactly the right behaviour — the worker exits.
///
/// **Slice 2 will replace this:** the handle will carry a back-channel to the manager
/// so `Drop` hands the worker back to the warm-pool instead of terminating it. Slice 1
/// keeps the type minimal so the slice-2 extension is additive.
pub struct WorkerHandle {
    worker: SupervisedWorker,
}

impl WorkerHandle {
    /// Construct a single-use handle. Module-private — only the lifecycle implementations
    /// in this file can build one.
    pub(crate) fn single_use(worker: SupervisedWorker) -> Self {
        Self { worker }
    }

    /// Exclusive `&mut` to the live worker. The intended caller is
    /// `tool_host::dispatch(pool, handle.worker_mut(), tool, method, params)`; the
    /// chokepoint seal (issue #16) is unchanged because `SupervisedWorker::call` itself
    /// stays module-private to `tool_host`.
    pub fn worker_mut(&mut self) -> &mut SupervisedWorker {
        &mut self.worker
    }
}

/// Lifecycle manager trait. `dyn`-safe (no generics, no associated types).
///
/// `acquire` is async because the `IdleTimeout` runtime (slice 2) will need to await
/// queue-slot availability when a request lands on a busy warm worker. `SingleUseLifecycle`
/// doesn't actually await anything inside `acquire`, but uses the same trait shape so
/// the dispatcher can hold an `Arc<dyn WorkerLifecycleManager>` without per-policy
/// branching.
#[async_trait]
pub trait WorkerLifecycleManager: Send + Sync {
    /// Acquire a `WorkerHandle` for `entry`'s tool. The handle's lifetime equals one
    /// JSON-RPC request: caller dispatches against it, then drops it. Slice 1 always
    /// terminates the underlying worker on drop; slice 2 may hand it back to a pool.
    async fn acquire(&self, entry: &ToolEntry) -> Result<WorkerHandle, ToolHostError>;
}

/// Single-use lifecycle: spawn one worker per acquire, terminate on drop.
///
/// Production impl for slice 1. Behaviour is byte-equivalent to the spawn path that
/// used to live inline in `scheduler::tool_dispatch::ToolHostStepDispatcher::dispatch_step`.
pub struct SingleUseLifecycle {
    sandbox: Arc<dyn SandboxBackend>,
}

impl SingleUseLifecycle {
    pub fn new(sandbox: Arc<dyn SandboxBackend>) -> Self {
        Self { sandbox }
    }
}

#[async_trait]
impl WorkerLifecycleManager for SingleUseLifecycle {
    async fn acquire(&self, entry: &ToolEntry) -> Result<WorkerHandle, ToolHostError> {
        // Per-call clone of the base policy so concurrent dispatches against the same
        // `ToolEntry` cannot mutate each other's policy. The clone matches the discipline
        // the pre-refactor inline path used.
        let policy = entry.policy.clone();
        let program = entry.binary.to_string_lossy().into_owned();
        let spec = WorkerSpec {
            policy: &policy,
            program: &program,
            args: &[],
            wall_clock_ms: entry.wall_clock_ms,
        };
        let worker = spawn_worker(self.sandbox.as_ref(), &spec)?;
        Ok(WorkerHandle::single_use(worker))
    }
}

/// Idle-timeout lifecycle stub.
///
/// **Slice 1 declares this type so downstream code can name it; runtime invocation
/// panics with `unimplemented!()`.** The `acquire` body intentionally panics rather than
/// returning an error so any accidental wiring of an idle-timeout worker into slice 1's
/// daemon trips loudly on the first request rather than silently falling through to a
/// `SPAWN_FAILED` audit row.
///
/// Slice 2 (the GLiNER-Relex prereq) replaces this body with the spawn-on-demand /
/// post-completion-cap / crash-recovery runtime per the spec.
pub struct IdleTimeoutLifecycle {
    _private: (),
}

impl IdleTimeoutLifecycle {
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for IdleTimeoutLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WorkerLifecycleManager for IdleTimeoutLifecycle {
    async fn acquire(&self, _entry: &ToolEntry) -> Result<WorkerHandle, ToolHostError> {
        unimplemented!(
            "idle_timeout lifecycle runtime — slice 2; \
             slice 1 ships SingleUseLifecycle only"
        )
    }
}
```

- [ ] **Step 4: Run the tests to verify the manager surface compiles**

```sh
source "$HOME/.cargo/env"
cargo build -p kastellan-core 2>&1 | tail -20
```

Expected: ONE failure — the test module refers to `ToolEntry { ..., lifecycle: lc }` but `ToolEntry` doesn't yet carry the `lifecycle` field. That's Task 3's work. The production code in `manager.rs` itself should compile clean (it only refers to `ToolEntry`'s existing fields). If you see other compile errors, fix them before proceeding.

- [ ] **Step 5: Commit the manager surface (tests intentionally still failing)**

```sh
git add core/src/worker_lifecycle/manager.rs core/src/worker_lifecycle/mod.rs
git commit -m "$(cat <<'EOF'
feat(core,worker_lifecycle): manager trait + SingleUseLifecycle + IdleTimeout stub

Adds runtime layer for slice 1:
  * `WorkerLifecycleManager` async trait (`acquire(entry) -> WorkerHandle`)
  * `WorkerHandle::worker_mut()` exposes `&mut SupervisedWorker` for dispatch
  * `SingleUseLifecycle` — production impl; spawn-per-acquire; behaviour
    byte-equivalent to today's `ToolHostStepDispatcher::dispatch_step` path
  * `IdleTimeoutLifecycle` — stub; `acquire()` calls `unimplemented!()` so a
    wiring mistake trips loudly. Slice 2 fills it in.

The two manager tests in this commit assume Task 3's `ToolEntry.lifecycle`
field; they currently fail to compile and turn green once Task 3 lands.
EOF
)"
```

---

## Task 3: `ToolEntry` gains `lifecycle: Lifecycle` field

**Files:**
- Modify: `core/src/scheduler/tool_dispatch.rs` (struct definition, `shell_exec_entry`, in-module `fake_entry` test fixture, in-module `tests` assertions)

- [ ] **Step 1: Update `ToolEntry` and `shell_exec_entry`**

In `core/src/scheduler/tool_dispatch.rs`, modify the `ToolEntry` struct (currently at lines 93-106) to add the new field:

```rust
#[derive(Clone, Debug)]
pub struct ToolEntry {
    /// Absolute path to the worker binary on the host. Bound into the
    /// jail by `policy.fs_read` (or via the worker prelude's Landlock
    /// allowlist — see `derive_lockdown_env`).
    pub binary: PathBuf,
    /// Base sandbox policy. Cloned per call. Per-step overrides (e.g.
    /// a per-step scratch dir) would mutate the clone before passing
    /// to `spawn_worker`.
    pub policy: SandboxPolicy,
    /// Wall-clock budget for the entire worker process lifetime, in
    /// milliseconds. `None` disables the watchdog. See
    /// [`WorkerSpec::wall_clock_ms`] for the semantics.
    pub wall_clock_ms: Option<u64>,
    /// Lifecycle policy. Defaults to [`Lifecycle::SingleUse`] (current
    /// behaviour); inference workers in slice 2+ will declare
    /// [`Lifecycle::IdleTimeout`].
    pub lifecycle: crate::worker_lifecycle::Lifecycle,
}
```

In the same file, modify `shell_exec_entry` (currently at lines 159-180) to set the new field explicitly. Find the `ToolEntry { ... }` literal at the bottom of the function and add the `lifecycle:` line:

```rust
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(30_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
    }
```

In the same file, modify the in-module test fixture `fake_entry()` (currently at lines 560-569) to add the new field:

```rust
    fn fake_entry() -> ToolEntry {
        ToolEntry {
            binary: PathBuf::from("/usr/local/bin/fake"),
            policy: SandboxPolicy {
                mem_mb: 32,
                ..SandboxPolicy::default()
            },
            wall_clock_ms: Some(5_000),
            lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        }
    }
```

- [ ] **Step 2: Add a pin test that shell-exec is single-use**

In the in-module `tests` module of `core/src/scheduler/tool_dispatch.rs`, somewhere alongside `shell_exec_entry_carries_allowlist_in_env`, add:

```rust
    #[test]
    fn shell_exec_entry_declares_single_use_lifecycle() {
        // Shell-exec must remain single-use forever — per-request isolation IS its
        // security model. If a future change to `shell_exec_entry` accidentally swaps
        // this for `IdleTimeout`, this test trips so the regression is caught at PR time
        // rather than in production.
        let entry = shell_exec_entry(PathBuf::from("/x"), &[]);
        assert!(matches!(
            entry.lifecycle,
            crate::worker_lifecycle::Lifecycle::SingleUse
        ));
    }
```

- [ ] **Step 3: Run the in-module tests + the manager tests**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib scheduler::tool_dispatch 2>&1 | tail -15
cargo test -p kastellan-core --lib worker_lifecycle 2>&1 | tail -15
```

Expected for `tool_dispatch`: all existing tests pass + the new `shell_exec_entry_declares_single_use_lifecycle` passes. Expected for `worker_lifecycle::manager`: now the `idle_timeout_lifecycle_acquire_panics_until_slice_2` test runs and the `#[should_panic]` assertion turns green; `single_use_lifecycle_constructor_holds_the_sandbox_backend` and the `worker_handle_exposes_worker_mut` shape pin pass.

- [ ] **Step 4: Build the workspace to surface anything we missed**

```sh
source "$HOME/.cargo/env"
cargo build --workspace 2>&1 | tail -25
```

Expected: ONE failure in `core/tests/scheduler_step_dispatch_e2e.rs` (the `broken-tool` `ToolEntry { ... }` literal at lines 382-393 is missing the new `lifecycle` field). That's Task 4's work.

- [ ] **Step 5: Commit**

```sh
git add core/src/scheduler/tool_dispatch.rs
git commit -m "$(cat <<'EOF'
feat(core,scheduler,tool_dispatch): ToolEntry carries Lifecycle field

Adds `lifecycle: Lifecycle` to `ToolEntry`; defaults to `SingleUse` for
shell-exec (declared explicitly in `shell_exec_entry`). The in-module
`fake_entry()` fixture is updated to match.

New pin test `shell_exec_entry_declares_single_use_lifecycle` locks
shell-exec to single-use forever — per-request isolation IS its security
model, so an accidental switch to `IdleTimeout` should trip at PR time.

The two `worker_lifecycle::manager` tests added in the previous commit
now run green: this commit was their missing dependency.
EOF
)"
```

---

## Task 4: `ToolHostStepDispatcher` routes through `WorkerLifecycleManager`

**Files:**
- Modify: `core/src/scheduler/tool_dispatch.rs` (`ToolHostStepDispatcher` struct + `new` + `dispatch_step`, drop the unused `Profile` import if applicable, drop the unused `WorkerSpec` import once the inline spawn block goes away)
- Modify: `core/tests/scheduler_step_dispatch_e2e.rs` (the `broken-tool` literal + the dispatcher's `new` call site)

- [ ] **Step 1: Update `ToolHostStepDispatcher` struct + constructor**

In `core/src/scheduler/tool_dispatch.rs`, replace the `ToolHostStepDispatcher` struct (currently at lines 305-319) with:

```rust
/// Production [`StepDispatcher`]: looks up `step.tool` in a
/// [`ToolRegistry`], asks the [`WorkerLifecycleManager`] for a
/// [`WorkerHandle`], calls [`tool_host::dispatch`], and maps the result
/// into a [`StepOutcome`].
///
/// **Slice-1 architecture note:** the previous version held an
/// `Arc<dyn SandboxBackend>` and called `spawn_worker` inline. That
/// spawn path now lives behind the [`WorkerLifecycleManager::acquire`]
/// seam so slice 2 can swap `SingleUseLifecycle` for an idle-timeout
/// pool without touching this struct.
///
/// Cheap to clone (all fields are `Arc`/`PgPool`); the daemon's
/// scheduler holds a single instance and the inner loop calls
/// `dispatch_step` directly on it.
pub struct ToolHostStepDispatcher {
    pool: PgPool,
    lifecycle: Arc<dyn crate::worker_lifecycle::WorkerLifecycleManager>,
    registry: Arc<ToolRegistry>,
}

impl ToolHostStepDispatcher {
    pub fn new(
        pool: PgPool,
        lifecycle: Arc<dyn crate::worker_lifecycle::WorkerLifecycleManager>,
        registry: Arc<ToolRegistry>,
    ) -> Self {
        Self { pool, lifecycle, registry }
    }
}
```

- [ ] **Step 2: Rewrite `dispatch_step` to delegate spawning to the lifecycle manager**

In `core/src/scheduler/tool_dispatch.rs`, replace the body of `dispatch_step` (currently at lines 322 onwards). Keep the `step.unknown_tool` short-circuit block byte-identical; rewrite the policy-clone + spawn block (currently lines 372-426) to route through the manager. The final shape (only the changed middle section shown for brevity; the unknown-tool block above and the `map_dispatch_result` block below stay exactly as they are):

Find this section (lines 372-426 in the pre-slice file):

```rust
        // Per-call clone of the base policy. Per-step overrides (e.g.
        // a fresh scratch dir) would mutate the clone before spawn;
        // none today, but the seam exists.
        let policy = entry.policy.clone();
        let program = entry.binary.to_string_lossy().into_owned();
        let spec = WorkerSpec {
            policy: &policy,
            program: &program,
            args: &[],
            wall_clock_ms: entry.wall_clock_ms,
        };

        let mut worker = match spawn_worker(self.sandbox.as_ref(), &spec) {
            Ok(w) => w,
            Err(e) => {
                // ... SPAWN_FAILED audit block ...
                return StepOutcome::Err {
                    code: "SPAWN_FAILED".into(),
                    detail: err_string,
                };
            }
        };

        let result = dispatch(
            &self.pool,
            &mut worker,
            &step.tool,
            &step.method,
            step.parameters.clone(),
        )
        .await;
```

Replace it with:

```rust
        // Slice-1 lifecycle seam: `SingleUseLifecycle::acquire` does the same
        // `spawn_worker(self.sandbox.as_ref(), &spec)` call inline as the old code
        // did. The `ToolHostError` it returns is byte-equivalent to the previous
        // direct-call shape, so the `SPAWN_FAILED` audit path below is unchanged.
        // Slice 2's `IdleTimeoutLifecycle` will instead return an `Err(ToolHostError)`
        // only on real spawn failures; warm-cache hits never reach this `match` arm
        // at all.
        let mut handle = match self.lifecycle.acquire(entry).await {
            Ok(h) => h,
            Err(e) => {
                let err_string = e.to_string();
                tracing::error!(
                    tool = %step.tool, method = %step.method, error = %err_string,
                    "ToolHostStepDispatcher: lifecycle.acquire failed"
                );

                let elapsed_ms = started.elapsed().as_millis() as u64;
                let payload = build_scheduler_step_failure_payload(
                    &step.tool,
                    &step.method,
                    step.parameters.clone(),
                    Some(&err_string),
                    elapsed_ms,
                );
                if let Err(audit_err) = kastellan_db::audit::insert(
                    &self.pool,
                    SCHEDULER_AUDIT_ACTOR,
                    ACTION_STEP_SPAWN_FAILED,
                    payload,
                )
                .await
                {
                    tracing::error!(
                        tool = %step.tool, method = %step.method, error = %audit_err,
                        "step.spawn_failed audit_log INSERT failed; outcome still propagated"
                    );
                }

                return StepOutcome::Err {
                    code: "SPAWN_FAILED".into(),
                    detail: err_string,
                };
            }
        };

        let result = dispatch(
            &self.pool,
            handle.worker_mut(),
            &step.tool,
            &step.method,
            step.parameters.clone(),
        )
        .await;
```

The lines further down (`// Drop closes stdio + cancels the watchdog.` etc.) stay exactly as they are — `handle` drops at end of scope just like the previous `worker` did, and `WorkerHandle`'s default Drop drops the inner `SupervisedWorker`.

- [ ] **Step 3: Drop the now-unused `spawn_worker` and `WorkerSpec` imports if they're not used elsewhere in the file**

After the rewrite, `spawn_worker` and `WorkerSpec` are no longer referenced in `tool_dispatch.rs` (the manager owns them). Check the imports at the top of the file:

```sh
grep -n "use crate::tool_host" /home/hherb/src/kastellan/core/src/scheduler/tool_dispatch.rs
```

If it shows the previous line `use crate::tool_host::{dispatch, spawn_worker, ToolHostError, WorkerSpec};`, narrow it to:

```rust
use crate::tool_host::{dispatch, ToolHostError};
```

(`ToolHostError` is still used by `map_dispatch_result`. `dispatch` is still called above.)

- [ ] **Step 4: Update the e2e test's broken-tool fixture and dispatcher constructor**

In `core/tests/scheduler_step_dispatch_e2e.rs`, find the `broken-tool` `ToolEntry { ... }` literal at lines 382-393 and add the new field. The block becomes:

```rust
        registry.insert(
            "broken-tool",
            ToolEntry {
                binary: worker.clone(),
                policy: SandboxPolicy {
                    // Relative path here is the rejection trigger; both
                    // sandbox backends validate absolute-path-ness before
                    // doing anything else.
                    fs_read: vec![PathBuf::from("relative/path/triggers/rejection")],
                    mem_mb: 32,
                    ..SandboxPolicy::default()
                },
                wall_clock_ms: Some(5_000),
                lifecycle: kastellan_core::worker_lifecycle::Lifecycle::SingleUse,
            },
        );
```

Then update the dispatcher's `new` call site (lines 399-403 in the pre-slice file). The previous shape was:

```rust
        let sandbox = sandbox_arc();
        let dispatcher = ToolHostStepDispatcher::new(
            pool.clone(),
            sandbox,
            registry,
        );
```

Replace it with:

```rust
        let sandbox = sandbox_arc();
        let lifecycle: std::sync::Arc<dyn kastellan_core::worker_lifecycle::WorkerLifecycleManager> =
            std::sync::Arc::new(kastellan_core::worker_lifecycle::SingleUseLifecycle::new(
                sandbox,
            ));
        let dispatcher = ToolHostStepDispatcher::new(
            pool.clone(),
            lifecycle,
            registry,
        );
```

- [ ] **Step 5: Verify the workspace builds**

```sh
source "$HOME/.cargo/env"
cargo build --workspace 2>&1 | tail -25
```

Expected: clean build (zero warnings, zero errors). The remaining failure in `main.rs` from Task 3's check is the next step.

- [ ] **Step 6: Commit**

```sh
git add core/src/scheduler/tool_dispatch.rs core/tests/scheduler_step_dispatch_e2e.rs
git commit -m "$(cat <<'EOF'
feat(core,scheduler,tool_dispatch): route ToolHostStepDispatcher through lifecycle manager

ToolHostStepDispatcher's `sandbox: Arc<dyn SandboxBackend>` field is replaced
by `lifecycle: Arc<dyn WorkerLifecycleManager>`. `dispatch_step` now asks the
manager for a `WorkerHandle` instead of calling `spawn_worker` directly.

For shell-exec (declared SingleUse) the byte-equivalent spawn path remains;
the manager's `SingleUseLifecycle::acquire` performs the same `spawn_worker`
call the dispatcher used to do inline. The SPAWN_FAILED audit row continues
to be emitted on `ToolHostError` from acquire — the chokepoint posture is
unchanged.

The integration test `scheduler_step_dispatch_e2e` updates its
`broken-tool` literal + its dispatcher `new` call to match.
EOF
)"
```

---

## Task 5: Wire `SingleUseLifecycle` into `core::main`

**Files:**
- Modify: `core/src/main.rs` (lines 147-154 — the dispatcher's construction)

- [ ] **Step 1: Update the daemon's dispatcher wiring**

In `core/src/main.rs`, find the existing dispatcher block (lines 147-154):

```rust
    let dispatcher: Arc<dyn kastellan_core::scheduler::inner_loop::StepDispatcher> =
        Arc::new(
            kastellan_core::scheduler::tool_dispatch::ToolHostStepDispatcher::new(
                pool.clone(),
                sandbox.clone(),
                tool_registry,
            ),
        );
```

Replace it with:

```rust
    // Slice 1 of the worker-lifecycle work (spec
    // `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md`)
    // puts the spawn-per-request path behind a manager. `SingleUseLifecycle` is
    // the only impl shipped in slice 1; shell-exec declares
    // `Lifecycle::SingleUse` so behaviour is byte-equivalent to pre-slice main.
    let lifecycle: Arc<dyn kastellan_core::worker_lifecycle::WorkerLifecycleManager> =
        Arc::new(kastellan_core::worker_lifecycle::SingleUseLifecycle::new(
            sandbox.clone(),
        ));

    let dispatcher: Arc<dyn kastellan_core::scheduler::inner_loop::StepDispatcher> =
        Arc::new(
            kastellan_core::scheduler::tool_dispatch::ToolHostStepDispatcher::new(
                pool.clone(),
                lifecycle,
                tool_registry,
            ),
        );
```

- [ ] **Step 2: Verify the daemon binary builds**

```sh
source "$HOME/.cargo/env"
cargo build --workspace 2>&1 | tail -10
```

Expected: clean build.

- [ ] **Step 3: Commit**

```sh
git add core/src/main.rs
git commit -m "$(cat <<'EOF'
feat(core,main): wire SingleUseLifecycle into the daemon's dispatcher

`kastellan`'s main now constructs `SingleUseLifecycle` from the existing
sandbox backend and passes it to `ToolHostStepDispatcher::new`. Behaviour
for shell-exec is byte-equivalent to pre-slice main — `SingleUseLifecycle::acquire`
calls the same `spawn_worker` the dispatcher used to call inline.

Slice-1 wiring is now complete end-to-end: type → manager → dispatcher → daemon.
EOF
)"
```

---

## Task 6: Full workspace regression run

**Files:**
- No code changes — this task verifies the slice as a whole.

- [ ] **Step 1: Run the full workspace test suite**

```sh
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | tail -40
```

Expected: **733+ passed, 0 failed, 4 ignored** (baseline 721 + Task 1's 5 tests + Task 2's 3 tests + Task 3's 1 test = 730+; rounding accounts for any additional tests added during implementation). Zero `[SKIP]` lines on Linux (per the AppArmor profile installed via `scripts/linux/install-bwrap-apparmor-profile.sh`).

If `[SKIP]` lines appear on Linux, the bwrap AppArmor profile is missing — see CLAUDE.md "Linux host setup". Do not proceed until skips are zero, otherwise the integration tests are providing a false green.

- [ ] **Step 2: Confirm there are no new warnings**

```sh
source "$HOME/.cargo/env"
cargo build --workspace 2>&1 | grep -c "warning:"
```

Expected: `0`.

- [ ] **Step 3: Spot-check the headline integration test for slice 1**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test scheduler_step_dispatch_e2e 2>&1 | tail -10
```

Expected: all 4 dispatch scenarios pass (happy path, POLICY_DENIED, UNKNOWN_TOOL, SPAWN_FAILED).

- [ ] **Step 4: Spot-check the `cli_ask_e2e` end-to-end pin**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test cli_ask_e2e 2>&1 | tail -10
```

Expected: both `cli_ask_e2e` scenarios pass. This is the strongest regression pin slice 1 has — it exercises the full production chain (real `kastellan-cli` → real `kastellan` daemon → real `SingleUseLifecycle::acquire` → real `kastellan-worker-shell-exec` → real Postgres → mock LLM only).

---

## Task 7: HANDOVER + ROADMAP + final docs commit

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md` (top header + new "Recently completed" entry + "Next TODO" update)
- Modify: `docs/devel/ROADMAP.md` (tick the worker-lifecycle entry under Phase 1)

- [ ] **Step 1: Update HANDOVER.md header fields**

Bump the `**Last updated:**` and `**Last commit on `main`:**` lines at the top to reflect today's session. Add a new "Recently completed (this session, 2026-05-18 — worker lifecycle slice 1)" section near the top of the "Recently completed" stack, describing:

- Branch name (`feat/worker-lifecycle-slice-1`)
- Test count delta (baseline 721 → final number)
- What's in the slice: `Lifecycle` enum + `WorkerLifecycleManager` trait + `SingleUseLifecycle` (production) + `IdleTimeoutLifecycle` (stub, `unimplemented!()`)
- What's deliberately NOT in the slice: idle_timeout runtime, worker manifest plumbing, GLiNER-Relex
- What unblocks next: slice 2 (idle_timeout runtime) → GLiNER-Relex worker

Move the "Worker lifecycle policy — implementation slice 1" bullet out of the "Next TODO" section. The next pickup becomes slice 2 (idle_timeout runtime) OR — equally valid — entity extraction v1 implementation, the L3 skill crystallisation spec, or the macOS micro-VM spike. Restate them in priority order at the top of the "Next TODO" list.

- [ ] **Step 2: Tick the ROADMAP entry**

In `docs/devel/ROADMAP.md` under "Phase 1 — Memory & Loop", find the Worker-lifecycle-policy unchecked bullet (currently `- [ ] **Worker lifecycle policy (design spec only — 2026-05-18)** — ...`). Either:

- **Replace** with a new bullet of the form `- [x] **Worker lifecycle policy — slice 1 (single_use runtime + idle_timeout types)** — landed 2026-05-18 on branch `feat/worker-lifecycle-slice-1` (N commits, merged via PR #?? at `<hash>`). ...`
- **Or keep** the original "design spec only" bullet as-is and add a new `- [x]` bullet directly below it marking slice 1 done — depends on whether the operator wants the design-spec milestone tracked separately. Use precedent from the L1 promotion writer entry (which has both `- [x] **L1 memory layer (storage primitive)**` and a separate later entry for the writer).

Recommended: add a new `- [x]` bullet so the design-spec landing is preserved as its own milestone.

- [ ] **Step 3: Commit the docs**

```sh
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "$(cat <<'EOF'
docs(handover,roadmap): worker lifecycle slice 1 — single_use runtime + idle_timeout types

Records slice 1 of the worker-lifecycle work shipped this session:
  * `core::worker_lifecycle` module with `Lifecycle` enum, manager trait,
    `SingleUseLifecycle` (production), `IdleTimeoutLifecycle` (stub)
  * `ToolHostStepDispatcher` routes through the manager; behaviour
    byte-equivalent for shell-exec
  * Test count <baseline> → <final>

Next pickup options now headline the TODO list.
EOF
)"
```

---

## Self-review (done after writing the plan, before handing off)

**1. Spec coverage**

Section-by-section against `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md`:

- §"The two policies" — `single_use` (Task 2 `SingleUseLifecycle`) + `idle_timeout` types (Task 1 + Task 2 stub).
- §"Cap-check semantics" — caps types defined in Task 1; runtime in slice 2.
- §"The stateless contract" — `Contract { stateless: bool }` + validated constructor rejecting `stateless=false` under idle_timeout (Task 1 test 5).
- §"Manifest schema additions" — deferred (slice 1 ships `Lifecycle` on `ToolEntry`, not a TOML manifest; matches user recommendation D3).
- §"Supervisor responsibilities" — Task 2 covers `SingleUseLifecycle::acquire`; slices 2+ cover queue / cap eval / crash detect / health.
- §"Security model" — preserved by construction (per-worker sandbox unchanged; `SingleUseLifecycle` calls the same `spawn_worker`).
- §"Migration plan" — shell-exec keeps current behaviour via `Lifecycle::SingleUse` in `shell_exec_entry` (Task 3).

**2. Placeholder scan**

- No "TBD", "TODO", "implement later" in the plan.
- All test bodies have actual code.
- All file paths are exact.

**3. Type consistency**

- `Lifecycle::SingleUse` / `Lifecycle::IdleTimeout { caps, contract }` — used identically across Tasks 1, 3, 4, 5.
- `WorkerLifecycleManager::acquire(&self, entry: &ToolEntry) -> Result<WorkerHandle, ToolHostError>` — used identically across Tasks 2, 4.
- `WorkerHandle::worker_mut(&mut self) -> &mut SupervisedWorker` — used identically across Tasks 2, 4.

**4. Risk register**

- **`async_trait` macro**: already a dependency (`scheduler::tool_dispatch::StepDispatcher` uses it). No new dep needed.
- **`SandboxBackend` trait**: pub-exported from `kastellan_sandbox`; both `SingleUseLifecycle` and the existing `tool_host::spawn_worker` consume `&dyn SandboxBackend`, so no boxing/lifetime issues.
- **Drop order on `WorkerHandle`**: Rust drops fields in declaration order, then runs the struct's own `Drop`. With one field (`worker: SupervisedWorker`), the default Drop is byte-equivalent to today's `let mut worker = …; … // worker drops at end of scope` behaviour.
- **`tool_host::dispatch` chokepoint**: unchanged. `WorkerHandle::worker_mut` exposes `&mut SupervisedWorker`, not the module-private `WorkerCommand` constructor. The seal still holds.

---

## Worktree setup (do this before Task 1)

This slice modifies several files in `core/` plus tests. Per the project convention, work happens in a git worktree off `main`. The shape:

```sh
# From the main repo at /home/hherb/src/kastellan
git fetch origin
git checkout -b feat/worker-lifecycle-slice-1 main
# Or use a worktree if preferred:
# git worktree add ../kastellan-feat-worker-lifecycle-slice-1 -b feat/worker-lifecycle-slice-1 main
# cd ../kastellan-feat-worker-lifecycle-slice-1
```

Verify clean baseline before starting:

```sh
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | tail -5
```

Expected: 721 passed (or whatever number is current on `main`), 0 failed, 0 [SKIP] lines.

If the baseline isn't clean, fix or skip the failing test before starting slice 1 — never let a pre-existing failure mask a regression introduced by this slice.

---

## Out of scope (filed as slice-2+ pickups, not implemented here)

- **Idle-timeout runtime.** Spawn-on-demand, post-completion cap eval, idle teardown, crash recovery, request queuing. The whole supervisor-responsibilities section of the spec.
- **Worker manifest plumbing.** Whether manifests are TOML files on disk or Rust consts is open question 1 in the spec. Slice 1 ships `Lifecycle` directly on `ToolEntry`; slice 2 (or a parallel slice) can add a `WorkerManifest` struct and a registration pipeline.
- **GLiNER-Relex worker.** Per the spec's "Next slice" section, this is the next-next slice that consumes slice 2's `idle_timeout` runtime.
- **macOS `container` micro-VM spike** (issue #55). Independent of this slice.
- **`kastellan-cli.rs` (1419 LOC) split**. Independent of this slice.
- **Migrating `kastellan-supervisor`** (the OS-unit-installer crate) into anything resembling worker lifecycle management. The naming overlap with the spec's "supervisor" wording is irrelevant; the OS-unit crate stays untouched.
