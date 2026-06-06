# `hhagent.target` Bring-up Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an orchestrating `hhagent.target` that brings up Postgres + core as one unit — native systemd `.target` on Linux, readiness-based bundle on macOS.

**Architecture:** Two backend-neutral ordering fields on `ServiceSpec` (`after`, `part_of`) plus a `TargetSpec` bundle type. Four new `dyn`-safe `Supervisor` methods carry default implementations (the generic install-and-start-in-order bundle that macOS/launchd uses); `SystemdUser` overrides them to emit and drive a real `.target` unit. macOS ordering relies on core's existing fail-closed-restart-until-Postgres-ready loop.

**Tech Stack:** Rust, `hhagent-supervisor` crate, systemd `--user` units, launchd LaunchAgents. Pure string builders + thin `systemctl`/`launchctl` drivers, mirroring the existing `build_unit_file`/`build_plist` pattern.

**Spec:** [`docs/superpowers/specs/2026-06-06-hhagent-target-bring-up-design.md`](../specs/2026-06-06-hhagent-target-bring-up-design.md)

**Build prelude (every test/build step):** `source "$HOME/.cargo/env"` first; cargo is not on the non-interactive `PATH`. Crate-scoped runs: `cargo test -p hhagent-supervisor`.

---

## File structure

| File | Responsibility | Change |
| ---- | -------------- | ------ |
| `supervisor/src/lib.rs` | `ServiceSpec` (+`after`/`part_of`), new `TargetSpec`, `Supervisor` trait (+4 default target methods) | Modify |
| `supervisor/src/specs.rs` | `HHAGENT_TARGET_NAME`, ordering on the two builders, `hhagent_target_spec()` | Modify |
| `supervisor/src/systemd_user.rs` | `build_unit_file` emits `After=`/`PartOf=`; new `build_target_unit`; `SystemdUser` overrides the 4 target methods | Modify (already over-cap — keep additions minimal; flag a future split) |
| `supervisor/src/launchd_agents/builders.rs` | unit test pinning `build_plist` ignores the new fields | Modify (test only) |
| `supervisor/tests/target_smoke.rs` | gated e2e: install/start/stop/uninstall the target round-trip (Linux native target + macOS bundle) | Create |
| `docs/devel/ROADMAP.md` | tick the Phase-0 target line | Modify |

**Construction sites to update when adding the two `ServiceSpec` fields** (all in `supervisor/`; `core` uses the builder fns, no churn): `specs.rs` (×2 builders — set real values), and these test literals (add `after: vec![], part_of: None`): `systemd_user.rs:509` (`minimal_spec`), `launchd_agents/builders.rs:240` (`minimal_spec`) + `:396`, `launchd_agents/tests.rs:16` (`minimal_spec`), `tests/systemd_user_smoke.rs:112`, `tests/launchd_agents_smoke.rs:131/187/227`.

---

## Task 1: `TargetSpec` type + `ServiceSpec` ordering fields

**Files:**
- Modify: `supervisor/src/lib.rs`
- Modify (compile-fix literals): `supervisor/src/specs.rs`, `supervisor/src/systemd_user.rs`, `supervisor/src/launchd_agents/builders.rs`, `supervisor/src/launchd_agents/tests.rs`, `supervisor/tests/systemd_user_smoke.rs`, `supervisor/tests/launchd_agents_smoke.rs`

- [ ] **Step 1: Write the failing test**

Append a test module at the end of `supervisor/src/lib.rs`:

```rust
#[cfg(test)]
mod target_spec_tests {
    use super::*;

    #[test]
    fn target_spec_holds_name_and_ordered_members() {
        let t = TargetSpec {
            name: "hhagent".into(),
            members: vec!["hhagent-postgres".into(), "hhagent-core".into()],
        };
        assert_eq!(t.name, "hhagent");
        assert_eq!(t.members, vec!["hhagent-postgres", "hhagent-core"]);
    }

    #[test]
    fn service_spec_ordering_fields_default_empty() {
        // A spec with no ordering opts in to nothing.
        let s = ServiceSpec {
            name: "svc".into(),
            program: std::path::PathBuf::from("/bin/true"),
            args: vec![],
            env: vec![],
            working_dir: None,
            keep_alive: false,
            stdout_log: None,
            stderr_log: None,
            after: vec![],
            part_of: None,
        };
        assert!(s.after.is_empty());
        assert!(s.part_of.is_none());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-supervisor target_spec_tests 2>&1 | tail -20`
Expected: FAIL to compile — `cannot find type TargetSpec`, and `ServiceSpec` has no field `after`.

- [ ] **Step 3: Add the fields and the type**

In `supervisor/src/lib.rs`, inside `struct ServiceSpec` (after the `stderr_log` field, before the closing brace at line ~61):

```rust
    /// Names of services that must start *before* this one. Maps to a
    /// systemd `After=<name>.service` line per entry. **Ignored on
    /// launchd** — launchd has no inter-agent ordering, so on macOS the
    /// equivalent guarantee comes from each service's own readiness
    /// behaviour (core fail-closed-restarts until Postgres is reachable).
    /// Default empty: a spec that sets nothing here emits exactly today's
    /// unit file (see `build_unit_file`'s behaviour-preserving test).
    #[serde(default)]
    pub after: Vec<String>,
    /// The target bundle this service belongs to, if any. When `Some`,
    /// systemd emits `PartOf=<target>.target` (so stopping the target
    /// stops this service) and switches the `[Install] WantedBy=` to
    /// `<target>.target`. **Ignored on launchd.** Default `None`.
    #[serde(default)]
    pub part_of: Option<String>,
```

Then, after the `ServiceSpec` struct's closing brace, add the bundle type:

```rust
/// A named bundle of services brought up and torn down together.
///
/// `members` are service names listed in **start order** (dependencies
/// first); teardown reverses the order. On systemd this compiles to a
/// real `hhagent.target` unit; on launchd (which has no target concept)
/// the [`Supervisor`] default methods install and start the members in
/// this order, relying on each service's own readiness behaviour for
/// correctness.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TargetSpec {
    /// Bundle name. Becomes `<name>.target` on systemd; on launchd it is
    /// only an identifier for the member set (no file is written for it).
    pub name: String,
    /// Member service names, in start order (dependencies first).
    pub members: Vec<String>,
}
```

- [ ] **Step 4: Fix every `ServiceSpec` literal to add the two fields**

Add `after: vec![], part_of: None,` (before the closing `}`) to each of these literals:
- `supervisor/src/systemd_user.rs` `minimal_spec` (~line 509)
- `supervisor/src/launchd_agents/builders.rs` `minimal_spec` (~line 240) and the literal at ~line 396
- `supervisor/src/launchd_agents/tests.rs` `minimal_spec` (~line 16)
- `supervisor/tests/systemd_user_smoke.rs` (~line 112)
- `supervisor/tests/launchd_agents_smoke.rs` (~lines 131, 187, 227)

(`supervisor/src/specs.rs` literals are updated in Task 2 with real values — leave them broken until then is fine *only if* you do Task 2 before running the full suite; to keep this task self-contained, also add `after: vec![], part_of: None,` to both `specs.rs` literals now, then Task 2 replaces them with real values.)

- [ ] **Step 5: Run test to verify it passes**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-supervisor target_spec_tests 2>&1 | tail -20`
Expected: PASS (2 tests).

- [ ] **Step 6: Commit**

```bash
git add supervisor/src/lib.rs supervisor/src/specs.rs supervisor/src/systemd_user.rs supervisor/src/launchd_agents/ supervisor/tests/
git commit -m "feat(supervisor): add ServiceSpec ordering fields + TargetSpec type"
```

---

## Task 2: `specs.rs` — ordering on builders + `hhagent_target_spec()`

**Files:**
- Modify: `supervisor/src/specs.rs`

- [ ] **Step 1: Write the failing test**

In `supervisor/src/specs.rs` `#[cfg(test)] mod tests` (after the existing tests), add:

```rust
    #[test]
    fn postgres_spec_belongs_to_target_with_no_dependency() {
        let spec = postgres_service_spec(
            Path::new("/usr/lib/postgresql/18/bin/postgres"),
            Path::new("/var/lib/hhagent/pgdata"),
            Path::new("/tmp/logs"),
        );
        assert!(spec.after.is_empty(), "postgres is the dependency leaf");
        assert_eq!(spec.part_of.as_deref(), Some(HHAGENT_TARGET_NAME));
    }

    #[test]
    fn core_spec_starts_after_postgres_and_belongs_to_target() {
        let spec = core_service_spec(Path::new("/opt/hhagent/hhagent"), Path::new("/tmp/logs"));
        assert_eq!(spec.after, vec![POSTGRES_SERVICE_NAME.to_string()]);
        assert_eq!(spec.part_of.as_deref(), Some(HHAGENT_TARGET_NAME));
    }

    #[test]
    fn hhagent_target_lists_postgres_then_core_in_order() {
        let t = hhagent_target_spec();
        assert_eq!(t.name, HHAGENT_TARGET_NAME);
        assert_eq!(
            t.members,
            vec![POSTGRES_SERVICE_NAME.to_string(), CORE_SERVICE_NAME.to_string()],
            "Postgres must precede core (start order)"
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-supervisor specs:: 2>&1 | tail -20`
Expected: FAIL to compile — `HHAGENT_TARGET_NAME` and `hhagent_target_spec` not found; `spec.part_of` is `None`.

- [ ] **Step 3: Implement the const, builder ordering, and target builder**

In `supervisor/src/specs.rs`:

Add the const after `POSTGRES_SERVICE_NAME` (~line 32):

```rust
/// Canonical name of the service bundle that brings up the whole agent.
/// Becomes `hhagent.target` on systemd; on launchd it names the member
/// set only. Same string on both OSes (see [`CORE_SERVICE_NAME`]).
pub const HHAGENT_TARGET_NAME: &str = "hhagent";
```

Add the new import at the top (`use crate::ServiceSpec;` becomes):

```rust
use crate::{ServiceSpec, TargetSpec};
```

In `core_service_spec`, set the two fields in the returned literal (replace the `after`/`part_of` placeholders added in Task 1):

```rust
        after: vec![POSTGRES_SERVICE_NAME.to_string()],
        part_of: Some(HHAGENT_TARGET_NAME.to_string()),
```

In `postgres_service_spec`, set:

```rust
        after: vec![],
        part_of: Some(HHAGENT_TARGET_NAME.to_string()),
```

Add the target builder at the end of the production region (before `#[cfg(test)]`):

```rust
/// Build the canonical [`TargetSpec`] that brings up the whole agent.
///
/// Members in **start order**: Postgres first (the dependency leaf),
/// then core (which must start after Postgres). Inference is **not** a
/// member — it is an operator-managed external dependency that core's
/// startup probe health-checks. Workers are **not** members either —
/// `tool_host` spawns them on demand inside sandboxes when core runs.
///
/// Pure: no I/O, same call → same value.
pub fn hhagent_target_spec() -> TargetSpec {
    TargetSpec {
        name: HHAGENT_TARGET_NAME.to_string(),
        members: vec![
            POSTGRES_SERVICE_NAME.to_string(),
            CORE_SERVICE_NAME.to_string(),
        ],
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-supervisor specs:: 2>&1 | tail -20`
Expected: PASS (existing specs tests + 3 new).

- [ ] **Step 5: Commit**

```bash
git add supervisor/src/specs.rs
git commit -m "feat(supervisor): hhagent_target_spec + ordering on core/postgres specs"
```

---

## Task 3: `build_unit_file` emits `After=`/`PartOf=` (with behaviour-preserving pin)

**Files:**
- Modify: `supervisor/src/systemd_user.rs`

- [ ] **Step 1: Write the failing tests**

In `supervisor/src/systemd_user.rs` `#[cfg(test)] mod tests`, add:

```rust
    #[test]
    fn unit_file_emits_after_and_part_of_when_set() {
        let mut spec = minimal_spec("hhagent-core");
        spec.after = vec!["hhagent-postgres".into()];
        spec.part_of = Some("hhagent".into());
        let body = build_unit_file(&spec);
        assert!(body.contains("After=hhagent-postgres.service\n"), "{body}");
        assert!(body.contains("PartOf=hhagent.target\n"), "{body}");
        assert!(body.contains("WantedBy=hhagent.target\n"), "{body}");
        assert!(!body.contains("WantedBy=default.target\n"), "target member must not target default.target: {body}");
    }

    #[test]
    fn unit_file_unchanged_when_ordering_unset() {
        // The behaviour-preserving pin: a spec with no ordering emits
        // neither After= nor PartOf=, and keeps WantedBy=default.target.
        let body = build_unit_file(&minimal_spec("svc"));
        assert!(!body.contains("After="), "{body}");
        assert!(!body.contains("PartOf="), "{body}");
        assert!(body.contains("WantedBy=default.target\n"), "{body}");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-supervisor unit_file_emits_after_and_part_of 2>&1 | tail -20`
Expected: FAIL — `After=`/`PartOf=` not present.

- [ ] **Step 3: Implement**

In `build_unit_file` (`supervisor/src/systemd_user.rs` ~line 92-94), extend the `[Unit]` section. Replace:

```rust
    // [Unit] section.
    out.push_str("[Unit]\n");
    out.push_str(&format!("Description=hhagent service: {}\n", spec.name));
    out.push('\n');
```

with:

```rust
    // [Unit] section.
    out.push_str("[Unit]\n");
    out.push_str(&format!("Description=hhagent service: {}\n", spec.name));
    // Ordering: one After= per dependency. systemd only *orders* against
    // units present in the same start transaction — harmless if absent.
    for dep in &spec.after {
        out.push_str(&format!("After={dep}.service\n"));
    }
    // PartOf binds this unit's stop/restart to the target's: `systemctl
    // stop <target>.target` propagates to PartOf members.
    if let Some(target) = &spec.part_of {
        out.push_str(&format!("PartOf={target}.target\n"));
    }
    out.push('\n');
```

In the `[Install]` section (~line 144-145), replace:

```rust
    out.push_str("[Install]\n");
    out.push_str("WantedBy=default.target\n");
```

with:

```rust
    out.push_str("[Install]\n");
    // A target member is wanted by its target; a standalone service is
    // wanted by default.target so `enable` starts it at login.
    match &spec.part_of {
        Some(target) => out.push_str(&format!("WantedBy={target}.target\n")),
        None => out.push_str("WantedBy=default.target\n"),
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-supervisor --lib systemd_user 2>&1 | tail -20`
Expected: PASS (new 2 + all existing systemd_user unit tests; the existing `build_unit_file_emits_three_sections_in_order` still passes because `minimal_spec` sets no ordering).

- [ ] **Step 5: Commit**

```bash
git add supervisor/src/systemd_user.rs
git commit -m "feat(supervisor): build_unit_file emits After=/PartOf= for target members"
```

---

## Task 4: `build_target_unit` — the systemd `.target` builder

**Files:**
- Modify: `supervisor/src/systemd_user.rs`

- [ ] **Step 1: Write the failing test**

In `supervisor/src/systemd_user.rs` tests:

```rust
    #[test]
    fn target_unit_wants_all_members() {
        let t = crate::TargetSpec {
            name: "hhagent".into(),
            members: vec!["hhagent-postgres".into(), "hhagent-core".into()],
        };
        let body = build_target_unit(&t);
        assert!(body.starts_with("[Unit]\n"), "{body}");
        assert!(
            body.contains("Wants=hhagent-postgres.service hhagent-core.service\n"),
            "{body}"
        );
        assert!(body.contains("[Install]\nWantedBy=default.target\n"), "{body}");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-supervisor target_unit_wants_all_members 2>&1 | tail -20`
Expected: FAIL — `build_target_unit` not found.

- [ ] **Step 3: Implement**

Add next to `build_unit_file` in `supervisor/src/systemd_user.rs`:

```rust
/// Build the systemd `.target` unit body for a [`TargetSpec`].
///
/// The target `Wants=` all its members, so `systemctl --user start
/// <name>.target` pulls them in; per-member `After=` lines (emitted by
/// [`build_unit_file`] from each member's `ServiceSpec.after`) order the
/// start. We use `Wants=` (soft) rather than `Requires=` so a single
/// member failing does not tear the whole target down — the agent is
/// still useful if, say, an optional future member is absent.
///
/// Pure: no I/O. Same `TargetSpec` → same body.
pub fn build_target_unit(target: &crate::TargetSpec) -> String {
    let mut out = String::with_capacity(256);
    out.push_str("[Unit]\n");
    out.push_str(&format!("Description=hhagent service bundle: {}\n", target.name));
    if !target.members.is_empty() {
        let wants: Vec<String> = target
            .members
            .iter()
            .map(|m| format!("{m}.service"))
            .collect();
        out.push_str(&format!("Wants={}\n", wants.join(" ")));
    }
    out.push('\n');
    out.push_str("[Install]\n");
    out.push_str("WantedBy=default.target\n");
    out
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-supervisor target_unit_wants_all_members 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add supervisor/src/systemd_user.rs
git commit -m "feat(supervisor): build_target_unit emits the hhagent.target unit"
```

---

## Task 5: `Supervisor` default target methods (the generic bundle)

**Files:**
- Modify: `supervisor/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Append to `supervisor/src/lib.rs` a new test module that uses an in-memory fake `Supervisor` to record the call order the default methods produce:

```rust
#[cfg(test)]
mod default_target_tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct RecordingSupervisor {
        calls: RefCell<Vec<String>>,
    }
    impl Supervisor for RecordingSupervisor {
        fn install(&self, spec: &ServiceSpec) -> Result<(), SupervisorError> {
            self.calls.borrow_mut().push(format!("install:{}", spec.name));
            Ok(())
        }
        fn start(&self, name: &str) -> Result<(), SupervisorError> {
            self.calls.borrow_mut().push(format!("start:{name}"));
            Ok(())
        }
        fn stop(&self, name: &str) -> Result<(), SupervisorError> {
            self.calls.borrow_mut().push(format!("stop:{name}"));
            Ok(())
        }
        fn uninstall(&self, name: &str) -> Result<(), SupervisorError> {
            self.calls.borrow_mut().push(format!("uninstall:{name}"));
            Ok(())
        }
        fn status(&self, _name: &str) -> Result<ServiceStatus, SupervisorError> {
            Ok(ServiceStatus::Active)
        }
    }

    fn spec(name: &str) -> ServiceSpec {
        ServiceSpec {
            name: name.into(),
            program: std::path::PathBuf::from("/bin/true"),
            args: vec![],
            env: vec![],
            working_dir: None,
            keep_alive: true,
            stdout_log: None,
            stderr_log: None,
            after: vec![],
            part_of: Some("hhagent".into()),
        }
    }

    #[test]
    fn default_bundle_installs_then_starts_in_member_order() {
        let sup = RecordingSupervisor::default();
        let target = TargetSpec {
            name: "hhagent".into(),
            members: vec!["hhagent-postgres".into(), "hhagent-core".into()],
        };
        let members = [spec("hhagent-postgres"), spec("hhagent-core")];
        sup.install_target(&target, &members).unwrap();
        sup.start_target(&target).unwrap();
        let calls = sup.calls.borrow().clone();
        assert_eq!(
            calls,
            vec![
                "install:hhagent-postgres",
                "install:hhagent-core",
                "start:hhagent-postgres",
                "start:hhagent-core",
            ]
        );
    }

    #[test]
    fn default_bundle_stops_in_reverse_member_order() {
        let sup = RecordingSupervisor::default();
        let target = TargetSpec {
            name: "hhagent".into(),
            members: vec!["hhagent-postgres".into(), "hhagent-core".into()],
        };
        sup.stop_target(&target).unwrap();
        assert_eq!(
            sup.calls.borrow().clone(),
            vec!["stop:hhagent-core", "stop:hhagent-postgres"]
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-supervisor default_target_tests 2>&1 | tail -20`
Expected: FAIL to compile — no method `install_target` on `Supervisor`.

- [ ] **Step 3: Implement the default methods**

In `supervisor/src/lib.rs`, inside `pub trait Supervisor`, after `fn status(...)`, add four methods **with default bodies**:

```rust
    /// Install every member of a [`TargetSpec`] (the generic bundle).
    ///
    /// Default implementation installs each member spec in order. The
    /// systemd backend overrides this to additionally write a native
    /// `.target` unit. The macOS/launchd backend uses this default —
    /// there is no target file on launchd.
    fn install_target(
        &self,
        _target: &TargetSpec,
        members: &[ServiceSpec],
    ) -> Result<(), SupervisorError> {
        for spec in members {
            self.install(spec)?;
        }
        Ok(())
    }

    /// Start every member in `target.members` order (dependencies first).
    ///
    /// Default implementation starts each member sequentially. There is
    /// **no explicit readiness wait** — on launchd, inter-service
    /// ordering is not enforced and correctness relies on each service's
    /// own readiness behaviour (core fail-closed-restarts until Postgres
    /// is reachable). The systemd backend overrides this to `systemctl
    /// start <name>.target`, letting systemd resolve ordering from
    /// `After=`.
    fn start_target(&self, target: &TargetSpec) -> Result<(), SupervisorError> {
        for name in &target.members {
            self.start(name)?;
        }
        Ok(())
    }

    /// Stop every member in **reverse** `target.members` order.
    fn stop_target(&self, target: &TargetSpec) -> Result<(), SupervisorError> {
        for name in target.members.iter().rev() {
            self.stop(name)?;
        }
        Ok(())
    }

    /// Uninstall every member in **reverse** order.
    fn uninstall_target(&self, target: &TargetSpec) -> Result<(), SupervisorError> {
        for name in target.members.iter().rev() {
            self.uninstall(name)?;
        }
        Ok(())
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-supervisor default_target_tests 2>&1 | tail -20`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add supervisor/src/lib.rs
git commit -m "feat(supervisor): Supervisor default target methods (generic bundle)"
```

---

## Task 6: `SystemdUser` overrides — native `.target`

**Files:**
- Modify: `supervisor/src/systemd_user.rs`

- [ ] **Step 1: Write the failing test**

In `supervisor/src/systemd_user.rs` tests (these use a custom `units_dir` so no live `systemctl` is touched — exactly like the existing file-writing unit tests). Add a helper to read the written file and the test:

```rust
    #[test]
    fn install_target_writes_target_unit_and_members_into_units_dir() {
        let dir = std::env::temp_dir().join(format!(
            "hhagent-target-unit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let sup = SystemdUser::with_units_dir(dir.clone());

        let mut pg = minimal_spec("hhagent-postgres");
        pg.program = std::path::PathBuf::from("/usr/lib/postgresql/18/bin/postgres");
        pg.part_of = Some("hhagent".into());
        let mut core = minimal_spec("hhagent-core");
        core.program = std::path::PathBuf::from("/opt/hhagent/hhagent");
        core.after = vec!["hhagent-postgres".into()];
        core.part_of = Some("hhagent".into());

        let target = crate::TargetSpec {
            name: "hhagent".into(),
            members: vec!["hhagent-postgres".into(), "hhagent-core".into()],
        };
        sup.install_target(&target, &[pg, core]).expect("install_target");

        // Target unit written with Wants= of both members.
        let target_body =
            std::fs::read_to_string(dir.join("hhagent.target")).expect("target file");
        assert!(
            target_body.contains("Wants=hhagent-postgres.service hhagent-core.service\n"),
            "{target_body}"
        );
        // Member units written, core ordered After= postgres.
        assert!(dir.join("hhagent-postgres.service").exists());
        let core_body =
            std::fs::read_to_string(dir.join("hhagent-core.service")).expect("core file");
        assert!(core_body.contains("After=hhagent-postgres.service\n"), "{core_body}");

        std::fs::remove_dir_all(&dir).ok();
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-supervisor install_target_writes_target_unit 2>&1 | tail -20`
Expected: FAIL — `hhagent.target` file not found (the default `install_target` writes only member units, no target file).

- [ ] **Step 3: Implement the overrides**

First, add `TargetSpec` to the crate import at the top of `systemd_user.rs`. Find the existing line (it imports `ServiceSpec, ServiceStatus, Supervisor, SupervisorError`) and add `TargetSpec`:

```rust
use crate::{ServiceSpec, ServiceStatus, Supervisor, SupervisorError, TargetSpec};
```

Add a helper method in `impl SystemdUser` (near `unit_path`, ~line 272):

```rust
    /// Path the driver would write `<name>.target` to.
    pub fn target_path(&self, name: &str) -> PathBuf {
        self.units_dir.join(format!("{name}.target"))
    }
```

In `impl Supervisor for SystemdUser` (after `fn status`), add the four overrides:

```rust
    fn install_target(
        &self,
        target: &TargetSpec,
        members: &[ServiceSpec],
    ) -> Result<(), SupervisorError> {
        validate_service_name(&target.name)?;
        // Member units first (reuses install's absolute-path validation).
        for spec in members {
            self.install(spec)?;
        }
        // Then the .target unit that Wants= them.
        fs::create_dir_all(&self.units_dir).map_err(|e| {
            SupervisorError::Io(format!("create {}: {e}", self.units_dir.display()))
        })?;
        let path = self.target_path(&target.name);
        write_atomic(&path, build_target_unit(target).as_bytes())?;
        if self.is_default_units_dir() {
            self.daemon_reload()?;
        }
        Ok(())
    }

    fn start_target(&self, target: &TargetSpec) -> Result<(), SupervisorError> {
        validate_service_name(&target.name)?;
        // systemd resolves member ordering from each member's After=.
        run_systemctl_user(&["start", &format!("{}.target", target.name)]).map(|_| ())
    }

    fn stop_target(&self, target: &TargetSpec) -> Result<(), SupervisorError> {
        validate_service_name(&target.name)?;
        // PartOf= on members propagates the stop to them.
        run_systemctl_user(&["stop", &format!("{}.target", target.name)]).map(|_| ())
    }

    fn uninstall_target(&self, target: &TargetSpec) -> Result<(), SupervisorError> {
        validate_service_name(&target.name)?;
        // Stop the target (propagates to members via PartOf=), then
        // remove every member unit and the target unit file.
        let _ = run_systemctl_user(&["stop", &format!("{}.target", target.name)]);
        for name in target.members.iter().rev() {
            let _ = self.uninstall(name);
        }
        let path = self.target_path(&target.name);
        if path.exists() {
            fs::remove_file(&path).map_err(|e| {
                SupervisorError::Io(format!("remove {}: {e}", path.display()))
            })?;
        }
        if self.is_default_units_dir() {
            self.daemon_reload()?;
        }
        Ok(())
    }
```

Then update the `use` at the top of `systemd_user.rs` to import `TargetSpec` (find the existing `use crate::{...}` line — currently it imports `ServiceSpec, ServiceStatus, Supervisor, SupervisorError`; add `TargetSpec`). Delete the placeholder `install_target` stub you pasted first.

- [ ] **Step 4: Run test to verify it passes**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-supervisor --lib systemd_user 2>&1 | tail -20`
Expected: PASS (new target-install test + all existing systemd_user unit tests).

- [ ] **Step 5: Commit**

```bash
git add supervisor/src/systemd_user.rs
git commit -m "feat(supervisor): SystemdUser native hhagent.target install/start/stop/uninstall"
```

---

## Task 7: launchd ignores the new fields (macOS pin) + module doc note

**Files:**
- Modify: `supervisor/src/launchd_agents/builders.rs`
- Modify: `supervisor/src/launchd_agents.rs` (doc comment only)

- [ ] **Step 1: Write the failing test**

In `supervisor/src/launchd_agents/builders.rs` tests:

```rust
    #[test]
    fn build_plist_ignores_after_and_part_of() {
        // launchd has no ordering / target concept: setting these fields
        // must not change the emitted plist. This pins the documented
        // "ignored on launchd" contract.
        let base = minimal_spec("hhagent-core");
        let mut with_ordering = minimal_spec("hhagent-core");
        with_ordering.after = vec!["hhagent-postgres".into()];
        with_ordering.part_of = Some("hhagent".into());
        assert_eq!(build_plist(&base), build_plist(&with_ordering));
    }
```

- [ ] **Step 2: Run test to verify it fails or passes**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-supervisor build_plist_ignores_after 2>&1 | tail -20`
Expected: PASS immediately (build_plist never read the new fields). This is a *characterization/pin* test — if it already passes, that confirms the contract; keep it as a regression guard. (If it somehow fails, `build_plist` is touching the fields and must be fixed to ignore them.)

- [ ] **Step 3: Add the module doc note**

In `supervisor/src/launchd_agents.rs` module-level doc comment (top of file), add a paragraph documenting the no-ordering reliance:

```rust
//! ## No native ordering — readiness-based bundles
//!
//! launchd has no inter-agent ordering and no target/aggregation
//! concept. The [`crate::Supervisor`] default `*_target` methods install
//! and start members in declared order, but launchd may still race their
//! startup. hhagent tolerates this because core fail-closed-restarts
//! until Postgres is reachable (`KeepAlive=true`), so the bundle
//! converges regardless of launchd start order. `ServiceSpec.after` and
//! `ServiceSpec.part_of` are therefore **ignored** by `build_plist`.
```

- [ ] **Step 4: Run test to verify it passes**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-supervisor build_plist_ignores_after 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add supervisor/src/launchd_agents.rs supervisor/src/launchd_agents/builders.rs
git commit -m "test(supervisor): pin build_plist ignores ordering fields + document launchd bundle"
```

---

## Task 8: Gated e2e — target round-trip

**Files:**
- Create: `supervisor/tests/target_smoke.rs`

- [ ] **Step 1: Write the test**

Create `supervisor/tests/target_smoke.rs`. The Linux path drives the real native target; the macOS path drives the generic bundle. Both skip-as-pass on probe failure and clean up via RAII.

```rust
//! End-to-end smoke test for the target bring-up (`install_target` →
//! `start_target` → `stop_target` → `uninstall_target`).
//!
//! Linux exercises the native `hhagent.target` (real `systemctl --user`).
//! macOS exercises the generic readiness-based bundle (real `launchctl`).
//! Both use trivial long-running dummy programs (`sleep`) so the test
//! validates the *target orchestration mechanics* in isolation — real
//! Postgres + core bring-up is a heavier system test, out of scope here.
//!
//! Skips silently (`[SKIP]` on `--nocapture`) when the per-user service
//! manager is unreachable, mirroring `systemd_user_smoke.rs`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use hhagent_supervisor::{ServiceSpec, ServiceStatus, Supervisor, TargetSpec};

fn unique(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}-{}-{}", std::process::id(), nanos)
}

fn dummy_spec(name: &str, target: &str, after: Vec<String>) -> ServiceSpec {
    ServiceSpec {
        name: name.into(),
        program: PathBuf::from(SLEEP_BIN),
        args: vec!["30".into()],
        env: vec![],
        working_dir: None,
        keep_alive: false,
        stdout_log: None,
        stderr_log: None,
        after,
        part_of: Some(target.into()),
    }
}

#[cfg(target_os = "linux")]
const SLEEP_BIN: &str = "/usr/bin/sleep";
#[cfg(target_os = "macos")]
const SLEEP_BIN: &str = "/bin/sleep";

fn wait_for(
    sup: &dyn Supervisor,
    name: &str,
    want: ServiceStatus,
    timeout: Duration,
) -> Result<(), String> {
    let start = Instant::now();
    loop {
        let got = sup.status(name).map_err(|e| format!("status({name}): {e}"))?;
        if got == want {
            return Ok(());
        }
        if start.elapsed() > timeout {
            return Err(format!("timeout waiting status={want:?}, last={got:?}"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use hhagent_supervisor::systemd_user::{probe, SystemdUser};

    struct Guard {
        sup: SystemdUser,
        target: TargetSpec,
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.sup.uninstall_target(&self.target);
        }
    }

    #[test]
    fn target_round_trip_native_systemd() {
        if let Err(e) = probe() {
            eprintln!("\n[SKIP] systemctl --user probe failed: {e}\n");
            return;
        }
        let sup = SystemdUser::new();
        let target_name = unique("hhagent-test-target");
        let pg = unique("hhagent-test-pg");
        let core = unique("hhagent-test-core");
        let target = TargetSpec {
            name: target_name.clone(),
            members: vec![pg.clone(), core.clone()],
        };
        let _guard = Guard {
            sup: SystemdUser::new(),
            target: target.clone(),
        };

        let members = [
            dummy_spec(&pg, &target_name, vec![]),
            dummy_spec(&core, &target_name, vec![pg.clone()]),
        ];
        sup.install_target(&target, &members).expect("install_target");

        // The target unit Wants= both members; core is ordered After= pg.
        let units = sup.units_dir();
        let target_body =
            std::fs::read_to_string(units.join(format!("{target_name}.target"))).expect("target unit");
        assert!(target_body.contains(&format!("Wants={pg}.service {core}.service\n")), "{target_body}");
        let core_body =
            std::fs::read_to_string(units.join(format!("{core}.service"))).expect("core unit");
        assert!(core_body.contains(&format!("After={pg}.service\n")), "{core_body}");

        sup.start_target(&target).expect("start_target");
        wait_for(&sup, &pg, ServiceStatus::Active, Duration::from_secs(5)).expect("pg active");
        wait_for(&sup, &core, ServiceStatus::Active, Duration::from_secs(5)).expect("core active");

        sup.stop_target(&target).expect("stop_target");
        wait_for(&sup, &core, ServiceStatus::Inactive, Duration::from_secs(5)).expect("core inactive");

        sup.uninstall_target(&target).expect("uninstall_target");
        assert_eq!(sup.status(&pg).unwrap(), ServiceStatus::NotInstalled);
        assert_eq!(sup.status(&core).unwrap(), ServiceStatus::NotInstalled);
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use hhagent_supervisor::launchd_agents::{probe, LaunchAgents};

    struct Guard {
        sup: LaunchAgents,
        target: TargetSpec,
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.sup.uninstall_target(&self.target);
        }
    }

    #[test]
    fn target_round_trip_generic_bundle() {
        if let Err(e) = probe() {
            eprintln!("\n[SKIP] launchctl probe failed: {e}\n");
            return;
        }
        let sup = LaunchAgents::new();
        let target_name = unique("hhagent-test-target");
        let pg = unique("hhagent-test-pg");
        let core = unique("hhagent-test-core");
        let target = TargetSpec {
            name: target_name.clone(),
            members: vec![pg.clone(), core.clone()],
        };
        let _guard = Guard {
            sup: LaunchAgents::new(),
            target: target.clone(),
        };

        let members = [
            dummy_spec(&pg, &target_name, vec![]),
            dummy_spec(&core, &target_name, vec![pg.clone()]),
        ];
        sup.install_target(&target, &members).expect("install_target");
        sup.start_target(&target).expect("start_target");
        wait_for(&sup, &pg, ServiceStatus::Active, Duration::from_secs(5)).expect("pg active");
        wait_for(&sup, &core, ServiceStatus::Active, Duration::from_secs(5)).expect("core active");

        sup.stop_target(&target).expect("stop_target");
        sup.uninstall_target(&target).expect("uninstall_target");
        assert_eq!(sup.status(&pg).unwrap(), ServiceStatus::NotInstalled);
        assert_eq!(sup.status(&core).unwrap(), ServiceStatus::NotInstalled);
    }
}
```

- [ ] **Step 2: Run the e2e (on the DGX, native systemd)**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-supervisor --test target_smoke -- --nocapture 2>&1 | tail -30`
Expected: PASS (or `[SKIP]` if `systemctl --user` has no live manager — re-check on a host with `loginctl enable-linger`).

- [ ] **Step 3: Commit**

```bash
git add supervisor/tests/target_smoke.rs
git commit -m "test(supervisor): gated e2e for hhagent.target round-trip (systemd + launchd)"
```

---

## Task 9: Full-workspace verification + ROADMAP tick

**Files:**
- Modify: `docs/devel/ROADMAP.md`

- [ ] **Step 1: Run the full suite + clippy (the project's green-bar gate)**

Run:
```bash
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | tail -15
cargo clippy --workspace --all-targets --locked -- -D warnings 2>&1 | tail -5
```
Expected: all tests pass (native-Linux baseline was **1311 / 0 / 4** at `cdadea1`; this slice adds supervisor unit + e2e tests, so the passed count rises and failed stays 0); clippy exits 0.

- [ ] **Step 2: Tick the ROADMAP line**

In `docs/devel/ROADMAP.md` line ~60, change:

```markdown
- [ ] `hhagent.target` that brings up Postgres, inference, core, workers
```

to (with a note recording the resolved scope):

```markdown
- [x] `hhagent.target` that brings up Postgres + core — native systemd `.target` (Linux) / readiness-based bundle (macOS); inference is an external health-checked dependency, workers are core-owned (spawned on demand). `TargetSpec` + `Supervisor::{install,start,stop,uninstall}_target` + `hhagent_target_spec()` — 2026-06-06
```

- [ ] **Step 3: Commit**

```bash
git add docs/devel/ROADMAP.md
git commit -m "docs(roadmap): hhagent.target bring-up shipped (Postgres + core)"
```

---

## Self-review notes (for the implementer)

- **`systemd_user.rs` is already over the 500-LOC cap (798).** This slice adds ~90 prod + ~40 test lines. Do **not** attempt a file split in this PR — it is a pre-existing condition; record it in HANDOVER's refactor bucket as a follow-up so the feature PR stays focused.
- **Task 6 requires `TargetSpec` in the `use crate::{...}` line** at the top of `systemd_user.rs` (Step 3 adds it). The four override method signatures take `&TargetSpec`.
- **macOS e2e** validates the generic bundle; it cannot assert ordering (launchd doesn't guarantee it) — it only asserts both members reach `Active` and tear down cleanly. That is the honest, correct assertion for that backend.
- **HANDOVER.md end-of-session update** (rule 8) happens after Task 9, outside this plan: record the shipped slice, new test baseline, and the `systemd_user.rs` split follow-up.
