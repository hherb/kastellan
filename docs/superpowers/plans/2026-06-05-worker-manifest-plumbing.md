# Worker Manifest Plumbing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the hardcoded per-worker branches in `registry_build.rs` with a uniform `WorkerManifest` trait that each worker implements, driven by a single static list and one builder loop, plus a `current_exe()`-relative binary-discovery default.

**Architecture:** A new `worker_manifest` module defines a `WorkerManifest` trait whose pure `resolve(ctx)` returns `Register(ToolEntry)` / `Disabled` / `Misconfigured`. Each worker provides an impl in its own host-side module. A pure `assemble_registry` helper iterates a static `WORKER_MANIFESTS` list and builds the `ToolRegistry`; the async `build_tool_registry` shell pre-fetches DB allowlists and constructs the real `ResolveCtx` around it. Behaviour-preserving: every produced `ToolEntry` is byte-identical to today's.

**Tech Stack:** Rust (hhagent-core lib+bin), `sqlx`/Postgres for the allowlist fetch, `tracing` for logs. Tests are closure-injected pure units plus the existing PG-backed integration pins.

**Spec:** [`docs/superpowers/specs/2026-06-05-worker-manifest-plumbing-design.md`](../specs/2026-06-05-worker-manifest-plumbing-design.md)

---

## File Structure

| File | Responsibility |
|------|----------------|
| `core/src/worker_manifest.rs` *(new)* | `WorkerManifest` trait, `Resolution` enum, `ResolveCtx`, `discover_binary` helper. No worker-specific knowledge. |
| `core/src/workers/shell_exec.rs` *(new)* | `ShellExecManifest` impl + the relocated `shell_exec_entry` constructor (re-exported from `tool_dispatch` to preserve `scheduler::shell_exec_entry`). |
| `core/src/workers/gliner_relex.rs` *(modify)* | add `GlinerRelexManifest` impl wrapping the existing `resolve_env` + `gliner_relex_entry`. |
| `core/src/workers/gliner_relex/tests.rs` *(modify)* | add `GlinerRelexManifest::resolve` unit tests. |
| `core/src/workers/mod.rs` *(modify)* | `pub mod shell_exec;` + refresh the module doc (the TOML-deferred note is now resolved). |
| `core/src/registry_build.rs` *(modify)* | `WORKER_MANIFESTS` static, pure `assemble_registry`, rewritten async `build_tool_registry(pool, exe_dir)`; delete `build_gliner_relex_entry` + `log_gliner_relex_skip`. |
| `core/src/scheduler/tool_dispatch.rs` *(modify)* | remove `shell_exec_entry` body; add `pub use crate::workers::shell_exec::shell_exec_entry;` re-export. |
| `core/src/lib.rs` *(modify)* | `pub mod worker_manifest;`. |
| `core/src/main.rs` *(modify)* | drop the `build_gliner_relex_entry()` pre-call; compute `exe_dir` via `current_exe()`; call `build_tool_registry(&pool, exe_dir)`; read the gliner entry back via `tool_registry.lookup(...)` for the extractor. |

**Layering note:** `WORKER_MANIFESTS` lives in `registry_build.rs` (next to its only consumer `assemble_registry`), not in `worker_manifest.rs` as the spec sketch showed — this keeps `worker_manifest.rs` free of any dependency on the concrete `workers::*` impls (clean one-way dependency: `registry_build` → `worker_manifest` + `workers::*`; `workers::*` → `worker_manifest`). Functionally identical.

**Key reference — today's `shell_exec_entry` body (relocated verbatim in Task 2):**
```rust
pub fn shell_exec_entry(binary: PathBuf, allowlist: &[String]) -> ToolEntry {
    let allow_json = serde_json::to_string(allowlist)
        .expect("serializing Vec<String> never fails");
    let policy = SandboxPolicy {
        fs_read: vec![binary.clone()],
        fs_write: vec![],
        net: Net::Deny,
        cpu_ms: 5_000,
        mem_mb: 256,
        profile: Profile::WorkerStrict,
        env: vec![("HHAGENT_SHELL_ALLOWLIST".to_string(), allow_json)],
        cpu_quota_pct: None,
        tasks_max: None,
    };
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(30_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: None,
        container_image: None,
    }
}
```

**Build/test prelude (every Run command assumes this once per shell):**
```sh
source "$HOME/.cargo/env"
```

---

## Task 1: `worker_manifest` module — trait, `Resolution`, `ResolveCtx`, `discover_binary`

**Files:**
- Create: `core/src/worker_manifest.rs`
- Modify: `core/src/lib.rs` (add `pub mod worker_manifest;` in alphabetical-ish position, after `pub mod tool_host;`)

- [ ] **Step 1: Register the module**

In `core/src/lib.rs`, add after line `pub mod tool_host;`:
```rust
pub mod worker_manifest;
```

- [ ] **Step 2: Write the module with the failing tests**

Create `core/src/worker_manifest.rs`:
```rust
//! Uniform, declarative worker self-description.
//!
//! Each worker implements [`WorkerManifest`]; the daemon iterates a static
//! list of them at startup (see [`crate::registry_build`]) to build the
//! [`crate::scheduler::ToolRegistry`], replacing the hardcoded per-worker
//! branches that used to live in `registry_build.rs`.
//!
//! Design: `docs/superpowers/specs/2026-06-05-worker-manifest-plumbing-design.md`.

use std::path::{Path, PathBuf};

use crate::scheduler::ToolEntry;

/// A worker's self-description. One impl per worker, living in that worker's
/// host-side module. `resolve` is **pure** — every input arrives via
/// [`ResolveCtx`], so each impl is unit-testable with fakes (no `std::env`,
/// no real filesystem access inside the impl).
pub trait WorkerManifest: Sync {
    /// Tool name the registry/planner keys on (e.g. `"shell-exec"`).
    fn name(&self) -> &'static str;

    /// If this worker needs the operational argv allowlist from the
    /// `tool_allowlists` DB table, the tool name to query (usually
    /// `== name()`). `None` ⇒ no allowlist. The async fetch stays in the
    /// builder; the result is threaded into [`ResolveCtx::allowlist`].
    fn allowlist_tool(&self) -> Option<&'static str> {
        None
    }

    /// Pure resolution: host env + fs probes + pre-fetched allowlist → outcome.
    fn resolve(&self, ctx: &ResolveCtx<'_>) -> Resolution;
}

/// The three outcomes every worker produces, unified so the builder logs each
/// at one consistent severity.
pub enum Resolution {
    /// Resolved → insert this entry into the registry.
    Register(ToolEntry),
    /// Intentionally absent (e.g. feature flag off). Logged at INFO.
    Disabled { detail: String },
    /// Wanted to register but its environment is broken (missing binary,
    /// missing weights dir). Logged at ERROR; the daemon still starts
    /// (fail-soft — same posture as today).
    Misconfigured { detail: String },
}

/// Minimal, *universal* resolve inputs — deliberately not a per-worker kitchen
/// sink. Arbitrary worker-specific config arrives through `get_env` (the
/// universal extension point), so adding an exotic worker never widens this
/// struct.
pub struct ResolveCtx<'a> {
    /// Read an environment variable. Injected (not `std::env`) so resolvers
    /// are pure and unit-testable with a fake env.
    pub get_env: &'a dyn Fn(&str) -> Option<String>,
    /// Probe: does this path exist?
    pub exists: &'a dyn Fn(&Path) -> bool,
    /// Probe: is this path a directory?
    pub is_dir: &'a dyn Fn(&Path) -> bool,
    /// Directory of the running `hhagent` binary, for `current_exe()`-relative
    /// worker discovery. `None` when it can't be determined (fail-soft).
    pub exe_dir: Option<&'a Path>,
    /// Operational argv allowlist, pre-fetched from the DB by the builder,
    /// keyed by tool name. A worker that declared `allowlist_tool()` looks
    /// itself up here; absent ⇒ empty.
    pub allowlist: &'a dyn Fn(&str) -> Vec<String>,
}

/// Locate a worker binary. Precedence:
///   1. the explicit override env var (e.g. `"HHAGENT_SHELL_EXEC_BIN"`) if it
///      names an existing file — preserves every current deployment/test;
///   2. else the exe-relative sibling default `<exe_dir>/<default_name>`, if
///      it exists.
/// Returns `None` when neither yields an existing file (the caller maps that
/// to [`Resolution::Misconfigured`]).
pub fn discover_binary(
    ctx: &ResolveCtx<'_>,
    override_env: &str,
    default_name: &str,
) -> Option<PathBuf> {
    if let Some(raw) = (ctx.get_env)(override_env) {
        let p = PathBuf::from(raw);
        if (ctx.exists)(&p) {
            return Some(p);
        }
        // Override set but missing: fall through to the sibling default
        // rather than hard-failing (design §4 precedence).
    }
    if let Some(dir) = ctx.exe_dir {
        let p = dir.join(default_name);
        if (ctx.exists)(&p) {
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Build a ResolveCtx from simple closures for discovery tests. The
    /// allowlist closure is unused here (returns empty).
    fn ctx<'a>(
        get_env: &'a dyn Fn(&str) -> Option<String>,
        exists: &'a dyn Fn(&Path) -> bool,
        exe_dir: Option<&'a Path>,
    ) -> ResolveCtx<'a> {
        ResolveCtx {
            get_env,
            exists,
            is_dir: &|_p| false,
            exe_dir,
            allowlist: &|_t| Vec::new(),
        }
    }

    #[test]
    fn override_env_pointing_at_existing_file_wins_over_sibling() {
        let get_env = |k: &str| (k == "OVERRIDE").then(|| "/opt/custom/worker".to_string());
        // Both the override path AND the sibling exist; override must win.
        let exists = |_p: &Path| true;
        let exe = PathBuf::from("/usr/bin");
        let c = ctx(&get_env, &exists, Some(&exe));
        assert_eq!(
            discover_binary(&c, "OVERRIDE", "worker"),
            Some(PathBuf::from("/opt/custom/worker"))
        );
    }

    #[test]
    fn no_override_falls_back_to_exe_relative_sibling() {
        let get_env = |_k: &str| None;
        let exe = PathBuf::from("/usr/bin");
        let sibling = exe.join("worker");
        let exists = move |p: &Path| p == sibling.as_path();
        let c = ctx(&get_env, &exists, Some(&exe));
        assert_eq!(
            discover_binary(&c, "OVERRIDE", "worker"),
            Some(PathBuf::from("/usr/bin/worker"))
        );
    }

    #[test]
    fn neither_override_nor_sibling_exists_returns_none() {
        let get_env = |_k: &str| None;
        let exists = |_p: &Path| false;
        let exe = PathBuf::from("/usr/bin");
        let c = ctx(&get_env, &exists, Some(&exe));
        assert_eq!(discover_binary(&c, "OVERRIDE", "worker"), None);
    }

    #[test]
    fn missing_exe_dir_uses_override_only_and_does_not_panic() {
        let get_env = |k: &str| (k == "OVERRIDE").then(|| "/opt/worker".to_string());
        let exists = |_p: &Path| true;
        let c = ctx(&get_env, &exists, None);
        assert_eq!(
            discover_binary(&c, "OVERRIDE", "worker"),
            Some(PathBuf::from("/opt/worker"))
        );

        // And with no override + no exe_dir → None, still no panic.
        let get_env2 = |_k: &str| None;
        let c2 = ctx(&get_env2, &exists, None);
        assert_eq!(discover_binary(&c2, "OVERRIDE", "worker"), None);
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail to compile / fail**

Run: `cargo test -p hhagent-core --lib worker_manifest`
Expected: at this point the module is new and complete, so it should COMPILE and PASS. If a prior partial edit left it failing, fix until green. (The "failing first" discipline is satisfied by writing tests alongside the first real behavior; `discover_binary` is the unit under test.)

- [ ] **Step 4: Run the full lib build + clippy to confirm no dead-code/warning regressions**

Run: `cargo build -p hhagent-core && cargo clippy -p hhagent-core --all-targets --locked -- -D warnings`
Expected: exit 0. (`Resolution`/`ResolveCtx`/`WorkerManifest` are `pub` lib API, so they are not flagged as dead code despite having no consumer yet.)

- [ ] **Step 5: Commit**

```bash
git add core/src/worker_manifest.rs core/src/lib.rs
git commit -m "feat(core): WorkerManifest trait + discover_binary helper"
```

---

## Task 2: `ShellExecManifest` + relocate `shell_exec_entry`

**Files:**
- Create: `core/src/workers/shell_exec.rs`
- Modify: `core/src/workers/mod.rs` (add `pub mod shell_exec;`, refresh doc)
- Modify: `core/src/scheduler/tool_dispatch.rs` (delete the `shell_exec_entry` body lines 207–231; add a re-export)

- [ ] **Step 1: Create the new module with the manifest + relocated constructor + failing tests**

Create `core/src/workers/shell_exec.rs`:
```rust
//! Host-side manifest + `ToolEntry` constructor for the shell-exec worker.

use std::path::PathBuf;

use hhagent_sandbox::{Net, Profile, SandboxPolicy};

use crate::scheduler::ToolEntry;
use crate::worker_manifest::{discover_binary, ResolveCtx, Resolution, WorkerManifest};

/// Tool name the registry keys shell-exec on.
const TOOL_NAME: &str = "shell-exec";
/// Operator override for the worker binary path.
const BIN_ENV: &str = "HHAGENT_SHELL_EXEC_BIN";
/// Exe-relative sibling default (cargo `target/debug` + flat installs).
const DEFAULT_BIN_NAME: &str = "hhagent-worker-shell-exec";

/// Build the [`ToolEntry`] for the shell-exec worker. The administrator
/// controls the argv allowlist (sourced from the `tool_allowlists` DB table by
/// the daemon); the LLM-supplied `step.parameters` cannot widen it.
///
/// Defaults: `Net::Deny`, `Profile::WorkerStrict` (no `socket(2)`), `cpu_ms =
/// 5_000`, `mem_mb = 256`, `wall_clock_ms = Some(30_000)`, `SingleUse`.
pub fn shell_exec_entry(binary: PathBuf, allowlist: &[String]) -> ToolEntry {
    let allow_json = serde_json::to_string(allowlist)
        .expect("serializing Vec<String> never fails");
    let policy = SandboxPolicy {
        fs_read: vec![binary.clone()],
        fs_write: vec![],
        net: Net::Deny,
        cpu_ms: 5_000,
        mem_mb: 256,
        profile: Profile::WorkerStrict,
        env: vec![("HHAGENT_SHELL_ALLOWLIST".to_string(), allow_json)],
        cpu_quota_pct: None,
        tasks_max: None,
    };
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(30_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: None,
        container_image: None,
    }
}

/// shell-exec's manifest. Discovery: `HHAGENT_SHELL_EXEC_BIN` override wins,
/// else the exe-relative sibling `hhagent-worker-shell-exec`.
pub struct ShellExecManifest;

impl WorkerManifest for ShellExecManifest {
    fn name(&self) -> &'static str {
        TOOL_NAME
    }

    fn allowlist_tool(&self) -> Option<&'static str> {
        Some(TOOL_NAME)
    }

    fn resolve(&self, ctx: &ResolveCtx<'_>) -> Resolution {
        let binary = match discover_binary(ctx, BIN_ENV, DEFAULT_BIN_NAME) {
            Some(b) => b,
            None => {
                return Resolution::Misconfigured {
                    detail: format!(
                        "{BIN_ENV} unset/missing and no sibling {DEFAULT_BIN_NAME} found"
                    ),
                };
            }
        };
        let allowlist = (ctx.allowlist)(TOOL_NAME);
        Resolution::Register(shell_exec_entry(binary, &allowlist))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn ctx<'a>(
        get_env: &'a dyn Fn(&str) -> Option<String>,
        exists: &'a dyn Fn(&Path) -> bool,
        allowlist: &'a dyn Fn(&str) -> Vec<String>,
    ) -> ResolveCtx<'a> {
        ResolveCtx {
            get_env,
            exists,
            is_dir: &|_p| false,
            exe_dir: None,
            allowlist,
        }
    }

    #[test]
    fn resolve_registers_with_byte_identical_policy() {
        let get_env = |k: &str| (k == BIN_ENV).then(|| "/opt/shell-exec".to_string());
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["ls".to_string(), "cat".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);

        match ShellExecManifest.resolve(&c) {
            Resolution::Register(entry) => {
                // Same shape the daemon shipped before this slice.
                assert_eq!(entry.binary, PathBuf::from("/opt/shell-exec"));
                assert_eq!(entry.policy.fs_read, vec![PathBuf::from("/opt/shell-exec")]);
                assert!(entry.policy.fs_write.is_empty());
                assert_eq!(entry.policy.cpu_ms, 5_000);
                assert_eq!(entry.policy.mem_mb, 256);
                assert_eq!(entry.wall_clock_ms, Some(30_000));
                // Allowlist packed as JSON into the env, key unchanged.
                let (k, v) = &entry.policy.env[0];
                assert_eq!(k, "HHAGENT_SHELL_ALLOWLIST");
                assert_eq!(v, r#"["ls","cat"]"#);
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn resolve_misconfigured_when_no_binary_found() {
        let get_env = |_k: &str| None;
        let exists = |_p: &Path| false;
        let allowlist = |_t: &str| Vec::new();
        let c = ctx(&get_env, &exists, &allowlist);

        match ShellExecManifest.resolve(&c) {
            Resolution::Misconfigured { detail } => {
                assert!(detail.contains("hhagent-worker-shell-exec"), "detail: {detail}");
            }
            other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
        }
    }

    fn outcome_label(r: &Resolution) -> &'static str {
        match r {
            Resolution::Register(_) => "Register",
            Resolution::Disabled { .. } => "Disabled",
            Resolution::Misconfigured { .. } => "Misconfigured",
        }
    }
}
```

- [ ] **Step 2: Register the submodule**

In `core/src/workers/mod.rs`, add `pub mod shell_exec;` (after `pub mod gliner_relex;`) and update the module doc paragraph that currently reads "Manifests stay as Rust functions ... the TOML-manifest-on-disk option is deferred" to:
```rust
//! Each submodule owns one worker's host-side manifest — a
//! [`crate::worker_manifest::WorkerManifest`] impl plus its
//! [`crate::scheduler::ToolEntry`] constructor and the request/response serde
//! types that pin its JSON-RPC wire contract. Manifests are Rust (compiled in,
//! not on-disk TOML) per the 2026-06-05 worker-manifest-plumbing design.
```

- [ ] **Step 3: Remove the old `shell_exec_entry` body and re-export the relocated one**

In `core/src/scheduler/tool_dispatch.rs`, delete the entire `pub fn shell_exec_entry(...) { ... }` definition (the doc-comment block + body, ~lines 195–231) and replace it with a re-export so `scheduler::tool_dispatch::shell_exec_entry` and `scheduler::shell_exec_entry` keep resolving:
```rust
// `shell_exec_entry` now lives in `crate::workers::shell_exec` (the worker
// owns its own manifest + constructor). Re-exported here so the existing
// `scheduler::tool_dispatch::shell_exec_entry` / `scheduler::shell_exec_entry`
// paths are unchanged for callers.
pub use crate::workers::shell_exec::shell_exec_entry;
```
Leave the `result_mapping` and `SCHEDULER_AUDIT_ACTOR` items that follow untouched. Remove any now-unused imports in `tool_dispatch.rs` that were only used by the deleted body (`Net`, `Profile`, `SandboxPolicy` — check whether other code in the file still uses them before removing; `PathBuf` and `ToolEntry` are still used elsewhere).

- [ ] **Step 4: Run the new tests to verify they pass**

Run: `cargo test -p hhagent-core --lib workers::shell_exec`
Expected: PASS (2 tests).

- [ ] **Step 5: Run the lib build + clippy to confirm the relocation didn't break paths**

Run: `cargo build -p hhagent-core && cargo clippy -p hhagent-core --all-targets --locked -- -D warnings`
Expected: exit 0. (Watch for unused-import warnings in `tool_dispatch.rs` — remove any import that the deleted body owned.)

- [ ] **Step 6: Commit**

```bash
git add core/src/workers/shell_exec.rs core/src/workers/mod.rs core/src/scheduler/tool_dispatch.rs
git commit -m "feat(core): ShellExecManifest; relocate shell_exec_entry to workers::shell_exec"
```

---

## Task 3: `assemble_registry` pure helper

**Files:**
- Modify: `core/src/registry_build.rs` (add `assemble_registry` + tests; keep all existing helpers)

- [ ] **Step 1: Write the failing tests**

In `core/src/registry_build.rs`, extend the existing `#[cfg(test)] mod tests` with a fake manifest and assembly tests. Add at the top of the `mod tests` block:
```rust
    use crate::worker_manifest::{ResolveCtx, Resolution, WorkerManifest};
    use std::path::{Path, PathBuf};

    /// A fake worker for assembly tests. `outcome` selects which arm
    /// `resolve` returns; `allowlist_name` (if Some) is reported from
    /// `allowlist_tool()` so the prefetch-keying path is exercised.
    struct FakeManifest {
        name: &'static str,
        outcome: FakeOutcome,
        allowlist_name: Option<&'static str>,
    }
    enum FakeOutcome {
        Register,
        Disabled,
        Misconfigured,
    }
    impl WorkerManifest for FakeManifest {
        fn name(&self) -> &'static str {
            self.name
        }
        fn allowlist_tool(&self) -> Option<&'static str> {
            self.allowlist_name
        }
        fn resolve(&self, ctx: &ResolveCtx<'_>) -> Resolution {
            match self.outcome {
                FakeOutcome::Register => Resolution::Register(
                    crate::workers::shell_exec::shell_exec_entry(
                        PathBuf::from(format!("/fake/{}", self.name)),
                        &(ctx.allowlist)(self.name),
                    ),
                ),
                FakeOutcome::Disabled => Resolution::Disabled { detail: "off".into() },
                FakeOutcome::Misconfigured => {
                    Resolution::Misconfigured { detail: "broken".into() }
                }
            }
        }
    }

    fn test_ctx<'a>(allowlist: &'a dyn Fn(&str) -> Vec<String>) -> ResolveCtx<'a> {
        ResolveCtx {
            get_env: &|_k| None,
            exists: &|_p: &Path| false,
            is_dir: &|_p: &Path| false,
            exe_dir: None,
            allowlist,
        }
    }

    #[test]
    fn assemble_inserts_registered_and_records_allowlist_hash() {
        let allowlist = |t: &str| {
            if t == "alpha" {
                vec!["ls".to_string()]
            } else {
                Vec::new()
            }
        };
        let ctx = test_ctx(&allowlist);
        let m_alpha = FakeManifest {
            name: "alpha",
            outcome: FakeOutcome::Register,
            allowlist_name: Some("alpha"),
        };
        let manifests: &[&dyn WorkerManifest] = &[&m_alpha];

        let (reg, loaded) = assemble_registry(manifests, &ctx);

        assert!(reg.lookup("alpha").is_some(), "alpha should be registered");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "alpha");
        assert_eq!(loaded[0].allowlist_len, 1);
        assert_eq!(loaded[0].allowlist_sha256, sha256_argv0_list(&["ls".to_string()]));
        assert_eq!(loaded[0].binary, "/fake/alpha");
    }

    #[test]
    fn assemble_skips_disabled_and_misconfigured_without_recording() {
        let allowlist = |_t: &str| Vec::new();
        let ctx = test_ctx(&allowlist);
        let m_off = FakeManifest {
            name: "off",
            outcome: FakeOutcome::Disabled,
            allowlist_name: None,
        };
        let m_bad = FakeManifest {
            name: "bad",
            outcome: FakeOutcome::Misconfigured,
            allowlist_name: None,
        };
        let manifests: &[&dyn WorkerManifest] = &[&m_off, &m_bad];

        let (reg, loaded) = assemble_registry(manifests, &ctx);

        assert!(reg.lookup("off").is_none());
        assert!(reg.lookup("bad").is_none());
        assert!(loaded.is_empty(), "skipped workers produce no records");
    }
```

- [ ] **Step 2: Run to verify the tests fail (function not defined)**

Run: `cargo test -p hhagent-core --lib registry_build::tests::assemble`
Expected: FAIL — `cannot find function assemble_registry in this scope`.

- [ ] **Step 3: Implement `assemble_registry`**

In `core/src/registry_build.rs`, add the imports near the top (with the existing `use` lines):
```rust
use crate::worker_manifest::{ResolveCtx, Resolution, WorkerManifest};
```
and add the pure helper (place it after `build_registry_loaded_payload`):
```rust
/// Pure assembly: iterate a worker-manifest list against a fully-built
/// [`ResolveCtx`] and produce the registry + the per-tool records for the
/// `registry.loaded` audit row. No async, no DB — unit-testable with fakes.
///
/// `Register` ⇒ insert + record + INFO log; `Disabled` ⇒ INFO log only;
/// `Misconfigured` ⇒ ERROR log only (the daemon still starts — fail-soft).
pub fn assemble_registry(
    manifests: &[&dyn WorkerManifest],
    ctx: &ResolveCtx<'_>,
) -> (ToolRegistry, Vec<LoadedToolRecord>) {
    let mut reg = ToolRegistry::new();
    let mut loaded: Vec<LoadedToolRecord> = Vec::new();
    for m in manifests {
        match m.resolve(ctx) {
            Resolution::Register(entry) => {
                let name = m.name();
                let allowlist = (ctx.allowlist)(name);
                tracing::info!(
                    tool = name,
                    binary = %entry.binary.display(),
                    allowlist_len = allowlist.len(),
                    "registering tool"
                );
                loaded.push(LoadedToolRecord {
                    name: name.to_string(),
                    binary: entry.binary.display().to_string(),
                    allowlist_len: allowlist.len(),
                    allowlist_sha256: sha256_argv0_list(&allowlist),
                });
                reg.insert(name, entry);
            }
            Resolution::Disabled { detail } => {
                tracing::info!(tool = m.name(), %detail, "worker disabled; skipping");
            }
            Resolution::Misconfigured { detail } => {
                tracing::error!(tool = m.name(), %detail, "worker misconfigured; skipping");
            }
        }
    }
    (reg, loaded)
}
```

- [ ] **Step 4: Run to verify the tests pass**

Run: `cargo test -p hhagent-core --lib registry_build::tests`
Expected: PASS (the two new assemble tests + the pre-existing `sha256_argv0_list...` and `build_registry_loaded_payload...` tests).

- [ ] **Step 5: Commit**

```bash
git add core/src/registry_build.rs
git commit -m "feat(core): pure assemble_registry over a WorkerManifest list"
```

---

## Task 4: `GlinerRelexManifest`

**Files:**
- Modify: `core/src/workers/gliner_relex.rs` (add the impl + a `skip_detail` helper)
- Modify: `core/src/workers/gliner_relex/tests.rs` (add resolve tests)

- [ ] **Step 1: Write the failing tests**

In `core/src/workers/gliner_relex/tests.rs`, add:
```rust
    use crate::worker_manifest::{ResolveCtx, Resolution, WorkerManifest};
    use std::path::Path;

    /// Build a ResolveCtx whose env is a closure over a fixed map. fs probes
    /// are supplied per-test; allowlist is unused for gliner (returns empty).
    fn gliner_ctx<'a>(
        get_env: &'a dyn Fn(&str) -> Option<String>,
        is_dir: &'a dyn Fn(&Path) -> bool,
        exists: &'a dyn Fn(&Path) -> bool,
    ) -> ResolveCtx<'a> {
        ResolveCtx {
            get_env,
            exists,
            is_dir,
            exe_dir: None,
            allowlist: &|_t| Vec::new(),
        }
    }

    #[test]
    fn manifest_disabled_when_enable_flag_absent() {
        let get_env = |_k: &str| None;
        let is_dir = |_p: &Path| false;
        let exists = |_p: &Path| false;
        let c = gliner_ctx(&get_env, &is_dir, &exists);
        match GlinerRelexManifest.resolve(&c) {
            Resolution::Disabled { .. } => {}
            _ => panic!("expected Disabled when HHAGENT_GLINER_RELEX_ENABLE unset"),
        }
    }

    #[test]
    fn manifest_misconfigured_when_weights_dir_env_missing() {
        let get_env =
            |k: &str| (k == "HHAGENT_GLINER_RELEX_ENABLE").then(|| "1".to_string());
        let is_dir = |_p: &Path| false;
        let exists = |_p: &Path| false;
        let c = gliner_ctx(&get_env, &is_dir, &exists);
        match GlinerRelexManifest.resolve(&c) {
            Resolution::Misconfigured { detail } => {
                assert!(detail.contains("HHAGENT_GLINER_RELEX_WEIGHTS_DIR"), "detail: {detail}");
            }
            _ => panic!("expected Misconfigured when weights dir env missing"),
        }
    }

    #[test]
    fn manifest_registers_on_happy_path() {
        // enable=1, weights dir is a dir, explicit venv dir, shim exists.
        let get_env = |k: &str| match k {
            "HHAGENT_GLINER_RELEX_ENABLE" => Some("1".to_string()),
            "HHAGENT_GLINER_RELEX_WEIGHTS_DIR" => Some("/weights".to_string()),
            "HHAGENT_GLINER_RELEX_VENV_DIR" => Some("/data/.venv".to_string()),
            _ => None,
        };
        let is_dir = |p: &Path| p == Path::new("/weights");
        // resolve_env checks the shim path `<venv>/bin/hhagent-worker-gliner-relex`.
        let exists = |p: &Path| p == Path::new("/data/.venv/bin/hhagent-worker-gliner-relex");
        let c = gliner_ctx(&get_env, &is_dir, &exists);
        match GlinerRelexManifest.resolve(&c) {
            Resolution::Register(entry) => {
                // gliner is IdleTimeout, not SingleUse — pins the lifecycle wiring.
                assert!(
                    matches!(entry.lifecycle, crate::worker_lifecycle::Lifecycle::IdleTimeout { .. }),
                    "gliner must register IdleTimeout"
                );
            }
            _ => panic!("expected Register on the happy path"),
        }
    }
```
> Note: confirm the exact shim sub-path `resolve_env` checks (`<venv_dir>/bin/hhagent-worker-gliner-relex`) by reading `core/src/workers/gliner_relex.rs` around the `ScriptShimMissing` construction, and match the `exists` closure to it.

- [ ] **Step 2: Run to verify the tests fail**

Run: `cargo test -p hhagent-core --lib workers::gliner_relex::tests::manifest`
Expected: FAIL — `cannot find ... GlinerRelexManifest`.

- [ ] **Step 3: Implement the manifest**

In `core/src/workers/gliner_relex.rs`, add (after `gliner_relex_entry` / near the `ResolveSkipReason` definitions):
```rust
/// gliner-relex's host-side manifest. Wraps the existing pure `resolve_env`
/// (env → `GlinerRelexEnv`) + `gliner_relex_entry` (env → `ToolEntry`),
/// mapping its typed skip reasons onto the uniform [`Resolution`] outcomes.
pub struct GlinerRelexManifest;

impl crate::worker_manifest::WorkerManifest for GlinerRelexManifest {
    fn name(&self) -> &'static str {
        Client::TOOL_NAME
    }

    // No argv allowlist: gliner-relex is a single stateless inference service,
    // not an argv-dispatch worker. (allowlist_tool defaults to None.)

    fn resolve(
        &self,
        ctx: &crate::worker_manifest::ResolveCtx<'_>,
    ) -> crate::worker_manifest::Resolution {
        use crate::worker_manifest::Resolution;
        match resolve_env(
            |k| (ctx.get_env)(k),
            |p| (ctx.is_dir)(p),
            |p| (ctx.exists)(p),
        ) {
            Ok(env) => Resolution::Register(gliner_relex_entry(&env)),
            Err(ResolveSkipReason::Disabled) => Resolution::Disabled {
                detail: "HHAGENT_GLINER_RELEX_ENABLE != \"1\"".to_string(),
            },
            Err(other) => Resolution::Misconfigured {
                detail: gliner_skip_detail(&other),
            },
        }
    }
}

/// Human-readable detail for a non-`Disabled` skip reason. Mirrors the
/// messages the deleted `registry_build::log_gliner_relex_skip` emitted, so
/// the operator log wording is unchanged.
fn gliner_skip_detail(reason: &ResolveSkipReason) -> String {
    match reason {
        ResolveSkipReason::Disabled => {
            // Handled by the Disabled arm above; included for exhaustiveness.
            "HHAGENT_GLINER_RELEX_ENABLE != \"1\"".to_string()
        }
        ResolveSkipReason::WeightsDirEnvMissing => {
            "HHAGENT_GLINER_RELEX_WEIGHTS_DIR unset".to_string()
        }
        ResolveSkipReason::WeightsDirNotADir { path } => {
            format!("weights dir missing on disk: {}", path.display())
        }
        ResolveSkipReason::VenvDirUnresolvable => {
            "venv dir unresolvable (HHAGENT_GLINER_RELEX_VENV_DIR, \
             HHAGENT_DATA_DIR, and HOME all unset)"
                .to_string()
        }
        ResolveSkipReason::ScriptShimMissing { path } => {
            format!("venv shim missing: {}", path.display())
        }
    }
}
```

- [ ] **Step 4: Run to verify the tests pass**

Run: `cargo test -p hhagent-core --lib workers::gliner_relex::tests`
Expected: PASS (3 new manifest tests + the existing gliner unit tests).

- [ ] **Step 5: Commit**

```bash
git add core/src/workers/gliner_relex.rs core/src/workers/gliner_relex/tests.rs
git commit -m "feat(core): GlinerRelexManifest wrapping resolve_env"
```

---

## Task 5: Wire the builder + main.rs; delete the old helpers

**Files:**
- Modify: `core/src/registry_build.rs` (`WORKER_MANIFESTS`, rewrite `build_tool_registry`, delete `build_gliner_relex_entry` + `log_gliner_relex_skip`)
- Modify: `core/src/main.rs` (compute `exe_dir`; new call signature; read gliner entry from the registry for the extractor)

- [ ] **Step 1: Add the static list + rewrite the builder**

In `core/src/registry_build.rs`:

Add the static list near the top (after imports):
```rust
/// Every worker the daemon may register. Adding a worker = add its
/// `WorkerManifest` impl + one line here. Order is irrelevant (the registry
/// is a keyed map).
pub static WORKER_MANIFESTS: &[&dyn WorkerManifest] = &[
    &crate::workers::shell_exec::ShellExecManifest,
    &crate::workers::gliner_relex::GlinerRelexManifest,
];
```

Replace the entire existing `pub async fn build_tool_registry(...)` body with:
```rust
/// Build the registry of tools the scheduler may dispatch by resolving every
/// [`WORKER_MANIFESTS`] entry against the host environment. Pre-fetches each
/// manifest's argv allowlist from the `tool_allowlists` DB table (the only
/// async step), then delegates to the pure [`assemble_registry`].
///
/// `exe_dir` (the directory of the running `hhagent` binary, from
/// `current_exe()`) seeds the exe-relative sibling discovery default; pass
/// `None` to disable that fallback (override-env-only).
///
/// **Writes no audit row** — returns the per-tool records so the daemon can
/// write `registry.loaded` itself.
pub async fn build_tool_registry(
    pool: &sqlx::PgPool,
    exe_dir: Option<std::path::PathBuf>,
) -> Result<(ToolRegistry, Vec<LoadedToolRecord>), hhagent_db::DbError> {
    use std::collections::HashMap;
    use std::path::Path;

    // 1. Pre-fetch allowlists for every manifest that declares one.
    let mut allowlists: HashMap<String, Vec<String>> = HashMap::new();
    for m in WORKER_MANIFESTS {
        if let Some(tool) = m.allowlist_tool() {
            let al = hhagent_db::tool_allowlists::list_for_tool(pool, tool)
                .await
                .map_err(|e| {
                    hhagent_db::DbError::Query(format!("loading {tool} allowlist: {e}"))
                })?;
            allowlists.insert(tool.to_string(), al);
        }
    }

    // Preserve the deprecation breadcrumb for the retired env-var allowlist.
    if std::env::var_os("HHAGENT_SHELL_EXEC_ALLOWLIST").is_some() {
        tracing::warn!(
            "HHAGENT_SHELL_EXEC_ALLOWLIST is no longer honored; \
             use 'hhagent-cli tools allowlist add <tool> <argv0>' to populate the DB"
        );
    }

    // 2. Build the real ResolveCtx over std::env + the live filesystem.
    let get_env = |k: &str| std::env::var(k).ok();
    let exists = |p: &Path| p.exists();
    let is_dir = |p: &Path| p.is_dir();
    let allowlist = |tool: &str| allowlists.get(tool).cloned().unwrap_or_default();
    let ctx = ResolveCtx {
        get_env: &get_env,
        exists: &exists,
        is_dir: &is_dir,
        exe_dir: exe_dir.as_deref(),
        allowlist: &allowlist,
    };

    // 3. Pure assembly.
    Ok(assemble_registry(WORKER_MANIFESTS, &ctx))
}
```

Delete `build_gliner_relex_entry` and `log_gliner_relex_skip` entirely (their logic now lives in `GlinerRelexManifest`). Remove imports that only they used.

- [ ] **Step 2: Update `main.rs`**

In `core/src/main.rs`:

Delete the `let gliner_relex_entry = hhagent_core::registry_build::build_gliner_relex_entry();` line (and shrink the surrounding comment block that explained the double-use, since the entry is no longer pre-resolved here).

Compute the exe dir and call the new builder signature:
```rust
    // Directory of the running `hhagent` binary — seeds exe-relative sibling
    // discovery so plain workers (e.g. shell-exec) are found in a flat install
    // with no HHAGENT_*_BIN env set. None (rare current_exe() failure) ⇒
    // override-env-only discovery.
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));

    let (registry, loaded_tool_records) =
        hhagent_core::registry_build::build_tool_registry(&pool, exe_dir).await?;
    let tool_registry = Arc::new(registry);
```

Replace the extractor `match gliner_relex_entry { ... }` (around line 182) with a read-back from the now-authoritative registry:
```rust
    let entity_extractor: Arc<dyn hhagent_core::entity_extraction::EntityExtractor> =
        match tool_registry
            .lookup(hhagent_core::workers::gliner_relex::Client::TOOL_NAME)
            .cloned()
        {
            Some(entry) => {
                tracing::info!(
                    target: "hhagent::main",
                    "gliner-relex configured; constructing v2 entity extractor",
                );
                let client = hhagent_core::workers::gliner_relex::Client::new(
                    lifecycle.clone(),
                    pool.clone(),
                    entry,
                );
                Arc::new(
                    hhagent_core::entity_extraction::gliner_relex::GlinerRelexExtractor::new(
                        client,
                        pool.clone(),
                    ),
                )
            }
            None => {
                tracing::warn!(
                    target: "hhagent::main",
                    "gliner-relex not configured; using NoOpEntityExtractor (graph lane disabled)",
                );
                Arc::new(hhagent_core::entity_extraction::NoOpEntityExtractor::new())
            }
        };
```

- [ ] **Step 3: Build the whole workspace**

Run: `cargo build --workspace`
Expected: exit 0. Fix any remaining references to the deleted `build_gliner_relex_entry` / old `build_tool_registry` arity.

- [ ] **Step 4: Run the core lib + the registry/registry-reading integration tests**

Run: `cargo test -p hhagent-core --lib`
Expected: PASS.

Run (live PG required; on the DGX): `cargo test -p hhagent-core --test cli_ask_e2e`
Expected: PASS — the `registry.loaded` summary-row assertions still see exactly one row, proving the audit payload is unchanged.

- [ ] **Step 5: Commit**

```bash
git add core/src/registry_build.rs core/src/main.rs
git commit -m "feat(core): drive registry build from WORKER_MANIFESTS; read gliner entry back for extractor"
```

---

## Task 6: Zero-env discovery payoff test (the production-convention proof)

**Files:**
- Modify: `core/src/registry_build.rs` (one more test in `mod tests`, exercising the REAL `WORKER_MANIFESTS`)

- [ ] **Step 1: Write the failing test**

This proves the headline payoff: with `HHAGENT_SHELL_EXEC_BIN` unset, shell-exec still registers via the exe-relative sibling default. It is pure (no PG): it calls `assemble_registry` directly against the real manifest list with an injected ctx whose `exe_dir` holds a fake sibling.

Add to `core/src/registry_build.rs`'s `mod tests`:
```rust
    #[test]
    fn shell_exec_registers_with_no_override_env_via_exe_sibling() {
        let exe_dir = PathBuf::from("/install/bin");
        let sibling = exe_dir.join("hhagent-worker-shell-exec");
        // No HHAGENT_SHELL_EXEC_BIN; only the sibling exists.
        let get_env = |_k: &str| None;
        let exists = {
            let sibling = sibling.clone();
            move |p: &Path| p == sibling.as_path()
        };
        let allowlist = |_t: &str| Vec::new();
        let ctx = ResolveCtx {
            get_env: &get_env,
            exists: &exists,
            is_dir: &|_p: &Path| false,
            exe_dir: Some(exe_dir.as_path()),
            allowlist: &allowlist,
        };

        // Real manifest list. gliner is Disabled (no enable flag) and skipped.
        let (reg, loaded) = assemble_registry(WORKER_MANIFESTS, &ctx);

        let entry = reg
            .lookup("shell-exec")
            .expect("shell-exec must register from the exe-relative sibling with no env override");
        assert_eq!(entry.binary, sibling);
        assert!(reg.lookup("gliner-relex").is_none(), "gliner disabled → not registered");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "shell-exec");
    }
```

- [ ] **Step 2: Run to verify it passes**

Run: `cargo test -p hhagent-core --lib registry_build::tests::shell_exec_registers_with_no_override_env_via_exe_sibling`
Expected: PASS. (If it fails because `WORKER_MANIFESTS`/`assemble_registry` aren't in scope of the test module, add `use super::*;` — already present in the existing `mod tests`.)

- [ ] **Step 3: Commit**

```bash
git add core/src/registry_build.rs
git commit -m "test(core): shell-exec registers via exe-sibling default with no env override"
```

---

## Task 7: Full verification + handover/roadmap update

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md`

- [ ] **Step 1: Full workspace test (native Linux / live PG on the DGX)**

Run: `cargo test --workspace`
Expected: green, zero `[SKIP]`, count = prior 1297 baseline + the new units (4 discover + 2 shell-exec + 2 assemble + 3 gliner-manifest + 1 zero-env = 12 new). Note the exact passed/failed/ignored/`[SKIP]` counts for the handover.

- [ ] **Step 2: Clippy gate**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: exit 0.

- [ ] **Step 3: Update HANDOVER.md**

Bump the header (`Last updated`, current-state commit, `Session-end verification` counts). Move the worker-manifest slice into "Recently completed" with: the `WorkerManifest` trait + `discover_binary` convention, the per-worker impls, the `assemble_registry`/`build_tool_registry(pool, exe_dir)` rewrite, the main.rs registry-read-back for the extractor, and that it resolved worker-lifecycle open question 1 + advanced discovery open question 6. Update the "Working state" `registry_build`/`workers` descriptions. Write a fresh "Next TODO".

- [ ] **Step 4: Tick ROADMAP.md**

Mark worker-manifest plumbing (item 11) done with the merge commit hash; note open question 1 resolved.

- [ ] **Step 5: Commit**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs(handover): worker manifest plumbing shipped (item 11)"
```

- [ ] **Step 6: Open the PR**

```bash
git push -u origin feat/worker-manifest-plumbing
gh pr create --fill
```

---

## Notes for the implementer

- **Behaviour-preserving is the contract.** The two existing workers must produce byte-identical `ToolEntry`s. The only intentional new behaviour is the additive exe-sibling discovery fallback (override still wins). If `cli_ask_e2e` / `shell_exec_e2e` / `cli_memory_l3_run_daemon_e2e` change behaviour, something is wrong.
- **`shell_exec_entry` re-export is load-bearing.** Many call sites use `scheduler::shell_exec_entry`; the `pub use` in `tool_dispatch.rs` keeps them compiling. Don't delete it.
- **gliner stays env-driven.** Its `resolve` does NOT call `discover_binary` — it owns its venv/weights resolution via the existing `resolve_env`. Only plain compiled workers use the sibling default.
- **`exe_dir` is computed by `main.rs`, not inside the builder** — keeps `current_exe()` in one place and makes `build_tool_registry` injectable for any future integration test.
- **File-size watch:** `registry_build.rs` grows (assemble + builder + tests). If it crosses ~500 LOC, lift the `mod tests` block to a sibling `registry_build/tests.rs` as a final step (matches the codebase test-lift pattern).
