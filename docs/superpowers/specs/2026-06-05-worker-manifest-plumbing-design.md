# Worker manifest plumbing — design

**Date:** 2026-06-05
**Status:** design approved, ready for planning slice
**Roadmap item:** #11 (worker manifest plumbing); resolves worker-lifecycle spec
open question 1 (TOML vs Rust consts) and advances open question 6 (production
worker-binary discovery convention).

## Problem

Today the daemon's `ToolRegistry` is assembled by hand. Each worker has a
bespoke Rust constructor — `shell_exec_entry(binary, &allowlist)` in
`core/src/scheduler/tool_dispatch.rs` and `gliner_relex_entry(&env)` in
`core/src/workers/gliner_relex.rs` — and `core/src/registry_build.rs` contains
**hardcoded per-worker branches** that decide which workers exist, discover
their binaries from per-worker env vars, call each bespoke constructor, and
insert by name. Adding a worker means writing a new constructor *and*
hand-editing `build_tool_registry` (and its gliner-specific skip-logging
helper).

Two costs follow:

1. **No uniform registration substrate.** With "a lot more workers with diverse
   inputs coming soon", every addition is another hand-edit of a central
   function and another chance to write an inconsistent or subtly-insecure
   entry. There is no single declarative place a worker says what it is.
2. **No production discovery convention.** Binaries are found via per-worker env
   vars (`KASTELLAN_SHELL_EXEC_BIN`, gliner's venv dir) plus `target/debug` for
   tests. A deployed daemon has no stable, env-free way to locate its workers
   (worker-lifecycle open question 6).

## Goals

- One **uniform, declarative** way each worker describes itself, replacing the
  hardcoded branches in `registry_build.rs` with a single iterate-the-list loop.
- A **production binary-discovery convention** resolved relative to the running
  `kastellan` binary, so a deployed daemon finds plain workers with no env vars
  set, while existing env-var overrides keep working.
- **Behaviour-preserving** for the two workers that exist today: every produced
  `ToolEntry` is byte-identical, every integration pin stays green, no schema or
  audit-row change.

## Non-goals

- **No on-disk / TOML manifest, no deploy-time tuning of any field.** The
  containment shape (`SandboxPolicy` + `Lifecycle`) stays compiled into the
  binary. Rationale: the threat model says a worst-case compromise reaches at
  most the agent's own OS user; a manifest file writable by that user would be a
  containment-escalation surface. Host-dependent resource knobs that genuinely
  need tuning (e.g. gliner's `mem_mb`) already have per-worker env-var overrides
  inside their own resolution, which carry no extra attack surface. If real
  demand for operator-tunable resource limits appears later, revisit then.
- **No change to the operational argv allowlist.** It stays in the
  `tool_allowlists` DB table — operator-tunable, audited — and is threaded into
  resolution, *not* baked into the manifest. Containment (manifest, compiled)
  and operational allowlist (DB, mutable) remain separate, exactly as today.
- **No dynamic/runtime registration.** The registry is still built once at
  startup and shared immutably.

## Key constraint that shapes the design

A worker's `SandboxPolicy` is only **partly static**. shell-exec's `fs_read`
holds its own (discovered) binary path; gliner's holds the weights dir, venv
dir, and editable `src/` dir, all resolved from the environment at startup. So a
manifest cannot be pure data — there is always a **`resolve(env, probes) →
ToolEntry`** step. The current `*_entry(env)` constructors *are* that step; they
are merely non-uniform and invoked from hardcoded branches. The design makes the
resolve step a uniform, pure, per-worker function behind a trait.

## Design

### 1. The `WorkerManifest` trait

New module `core/src/worker_manifest.rs`:

```rust
/// A worker's self-description. One impl per worker, living in that worker's
/// host-side module. The daemon iterates a static list of these at startup to
/// build the ToolRegistry — replacing the hardcoded per-worker branches in
/// registry_build.rs.
pub trait WorkerManifest {
    /// Tool name the registry/planner keys on (e.g. "shell-exec").
    fn name(&self) -> &'static str;

    /// If this worker needs the operational argv allowlist from the
    /// `tool_allowlists` DB table, the tool name to query (usually == name()).
    /// None ⇒ no allowlist. The async fetch stays in the builder; the result is
    /// threaded into resolve via ctx.
    fn allowlist_tool(&self) -> Option<&'static str> {
        None
    }

    /// PURE resolution: host env + fs probes + pre-fetched allowlist → outcome.
    /// No std::env / no real fs access inside — everything arrives via ctx, so
    /// each impl is independently unit-testable with fakes (TDD).
    fn resolve(&self, ctx: &ResolveCtx<'_>) -> Resolution;
}
```

The trait is where each worker's diversity lives: its own impl in its own
module, with room to grow further methods later (health checks, richer metadata)
without disturbing any shared signature.

### 2. `Resolution` — the three uniform outcomes

```rust
/// The three outcomes every worker already produces today, unified so the
/// builder logs each at one consistent severity.
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
```

Mapping from today's behaviour:

- gliner's 5-variant `ResolveSkipReason` collapses to `Disabled` (the
  `KASTELLAN_GLINER_RELEX_ENABLE != "1"` case) vs `Misconfigured` (the four
  broken-env cases). The per-variant `log_gliner_relex_skip` helper in
  `registry_build.rs` is deleted; its severity choices move into the gliner
  manifest's `resolve`.
- shell-exec's "binary not a file" `warn!` becomes `Misconfigured`.

### 3. `ResolveCtx` — minimal, universal resolve inputs

```rust
/// Minimal, *universal* resolve inputs — deliberately not a per-worker kitchen
/// sink. Arbitrary worker-specific config arrives through `get_env` (the
/// universal extension point), so adding an exotic worker never widens this
/// struct.
pub struct ResolveCtx<'a> {
    /// Read an environment variable. Injected (not std::env) so resolvers are
    /// pure and unit-testable with a fake env.
    pub get_env: &'a dyn Fn(&str) -> Option<String>,
    /// Probe: does this path exist?
    pub exists: &'a dyn Fn(&Path) -> bool,
    /// Probe: is this path a directory?
    pub is_dir: &'a dyn Fn(&Path) -> bool,
    /// Directory of the running `kastellan` binary, for current_exe()-relative
    /// worker discovery. None when it cannot be determined (fail-soft).
    pub exe_dir: Option<&'a Path>,
    /// Operational argv allowlist, pre-fetched from the DB by the builder,
    /// keyed by tool name. A worker that declared `allowlist_tool()` looks
    /// itself up here; absent ⇒ empty.
    pub allowlist: &'a dyn Fn(&str) -> Vec<String>,
}
```

`get_env` (closure-injected env reads, same style as gliner's existing
`resolve_env(get_env, is_dir, exists)`) is the extension point: a future worker
with novel inputs reads its own env vars through it — no new `ResolveCtx` field,
no coupling between workers' input surfaces. The one non-env dynamic input (the
async DB allowlist) arrives as a generic keyed closure, not a per-worker field.

### 4. Binary-discovery convention

Shared pure helper in `worker_manifest.rs`:

```rust
/// Locate a worker binary. Precedence:
///   1. explicit override env var (e.g. "KASTELLAN_SHELL_EXEC_BIN") if it names
///      an existing file — preserves every current deployment/test;
///   2. else the exe-relative sibling default `<exe_dir>/<default_name>`, if it
///      exists.
/// Returns None when neither yields an existing file (resolver → Misconfigured).
pub fn discover_binary(
    ctx: &ResolveCtx<'_>,
    override_env: &str,
    default_name: &str,
) -> Option<PathBuf>;
```

> **Post-review correction (2026-06-05, PR #187).** The first sketch had a
> set-but-invalid override *fall through* to the sibling default. That was wrong:
> in a security-first daemon an explicit override is a statement of intent, and
> silently running a *different* binary than the operator named is a footgun. The
> shipped semantics make a set override **authoritative** — honoured iff it names
> a runnable file, else **fail closed** (`None` → `Misconfigured`); the sibling
> default applies *only* when the override is unset. This also restores exact
> parity with the pre-manifest behaviour (`KASTELLAN_SHELL_EXEC_BIN` set but not a
> file ⇒ not registered).

**The convention: a plain compiled worker lives as a *sibling of the `kastellan`
binary* (`<exe_dir>/<worker-name>`), discoverable with no env vars set.**

- **Production:** a flat install (`kastellan` + its workers in one bindir,
  controlled by the systemd/launchd unit's install path) just works — the daemon
  finds workers via `current_exe()` with zero `KASTELLAN_*_BIN` env. This is the
  stable install-location convention open question 6 asks for.
- **Dev/test:** cargo already places `target/debug/kastellan` and
  `target/debug/kastellan-worker-shell-exec` side by side, so the same default
  works in the test tree. Override-wins precedence means tests that *do* set
  `KASTELLAN_SHELL_EXEC_BIN` keep passing unchanged; we additionally gain a test
  proving zero-env discovery.

`exe_dir` is computed once by the builder via `std::env::current_exe()` and
threaded through `ResolveCtx`. If `current_exe()` fails (rare), `exe_dir` is
`None` and discovery falls back to override-env-only (fail-soft, logged).

**Asymmetry — gliner is exempt.** The sibling default is only for plain compiled
workers. gliner-relex is a Python venv shim at `.venv/bin/...` under a data dir,
plus weights, so its `resolve` does its own env-driven resolution exactly as
today. `discover_binary` is a helper plain workers opt into, not a mold forced
on everyone — which is the point of per-worker `resolve`.

An FHS-style `<prefix>/libexec/kastellan/<worker>` layout was considered and
rejected for now: it breaks the cargo-sibling property (forcing `../libexec/...`
traversal and env vars in tests) for no real gain at this stage. Note it as a
future packaging refinement, not built here.

### 5. The static list and the builder loop

The single registration point, in `worker_manifest.rs`:

```rust
/// Every worker the daemon may register. Order is irrelevant (the registry is a
/// keyed map). Adding a worker = add its manifest impl + one line here.
pub static WORKER_MANIFESTS: &[&dyn WorkerManifest] =
    &[&ShellExecManifest, &GlinerRelexManifest];
```

`build_tool_registry` in `registry_build.rs` becomes one uniform loop; the
hardcoded `if KASTELLAN_SHELL_EXEC_BIN { … }` / `if let Some(gliner) { … }`
branches are deleted:

```rust
pub async fn build_tool_registry(pool: &PgPool)
    -> Result<(ToolRegistry, Vec<LoadedToolRecord>), DbError>
{
    // 1. Pre-fetch allowlists (async) for every manifest that declares one,
    //    into a HashMap<String, Vec<String>>. The ONLY async step.
    // 2. Compute exe_dir once via current_exe().
    // 3. Build a real ResolveCtx (get_env = std::env::var, real fs probes,
    //    exe_dir, allowlist = closure over the prefetched map).
    // 4. for m in WORKER_MANIFESTS:
    //        match m.resolve(&ctx) {
    //          Register(entry)       => reg.insert(m.name(), entry);
    //                                   loaded.push(record(m.name(), &entry, &allowlist));
    //                                   info!(tool = m.name(), "registering tool");
    //          Disabled{detail}      => info!(tool = m.name(), %detail, "worker disabled; skipping"),
    //          Misconfigured{detail} => error!(tool = m.name(), %detail, "worker misconfigured; skipping"),
    //        }
    // 5. Ok((reg, loaded))
}
```

`build_tool_registry`'s signature simplifies — it no longer takes a pre-built
`gliner_relex_entry: Option<ToolEntry>` argument; that construction now happens
inside `GlinerRelexManifest::resolve`. The caller in `main.rs` drops its
`build_gliner_relex_entry()` pre-call.

The `LoadedToolRecord` / `sha256_argv0_list` / `build_registry_loaded_payload`
machinery is **unchanged** — same `registry.loaded` audit row, same snapshot the
L3 approval gate reads.

To keep the async/DB shell thin over a pure core (coding rule #1), the loop body
is split into a pure helper:

```rust
/// Pure: given the manifest list and a fully-built ResolveCtx, produce the
/// registry + per-tool records. No async, no DB — unit-testable with a fake
/// manifest list and a fake allowlist closure.
fn assemble_registry(
    manifests: &[&dyn WorkerManifest],
    ctx: &ResolveCtx<'_>,
) -> (ToolRegistry, Vec<LoadedToolRecord>);
```

`build_tool_registry` then = (async allowlist prefetch + ctx construction) →
`assemble_registry`.

### 6. File layout

Each worker owns its host-side manifest ("a Rust struct in each worker crate").

| File | Change |
|------|--------|
| `core/src/worker_manifest.rs` *(new)* | `WorkerManifest` trait, `Resolution`, `ResolveCtx`, `discover_binary`, `WORKER_MANIFESTS`, `assemble_registry` (or `assemble_registry` may live in `registry_build.rs` — planner's call) |
| `core/src/workers/shell_exec.rs` *(new)* | `ShellExecManifest` impl; **`shell_exec_entry` relocated here** from `tool_dispatch.rs`, re-exported so `scheduler::shell_exec_entry` paths don't break — also trims `tool_dispatch.rs` |
| `core/src/workers/gliner_relex.rs` | add `GlinerRelexManifest` impl wrapping the existing `resolve_env` + `gliner_relex_entry`; the skip → `Disabled`/`Misconfigured` mapping lives here |
| `core/src/registry_build.rs` | `build_tool_registry` rewritten to the loop; `build_gliner_relex_entry` / `log_gliner_relex_skip` deleted; record/audit helpers kept |
| `core/src/main.rs` | drop the `build_gliner_relex_entry()` pre-call; call the simplified `build_tool_registry(pool)` |

## Testing (TDD)

**Pure unit tests** (closure-injected — no PG, no real fs):

- `discover_binary`: (a) override env naming an existing file wins even when a
  sibling also exists; (b) no override → exe-relative sibling found; (c) neither
  exists → `None`; (d) `exe_dir = None` → override-only, no panic.
- `ShellExecManifest::resolve`: happy path (binary discovered, injected
  allowlist closure) → `Register` with the **byte-identical** `SandboxPolicy`
  shipped today (`fs_read = [binary]`, `KASTELLAN_SHELL_ALLOWLIST` env = JSON of
  the allowlist, `Net::Deny`, `cpu_ms 5000`, `mem_mb 256`, `WorkerStrict`,
  `wall_clock_ms 30_000`, `SingleUse`); binary absent → `Misconfigured`.
- `GlinerRelexManifest::resolve`: `Disabled` flag → `Disabled`; each broken-env
  case (weights dir missing, venv unresolvable, shim missing) → `Misconfigured`;
  happy path → `Register` with today's exact gliner policy + `IdleTimeout` caps.
  Reuses `resolve_env` underneath — mostly an adapter test.
- `assemble_registry`: with an injected fake manifest list + fake allowlist
  closure — `Register` → inserted + recorded; `Disabled` / `Misconfigured` →
  skipped, not recorded.

**Integration pins (behaviour-preserving — must stay green unchanged):**

- `cli_ask_e2e` (full prod chain through dispatch), `shell_exec_e2e` (core →
  sandbox → shell-exec round-trip), `cli_memory_l3_run_daemon_e2e` (reads the
  `registry.loaded` snapshot). Byte-identical `ToolEntry`s ⇒ all pass untouched.
- **One new integration test** proving the payoff: a registry build with
  `KASTELLAN_SHELL_EXEC_BIN` **unset** still registers shell-exec via the
  exe-relative sibling default. The only genuinely new behaviour, and additive.

## Migration / behaviour preservation

Migration is behaviour-preserving by construction. Every `ToolEntry` the two
workers produce is identical to today's; the only new reachable behaviour is the
additive sibling-discovery fallback (override still wins). The
`KASTELLAN_SHELL_EXEC_ALLOWLIST`-deprecation warning is preserved. No migration,
no schema change, no audit-row change.

## Verification

On the DGX (native Linux, live PG), per the handover convention:

```sh
cargo test --workspace          # current 1297 baseline + new resolve/discovery/
                                # assemble units + the zero-env integration test,
                                # all green, zero [SKIP]
cargo clippy --workspace --all-targets --locked -- -D warnings   # exit 0
```

## Resolved open questions

- **Worker-lifecycle open question 1 (manifest format).** Resolved: Rust trait +
  per-worker impl, compiled in. No TOML, no on-disk config — chosen on
  threat-model grounds (no file-mutable containment shape) and because operators
  do not need to edit manifests.
- **Handover / design-plan open question 6 (production worker-binary
  discovery).** Advanced: exe-relative sibling default
  (`current_exe()`-relative), env-var override wins. Plain compiled workers
  resolve env-free in a flat install; complex workers (gliner) keep bespoke
  discovery. (FHS `libexec` layout noted as a future packaging refinement.)

## Out of scope / deferred

- libexec / FHS install layout (future packaging slice).
- Operator-tunable resource limits via config file (revisit only with real
  demand; env-var overrides cover the known case today).
- Richer per-worker trait methods (health checks, introspection) — the trait
  leaves room; nothing built now.
