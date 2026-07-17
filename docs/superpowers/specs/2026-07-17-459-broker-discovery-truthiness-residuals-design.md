# Design — #459 guard residuals: resolve-time broker-binary discovery + env-flag truthiness unification

**Date:** 2026-07-17
**Issue:** [#459](https://github.com/hherb/kastellan/issues/459) (the two residuals that stayed open after slice 1, PR #462)
**Status:** approved, ready for a plan
**Scope:** residuals #1 (broker-binary discovery at resolve) + #2 (truthiness unification). Residual #3 (port-bearing `tool_allowlists` row) is **deferred** — it fails closed at the proxy today and its real fix is upstream row validation, a larger slice.

---

## Context

#457 added a resolve-time guard refusing a force-routed worker whose endpoint is a
statically-dead `localhost`-**name**; #459 slice 1 (PR #462) generalized it into the
`assemble_registry` `NetScreen`. The post-merge review left two guard-adjacent residuals
open on #459, each its own slice:

1. **Broker-exemption vs a missing broker binary.** A worker that opts into a broker
   (`KASTELLAN_WEB_SEARCH_USE_BROKER`, `KASTELLAN_WEB_RESEARCH_USE_{SEARCH,EMBED}_BROKER`)
   drops its backend host from the allowlist → **empty** `Net::Allowlist` → the `NetScreen`
   deliberately exempts it (broker/zero-egress posture). But broker-binary discovery is
   best-effort at daemon startup: `BrokerConfigs::from_env` holds `None` for a kind whose
   binary is absent, and the fail-closed refusal only fires **per dispatch** at the spawn
   chokepoint (`spawn_worker_with_optional_broker` → `for_kind(kind).ok_or_else(...)`,
   "no matching broker config"). So a broker-declaring worker with an absent broker binary
   **registers, is advertised to the planner, and looks healthy** — a registered-but-dead
   tool, exactly the class the resolve-time guard exists to refuse.

2. **Truthiness dialects.** `KASTELLAN_EGRESS_FORCE_ROUTING` parses through
   `force_route::env_flag_enabled` (`1|true|yes|on`, trimmed, case-insensitive) while every
   worker `USE_*` / `ENABLE` flag parses `.trim() == "1"`. No drift *within* a single
   decision, but `…=true` enables force-routing while silently reading a neighbouring
   `…_USE_BROKER=true` as **off** — and the #452/#459 guard's remedy then tells the operator
   to set a flag they believe is already set.

## Non-goals

- Residual #3 (a port-bearing row like `localhost:8888` → net entry `localhost:8888:443`
  whose `host_of_entry` yields `localhost:8888`, not a `localhost` *name*, so it escapes the
  screen). It fails closed at the proxy regardless (a port-bearing entry can never match a
  real CONNECT); the real fix is upstream `tool_allowlists` row validation/normalization.
  Stays open on #459.
- No new broker/sandbox/microvm code. No change to the spawn chokepoint's fail-closed refusal
  (it remains the authoritative runtime backstop; this slice adds an *earlier*, friendlier
  resolve-time refusal in front of it).

---

## Residual #1 — resolve-time broker-binary discovery

### Approach (Fork A1: re-discover via the `ResolveCtx`)

`main.rs` computes `exe_dir` once (line 116) and passes the **same** value to both
`broker::config::from_env(kind, exe_dir)` (the daemon's real broker discovery) and
`build_tool_registry(pool, exe_dir)` (the registry build). Broker discovery is a pure
function of `(env, filesystem, exe_dir)` — all three already live in the `ResolveCtx`
(`get_env`, `exists`, `is_dir`, `exe_dir`). So re-running discovery from the ctx yields the
**identical** answer `BrokerConfigs::from_env` produced: a drift-proof mirror, the same
pattern `endpoint_guard::egress_will_force_route` already uses for force-routing state.

Rejected — Fork A2 (thread the concrete `BrokerConfigs` into `assemble_registry`): more
faithful in principle but adds a parameter to `assemble_registry` + `build_tool_registry`
and forces the CLI `memory l3 run` caller to construct `BrokerConfigs` too, for a guarantee
A1 already provides via identical inputs. A1 keeps `assemble_registry(manifests, ctx)` a pure
two-argument function.

### Components

- **`core/src/broker/config.rs`** — new `pub(crate) fn broker_bin_present(kind: BrokerKind, ctx: &ResolveCtx) -> bool`
  that adapts the ctx closures to the existing DI core `discover_broker_bin_with(...).is_some()`.
  Single source of truth for "is this kind's broker binary discoverable" — the same helper the
  spawn path's discovery is built on. (`is_dir` is available on the ctx; the DI core already
  takes all four probes.)

- **`core/src/registry_build.rs::assemble_registry`** — in the `Resolution::Register(entry)`
  arm, after resolving `name`, an **unconditional** broker-presence check:

  ```
  if let Some(spec) = &entry.broker {
      if !crate::broker::config::broker_bin_present(spec.kind, ctx) {
          tracing::error!(tool = name, kind = ?spec.kind,
              "worker declares a {kind} broker but its binary is not discoverable; \
               skipping — it would register but every dispatch fails fail-closed at \
               the spawn chokepoint");
          continue;
      }
  }
  ```

  Placed adjacent to the existing `NetScreen` block. **Not** gated on force-routing: a
  broker-declaring worker with no broker binary is dead in every mode (the spawn chokepoint
  refuses unconditionally), so the resolve-time refusal is unconditional too — strictly
  stronger than, and orthogonal to, the force-routing-gated `NetScreen`. Order relative to
  `NetScreen` is cosmetic (a broker worker has an empty allowlist → `NetScreen::Ok`), so the
  two never both fire.

### Severity

**Refuse** (skip registration, ERROR log), identical handling to `NetScreen::Refuse` and
`Resolution::Misconfigured`. Not a warn — the worker is 100% unreachable, not degraded.

### Tests (TDD)

- `broker/config.rs`: `broker_bin_present` returns `true` when the kind's sibling/override
  binary exists via the ctx probes, `false` when absent (mirrors the existing
  `discover_broker_bin_with` tests).
- `registry_build.rs`: `FakeManifest` gains a broker-declaring Register outcome
  (`entry.broker = Some(BrokerSpec{ kind, endpoint })`, empty `Net::Allowlist`). Tests:
  - broker binary present (ctx `exists` true for the sibling) ⇒ registers + records.
  - broker binary absent ⇒ refused (not in registry, no `LoadedToolRecord`).
  - refused **even when not force-routed** (pins the unconditional gate).

---

## Residual #2 — env-flag truthiness unification

### Approach (Fork B1: a `ResolveCtx::flag_enabled` helper)

Route every boolean worker flag through the single existing primitive
`force_route::env_flag_enabled` (`pub(crate)`, tested, already reused by `endpoint_guard`).
Add an ergonomic accessor on the ctx every parse site already holds:

```
impl ResolveCtx<'_> {
    /// True iff env var `key` is set to a truthy value (`1|true|yes|on`,
    /// trimmed, case-insensitive) — the one daemon-wide flag dialect
    /// (shared with force-routing via `force_route::env_flag_enabled`).
    pub(crate) fn flag_enabled(&self, key: &str) -> bool {
        crate::worker_lifecycle::force_route::env_flag_enabled((self.get_env)(key))
    }
}
```

Rejected — Fork B2 (inline `force_route::env_flag_enabled((ctx.get_env)(KEY))` at each site):
no new method but repeats the module path ~11× and reads noisier.

### The single primitive + two call styles

`force_route::env_flag_enabled(Option<String>) -> bool` is the one truthiness primitive. It is
reached two ways depending on what a resolve site holds:

- **Sites with the `ResolveCtx`** (`web_fetch`, `web_search`, `web_research`, `python_exec` read
  `(ctx.get_env)(KEY)` directly inside `resolve()`) ⇒ the sugar `ctx.flag_enabled(KEY)`.
- **Sites behind a ctx-free `env_lookup` closure** (`browser_driver::resolve_env` and
  `gliner_relex::resolve_env` take `env_lookup: impl Fn(&str) -> Option<String>` for
  closure-based testability, so no `ctx` is in scope) ⇒ call
  `force_route::env_flag_enabled(env_lookup(KEY))` directly. Same primitive, same dialect.

### Sites re-pointed (all currently `.unwrap_or_default().trim() == "1"` / `v.trim() == "1"`)

- `workers/web_fetch.rs` — `USE_MICROVM` *(ctx)*
- `workers/web_search.rs` — `USE_BROKER`, `USE_MICROVM` *(ctx)*
- `workers/web_research.rs` — `USE_SEARCH_BROKER`, `USE_EMBED_BROKER`, `USE_MICROVM` *(ctx)*
- `workers/python_exec.rs` — `ENABLE` (macOS block + Linux block, and the host-mode re-check),
  `USE_CONTAINER`, `USE_MICROVM` *(ctx)*
- `workers/gliner_relex/resolve.rs` — `ENABLE`, `USE_CONTAINER` *(env_lookup closure)*
- `workers/browser_driver.rs` — `ENABLE` *(env_lookup closure)*

### Behaviour change

`=true` / `=yes` / `=on` / ` TRUE ` now **enable** flags that previously required exactly
`"1"`; `"1"` still enables; `0` / `false` / `off` / empty / `banana` / unset stay disabled.
This is intended (kills the silent-off footgun). Existing tests asserting `"1"` remain green;
new/updated tests pin the widened enable set and the disable set.

### Tests (TDD)

- `worker_manifest.rs`: `ResolveCtx::flag_enabled` — truthy set enables, falsy/unset disables
  (mirrors `endpoint_guard::host_mode_follows_the_force_routing_flag_truthiness`).
- Per-worker resolve tests: at least one widened-truthiness case per flag family (e.g.
  `USE_MICROVM=true` selects the VM entry; `USE_BROKER=on` selects broker mode) plus a falsy
  case, added to the existing resolve test modules.

---

## Verification

- **Mac (Seatbelt):** `cargo build -p kastellan-core`, targeted unit tests
  (`broker::config`, `registry_build`, `worker_manifest`, and each re-pointed worker's resolve
  tests), `clippy -p kastellan-core --all-targets -D warnings`.
- **DGX re-gate owed:** #1 touches `registry_build` (Linux-gated `entry_is_vm` + its VM
  screen test) and #2 re-points cfg-linux VM resolve paths — Mac clippy compiles those out
  (the `cfg-linux-e2e-deadcode-dgx-clippy` lesson). Close it with a DGX
  `clippy -p kastellan-core --all-targets -D warnings` + full-workspace `cargo test --workspace`
  (new baseline over 2564/0/47).

## Files touched (summary)

| File | Change |
|------|--------|
| `core/src/broker/config.rs` | + `broker_bin_present(kind, ctx)` + tests |
| `core/src/registry_build.rs` | + unconditional broker-presence refuse in Register arm; `FakeManifest` broker outcome + tests |
| `core/src/worker_manifest.rs` | + `ResolveCtx::flag_enabled(key)` + test |
| `core/src/workers/web_fetch.rs` | re-point `USE_MICROVM` + test |
| `core/src/workers/web_search.rs` | re-point `USE_BROKER`, `USE_MICROVM` + tests |
| `core/src/workers/web_research.rs` | re-point `USE_SEARCH_BROKER`, `USE_EMBED_BROKER`, `USE_MICROVM` + tests |
| `core/src/workers/python_exec.rs` | re-point `ENABLE` (all sites), `USE_CONTAINER`, `USE_MICROVM` via `ctx.flag_enabled` + tests |
| `core/src/workers/gliner_relex/resolve.rs` | re-point `ENABLE`, `USE_CONTAINER` via `env_flag_enabled(env_lookup(..))` + test |
| `core/src/workers/browser_driver.rs` | re-point `ENABLE` via `env_flag_enabled(env_lookup(..))` + test |

On completion: #459 residual #1 + #2 closed; #459 stays open only for residual #3.
