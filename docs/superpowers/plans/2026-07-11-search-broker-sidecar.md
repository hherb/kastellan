# Search-broker sidecar Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a force-routed, jailed `web-search` worker reach a loopback SearxNG through a trusted host-netns **search-broker** sidecar over a bound UDS, keeping the worker's direct network egress at zero — by generalizing the merged embed-broker into a kind-parameterized broker abstraction and adding a thin search broker.

**Architecture:** Generalize `core/src/embed_broker/` → `core/src/broker/` with a `BrokerKind` enum (`Embed`/`Search`) that supplies every per-kind string constant (binary name, env keys, socket file, scratch prefix). One `spawn_broker`, one `BrokerSidecar`, one `BrokerConfigs` registry, one `entry.broker` field, one spawn chokepoint. Add `kastellan-worker-search-broker` (mirrors `kastellan-worker-embed-broker`) and a worker-side `SearchProvider` seam (`Direct`/`Brokered`, mirroring the web-research `Embedder` seam). Full unification (spec "open decision" resolved to option 1).

**Tech Stack:** Rust workspace; `kastellan-sandbox`, `kastellan-protocol` (line-delimited JSON-RPC), `kastellan-worker-web-common` (`search`/`parse`/`http`), `kastellan-worker-prelude` (`lock_down`). No new dependencies.

## Global Constraints

- **AGPL-compatible deps only.** This feature adds **none** (reuses existing crates).
- **Cross-platform Linux + macOS.** No OS-gating except the pre-existing Firecracker paths and the Linux-only DGX e2e (Task 8). The broker runs host-netns on both.
- **rustc:** source `"$HOME/.cargo/env"` first (`cargo` not on the non-interactive PATH).
- **FOREGROUND cargo only, per-crate.** Never background a `cargo` job and Monitor-wait on it (documented wedge). Use `cargo test -p <crate> --lib` / `cargo test -p <crate> <name>`. No `&`, no polling.
- **Stage specific files.** `git add <exact paths>`, never `git add -A` (untracked `docs/essay-medium-draft.md` + lockfiles must stay out).
- **TDD.** Every behavior change is RED → GREEN. Renames are guarded by the existing (re-pointed) tests.
- **Each task ends green:** `cargo build -p <crate>` + `cargo clippy -p <crate> --all-targets -- -D warnings` + the task's tests, all clean.
- **Security invariant (do not regress):** a broker-mode worker has an empty `Net::Allowlist` and reaches the network only via the broker UDS; the broker's only egress is the single backend host; force-routing/SSRF are untouched; a broker-declaring worker with no discovered broker config **fails closed** (refuses to spawn).
- **Behavior-preserving for embed:** web-research's embed path must stay byte-identical in effect. `BrokerKind::Embed` reproduces every current string constant exactly (`KASTELLAN_EMBED_BROKER_UDS`, `embed.sock`, `embed-` prefix, `KASTELLAN_EMBED_BROKER_ENDPOINT`, `KASTELLAN_EMBED_BROKER_BIN`, `KASTELLAN_EMBED_BROKER_SCRATCH_DIR`).

---

## File Structure

**Created:**
- `workers/search-broker/Cargo.toml` — new crate manifest (mirrors `workers/embed-broker/Cargo.toml`).
- `workers/search-broker/src/lib.rs` — `SearchHandler` + `serve_connection` + caps.
- `workers/search-broker/src/main.rs` — broker binary entrypoint.
- `core/src/broker/kind.rs` — the `BrokerKind` enum (new file inside the renamed module).

**Renamed (git mv, then edited):**
- `core/src/embed_broker/` → `core/src/broker/` (`mod.rs`, `config.rs`, `spawn.rs`).

**Modified (core):**
- `core/src/lib.rs` — `pub mod embed_broker;` → `pub mod broker;`.
- `sandbox/src/lib.rs`, `sandbox/src/linux_bwrap.rs`, `sandbox/src/macos_seatbelt.rs` (+ their test modules), `sandbox/tests/macos_container_smoke.rs` — field rename.
- `core/src/scheduler/tool_dispatch.rs` (+ `tests.rs`) — `ToolEntry.embed_broker` → `ToolEntry.broker: Option<BrokerSpec>`.
- `core/src/worker_lifecycle/{force_route.rs,manager.rs,composite.rs,idle_timeout.rs}` (+ their test modules) — `BrokerConfigs` threading + chokepoint.
- `core/src/main.rs` — discover both configs into `BrokerConfigs`.
- `core/src/egress/scratch_sweep.rs` — add `SEARCH_SCRATCH_DIR_PREFIX`.
- `core/src/tool_host.rs`, `core/src/sandbox_health.rs`, `core/src/workers/*.rs` — mechanical `embed_broker:`/`embed_broker_uds:` field renames.
- `core/src/workers/web_research.rs` — `BrokerSpec::embed(..)`.
- `core/src/workers/web_search.rs` — broker-mode entry.
- `core/tests/embed_broker_egress_e2e.rs`, `core/tests/embed_broker_spawn_e2e.rs`, `core/tests/lifecycle_container_routing_e2e.rs`, `core/tests/kv_demo_firecracker_persistent_e2e.rs` — re-point renamed symbols.

**Modified (workers):**
- `workers/web-common/src/parse.rs` — `Hit` gains `Deserialize`.
- `workers/web-search/src/handler.rs` — `SearchProvider` seam + provider selection.
- `Cargo.toml` (workspace) — add `workers/search-broker` member.

**New (test/e2e):**
- `core/tests/search_broker_egress_e2e.rs` — Linux/DGX-gated zero-egress e2e (Task 8).
- `scripts/web-search/dgx-search-broker-cutover.md` — cutover runbook (Task 8).

---

## Task 1: Sandbox field `embed_broker_uds` → `broker_uds`

**Files:**
- Modify: `sandbox/src/lib.rs` (field def ~178, default ~203, test ~661)
- Modify: `sandbox/src/linux_bwrap.rs` (bind ~131/241, tests ~361-497)
- Modify: `sandbox/src/macos_seatbelt.rs` (~136/315/507) + `sandbox/src/macos_seatbelt/tests.rs`
- Modify: `sandbox/tests/macos_container_smoke.rs`
- Modify (mechanical, callers): every `embed_broker_uds:` initializer in `core/src/**` (from the enumerated grep list below)

**Interfaces:**
- Produces: `SandboxPolicy.broker_uds: Option<PathBuf>` (was `embed_broker_uds`); identical semantics, bind logic, and validation. The validation error label becomes `"broker_uds"`.

This is a pure rename guarded by the existing sandbox tests — no behavior change.

- [ ] **Step 1: Rename the field + all references in the sandbox crate**

In `sandbox/src/lib.rs`: rename the struct field and its `Default`/constructor initializer:
```rust
    /// When `Some`, a trusted broker sidecar's UDS is bound into the jail at this
    /// exact path (host path == jail path) and the worker reaches its backend only
    /// through it. Set by core's spawn chokepoint (never a manifest). `None` for
    /// every non-broker worker — then this field has zero effect on the argv and
    /// the netns decision. See `kastellan_core::broker`.
    pub broker_uds: Option<PathBuf>,
```
Rename every `embed_broker_uds` → `broker_uds` in `sandbox/src/lib.rs`, `linux_bwrap.rs`, `macos_seatbelt.rs` and their `#[cfg(test)]` modules, and in `sandbox/tests/macos_container_smoke.rs`. In `linux_bwrap.rs` the two `validate_linux_bind_path(uds, "embed_broker_uds")` / error-label sites become `"broker_uds"`; rename the test fns (`embed_broker_uds_*` → `broker_uds_*`) and their assertion strings accordingly.

- [ ] **Step 2: Rename the field in every core initializer**

These sites set `embed_broker_uds: None` (or read the field) and must become `broker_uds:`. Apply verbatim:
```
core/src/workers/web_research.rs:154, :223, :291
core/src/workers/web_search.rs:93
core/src/workers/web_fetch.rs (both entries)
core/src/workers/shell_exec.rs
core/src/workers/browser_driver.rs
core/src/workers/gliner_relex/entry.rs
core/src/workers/python_exec/entries.rs (all)
core/src/embed_broker/spawn.rs (broker_policy: `embed_broker_uds: None`, + the two tests asserting `.embed_broker_uds.is_none()`)
core/src/worker_lifecycle/force_route.rs (rewrite_policy_for_broker sets it; + tests)
core/tests/embed_broker_egress_e2e.rs, core/tests/lifecycle_container_routing_e2e.rs, core/tests/kv_demo_firecracker_persistent_e2e.rs
```
Find them all: `grep -rn "embed_broker_uds" sandbox core --include="*.rs"` must return **zero** hits after this step.

- [ ] **Step 3: Build + test the sandbox crate (guard tests carry the proof)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-sandbox`
Expected: PASS (the renamed `broker_uds_*` tests exercise bind + netns-neutrality + relative/`..` rejection exactly as before).

- [ ] **Step 4: Build the workspace to confirm no caller left behind**

Run: `cargo build --workspace`
Expected: clean (any missed `embed_broker_uds` initializer would be a hard compile error here).

- [ ] **Step 5: Clippy the sandbox crate**

Run: `cargo clippy -p kastellan-sandbox --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add sandbox/src core/src core/tests
git commit -m "refactor(sandbox): rename SandboxPolicy.embed_broker_uds -> broker_uds

One worker binds at most one broker socket; the field is broker-kind-neutral.
Byte-identical when None. Prep for the search-broker generalization."
```

---

## Task 2: Generalize the core broker module (`BrokerKind`, `BrokerConfigs`, `spawn_broker`, `entry.broker`)

**Files:**
- Rename: `core/src/embed_broker/` → `core/src/broker/` (git mv the dir: `mod.rs`, `config.rs`, `spawn.rs`)
- Create: `core/src/broker/kind.rs`
- Modify: `core/src/lib.rs` (module decl)
- Modify: `core/src/broker/{mod.rs,config.rs,spawn.rs}`
- Modify: `core/src/scheduler/tool_dispatch.rs` (+ `tests.rs`)
- Modify: `core/src/worker_lifecycle/{force_route.rs,manager.rs,composite.rs,idle_timeout.rs}` (+ test modules)
- Modify: `core/src/main.rs`
- Modify: `core/src/tool_host.rs` (sidecar field type name), `core/src/sandbox_health.rs`
- Modify: `core/src/workers/web_research.rs`
- Modify: `core/src/egress/scratch_sweep.rs`
- Modify: `core/tests/embed_broker_egress_e2e.rs`, `core/tests/embed_broker_spawn_e2e.rs`

**Interfaces:**
- Produces:
  - `broker::BrokerKind` (enum `Embed`/`Search`) with the const accessors below.
  - `broker::BrokerSpec { kind: BrokerKind, endpoint: String }` + `BrokerSpec::embed(endpoint)` / `BrokerSpec::search(endpoint)`.
  - `broker::BrokerConfig { kind, broker_bin, scratch_root }` + `config::from_env(kind, exe_dir) -> Option<Arc<BrokerConfig>>`.
  - `broker::BrokerConfigs { embed, search }` + `for_kind(kind) -> Option<&Arc<BrokerConfig>>`.
  - `broker::spawn_broker(cfg, spec, backend) -> Result<(BrokerSidecar, PathBuf), ToolHostError>`.
  - `broker::BrokerSidecar` (was `EmbedBrokerSidecar`).
  - `ToolEntry.broker: Option<BrokerSpec>` (was `embed_broker`).
- Consumes: `SandboxPolicy.broker_uds` (Task 1).

This task is a coherent, atomic refactor: the code must compile only at its end. Keep `BrokerKind::Embed`'s constants byte-identical to today so web-research is unaffected.

- [ ] **Step 1: Create `core/src/broker/kind.rs` with the kind descriptor**

```rust
//! The broker kind: which trusted sidecar a worker declares. One worker binds at
//! most one broker socket, so this is a plain enum, not a bitset. Each variant is
//! the single source of truth for that broker's binary name and its env / socket /
//! scratch naming contracts — `BrokerKind::Embed` reproduces every string the
//! merged embed-broker used, so web-research is byte-for-byte unaffected.

/// A trusted broker sidecar kind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BrokerKind {
    /// Embedding broker: `embed{model,input}` → OpenAI-compatible backend. web-research.
    Embed,
    /// Search broker: `search{query,count}` → SearxNG backend. web-search.
    Search,
}

impl BrokerKind {
    /// Exe-relative default binary name (used when the `*_BIN` override is unset).
    pub const fn broker_bin_default(self) -> &'static str {
        match self {
            BrokerKind::Embed => "kastellan-worker-embed-broker",
            BrokerKind::Search => "kastellan-worker-search-broker",
        }
    }
    /// Operator override env for the broker binary path.
    pub const fn bin_env(self) -> &'static str {
        match self {
            BrokerKind::Embed => "KASTELLAN_EMBED_BROKER_BIN",
            BrokerKind::Search => "KASTELLAN_SEARCH_BROKER_BIN",
        }
    }
    /// Override env for this kind's per-worker scratch root.
    pub const fn scratch_dir_env(self) -> &'static str {
        match self {
            BrokerKind::Embed => "KASTELLAN_EMBED_BROKER_SCRATCH_DIR",
            BrokerKind::Search => "KASTELLAN_SEARCH_BROKER_SCRATCH_DIR",
        }
    }
    /// Env the *broker binary* reads for the backend URL it forwards to.
    pub const fn endpoint_env(self) -> &'static str {
        match self {
            BrokerKind::Embed => "KASTELLAN_EMBED_BROKER_ENDPOINT",
            BrokerKind::Search => "KASTELLAN_SEARCH_BROKER_ENDPOINT",
        }
    }
    /// Env core injects into the *worker* carrying the bound UDS path.
    pub const fn uds_env(self) -> &'static str {
        match self {
            BrokerKind::Embed => "KASTELLAN_EMBED_BROKER_UDS",
            BrokerKind::Search => "KASTELLAN_SEARCH_BROKER_UDS",
        }
    }
    /// Basename of the broker's UDS under its scratch dir.
    pub const fn uds_file(self) -> &'static str {
        match self {
            BrokerKind::Embed => "embed.sock",
            BrokerKind::Search => "search.sock",
        }
    }
    /// Scratch-subdir name prefix (`<prefix><pid>-<seq>`), matched by the #251 sweep.
    pub const fn scratch_prefix(self) -> &'static str {
        match self {
            BrokerKind::Embed => "embed-",
            BrokerKind::Search => "search-",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_constants_are_byte_identical_to_the_merged_broker() {
        // Web-research relies on these exact strings; a change is a silent break.
        assert_eq!(BrokerKind::Embed.broker_bin_default(), "kastellan-worker-embed-broker");
        assert_eq!(BrokerKind::Embed.bin_env(), "KASTELLAN_EMBED_BROKER_BIN");
        assert_eq!(BrokerKind::Embed.scratch_dir_env(), "KASTELLAN_EMBED_BROKER_SCRATCH_DIR");
        assert_eq!(BrokerKind::Embed.endpoint_env(), "KASTELLAN_EMBED_BROKER_ENDPOINT");
        assert_eq!(BrokerKind::Embed.uds_env(), "KASTELLAN_EMBED_BROKER_UDS");
        assert_eq!(BrokerKind::Embed.uds_file(), "embed.sock");
        assert_eq!(BrokerKind::Embed.scratch_prefix(), "embed-");
    }

    #[test]
    fn search_constants_are_distinct_and_well_formed() {
        assert_eq!(BrokerKind::Search.broker_bin_default(), "kastellan-worker-search-broker");
        assert_eq!(BrokerKind::Search.uds_env(), "KASTELLAN_SEARCH_BROKER_UDS");
        assert_eq!(BrokerKind::Search.uds_file(), "search.sock");
        assert_eq!(BrokerKind::Search.scratch_prefix(), "search-");
        // No shared strings between the two kinds (a copy-paste slip would collide).
        assert_ne!(BrokerKind::Embed.uds_env(), BrokerKind::Search.uds_env());
        assert_ne!(BrokerKind::Embed.uds_file(), BrokerKind::Search.uds_file());
    }
}
```

- [ ] **Step 2: `git mv` the module and re-point the module decl**

```bash
git mv core/src/embed_broker core/src/broker
```
In `core/src/lib.rs`: `pub mod embed_broker;` → `pub mod broker;`.
In `core/src/broker/mod.rs`: add `pub mod kind;` and `pub use kind::BrokerKind;`. Update the module doc to describe the generalized broker (drop embed-only framing). Keep `pub mod config; pub mod spawn;`.

- [ ] **Step 3: Generalize `config.rs` — `BrokerConfig` + `BrokerConfigs` + kinded `from_env`**

Replace `EmbedBrokerConfig` with:
```rust
use super::kind::BrokerKind;

/// Everything core needs to spawn one broker sidecar of a given kind. Built once
/// at daemon startup (iff the kind's binary resolves) and shared behind an `Arc`.
pub struct BrokerConfig {
    pub(crate) kind: BrokerKind,
    pub(crate) broker_bin: PathBuf,
    pub(crate) scratch_root: PathBuf,
}

impl BrokerConfig {
    pub fn new(kind: BrokerKind, broker_bin: PathBuf, scratch_root: PathBuf) -> Self {
        Self { kind, broker_bin, scratch_root }
    }
}

/// Daemon-level registry: one config slot per broker kind. A `None` slot means
/// that kind's binary was not discovered — a worker declaring it then fails
/// closed at the spawn chokepoint. Cheap to clone (two `Option<Arc<_>>`).
#[derive(Default, Clone)]
pub struct BrokerConfigs {
    pub embed: Option<Arc<BrokerConfig>>,
    pub search: Option<Arc<BrokerConfig>>,
}

impl BrokerConfigs {
    pub fn for_kind(&self, kind: BrokerKind) -> Option<&Arc<BrokerConfig>> {
        match kind {
            BrokerKind::Embed => self.embed.as_ref(),
            BrokerKind::Search => self.search.as_ref(),
        }
    }
}

/// Discover one kind's broker config from the environment. The `*_BIN` override
/// wins (fail-closed if set-but-invalid), else the exe-relative sibling default.
/// Scratch root defaults to the egress root so the #251 sweep reclaims leaks.
pub fn from_env(kind: BrokerKind, exe_dir: Option<&Path>) -> Option<Arc<BrokerConfig>> {
    let broker_bin = discover_broker_bin(kind, exe_dir)?;
    let scratch_root = std::env::var_os(kind.scratch_dir_env())
        .map(PathBuf::from)
        .unwrap_or_else(crate::worker_lifecycle::force_route::default_egress_scratch_root);
    Some(Arc::new(BrokerConfig::new(kind, broker_bin, scratch_root)))
}
```
`discover_broker_bin(kind, exe_dir)` and `discover_broker_bin_with(...)` gain a `kind: BrokerKind` param and pass `kind.bin_env()` + `kind.broker_bin_default()` to `discover_binary`. Update the three existing discovery tests to pass `BrokerKind::Embed` and to use `BrokerKind::Embed.bin_env()` where they referenced `ENV_BROKER_BIN`. Delete the now-dead `ENV_BROKER_BIN`/`BROKER_BIN_DEFAULT`/`ENV_SCRATCH_DIR` consts (moved onto `BrokerKind`).

- [ ] **Step 4: Generalize `spawn.rs` — `spawn_broker` + `BrokerSidecar`, kind-driven**

Rename `EmbedBrokerSidecar` → `BrokerSidecar`, `spawn_embed_broker` → `spawn_broker`, `spawn_broker_in` stays. Drive the per-kind strings off `spec.kind`:
- `broker_policy(binary, endpoint, scratch, kind)` builds env from `kind.uds_env()`… **no** — the broker binary reads `kind.uds_env()`? It reads its socket path from `KASTELLAN_*_BROKER_UDS` and endpoint from `kind.endpoint_env()`. So env becomes:
  ```rust
  env: vec![
      (kind.uds_env().to_string(), uds.to_string_lossy().into_owned()),
      (kind.endpoint_env().to_string(), endpoint.to_string()),
  ],
  ```
  and `broker_uds: None` (the broker itself has no upstream broker). The UDS basename is `scratch.join(kind.uds_file())`.
- `make_broker_scratch_dir(scratch_root, kind)` uses `kind.scratch_prefix()` (was `EMBED_SCRATCH_DIR_PREFIX`) and `kind.uds_file()` for the sun_path projection.
- `spawn_broker(cfg, spec, backend)` reads `cfg.broker_bin`, `cfg.scratch_root`, and `spec.kind`/`spec.endpoint`. The `broker_allowlist_from_endpoint` guard, `wait_for_broker_ready`, `BrokerReady`, drain-both-pipes, and the RAII `Drop` are unchanged except for the type rename. The `BROKER_CPU_MS`/`SUN_PATH_MAX` consts stay.
- Update `spawn.rs` unit tests to construct `BrokerConfig::new(BrokerKind::Embed, ..)` + `BrokerSpec::embed(..)` and pass the kind where needed; the assertions (`Net::Allowlist` shape, lockdown env, sun_path guard, early-exit, malformed-endpoint rejection) are otherwise unchanged. Delete the module-local `ENV_BROKER_UDS`/`ENV_BROKER_ENDPOINT`/`UDS_FILE_NAME` consts (now from `BrokerKind`).

- [ ] **Step 5: Generalize `mod.rs` — `BrokerSpec` (kind + endpoint), drop `model`**

Replace `EmbedBrokerSpec` with:
```rust
use kind::BrokerKind;

/// Per-worker declaration that a worker wants a trusted broker sidecar of a
/// given kind, carrying the backend the broker forwards to. Set by a manifest;
/// core's chokepoint spawns the broker, binds its UDS into the jail
/// (`SandboxPolicy::broker_uds`), and injects `kind.uds_env()`.
///
/// The manifest also drops the backend host from the worker's `Net::Allowlist`
/// and omits the worker's direct-endpoint env, so the worker reaches the backend
/// only through the broker. Any model/param the *worker* needs (e.g. an embed
/// model) is set by the manifest in the worker's own env — it is not carried
/// here (the spawn path needs only kind + endpoint).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrokerSpec {
    pub kind: BrokerKind,
    pub endpoint: String,
}

impl BrokerSpec {
    pub fn embed(endpoint: impl Into<String>) -> Self {
        Self { kind: BrokerKind::Embed, endpoint: endpoint.into() }
    }
    pub fn search(endpoint: impl Into<String>) -> Self {
        Self { kind: BrokerKind::Search, endpoint: endpoint.into() }
    }
}
```
Delete `EMBED_BROKER_UDS_ENV` (callers now use `kind.uds_env()`). Update `pub use` re-exports: `pub use config::{from_env, BrokerConfig, BrokerConfigs}; pub use spawn::{spawn_broker, BrokerSidecar}; pub use kind::BrokerKind;`.

- [ ] **Step 6: Update the spawn chokepoint (`force_route.rs`)**

`spawn_worker_with_optional_broker` signature changes:
```rust
pub(crate) fn spawn_worker_with_optional_broker(
    force: Option<&ForceRoutingConfig>,
    broker_configs: &BrokerConfigs,
    backend: &dyn SandboxBackend,
    spec: &WorkerSpec<'_>,
    broker: Option<&BrokerSpec>,
    worker_name: &str,
) -> Result<SupervisedWorker, ToolHostError> {
    let Some(broker_spec) = broker else {
        return spawn_worker_maybe_forced(force, backend, spec, worker_name);
    };
    let cfg = broker_configs.for_kind(broker_spec.kind).ok_or_else(|| {
        ToolHostError::Io(std::io::Error::other(format!(
            "worker {worker_name:?} requests a {:?} broker but the daemon has no \
             matching broker config (binary not found); refusing to spawn — the \
             manifest already dropped the backend host from egress",
            broker_spec.kind
        )))
    })?;
    let (sidecar, uds) = spawn_broker(cfg, broker_spec, backend)?;
    let brokered = rewrite_policy_for_broker(spec.policy.clone(), &uds, broker_spec.kind);
    let brokered_spec = WorkerSpec { policy: &brokered, program: spec.program, args: spec.args, wall_clock_ms: spec.wall_clock_ms };
    let mut worker = spawn_worker_maybe_forced(force, backend, &brokered_spec, worker_name)?;
    worker.broker = Some(sidecar);
    Ok(worker)
}
```
`rewrite_policy_for_broker(policy, uds, kind)` sets `policy.broker_uds = Some(uds.clone())` and injects `(kind.uds_env().to_string(), uds.to_string_lossy().into_owned())` (drop any stale value for that key first). Update the import line to `use crate::broker::{spawn_broker, BrokerConfig, BrokerConfigs, BrokerKind, BrokerSpec};`. Re-point the `force_route/tests.rs` fail-closed test to build a `BrokerConfigs::default()` (empty) + a `BrokerSpec::embed(..)` and assert the refusal.

- [ ] **Step 7: Thread `BrokerConfigs` through the three lifecycle managers**

In `manager.rs`, `composite.rs`, `idle_timeout.rs`: replace every `embed_broker: Option<Arc<EmbedBrokerConfig>>` field/param with `broker_configs: BrokerConfigs` (owned; it's `Clone` and cheap). The cold-spawn call site passes `&self.broker_configs` and `entry.broker.as_ref()` to `spawn_worker_with_optional_broker`. Update `CompositeLifecycle::with_backoff_and_force_routing(sandboxes, backoff, force, broker_configs)` and the internal `manager::…::new(.., broker_configs)` constructors. In the three `manager/tests.rs` + `composite.rs` test constructors, pass `BrokerConfigs::default()`.

- [ ] **Step 8: Rename the `ToolEntry` field + the `SupervisedWorker` sidecar field**

`core/src/scheduler/tool_dispatch.rs`: `pub embed_broker: Option<crate::embed_broker::EmbedBrokerSpec>` → `pub broker: Option<crate::broker::BrokerSpec>` (update the doc comment to the generalized wording). `core/src/tool_host.rs`: `pub(crate) embed_broker: Option<crate::embed_broker::EmbedBrokerSidecar>` → `pub(crate) broker: Option<crate::broker::BrokerSidecar>`, and the `drop(embed_broker)` teardown line → `drop(broker)`; the teardown-order doc comment updates `embed_broker`→`broker`. Rename **every** `embed_broker: None` ToolEntry initializer → `broker: None` (all sites from the Task-1 grep list: `sandbox_health.rs`, `web_fetch.rs`, `shell_exec.rs`, `web_search.rs`, `browser_driver.rs`, `gliner_relex/entry.rs`, `python_exec/entries.rs`, `scheduler/tool_dispatch/tests.rs`, `web_research.rs:166,:303`).

- [ ] **Step 9: web-research manifest → `BrokerSpec::embed`**

`core/src/workers/web_research.rs:235`: `embed_broker: Some(crate::embed_broker::EmbedBrokerSpec::new(embed_endpoint, embed_model.unwrap_or(DEFAULT_EMBED_MODEL)))` → `broker: Some(crate::broker::BrokerSpec::embed(embed_endpoint))`. The embed model stays set in the worker env via `broker_env` (unchanged) — confirm `broker_env` still injects `EMBED_MODEL_ENV`. Update the web-research broker-mode tests that read `entry.embed_broker` → `entry.broker` and that asserted `spec.model` (drop that assertion; assert `spec.kind == BrokerKind::Embed` and `spec.endpoint` instead).

- [ ] **Step 10: Add the search scratch prefix to the sweep**

`core/src/egress/scratch_sweep.rs`: after `EMBED_SCRATCH_DIR_PREFIX`, add
```rust
/// Name prefix of the per-worker search-broker sidecar scratch dir. Kept in sync
/// with `BrokerKind::Search.scratch_prefix()`; holds the broker's `search.sock`.
pub(crate) const SEARCH_SCRATCH_DIR_PREFIX: &str = "search-";
```
and append it to `SCRATCH_DIR_PREFIXES`. Add a round-trip test mirroring the embed one (a `search-<pid>-<seq>` name parses to its pid).

- [ ] **Step 11: main.rs — discover both configs into `BrokerConfigs`**

Replace the `embed_broker_cfg` block (~153) with:
```rust
    // Broker configs (unified): discover each kind's sidecar binary. No daemon
    // enable gate — a manifest opts a worker in; the daemon holds a config iff the
    // binary resolves, and the spawn chokepoint fails closed if a declaring worker
    // has none.
    let broker_configs = kastellan_core::broker::BrokerConfigs {
        embed: kastellan_core::broker::config::from_env(
            kastellan_core::broker::BrokerKind::Embed, exe_dir.as_deref()),
        search: kastellan_core::broker::config::from_env(
            kastellan_core::broker::BrokerKind::Search, exe_dir.as_deref()),
    };
    if broker_configs.embed.is_some() {
        info!("embed-broker AVAILABLE — embed-declaring workers get a trusted embedding sidecar");
    }
    if broker_configs.search.is_some() {
        info!("search-broker AVAILABLE — search-declaring workers get a trusted search sidecar");
    }
```
and pass `broker_configs` (instead of `embed_broker_cfg`) into `CompositeLifecycle::with_backoff_and_force_routing(...)`. If the matrix channel spawn block below also consumed `embed_broker_cfg`, pass it `broker_configs.clone()` / the field it needs.

- [ ] **Step 12: Re-point the embed e2e tests**

`core/tests/embed_broker_egress_e2e.rs` + `embed_broker_spawn_e2e.rs`: rename imported symbols (`EmbedBrokerConfig`→`BrokerConfig`, `EmbedBrokerSpec`→`BrokerSpec::embed(..)`, `spawn_embed_broker`→`spawn_broker`, `embed_broker`→`broker` fields, `crate::embed_broker`→`kastellan_core::broker`). Behaviour assertions (zero embed egress, hybrid ranking, `broker_uds` bound) are unchanged. `rewrite_policy_for_broker` calls pass `BrokerKind::Embed`.

- [ ] **Step 13: Build + clippy + test the whole change**

Run (foreground, in order):
```
source "$HOME/.cargo/env"
cargo build --workspace
cargo clippy -p kastellan-core --all-targets -- -D warnings
cargo test -p kastellan-core --lib broker::
cargo test -p kastellan-core --lib
```
Expected: clean build/clippy; `broker::` module tests pass; full core lib green (the re-pointed web-research/force-route/spawn/config tests all pass). `grep -rn "embed_broker\b\|EmbedBroker" core/src` returns only doc-comment prose, not identifiers.

- [ ] **Step 14: Commit**

```bash
git add core/src core/tests
git commit -m "refactor(broker): generalize embed-broker into a kind-parameterized broker module

BrokerKind{Embed,Search} supplies every per-kind string; one spawn_broker,
BrokerSidecar, BrokerConfigs registry, entry.broker field, and spawn chokepoint.
Embed constants are byte-identical, so web-research is unaffected. Search config
is discovered but None until the search-broker crate lands."
```

---

## Task 3: `kastellan-worker-search-broker` crate — handler + serve (lib)

**Files:**
- Create: `workers/search-broker/Cargo.toml`
- Create: `workers/search-broker/src/lib.rs`
- Modify: `Cargo.toml` (workspace members)

**Interfaces:**
- Produces: `SearchHandler<T: HttpGet>` (`Handler` for method `search`), `serve_connection(&mut SearchHandler, UnixStream)`, `BROKER_MAX_RECORD_BYTES`, `READ_TIMEOUT`, `WRITE_TIMEOUT`.
- Consumes: `kastellan_worker_web_common::search::{search, validate_endpoint, SearchError, DEFAULT_COUNT, MAX_COUNT}`, `::allowlist::HostAllowlist`, `::http::HttpGet`, `::parse::Hit`; `kastellan_protocol::{codes, RpcError, server::{Handler, serve_capped, OnProtocolError}}`.

- [ ] **Step 1: Add the workspace member**

`Cargo.toml`: add `"workers/search-broker",` after `"workers/embed-broker",`.

- [ ] **Step 2: Write the crate manifest**

`workers/search-broker/Cargo.toml`:
```toml
[package]
name        = "kastellan-worker-search-broker"
description = "Trusted search broker sidecar: bridges a jailed worker's UDS to a SearxNG backend (query -> results), so the force-routed worker needs no search egress."
version.workspace      = true
edition.workspace      = true
rust-version.workspace = true
license.workspace      = true
authors.workspace      = true
repository.workspace   = true
readme      = "../../README.md"

[[bin]]
name = "kastellan-worker-search-broker"
path = "src/main.rs"

[dependencies]
kastellan-protocol          = { path = "../../protocol", version = "0.1.0" }
kastellan-worker-prelude    = { path = "../prelude", version = "0.1.0" }
kastellan-worker-web-common = { path = "../web-common", version = "0.1.0" }
serde      = { workspace = true }
serde_json = { workspace = true }
anyhow     = { workspace = true }
url        = { workspace = true }

[dev-dependencies]
kastellan-worker-web-common = { path = "../web-common", features = ["testing"] }
```

- [ ] **Step 3: RED — write the handler test first**

`workers/search-broker/src/lib.rs` (test module only, plus a stub `SearchHandler` that won't compile yet is NOT allowed — write the test, expect a compile-fail RED). Write:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_worker_web_common::http::RawResponse;
    use kastellan_worker_web_common::testing::{al, json_resp, FakeGet};
    use kastellan_protocol::codes;
    use url::Url;

    fn handler(responses: Vec<RawResponse>) -> SearchHandler<FakeGet> {
        SearchHandler::with_parts(
            Url::parse("http://127.0.0.1:8888/search").unwrap(),
            al(&["127.0.0.1"]),
            FakeGet::new(responses),
        )
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let mut h = handler(vec![]);
        let err = h.call("nope", serde_json::json!({})).unwrap_err();
        assert_eq!(err.code, codes::METHOD_NOT_FOUND);
    }

    #[test]
    fn empty_query_is_invalid_params() {
        let mut h = handler(vec![]);
        let err = h.call("search", serde_json::json!({"query": "  "})).unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    #[test]
    fn happy_path_returns_results_envelope() {
        let json = r#"{"results":[{"title":"T","url":"https://x.test","content":"c","engine":"e"}]}"#;
        let mut h = handler(vec![json_resp(json)]);
        let out = h.call("search", serde_json::json!({"query": "germany"})).unwrap();
        assert_eq!(out["results"][0]["url"], "https://x.test");
        assert_eq!(out["results"][0]["snippet"], "c");
    }

    #[test]
    fn backend_failure_maps_to_operation_failed() {
        let mut h = handler(vec![RawResponse { status: 500, location: None, content_type: "text/plain".into(), body: Vec::new() }]);
        let err = h.call("search", serde_json::json!({"query": "x"})).unwrap_err();
        assert_eq!(err.code, codes::OPERATION_FAILED);
    }
}
```
`al(&[&str])` is the web-common test helper (feature `testing`, enabled by this crate's dev-dep). The happy-path SearxNG fixture must include an `engine` field (it does above) — `Hit` has four fields (`title`, `url`, `snippet`, `engine`).

- [ ] **Step 4: Verify RED**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-search-broker --lib`
Expected: FAIL to compile (`SearchHandler` undefined).

- [ ] **Step 5: GREEN — implement `SearchHandler` + params + error mapping**

Prepend to `workers/search-broker/src/lib.rs`:
```rust
//! Trusted search broker sidecar.
//!
//! A force-routed jailed worker cannot reach a loopback SearxNG (the egress proxy
//! SSRF-blocks loopback). It talks JSON-RPC `search{query,count?}` to this broker
//! over a Unix socket core bind-mounts into its jail; the broker — running in the
//! host netns with `Net::Allowlist([searx host:port])` — forwards to SearxNG and
//! returns the parsed hits. All SearxNG coupling lives in web-common's `search`.

use kastellan_protocol::server::Handler;
use kastellan_protocol::{codes, RpcError};
use serde::Deserialize;
use url::Url;

use kastellan_worker_web_common::allowlist::HostAllowlist;
use kastellan_worker_web_common::http::HttpGet;
use kastellan_worker_web_common::search::{search, SearchError, DEFAULT_COUNT};

#[derive(Deserialize)]
struct SearchRpcParams {
    query: String,
    #[serde(default)]
    count: Option<usize>,
}

/// Map a web-common `SearchError` to a JSON-RPC error. A bad-config / denied
/// endpoint is `POLICY_DENIED`; an empty query is `INVALID_PARAMS`; anything else
/// (transport, status, parse, redirect) is `OPERATION_FAILED` — the broker never
/// partially succeeds.
fn search_err_to_rpc(e: SearchError) -> RpcError {
    match e {
        SearchError::EmptyQuery => RpcError::new(codes::INVALID_PARAMS, "query is empty".to_string()),
        SearchError::BadEndpoint(m) => RpcError::new(codes::POLICY_DENIED, format!("endpoint invalid: {m}")),
        SearchError::SchemeDenied(s) => RpcError::new(codes::POLICY_DENIED, format!("endpoint scheme {s:?} not allowed")),
        SearchError::HostDenied(h) => RpcError::new(codes::POLICY_DENIED, format!("endpoint host {h:?} not on allowlist")),
        SearchError::Transport(m) => RpcError::new(codes::OPERATION_FAILED, format!("backend transport: {m}")),
        SearchError::Redirected => RpcError::new(codes::OPERATION_FAILED, "backend returned an unexpected redirect".to_string()),
        SearchError::BadStatus(s) => RpcError::new(codes::OPERATION_FAILED, format!("backend status {s}")),
        SearchError::Parse(m) => RpcError::new(codes::OPERATION_FAILED, format!("parse failed: {m}")),
    }
}

/// JSON-RPC handler for the broker's single `search` method. Forwards to the
/// SearxNG backend via web-common's `search` (which re-checks the host allowlist
/// and enforces the `MAX_COUNT` cap). Generic over the transport for tests.
pub struct SearchHandler<T: HttpGet> {
    endpoint: Url,
    allowlist: HostAllowlist,
    transport: T,
}

impl<T: HttpGet> SearchHandler<T> {
    pub fn new(endpoint: Url, allowlist: HostAllowlist, transport: T) -> Self {
        Self { endpoint, allowlist, transport }
    }
    #[cfg(test)]
    fn with_parts(endpoint: Url, allowlist: HostAllowlist, transport: T) -> Self {
        Self { endpoint, allowlist, transport }
    }
}

impl<T: HttpGet> Handler for SearchHandler<T> {
    fn call(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
        if method != "search" {
            return Err(RpcError::new(codes::METHOD_NOT_FOUND, format!("unknown method: {method}")));
        }
        let p: SearchRpcParams = serde_json::from_value(params)
            .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("params: {e}")))?;
        let count = p.count.unwrap_or(DEFAULT_COUNT);
        let hits = search(&self.transport, &self.endpoint, &self.allowlist, &p.query, count)
            .map_err(search_err_to_rpc)?;
        serde_json::to_value(serde_json::json!({ "results": hits }))
            .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, format!("result encode: {e}")))
    }
}
```
(`search` returns `Vec<Hit>` and `Hit: Serialize`, so the `results` array serializes as `{title,url,snippet}` — the same shape web-search returns.)

- [ ] **Step 6: Verify GREEN (handler)**

Run: `cargo test -p kastellan-worker-search-broker --lib`
Expected: PASS (4 tests).

- [ ] **Step 7: GREEN — add `serve_connection` + caps (mirror embed-broker)**

Append to `lib.rs` (identical structure to `kastellan-worker-embed-broker::serve_connection`, swapping `SearchHandler` for `EmbedHandler`):
```rust
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Framing byte-cap for one JSON-RPC request record on the broker's socket. A
/// query + count is tiny; 1 MiB is ample and far below the protocol default.
pub const BROKER_MAX_RECORD_BYTES: usize = 1024 * 1024;

/// Idle read timeout for one broker connection (serial serve loop).
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Write timeout for one broker connection.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Serve one accepted UDS connection until EOF / timeout / protocol fault, via the
/// transport-generic `serve_capped` at `BROKER_MAX_RECORD_BYTES`.
pub fn serve_connection<T: HttpGet>(
    handler: &mut SearchHandler<T>,
    stream: UnixStream,
) -> std::io::Result<()> {
    // Match the exact body of kastellan-worker-embed-broker::serve_connection:
    // set read/write timeouts on both cloned halves, then call
    // kastellan_protocol::server::serve_capped(reader, writer, handler,
    // BROKER_MAX_RECORD_BYTES, OnProtocolError::Close). Read that function and
    // reproduce it verbatim with SearchHandler + these three consts.
}
```
Read `workers/embed-broker/src/lib.rs:204`+ and reproduce the `serve_connection` body exactly (timeouts on both halves, `serve_capped(..., OnProtocolError::Close)`), substituting the search types/consts.

- [ ] **Step 8: Verify GREEN + clippy**

Run:
```
cargo test -p kastellan-worker-search-broker --lib
cargo clippy -p kastellan-worker-search-broker --all-targets -- -D warnings
```
Expected: PASS, clean.

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml workers/search-broker/Cargo.toml workers/search-broker/src/lib.rs
git commit -m "feat(search-broker): SearchHandler + serve — forward search{query,count} to SearxNG

Trusted broker lib mirroring embed-broker: JSON-RPC search over a UDS, forwarded
to the backend via web-common::search (host re-check + MAX_COUNT cap). Fail-closed
error mapping; 4 handler tests over a FakeGet."
```

---

## Task 4: `kastellan-worker-search-broker` binary (`main.rs`)

**Files:**
- Create: `workers/search-broker/src/main.rs`

**Interfaces:**
- Consumes: `KASTELLAN_SEARCH_BROKER_UDS` (bind path), `KASTELLAN_SEARCH_BROKER_ENDPOINT` (SearxNG URL); `kastellan_worker_web_common::{http, search, allowlist}`, `kastellan_worker_prelude::lock_down`.

The binary has no unit tests (its bind→lockdown→serve wiring is exercised by the Task-8 e2e). Mirror `workers/embed-broker/src/main.rs`.

- [ ] **Step 1: Write `main.rs`**

```rust
//! Search broker sidecar binary.
//!
//! Spawned by core like the embed-broker: bind the UDS, apply the worker-prelude
//! lockdown, then serve JSON-RPC `search` over the socket, forwarding each to the
//! operator's SearxNG backend. Two env vars: `KASTELLAN_SEARCH_BROKER_UDS` (socket
//! path) and `KASTELLAN_SEARCH_BROKER_ENDPOINT` (the SearxNG search URL).

use std::os::unix::net::UnixListener;

use kastellan_worker_web_common::allowlist::HostAllowlist;
use kastellan_worker_web_common::search::validate_endpoint;
use kastellan_worker_search_broker::{serve_connection, SearchHandler};

fn main() -> anyhow::Result<()> {
    let uds = std::env::var("KASTELLAN_SEARCH_BROKER_UDS")
        .map_err(|_| anyhow::anyhow!("KASTELLAN_SEARCH_BROKER_UDS unset"))?;
    let endpoint_raw = std::env::var("KASTELLAN_SEARCH_BROKER_ENDPOINT")
        .map_err(|_| anyhow::anyhow!("KASTELLAN_SEARCH_BROKER_ENDPOINT unset"))?;

    // The broker's allowlist IS its single endpoint's host (one backend). Validate
    // the endpoint (https anywhere; http for loopback only) against it and fail
    // closed before binding if it is malformed or off-policy.
    let host = url::Url::parse(&endpoint_raw)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .ok_or_else(|| anyhow::anyhow!("KASTELLAN_SEARCH_BROKER_ENDPOINT has no host"))?;
    // One backend → the allowlist IS its host. `from_endpoints` with a bare host
    // yields an any-port rule so `is_allowed(host)` passes in `validate_endpoint`
    // and `search()`.
    let allowlist = HostAllowlist::from_endpoints(&[host]);
    let endpoint = validate_endpoint(&endpoint_raw, &allowlist)
        .map_err(|e| anyhow::anyhow!("search-broker endpoint rejected: {e:?}"))?;

    // A remote/TLS backend needs the rustls provider up front; a loopback http
    // backend never builds a TLS config, so this is a no-op there.
    if endpoint.scheme() == "https" {
        kastellan_worker_web_common::http::ensure_crypto_provider();
    }
    let transport = kastellan_worker_web_common::http::make_get("kastellan-search-broker/0")?;

    // Bind BEFORE lock-down (Landlock forbids fs mutation after) — the embed-broker
    // / egress-proxy ordering.
    let _ = std::fs::remove_file(&uds);
    let listener = UnixListener::bind(&uds)?;

    // Worker-side defense-in-depth (Linux Landlock+seccomp; no-op on macOS).
    let _report = kastellan_worker_prelude::lock_down()?;

    let mut handler = SearchHandler::new(endpoint, allowlist, transport);
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        if let Err(e) = serve_connection(&mut handler, conn) {
            eprintln!("search-broker: connection error: {e}");
        }
    }
    Ok(())
}
```
`HostAllowlist::from_endpoints(&[String])` and `validate_endpoint` (returns `SearchError`, which is `Debug`) both already exist; `make_get`/`ensure_crypto_provider` mirror `workers/embed-broker/src/main.rs`.

- [ ] **Step 2: Build + clippy the binary**

Run:
```
source "$HOME/.cargo/env"
cargo build -p kastellan-worker-search-broker
cargo clippy -p kastellan-worker-search-broker --all-targets -- -D warnings
```
Expected: clean (a runnable `kastellan-worker-search-broker` in `target/debug/`, so `BrokerConfig::from_env(Search, ..)` now discovers it as an exe-sibling).

- [ ] **Step 3: Commit**

```bash
git add workers/search-broker/src/main.rs
git commit -m "feat(search-broker): binary — bind UDS, lock down, serve search over the socket

Mirrors embed-broker main: validate the single SearxNG endpoint against its own
host allowlist, bind before lockdown, serve. Two env vars (UDS + endpoint)."
```

---

## Task 5: web-search worker — `SearchProvider` seam (`Direct` + `choose`)

**Files:**
- Modify: `workers/web-common/src/parse.rs` (`Hit` gains `Deserialize`)
- Modify: `workers/web-search/src/handler.rs`

**Interfaces:**
- Produces: `trait SearchProvider { fn search(&self, query: &str, count: usize) -> Result<Vec<Hit>, SearchError>; }`, `DirectSearchProvider<T>`, `choose_search_provider(broker_uds, endpoint) -> SearchProviderChoice`.
- Consumes: `Hit` (now `Serialize + Deserialize`).

- [ ] **Step 1: RED — `Hit` must round-trip through JSON**

`workers/web-common/src/parse.rs` tests: add
```rust
#[test]
fn hit_round_trips_through_json() {
    let h = Hit {
        title: "T".into(),
        url: "https://x.test".into(),
        snippet: "c".into(),
        engine: "e".into(),
    };
    let json = serde_json::to_string(&h).unwrap();
    let back: Hit = serde_json::from_str(&json).unwrap();
    assert_eq!(back, h);
}
```
(`Hit` has exactly four `String` fields — `title`, `url`, `snippet`, `engine` — with no `#[serde(rename)]` (the SearxNG `content`→`snippet` mapping happens in `parse_results`, not serde). All four must be present in any deserialized fixture, since none carries `#[serde(default)]`.)

- [ ] **Step 2: Verify RED**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-web-common --lib hit_round_trips`
Expected: FAIL to compile (`Hit: Deserialize` not satisfied).

- [ ] **Step 3: GREEN — derive `Deserialize` on `Hit`**

`parse.rs`: change `#[derive(serde::Serialize, Debug, PartialEq)]` on `Hit` to `#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]`. Keep the serialize/deserialize field naming symmetric (if a field renames `content`→`snippet` on serialize, the same rename applies on deserialize automatically with a plain `#[serde(rename)]`; verify).

- [ ] **Step 4: Verify GREEN**

Run: `cargo test -p kastellan-worker-web-common --lib`
Expected: PASS.

- [ ] **Step 5: RED — provider choice precedence**

In `workers/web-search/src/handler.rs` tests, add:
```rust
#[test]
fn choose_broker_wins_when_both_set() {
    match choose_search_provider(Some("/run/search.sock"), Some("https://searx/search")) {
        SearchProviderChoice::Broker { uds } => assert_eq!(uds, "/run/search.sock"),
        other => panic!("expected Broker, got {other:?}"),
    }
}
#[test]
fn choose_endpoint_when_only_endpoint_set() {
    match choose_search_provider(None, Some("https://searx/search")) {
        SearchProviderChoice::Endpoint { endpoint } => assert_eq!(endpoint, "https://searx/search"),
        other => panic!("expected Endpoint, got {other:?}"),
    }
}
#[test]
fn choose_none_when_neither_and_blank_is_unset() {
    assert!(matches!(choose_search_provider(None, None), SearchProviderChoice::None));
    assert!(matches!(choose_search_provider(Some("  "), None), SearchProviderChoice::None));
}
```

- [ ] **Step 6: Verify RED**

Run: `cargo test -p kastellan-worker-web-search --lib choose_`
Expected: FAIL to compile.

- [ ] **Step 7: GREEN — trait, `DirectSearchProvider`, `choose_search_provider`**

Add to `handler.rs` (mirrors web-research's `Embedder`/`EmbedderChoice`/`choose_embedder`):
```rust
use kastellan_worker_web_common::parse::Hit;
use kastellan_worker_web_common::search::search;

/// Run a search, returning parsed hits. The single network seam (faked in tests).
pub trait SearchProvider {
    fn search(&self, query: &str, count: usize) -> Result<Vec<Hit>, SearchError>;
}

/// Which provider `from_env` builds, decided purely from two env values.
#[derive(Debug, PartialEq)]
pub enum SearchProviderChoice<'a> {
    None,
    Broker { uds: &'a str },
    Endpoint { endpoint: &'a str },
}

/// Broker UDS wins over a direct endpoint when both are set; blank counts as unset.
pub fn choose_search_provider<'a>(
    broker_uds: Option<&'a str>,
    endpoint: Option<&'a str>,
) -> SearchProviderChoice<'a> {
    let broker = broker_uds.map(str::trim).filter(|s| !s.is_empty());
    let endpoint = endpoint.map(str::trim).filter(|s| !s.is_empty());
    match (broker, endpoint) {
        (Some(uds), _) => SearchProviderChoice::Broker { uds },
        (None, Some(endpoint)) => SearchProviderChoice::Endpoint { endpoint },
        (None, None) => SearchProviderChoice::None,
    }
}

/// Direct provider: validated endpoint + host allowlist + transport (today's path).
pub struct DirectSearchProvider<T: HttpGet> {
    endpoint: Url,
    allowlist: HostAllowlist,
    transport: T,
}
impl<T: HttpGet> DirectSearchProvider<T> {
    pub fn new(endpoint: Url, allowlist: HostAllowlist, transport: T) -> Self {
        Self { endpoint, allowlist, transport }
    }
}
impl<T: HttpGet> SearchProvider for DirectSearchProvider<T> {
    fn search(&self, query: &str, count: usize) -> Result<Vec<Hit>, SearchError> {
        search(&self.transport, &self.endpoint, &self.allowlist, query, count)
    }
}
```

- [ ] **Step 8: Verify GREEN + clippy**

Run:
```
cargo test -p kastellan-worker-web-search --lib
cargo clippy -p kastellan-worker-web-search --all-targets -- -D warnings
```
Expected: PASS, clean. (The handler is not yet re-wired onto the trait — that's Task 7. `search`/`validate_endpoint` imports may now be partly used by the provider; keep the handler compiling.)

- [ ] **Step 9: Commit**

```bash
git add workers/web-common/src/parse.rs workers/web-search/src/handler.rs
git commit -m "feat(web-search): SearchProvider seam + Hit Deserialize

Hit round-trips through JSON (broker boundary). Add SearchProvider trait,
DirectSearchProvider (today's path behind the seam), and choose_search_provider
precedence — mirrors the web-research Embedder seam."
```

---

## Task 6: web-search worker — `BrokeredSearchProvider`

**Files:**
- Modify: `workers/web-search/src/handler.rs`

**Interfaces:**
- Produces: `BrokeredSearchProvider` (implements `SearchProvider` over a broker UDS).
- Consumes: `kastellan_protocol::{Request, Response, read_capped_record, Record, MAX_RECORD_BYTES}`.

Mirror `workers/web-research/src/embed.rs::BrokeredEmbedder` and its stub-broker tests.

- [ ] **Step 1: RED — round-trip + error mapping against a stub broker**

Add to `handler.rs` tests (mirror the embed stub-broker helper):
```rust
use std::io::{BufReader as StdBufReader, Write as StdWrite};
use std::os::unix::net::UnixListener;

fn stub_broker(sock: std::path::PathBuf, response_json: String) -> std::thread::JoinHandle<()> {
    let listener = UnixListener::bind(&sock).unwrap();
    std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut br = StdBufReader::new(conn.try_clone().unwrap());
        let _ = kastellan_protocol::read_capped_record(&mut br, 1_000_000).unwrap();
        conn.write_all(response_json.as_bytes()).unwrap();
        conn.write_all(b"\n").unwrap();
        conn.flush().unwrap();
    })
}

#[test]
fn brokered_search_round_trip_returns_hits() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("search.sock");
    let h = stub_broker(sock.clone(),
        r#"{"jsonrpc":"2.0","id":1,"result":{"results":[{"title":"T","url":"https://x.test","snippet":"c","engine":"e"}]}}"#.to_string());
    let p = BrokeredSearchProvider::new(sock);
    let hits = p.search("germany", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].url, "https://x.test");
    h.join().unwrap();
}

#[test]
fn brokered_search_maps_broker_error() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("search.sock");
    let h = stub_broker(sock.clone(),
        r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32002,"message":"backend down"}}"#.to_string());
    let p = BrokeredSearchProvider::new(sock);
    let err = p.search("x", 10).unwrap_err();
    assert!(matches!(err, SearchError::Transport(_)), "got {err:?}");
    h.join().unwrap();
}

#[test]
fn brokered_search_absent_socket_is_transport_error() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("nope.sock");
    let p = BrokeredSearchProvider::new(sock);
    assert!(matches!(p.search("x", 10).unwrap_err(), SearchError::Transport(_)));
}
```
Add `tempfile = "3"` to web-search `[dev-dependencies]` (it is currently absent; match web-research's `Cargo.toml`). `Response.error` carries `{code, message}` (`kastellan_protocol::RpcError` shape) — mirror `BrokeredEmbedder`'s handling.

- [ ] **Step 2: Verify RED**

Run: `cargo test -p kastellan-worker-web-search --lib brokered_search`
Expected: FAIL to compile (`BrokeredSearchProvider` undefined).

- [ ] **Step 3: GREEN — implement `BrokeredSearchProvider`**

```rust
use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

/// Result envelope decoded from the broker's `search` reply: `{results:[Hit]}`.
#[derive(serde::Deserialize)]
struct BrokerSearchResult {
    results: Vec<Hit>,
}

/// Search via the trusted search-broker sidecar over a Unix socket. Sends JSON-RPC
/// `search{query,count}` (the broker's UDS is bind-mounted into this worker's jail)
/// and decodes the returned hits. The worker needs no search egress — the broker
/// holds the only route to SearxNG.
pub struct BrokeredSearchProvider {
    uds: PathBuf,
}
impl BrokeredSearchProvider {
    pub fn new(uds: PathBuf) -> Self {
        Self { uds }
    }
}
impl SearchProvider for BrokeredSearchProvider {
    fn search(&self, query: &str, count: usize) -> Result<Vec<Hit>, SearchError> {
        let mut stream = UnixStream::connect(&self.uds)
            .map_err(|e| SearchError::Transport(format!("connect broker {:?}: {e}", self.uds)))?;
        let req = kastellan_protocol::Request {
            jsonrpc: "2.0".into(),
            id: serde_json::json!(1),
            method: "search".into(),
            params: serde_json::json!({ "query": query, "count": count }),
        };
        let mut line = serde_json::to_vec(&req)
            .map_err(|e| SearchError::Parse(format!("request encode: {e}")))?;
        line.push(b'\n');
        stream.write_all(&line)
            .map_err(|e| SearchError::Transport(format!("write broker request: {e}")))?;
        stream.flush().ok();

        let mut br = BufReader::new(&stream);
        let buf = match kastellan_protocol::read_capped_record(&mut br, kastellan_protocol::MAX_RECORD_BYTES)
            .map_err(|e| SearchError::Transport(format!("read broker response: {e}")))?
        {
            kastellan_protocol::Record::Line(b) => b,
            kastellan_protocol::Record::Eof => return Err(SearchError::Transport("broker closed without responding".into())),
            kastellan_protocol::Record::TooLarge => return Err(SearchError::Parse("broker response exceeded record cap".into())),
        };
        let resp: kastellan_protocol::Response = serde_json::from_slice(&buf)
            .map_err(|e| SearchError::Parse(format!("broker response: {e}")))?;
        if let Some(err) = resp.error {
            // A broker JSON-RPC error surfaces as a transport-class failure to the
            // agent (the worker cannot itself retry the backend).
            return Err(SearchError::Transport(format!("broker error {}: {}", err.code, err.message)));
        }
        let result = resp.result
            .ok_or_else(|| SearchError::Parse("broker response missing result".into()))?;
        let decoded: BrokerSearchResult = serde_json::from_value(result)
            .map_err(|e| SearchError::Parse(format!("result decode: {e}")))?;
        Ok(decoded.results)
    }
}
```

- [ ] **Step 4: Verify GREEN + clippy**

Run:
```
cargo test -p kastellan-worker-web-search --lib
cargo clippy -p kastellan-worker-web-search --all-targets -- -D warnings
```
Expected: PASS, clean.

- [ ] **Step 5: Commit**

```bash
git add workers/web-search/Cargo.toml workers/web-search/src/handler.rs
git commit -m "feat(web-search): BrokeredSearchProvider — search over the broker UDS

JSON-RPC search{query,count} to the search-broker, decoding {results:[Hit]}.
Mirrors BrokeredEmbedder; stub-broker round-trip + error + absent-socket tests."
```

---

## Task 7: web-search worker — provider selection in `from_env` + manifest broker mode

**Files:**
- Modify: `workers/web-search/src/handler.rs` (`from_env` selects provider; `call` uses the trait)
- Modify: `core/src/workers/web_search.rs` (broker-mode entry + manifest branch)

**Interfaces:**
- Consumes: `KASTELLAN_SEARCH_BROKER_UDS` (worker side); `KASTELLAN_WEB_SEARCH_USE_BROKER` (manifest side); `broker::BrokerSpec::search`.
- Produces: `web_search_broker_entry(...)`; `WebSearchManifest` broker branch.

- [ ] **Step 1: Rewire the handler onto the trait (`Box<dyn SearchProvider>`)**

Change `WebSearchHandler` to hold `provider: Box<dyn SearchProvider>` (drop the direct `endpoint`/`allowlist`/`transport` fields). `call` becomes:
```rust
        let hits = self.provider.search(&p.query, count).map_err(search_err_to_rpc)?;
        let hit_count = hits.len();
        Ok(serde_json::json!({ "query": p.query, "results": hits, "count": hit_count }))
```
Update the existing handler tests' `with_parts` helper to build `WebSearchHandler { provider: Box::new(DirectSearchProvider::new(endpoint, allowlist, FakeGet::new(responses))) }`. All four existing tests keep their assertions.

- [ ] **Step 2: `from_env` selects the provider (broker UDS wins, endpoint optional in broker mode)**

```rust
    pub fn from_env() -> anyhow::Result<Self> {
        let broker_uds = std::env::var("KASTELLAN_SEARCH_BROKER_UDS").ok();
        let endpoint_raw = std::env::var("KASTELLAN_WEB_SEARCH_ENDPOINT").ok();
        let provider: Box<dyn SearchProvider> =
            match choose_search_provider(broker_uds.as_deref(), endpoint_raw.as_deref()) {
                SearchProviderChoice::Broker { uds } => {
                    Box::new(BrokeredSearchProvider::new(std::path::PathBuf::from(uds)))
                }
                SearchProviderChoice::Endpoint { endpoint } => {
                    let allow_raw = std::env::var("KASTELLAN_WEB_SEARCH_ALLOWLIST")
                        .unwrap_or_else(|_| "[]".to_string());
                    let allowlist = HostAllowlist::from_env_json(&allow_raw)?;
                    let url = validate_endpoint(endpoint, &allowlist)
                        .map_err(|e| anyhow::anyhow!(search_err_to_rpc(e).message))?;
                    let transport = make_get("kastellan-web-search/0")?;
                    Box::new(DirectSearchProvider::new(url, allowlist, transport))
                }
                SearchProviderChoice::None => {
                    anyhow::bail!("web-search: neither KASTELLAN_SEARCH_BROKER_UDS nor KASTELLAN_WEB_SEARCH_ENDPOINT set")
                }
            };
        Ok(Self { provider })
    }
```
(`from_env` now returns `WebSearchHandler` with a boxed provider; the type param on `WebSearchHandler` is dropped in favor of the trait object. Adjust the `impl` blocks: the generic `WebSearchHandler<T>` becomes a plain `WebSearchHandler` holding `Box<dyn SearchProvider>`; tests inject `DirectSearchProvider<FakeGet>` boxed.)

- [ ] **Step 3: Verify the worker crate green**

Run:
```
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-web-search --lib
cargo clippy -p kastellan-worker-web-search --all-targets -- -D warnings
```
Expected: PASS, clean.

- [ ] **Step 4: RED — the manifest broker-mode entry**

In `core/src/workers/web_search.rs` tests, add:
```rust
#[test]
fn resolve_broker_mode_drops_egress_and_declares_search_broker() {
    let get_env = |k: &str| match k {
        BIN_ENV => Some("/opt/web-search".to_string()),
        ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
        "KASTELLAN_WEB_SEARCH_USE_BROKER" => Some("1".to_string()),
        _ => None,
    };
    let exists = |_p: &Path| true;
    let allowlist = |_t: &str| Vec::<String>::new();
    let c = ctx(&get_env, &exists, &allowlist);

    match WebSearchManifest.resolve(&c) {
        Resolution::Register(entry) => {
            // Broker declared, carrying the SearxNG endpoint the broker forwards to.
            let spec = entry.broker.as_ref().expect("broker set in broker mode");
            assert_eq!(spec.kind, kastellan_core::broker::BrokerKind::Search);
            assert_eq!(spec.endpoint, "http://127.0.0.1:8888/search");
            // Worker has NO direct egress — empty allowlist.
            match &entry.policy.net {
                Net::Allowlist(hosts) => assert!(hosts.is_empty(), "broker-mode worker must have no egress: {hosts:?}"),
                other => panic!("expected empty Net::Allowlist, got {other:?}"),
            }
            // No direct-endpoint env leaked to the worker in broker mode.
            assert!(entry.policy.env.iter().all(|(k, _)| k != ENDPOINT_ENV),
                "broker-mode worker must not carry the direct endpoint env");
            // broker_uds is set at spawn, not by the manifest.
            assert!(entry.policy.broker_uds.is_none());
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}

#[test]
fn resolve_direct_mode_unchanged_when_use_broker_unset() {
    // Byte-identical to today's direct entry: endpoint host in Net::Allowlist,
    // endpoint env present, no broker.
    let get_env = |k: &str| match k {
        BIN_ENV => Some("/opt/web-search".to_string()),
        ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
        _ => None,
    };
    let exists = |_p: &Path| true;
    let allowlist = |_t: &str| Vec::<String>::new();
    let c = ctx(&get_env, &exists, &allowlist);
    match WebSearchManifest.resolve(&c) {
        Resolution::Register(entry) => {
            assert!(entry.broker.is_none());
            match &entry.policy.net {
                Net::Allowlist(hosts) => assert_eq!(hosts, &vec!["127.0.0.1:8888".to_string()]),
                other => panic!("got {other:?}"),
            }
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}
```

- [ ] **Step 5: Verify RED**

Run: `cargo test -p kastellan-core --lib workers::web_search::tests::resolve_broker_mode`
Expected: FAIL to compile (`web_search_broker_entry` / broker branch not present).

- [ ] **Step 6: GREEN — add the broker-mode entry + manifest branch**

Add a `USE_BROKER_ENV` const and a `web_search_broker_entry`:
```rust
/// Operator opt-in: route web-search through a trusted search-broker sidecar.
const USE_BROKER_ENV: &str = "KASTELLAN_WEB_SEARCH_USE_BROKER";

/// Build the web-search `ToolEntry` in **broker mode**: the worker reaches SearxNG
/// only through a core-spawned search-broker, so its `Net::Allowlist` is empty and
/// the direct endpoint/allowlist env is omitted. `entry.broker` carries the SearxNG
/// endpoint the broker forwards to; core's chokepoint spawns the broker, binds its
/// UDS into the jail, and injects `KASTELLAN_SEARCH_BROKER_UDS` so the worker's
/// `choose_search_provider` selects `BrokeredSearchProvider`.
pub fn web_search_broker_entry(binary: PathBuf, endpoint: &str) -> ToolEntry {
    let policy = SandboxPolicy {
        fs_read: vec![
            binary.clone(),
            PathBuf::from("/etc/resolv.conf"),
            PathBuf::from("/etc/hosts"),
            PathBuf::from("/etc/nsswitch.conf"),
        ],
        fs_write: vec![],
        // No direct egress — the broker holds the only route to SearxNG.
        net: Net::Allowlist(vec![]),
        cpu_ms: 5_000,
        mem_mb: 256,
        profile: Profile::WorkerNetClient,
        // No direct endpoint/allowlist env: the worker never reaches SearxNG itself.
        env: vec![],
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None,
        broker_uds: None,
        persistent_store: None,
    };
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(30_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: None,
        container_image: None,
        lockdown_shim: None,
        ephemeral_scratch: false,
        broker: Some(crate::broker::BrokerSpec::search(endpoint)),
    }
}
```
In `resolve`, after computing `endpoint`, branch:
```rust
        let use_broker = (ctx.get_env)(USE_BROKER_ENV).unwrap_or_default().trim() == "1";
        if use_broker {
            return Resolution::Register(web_search_broker_entry(binary, &endpoint));
        }
        let allowlist = host_allowlist_from_endpoint(&endpoint);
        Resolution::Register(web_search_entry(binary, &endpoint, &allowlist))
```
Note: broker mode intentionally does not require the endpoint host to be on any DB allowlist — the broker (trusted) owns the SearxNG allowlist. A blank endpoint under `USE_BROKER=1` still yields `BrokerSpec::search("")`; the broker fails closed at its own `validate_endpoint`, and the chokepoint rejects a hostless broker endpoint before spawn (Task 2 guard) — acceptable fail-closed, matching web-research.

- [ ] **Step 7: Verify GREEN + core lib**

Run:
```
cargo test -p kastellan-core --lib workers::web_search::
cargo test -p kastellan-core --lib
cargo clippy -p kastellan-core --all-targets -- -D warnings
```
Expected: PASS, clean.

- [ ] **Step 8: Commit**

```bash
git add workers/web-search/src/handler.rs core/src/workers/web_search.rs
git commit -m "feat(web-search): broker-mode manifest + provider selection

from_env picks Brokered vs Direct via choose_search_provider (broker UDS wins,
endpoint optional in broker mode). KASTELLAN_WEB_SEARCH_USE_BROKER=1 yields a
zero-egress entry declaring a Search broker; direct mode byte-identical."
```

---

## Task 8: DGX force-routed zero-egress e2e + cutover runbook

**Files:**
- Create: `core/tests/search_broker_egress_e2e.rs` (Linux + DGX-gated)
- Create: `scripts/web-search/dgx-search-broker-cutover.md`
- Delete: `scripts/web-search/setup-searxng-public.md` (superseded by the broker path)

**Interfaces:**
- Consumes: real bwrap + force-routing + a loopback SearxNG on the DGX; mirrors `core/tests/embed_broker_egress_e2e.rs` gating.

- [ ] **Step 1: Write the gated e2e (mirror the embed egress e2e's skip pattern)**

Model `core/tests/search_broker_egress_e2e.rs` on `embed_broker_egress_e2e.rs`: same `skip_if_no_userns()` early-return + `KASTELLAN_*_E2E` env gate. It must assert, against a real loopback SearxNG under force-routing:
1. web-search resolves in broker mode (`entry.broker.kind == Search`, empty `Net::Allowlist`).
2. A real `web.search{query}` dispatch returns a non-empty `results` array.
3. **Zero worker egress:** the worker's netns has no route to `127.0.0.1:8888` (the broker holds the only route). Assert via the same mechanism the embed e2e uses to prove zero embed egress (inspect that the worker policy carries no SearxNG host and the broker sidecar is the sole connector).

Read `embed_broker_egress_e2e.rs` fully and reproduce its harness (spawn the daemon/lifecycle, drive a dispatch, assert) with the search worker + `search-broker`.

- [ ] **Step 2: Run on the DGX (native Linux, real bwrap + real SearxNG)**

Per memory `dgx-native-linux-verification-over-ssh` (drive as exactly `ssh dgx '<cmd>'`), and `dgx-run-logs-tmp-scrubbed` (log to `~`, not `/tmp`):
```bash
ssh dgx 'cd ~/src/kastellan && git fetch && git checkout feat/search-broker-sidecar && setsid bash -lc "source ~/.cargo/env && cargo build -p kastellan-worker-search-broker && KASTELLAN_SEARCH_BROKER_E2E=1 cargo test -p kastellan-core --test search_broker_egress_e2e -- --nocapture > ~/search-broker-e2e.log 2>&1; echo DONE_EXIT=$? >> ~/search-broker-e2e.log" </dev/null & echo launched'
```
Poll `~/search-broker-e2e.log` for `DONE_EXIT=0` and a non-`[SKIP]` run (a `[SKIP]` line means no userns — install the AppArmor profile first per CLAUDE.md).
Expected: the e2e passes with real containment (the worker cannot reach loopback SearxNG; the broker can).

- [ ] **Step 3: Write the cutover runbook + delete the superseded public recipe**

`scripts/web-search/dgx-search-broker-cutover.md` — the exact production steps (from memory `dgx-force-routing-deploy-facts`):
1. Deploy the branch on the DGX (`git fetch && checkout && scripts/build-release.sh` — builds `kastellan-worker-search-broker` alongside the rest).
2. `kastellan-cli install --no-start …` (avoid uncontrolled cutover).
3. Re-add `Environment=KASTELLAN_EGRESS_FORCE_ROUTING=1` to `~/.config/systemd/user/kastellan-core.service` + `systemctl --user daemon-reload` (install drops it).
4. Append to `~/.config/kastellan/kastellan.env`: `KASTELLAN_WEB_SEARCH_ENDPOINT=http://127.0.0.1:8888/search` and `KASTELLAN_WEB_SEARCH_USE_BROKER=1`.
5. Confirm `kastellan-worker-search-broker` is installed next to the core binary (exe-sibling discovery) so `broker.search` resolves.
6. `systemctl --user restart kastellan-core.service`; verify in the log: force-routing on, `search-broker AVAILABLE`, web-search registers, `<tools>` includes `web.search`.
7. Test over Matrix: ask "what happened in Germany yesterday?" → expect a `web.search` step + a real answer.
Then `git rm scripts/web-search/setup-searxng-public.md`.

- [ ] **Step 4: Commit**

```bash
git add core/tests/search_broker_egress_e2e.rs scripts/web-search/dgx-search-broker-cutover.md
git rm scripts/web-search/setup-searxng-public.md
git commit -m "test(search-broker): DGX force-routed zero-egress e2e + cutover runbook

Proves a force-routed web-search worker answers via the search-broker with zero
direct SearxNG egress. Adds the DGX cutover runbook; drops the now-moot public
SearxNG recipe."
```

---

## Self-Review

**Spec coverage:** A (rename) → Task 1. B (generalize spawn) → Task 2. C (search-broker crate) → Tasks 3–4. D (worker seam + manifest) → Tasks 5–7. E (DGX e2e + cutover) → Task 8. Security properties: empty allowlist asserted (Task 7 Step 4), fail-closed chokepoint (Task 2 Step 6, guarded by re-pointed test), zero-egress e2e (Task 8). Embed byte-identical: `BrokerKind::Embed` const-equality test (Task 2 Step 1) + re-pointed embed e2e (Task 2 Step 12).

**Type consistency:** `BrokerSpec.kind: BrokerKind`, `entry.broker: Option<BrokerSpec>`, `worker.broker: Option<BrokerSidecar>`, `SandboxPolicy.broker_uds`, `broker_configs.for_kind(kind)`, `choose_search_provider → SearchProviderChoice`, `SearchProvider::search(query, count) -> Result<Vec<Hit>, SearchError>` — used consistently across Tasks 2/5/6/7. Broker binary reads `KASTELLAN_SEARCH_BROKER_{UDS,ENDPOINT}`; core injects `BrokerKind::Search.uds_env()` = `KASTELLAN_SEARCH_BROKER_UDS`; worker reads the same in `from_env` — closed loop.

**Placeholder scan:** The two "reproduce the body verbatim from the precedent" steps (Task 3 Step 7 `serve_connection`; Task 8 Step 1 harness) point at an exact existing function to copy rather than paste ~40 lines of framing that must stay identical to the precedent — deliberate, to prevent drift, not a gap. Every behavior-bearing signature and test is spelled out.

**web-common API shapes (resolved against the tree, 2026-07-11):** `HostAllowlist` exposes `from_env_json`, `from_endpoints(&[String])`, `is_allowed`, `is_allowed_endpoint` — there is **no** `from_hosts`. The broker builds its allowlist with `from_endpoints(&[host])`; tests use the `testing::al(&[&str])` helper. `Hit` = `{ title, url, snippet, engine }`, all `String`, no serde rename — adding `Deserialize` is clean and every fixture must supply all four fields. `web_common::search::search` re-checks the host allowlist and clamps `count` to `MAX_COUNT` internally, so the broker inherits both for free.
