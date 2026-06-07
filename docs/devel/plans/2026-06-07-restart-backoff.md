# Cross-Platform Restart Backoff (Option K) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give `ServiceSpec` an optional exponential restart backoff that the systemd backend emits as `RestartSteps`/`RestartMaxDelaySec`, the launchd backend warns-and-ignores, and the two long-running daemon specs (core, postgres) use with a 5s→300s/8-step curve.

**Architecture:** One additive `Option<RestartBackoff>` field on `ServiceSpec` (`#[serde(default)]`, so `None` reproduces today's output byte-for-byte). The systemd unit-file builder emits two extra directives inside its existing `keep_alive` block; the launchd `install()` logs one `tracing::warn!` and writes today's plist unchanged. Mirrors the existing additive `after`/`part_of` precedent exactly.

**Tech Stack:** Rust, `serde`, `tracing`, `cargo test`/`clippy`. All work is in the `hhagent-supervisor` crate. The launchd backend is `#[cfg(target_os = "macos")]`; the dev box is macOS, so both backends compile and test locally.

**Design doc:** [`docs/devel/specs/2026-06-07-restart-backoff-design.md`](../specs/2026-06-07-restart-backoff-design.md)

**Build prelude (every `cargo` step assumes this was sourced once):**
```sh
source "$HOME/.cargo/env"
```

---

## File Structure

- `supervisor/src/lib.rs` — add `RestartBackoff` type + `ServiceSpec.restart_backoff` field; extend serde tests. Fix the one test-fixture literal here.
- `supervisor/src/systemd_user/builder.rs` — emit `RestartSteps`/`RestartMaxDelaySec`; add 3 builder tests. Fix its fixture literal.
- `supervisor/src/launchd_agents.rs` — warn at install when backoff is set.
- `supervisor/src/launchd_agents/builders.rs` — add 1 "plist unchanged" test. Fix its 2 fixture literals.
- `supervisor/src/specs.rs` — wire core + postgres specs with the curve; add 2 tests. (2 production literals get the real value.)
- `supervisor/src/systemd_user/tests.rs`, `supervisor/src/launchd_agents/tests.rs`, `supervisor/tests/target_smoke.rs`, `supervisor/tests/launchd_agents_smoke.rs`, `supervisor/tests/systemd_user_smoke.rs` — each carries one or more `ServiceSpec` literal fixtures that must gain `restart_backoff: None,` to keep compiling.

**All 13 `ServiceSpec` literal sites** (adding a struct field breaks every literal):
- Get `restart_backoff: None,` (11): `lib.rs:316`, `systemd_user/builder.rs:256`, `launchd_agents/builders.rs:240`, `launchd_agents/builders.rs:398`, `systemd_user/tests.rs:20`, `launchd_agents/tests.rs:16`, `tests/target_smoke.rs:27`, `tests/launchd_agents_smoke.rs:131`, `tests/launchd_agents_smoke.rs:189`, `tests/launchd_agents_smoke.rs:231`, `tests/systemd_user_smoke.rs:112`.
- Get the real curve (2): `specs.rs` `core_service_spec` (~line 84), `specs.rs` `postgres_service_spec` (~line 143).

In every literal the new field is added **as the last field, after `part_of: ...,`** (all fixtures currently end with `part_of`).

---

### Task 1: `RestartBackoff` type + `ServiceSpec` field + restore compilation

**Files:**
- Modify: `supervisor/src/lib.rs` (add type after `ServiceSpec`, add field, fix fixture at `:316`, extend serde test at `:396`)
- Modify (mechanical field add): `supervisor/src/systemd_user/builder.rs:256`, `supervisor/src/launchd_agents/builders.rs:240` & `:398`, `supervisor/src/systemd_user/tests.rs:20`, `supervisor/src/launchd_agents/tests.rs:16`, `supervisor/tests/target_smoke.rs:27`, `supervisor/tests/launchd_agents_smoke.rs:131/189/231`, `supervisor/tests/systemd_user_smoke.rs:112`

- [ ] **Step 1: Write the failing serde round-trip test**

In `supervisor/src/lib.rs`, inside `mod spec_ordering_tests` (near line 396), add:

```rust
    #[test]
    fn service_spec_restart_backoff_round_trips() {
        let s = ServiceSpec {
            name: "svc".into(),
            program: PathBuf::from("/bin/true"),
            args: vec![],
            env: vec![],
            working_dir: None,
            keep_alive: true,
            stdout_log: None,
            stderr_log: None,
            after: vec![],
            part_of: None,
            restart_backoff: Some(RestartBackoff { max_delay_sec: 300, steps: 8 }),
        };
        let json = serde_json::to_string(&s).expect("serialize");
        let back: ServiceSpec = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            back.restart_backoff,
            Some(RestartBackoff { max_delay_sec: 300, steps: 8 })
        );
    }
```

`PathBuf` is already imported in `spec_ordering_tests`? It is not — add `use std::path::PathBuf;` at the top of that test module if the compiler reports it missing (the sibling `default_target_tests` already uses `std::path::PathBuf` fully-qualified, so prefer `std::path::PathBuf::from` inline to avoid touching imports). Adjust the literal above to `program: std::path::PathBuf::from("/bin/true"),` to sidestep the import question.

- [ ] **Step 2: Run the test to verify it fails (does not compile)**

Run: `cargo test -p hhagent-supervisor 2>&1 | head -30`
Expected: compile error — `cannot find type RestartBackoff` and `struct ServiceSpec has no field named restart_backoff`.

- [ ] **Step 3: Add the `RestartBackoff` type and the `ServiceSpec` field**

In `supervisor/src/lib.rs`, immediately **before** `pub struct ServiceSpec {` (line 36, above its doc comment is fine; place the type just before the struct), add:

```rust
/// Operator-tunable exponential restart backoff for a keep-alive service.
///
/// Only meaningful when [`ServiceSpec::keep_alive`] is `true`. The ramp
/// starts from the existing initial delay (systemd `RestartSec=5`) and grows
/// geometrically to `max_delay_sec` over `steps` steps. Ignored-with-warning
/// on launchd, which has no equivalent knob (see
/// [`launchd_agents::LaunchAgents::install`] on macOS).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartBackoff {
    /// Maximum delay (seconds) the ramp climbs to. systemd `RestartMaxDelaySec=`.
    pub max_delay_sec: u32,
    /// Number of steps over which the delay grows from the initial `RestartSec`
    /// to `max_delay_sec`. systemd `RestartSteps=`.
    pub steps: u32,
}
```

Then add the field to `ServiceSpec`, as the **last** field after `part_of` (after line 78):

```rust
    /// Optional exponential restart backoff. `None` (the default) preserves
    /// today's constant-`RestartSec=5` behaviour byte-for-byte. Only honoured
    /// when `keep_alive == true`. The systemd backend ramps `RestartSec` →
    /// `max_delay_sec` over `steps`; **launchd ignores it with an install-time
    /// warning** (no equivalent knob).
    #[serde(default)]
    pub restart_backoff: Option<RestartBackoff>,
```

- [ ] **Step 4: Add `restart_backoff: None,` to the `lib.rs` fixture and every other literal**

In `supervisor/src/lib.rs`, the `spec()` fixture in `mod default_target_tests` (ends with `part_of: Some("hhagent".into()),` near line 326) — add as the last field:
```rust
            restart_backoff: None,
```

Apply the identical one-line addition (`            restart_backoff: None,` — match the surrounding indentation) as the last field, after the `part_of: …` line, in each of these literals:
- `supervisor/src/systemd_user/builder.rs:256` (`minimal_spec`)
- `supervisor/src/launchd_agents/builders.rs:240` (`minimal_spec`)
- `supervisor/src/launchd_agents/builders.rs:398` (inline `spec` literal)
- `supervisor/src/systemd_user/tests.rs:20` (`minimal_spec`)
- `supervisor/src/launchd_agents/tests.rs:16` (`minimal_spec`)
- `supervisor/tests/target_smoke.rs:27` (`dummy_spec`)
- `supervisor/tests/launchd_agents_smoke.rs:131`, `:189`, `:231`
- `supervisor/tests/systemd_user_smoke.rs:112`

(The two `specs.rs` production literals are handled in Task 4, not here — but to keep the workspace compiling after this task, also add `restart_backoff: None,` to `core_service_spec` and `postgres_service_spec` for now; Task 4's failing test will flip them to the real value.)

- [ ] **Step 5: Extend the existing default-when-absent test**

In `supervisor/src/lib.rs`, `mod spec_ordering_tests`, the existing `service_spec_ordering_fields_default_when_absent` test (the JSON omits `restart_backoff`). Add one assertion at its end:
```rust
        assert!(s.restart_backoff.is_none());
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p hhagent-supervisor 2>&1 | tail -20`
Expected: all supervisor tests PASS, including `service_spec_restart_backoff_round_trips` and `service_spec_ordering_fields_default_when_absent`.

- [ ] **Step 7: Commit**

```bash
git add supervisor/src/lib.rs supervisor/src/specs.rs \
  supervisor/src/systemd_user/builder.rs supervisor/src/launchd_agents/builders.rs \
  supervisor/src/systemd_user/tests.rs supervisor/src/launchd_agents/tests.rs \
  supervisor/tests/target_smoke.rs supervisor/tests/launchd_agents_smoke.rs \
  supervisor/tests/systemd_user_smoke.rs
git commit -m "feat(supervisor): add ServiceSpec.restart_backoff field + RestartBackoff type

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: systemd backend emits the ramp directives

**Files:**
- Modify: `supervisor/src/systemd_user/builder.rs` (keep_alive block ~line 127; tests module ~line 367)

- [ ] **Step 1: Write the three failing builder tests**

In `supervisor/src/systemd_user/builder.rs`, inside its `#[cfg(test)] mod tests`, near the existing `build_unit_file_keep_alive_emits_restart_directives` (line 367), add a `use crate::RestartBackoff;` at the top of the test module (next to its other `use` lines) and these tests:

```rust
    #[test]
    fn build_unit_file_keep_alive_with_backoff_emits_steps_and_max_delay() {
        let mut spec = minimal_spec("svc");
        spec.keep_alive = true;
        spec.restart_backoff = Some(RestartBackoff { max_delay_sec: 300, steps: 8 });
        let s = build_unit_file(&spec);
        assert!(s.contains("RestartSteps=8"), "{s}");
        assert!(s.contains("RestartMaxDelaySec=300"), "{s}");
        // RestartSec must precede the ramp directives.
        let sec = s.find("RestartSec=").expect("RestartSec present");
        let steps = s.find("RestartSteps=").expect("RestartSteps present");
        let maxd = s.find("RestartMaxDelaySec=").expect("RestartMaxDelaySec present");
        assert!(sec < steps && steps < maxd, "directive order wrong:\n{s}");
    }

    #[test]
    fn build_unit_file_keep_alive_without_backoff_omits_steps_and_max_delay() {
        let mut spec = minimal_spec("svc");
        spec.keep_alive = true;
        spec.restart_backoff = None;
        let s = build_unit_file(&spec);
        assert!(!s.contains("RestartSteps="), "{s}");
        assert!(!s.contains("RestartMaxDelaySec="), "{s}");
    }

    #[test]
    fn build_unit_file_backoff_inert_without_keep_alive() {
        let mut spec = minimal_spec("svc");
        spec.keep_alive = false;
        spec.restart_backoff = Some(RestartBackoff { max_delay_sec: 300, steps: 8 });
        let s = build_unit_file(&spec);
        assert!(!s.contains("Restart="), "no restart directives without keep_alive:\n{s}");
        assert!(!s.contains("RestartSteps="), "{s}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p hhagent-supervisor build_unit_file_keep_alive_with_backoff 2>&1 | tail -20`
Expected: FAIL on the assertion `RestartSteps=8` not found (the builder doesn't emit it yet).

- [ ] **Step 3: Emit the directives in the `keep_alive` block**

In `supervisor/src/systemd_user/builder.rs`, replace the existing block (lines ~127–130):

```rust
    if spec.keep_alive {
        out.push_str("Restart=on-failure\n");
        out.push_str(&format!("RestartSec={}\n", DEFAULT_RESTART_SEC));
    }
```

with:

```rust
    if spec.keep_alive {
        out.push_str("Restart=on-failure\n");
        out.push_str(&format!("RestartSec={}\n", DEFAULT_RESTART_SEC));
        // Optional exponential ramp. RestartSteps/RestartMaxDelaySec need
        // systemd 252+; older systemd logs an "unknown directive" warning at
        // load but still starts the unit, so emitting them is a safe degrade.
        if let Some(b) = &spec.restart_backoff {
            out.push_str(&format!("RestartSteps={}\n", b.steps));
            out.push_str(&format!("RestartMaxDelaySec={}\n", b.max_delay_sec));
        }
    }
```

Also update the module-doc unit-file shape block near the top (line ~27) to list the two new directives under the `RestartSec=5` line:
```rust
//! RestartSec=5
//! RestartSteps=8                        # only when restart_backoff is set
//! RestartMaxDelaySec=300                 # only when restart_backoff is set
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p hhagent-supervisor build_unit_file 2>&1 | tail -20`
Expected: all `build_unit_file*` tests PASS.

- [ ] **Step 5: Commit**

```bash
git add supervisor/src/systemd_user/builder.rs
git commit -m "feat(supervisor): systemd emits RestartSteps/RestartMaxDelaySec for backoff

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: launchd warns at install, plist unchanged

**Files:**
- Modify: `supervisor/src/launchd_agents.rs` (`install`, ~line 223 before `build_plist`)
- Modify: `supervisor/src/launchd_agents/builders.rs` (tests module ~line 367)

- [ ] **Step 1: Write the failing "plist unchanged" test**

In `supervisor/src/launchd_agents/builders.rs`, inside its `#[cfg(test)] mod tests`, near the `build_plist_keep_alive_*` tests (line ~367), add:

```rust
    #[test]
    fn build_plist_identical_with_and_without_backoff() {
        let mut spec = minimal_spec("svc");
        spec.keep_alive = true;
        let without = build_plist(&spec);
        spec.restart_backoff =
            Some(crate::RestartBackoff { max_delay_sec: 300, steps: 8 });
        let with = build_plist(&spec);
        assert_eq!(
            without, with,
            "launchd plist must not change when restart_backoff is set"
        );
    }
```

(This test passes immediately because `build_plist` already ignores the field — it is a *regression guard* that pins the warn-and-ignore contract so a future edit can't silently start emitting backoff into the plist. That is intentional: it documents the design decision as an executable invariant.)

- [ ] **Step 2: Run the test**

Run: `cargo test -p hhagent-supervisor build_plist_identical_with_and_without_backoff 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 3: Add the install-time warning**

In `supervisor/src/launchd_agents.rs`, in `install`, immediately before `let body = build_plist(spec);` (line ~224), add:

```rust
        // launchd has no operator-controllable exponential restart backoff
        // (its only knob, ThrottleInterval, is a constant floor, not a ramp).
        // Honour the field by surfacing it, then fall back to KeepAlive's
        // default — same "degrade with a visible warning" posture as the
        // `after`/`part_of` fields, which launchd also cannot express.
        if spec.restart_backoff.is_some() {
            tracing::warn!(
                service = %spec.name,
                "restart_backoff requested but launchd has no equivalent; \
                 falling back to KeepAlive default"
            );
        }
```

- [ ] **Step 4: Run the supervisor tests**

Run: `cargo test -p hhagent-supervisor 2>&1 | tail -15`
Expected: all PASS (the warn is observability; the build_plist-identical test pins behaviour).

- [ ] **Step 5: Commit**

```bash
git add supervisor/src/launchd_agents.rs supervisor/src/launchd_agents/builders.rs
git commit -m "feat(supervisor): launchd warns on restart_backoff, plist unchanged

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: wire core + postgres specs with the curve

**Files:**
- Modify: `supervisor/src/specs.rs` (`use` line 18; `core_service_spec` ~line 84; `postgres_service_spec` ~line 143; tests module)

- [ ] **Step 1: Write the two failing spec tests**

In `supervisor/src/specs.rs`, inside `mod tests`, add (near the `*_keep_alive_is_true` tests):

```rust
    #[test]
    fn core_service_spec_carries_expected_backoff_curve() {
        let spec = core_service_spec(
            Path::new("/usr/local/bin/hhagent"),
            Path::new("/tmp"),
        );
        assert_eq!(
            spec.restart_backoff,
            Some(RestartBackoff { max_delay_sec: 300, steps: 8 })
        );
    }

    #[test]
    fn postgres_service_spec_carries_expected_backoff_curve() {
        let spec = postgres_service_spec(
            Path::new("/usr/lib/postgresql/18/bin/postgres"),
            Path::new("/d"),
            Path::new("/tmp"),
        );
        assert_eq!(
            spec.restart_backoff,
            Some(RestartBackoff { max_delay_sec: 300, steps: 8 })
        );
    }
```

`RestartBackoff` is in scope via the test module's `use super::*;` once Step 3 adds it to the `specs.rs` top-level `use`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p hhagent-supervisor _carries_expected_backoff_curve 2>&1 | tail -20`
Expected: FAIL — `restart_backoff` is `None` (set provisionally in Task 1), not `Some(...)`. (If `RestartBackoff` is not yet imported, the failure is a compile error — Step 3 fixes both.)

- [ ] **Step 3: Wire the curve into both production specs**

In `supervisor/src/specs.rs`, change the import (line 18) from:
```rust
use crate::{ServiceSpec, TargetSpec};
```
to:
```rust
use crate::{RestartBackoff, ServiceSpec, TargetSpec};
```

In `core_service_spec`, change the last field from `restart_backoff: None,` to:
```rust
        restart_backoff: Some(RestartBackoff { max_delay_sec: 300, steps: 8 }),
```

In `postgres_service_spec`, make the identical change.

Update each function's doc comment `keep_alive` bullet to note the ramp, e.g. add to `core_service_spec`'s doc: "On systemd the restart now ramps `RestartSec=5` → `RestartMaxDelaySec=300` over `RestartSteps=8` (`restart_backoff`); on launchd this is warned-and-ignored."

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p hhagent-supervisor 2>&1 | tail -15`
Expected: all PASS, including the two new curve tests and the existing `*_keep_alive_is_true` / `build_unit_file_keep_alive_emits_restart_directives` tests.

- [ ] **Step 5: Commit**

```bash
git add supervisor/src/specs.rs
git commit -m "feat(supervisor): wire core+postgres specs with 5s->300s/8-step backoff

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: file-size reconciliation (rule 4)

**Files:**
- Possibly modify: `supervisor/src/systemd_user/builder.rs`, `supervisor/src/launchd_agents/builders.rs`

- [ ] **Step 1: Measure the touched builder files**

Run: `wc -l supervisor/src/systemd_user/builder.rs supervisor/src/launchd_agents/builders.rs`
Expected: `builder.rs` is now ~515 LOC (over the 500 cap); `builders.rs` ~504.

- [ ] **Step 2: Lift `builder.rs`'s inline test module to a sibling (if > 500)**

Only if `builder.rs > 500`: move its entire `#[cfg(test)] mod tests { … }` block (starts ~line 250) verbatim into a new file `supervisor/src/systemd_user/builder/tests.rs`:
- One-level de-indent of the moved body.
- Add a `//!`-style header to the new file describing it as the lifted builder tests.
- In `builder.rs`, replace the moved block with `#[cfg(test)]\nmod tests;`.
- The child sees the parent via `use super::*;` (already at the top of the lifted block) — keep it.

Verify the production region of `builder.rs` is byte-identical to before the lift (only the test block moved):
Run: `git diff supervisor/src/systemd_user/builder.rs` and confirm the only removal is the test block.

- [ ] **Step 3: Decide on `builders.rs`**

If `builders.rs` is ≤ 500 after Task 3, leave it. If it is 501–527 (a few LOC over), the project's standing policy (HANDOVER "Refactor bucket (a)") is to **defer** sub-30-LOC-over lifts; note the residual in the session handover rather than lifting. If it exceeds ~530, lift its `#[cfg(test)] mod tests` block to `supervisor/src/launchd_agents/builders/tests.rs` using the same procedure as Step 2.

- [ ] **Step 4: Re-run the crate tests**

Run: `cargo test -p hhagent-supervisor 2>&1 | tail -15`
Expected: identical pass count to Task 4 (a test-lift is behaviour-preserving).

- [ ] **Step 5: Commit (only if a lift happened)**

```bash
git add supervisor/src/systemd_user/builder.rs supervisor/src/systemd_user/builder/tests.rs
git commit -m "refactor(supervisor): lift systemd builder test module under 500-LOC cap

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: full verification + clippy

**Files:** none (verification only)

- [ ] **Step 1: Full workspace test**

Run: `cargo test --workspace 2>&1 | tail -25`
Expected: macOS skip-as-pass baseline holds (no regressions vs. the 1350/0/3 baseline; supervisor gains ~7 new tests, so the passed count rises accordingly). No failures.

- [ ] **Step 2: Clippy gate on the supervisor crate**

Run: `cargo clippy -p hhagent-supervisor --all-targets --locked -- -D warnings 2>&1 | tail -15`
Expected: exit 0, no warnings.

- [ ] **Step 3: Confirm `None` reproduces today's systemd output**

Run: `cargo test -p hhagent-supervisor build_unit_file_keep_alive_without_backoff_omits_steps_and_max_delay -- --nocapture 2>&1 | tail -10`
Expected: PASS — proves an un-wired keep-alive spec still emits exactly `Restart=on-failure` + `RestartSec=5` and nothing more.

- [ ] **Step 4: No commit** (verification only). Proceed to the session-end handover/ROADMAP update and PR.

---

## Self-Review

**Spec coverage:**
- `RestartBackoff` type + field → Task 1. ✓
- systemd `RestartSteps`/`RestartMaxDelaySec` (252+, keep_alive-only) → Task 2. ✓
- launchd warn + plist unchanged → Task 3. ✓
- core + postgres wired with 300/8 → Task 4. ✓
- Tests: systemd Some/None/no-keep_alive (Task 2), launchd identical (Task 3), spec curves (Task 4), serde default + round-trip (Task 1). ✓ — every test in the design's Testing section maps to a step.
- File-size watch → Task 5. ✓

**Placeholder scan:** No TBD/TODO; every code step shows full code; the only conditional is Task 5's lift, gated on a measured `wc -l` with an explicit threshold and the exact lift procedure. ✓

**Type consistency:** `RestartBackoff { max_delay_sec, steps }` used identically in every task; field name `restart_backoff` consistent; the 300/8 curve identical across Tasks 1, 2, 3, 4. Import added to `specs.rs` (Task 4) and test-module import added where `RestartBackoff` is referenced (Tasks 2, 4). ✓
