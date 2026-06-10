# `kastellan.target` — supervise Postgres + core as one unit (Phase 0)

**Status:** design approved 2026-06-06. Implements ROADMAP.md Phase-0
("Service supervisor") line: *"`kastellan.target` that brings up Postgres,
inference, core, workers."*

## Problem

The supervisor crate already ships the building blocks — `core_service_spec`,
`postgres_service_spec`, the `Supervisor` trait, and both OS drivers
(`SystemdUser`, `LaunchAgents`) — but there is **no orchestrating target** that
brings the canonical services up together with the right ordering. An operator
must install and start each unit by hand, in the right order, with no single
"bring up kastellan" handle.

This slice adds that handle: one `kastellan.target` (systemd) / equivalent bundle
(launchd) that brings up **Postgres → core** in dependency order.

## Scope decisions (resolved during brainstorming)

1. **Inference is an external dependency, not a target member.** Local inference
   (vLLM on the DGX, Ollama/MLX on the Mac) stays operator-managed with its own
   venv/weights/quant config. Core's startup probe already health-checks the
   llm-router endpoint and fails closed if unreachable. No inference
   `ServiceSpec` is added.

2. **"Workers" drop out of the target.** In kastellan's architecture workers are
   not long-lived supervised units — `tool_host` spawns them on-demand per call,
   each in its own sandbox, and GLiNER-Relex is idle-timeout-managed *inside*
   core. They come up implicitly when core does. So target membership is
   **Postgres + core only**; the ROADMAP line's "workers" is satisfied by core
   owning them, not by separate units.

3. **Target mechanics: native `.target` on Linux, readiness-based bundle on
   macOS (Approach A).** systemd has native `.target` units with `After=`/
   `Wants=` ordering; launchd has **no target/aggregation concept and no
   reliable inter-agent ordering**. We use each OS's idiomatic mechanism rather
   than forcing symmetry:
   - **Linux:** a real `kastellan.target` unit; `systemctl --user start
     kastellan.target` pulls in the members and orders them via `After=`.
   - **macOS:** no target unit; the supervisor bootstraps the members in order.
     Inter-service ordering is **not** enforced by launchd — it relies on core's
     existing fail-closed-restart-until-Postgres-ready loop (`KeepAlive=true`).
     This asymmetry is documented honestly, not hidden.

4. **Slice scope: supervisor library layer + e2e.** No `kastellan-cli` command
   this session (an operator drives the target via `systemctl`/`launchctl`, or a
   later "Slice 3 operator surface" adds the CLI). The slice lands the ROADMAP
   item end-to-end at the library level.

## Design

### Data model (`supervisor/src/lib.rs`)

Add two backend-neutral ordering fields to `ServiceSpec`. **Both default to
empty/`None`**, so any spec that does not opt in emits byte-identical output to
today — existing single-service installs are unaffected (a behaviour-preserving
invariant pinned by a unit test).

```rust
/// Names of services this one must start *after* (systemd `After=`).
/// Ignored on launchd (which has no ordering; see module docs).
pub after: Vec<String>,
/// The target this service belongs to. When set, systemd emits
/// `PartOf=<target>.target` and `WantedBy=<target>.target`; launchd ignores it.
pub part_of: Option<String>,
```

New pure type, also in `lib.rs`:

```rust
/// A named bundle of services brought up together. `members` are listed in
/// start order (dependencies first); teardown reverses the order.
pub struct TargetSpec {
    pub name: String,
    pub members: Vec<String>,
}
```

### Pure builders (`supervisor/src/specs.rs`)

- `KASTELLAN_TARGET_NAME: &str = "kastellan"`.
- `postgres_service_spec(...)` sets `after: vec![]`, `part_of:
  Some(KASTELLAN_TARGET_NAME)` — the dependency leaf.
- `core_service_spec(...)` sets `after: vec![POSTGRES_SERVICE_NAME.into()]`,
  `part_of: Some(KASTELLAN_TARGET_NAME)` — core genuinely must start after
  Postgres.
- New `kastellan_target_spec() -> TargetSpec` → `{ name: KASTELLAN_TARGET_NAME,
  members: vec![POSTGRES_SERVICE_NAME, CORE_SERVICE_NAME] }`.

All pure: no I/O, same call → same value.

### Trait surface (`supervisor/src/lib.rs`)

Four `dyn`-safe methods on `Supervisor`, **with default implementations** so
existing backends keep compiling and the macOS path needs no override:

```rust
fn install_target(&self, target: &TargetSpec, members: &[ServiceSpec])
    -> Result<(), SupervisorError>;
fn start_target(&self, target: &TargetSpec) -> Result<(), SupervisorError>;
fn stop_target(&self, target: &TargetSpec) -> Result<(), SupervisorError>;
fn uninstall_target(&self, target: &TargetSpec) -> Result<(), SupervisorError>;
```

**Default implementation = the readiness-based bundle** (exactly what launchd
needs):
- `install_target`: `self.install(member)` for each member spec.
- `start_target`: `self.start(name)` for each name in `target.members` order
  (Postgres first, then core). No explicit readiness wait — core's
  fail-closed-restart loop handles the "Postgres not ready yet" window.
- `stop_target`: `self.stop(name)` in **reverse** member order.
- `uninstall_target`: `self.uninstall(name)` per member (reverse order).

`LaunchAgents`, `NotYetImplemented`, and any future Unix backend inherit these
defaults unchanged. The default never references a target *unit* — there is none
on launchd.

### systemd override (`supervisor/src/systemd_user.rs`)

`SystemdUser` overrides the four methods to use the native target:
- `install_target`: write each member unit (via the existing `install` path) and
  write a `kastellan.target` unit file via a new pure `build_target_unit(&TargetSpec)
  -> String` that emits:
  ```
  [Unit]
  Description=kastellan service bundle
  Wants=kastellan-postgres.service kastellan-core.service

  [Install]
  WantedBy=default.target
  ```
  then `daemon-reload`.
- `start_target`: `systemctl --user start kastellan.target` (systemd resolves
  member ordering from each member unit's `After=`).
- `stop_target`: `systemctl --user stop kastellan.target`.
- `uninstall_target`: stop the target, remove the `.target` unit and member unit
  files, `daemon-reload`.

`build_unit_file` gains `After=` and `PartOf=` lines in the `[Unit]` section, and
switches the `[Install] WantedBy=` to `<target>.target`, **only when the
corresponding fields are set**. A spec with empty `after` and `None` `part_of`
emits exactly today's output.

## Testing (TDD)

**Pure unit tests:**
- `build_target_unit` emits `Wants=` listing both member services.
- `build_unit_file` emits `After=kastellan-postgres.service` and
  `PartOf=kastellan.target` when those fields are set.
- `build_unit_file` omits both and keeps `WantedBy=default.target` when they are
  unset — the **behaviour-preserving pin** for existing single-service installs.
- `specs.rs`: `postgres_service_spec`/`core_service_spec` carry the expected
  `after`/`part_of`; `kastellan_target_spec` lists `[postgres, core]` in order.
- launchd `build_plist` is unchanged (it ignores `after`/`part_of`) — an
  assertion pins that the plist body is identical with/without those fields set.

**Gated e2e** (`supervisor/tests/`, mirrors the existing
`systemd_user_smoke.rs` / `launchd_agents_smoke.rs` skip-as-pass-on-`probe`-
failure pattern):
- Build a 2-member `TargetSpec` of **trivial long-running dummy programs** (e.g.
  `/bin/sleep <large>` or a tiny script) installed into temp unit/agent dirs.
- `install_target` → `start_target` → assert both members reach
  `ServiceStatus::Active`.
- On systemd, assert the written `kastellan.target` contains `Wants=` for both
  members and the core unit contains `After=`.
- `stop_target` → `uninstall_target` → assert both members report
  `NotInstalled`.
- This validates **target orchestration mechanics in isolation**. Real
  Postgres + core bring-up via the target is a heavier system test, out of scope
  for this slice.

## Caveats / known limitations (stated, not hidden)

- **launchd has no native ordering.** macOS inter-service ordering relies on
  core's existing fail-closed-restart-until-Postgres-ready behaviour. If that
  loop is ever removed, macOS bring-up ordering regresses — the dependency is
  load-bearing and noted here and in the launchd module docs.
- **`systemd_user.rs` is already over the 500-LOC cap (798).** This slice keeps
  its additions minimal (`build_target_unit` + four small override methods) and
  flags the file as a pre-existing split candidate; the split is deliberately
  **not** bundled into this feature PR to keep it focused.
- **No readiness probe in the default bundle.** A future enhancement could have
  `start_target` poll `status` between members; today we rely on core's restart
  loop, which is simpler and already proven.

## Out of scope (explicit)

- Inference-server lifecycle management (external dependency by decision 1).
- An `kastellan-cli supervisor up/down` operator command (decision 4).
- Exponential restart backoff (tracked separately as Option K / ROADMAP line 61).
- Real Postgres+core system-level bring-up test.
