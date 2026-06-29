# Firecracker micro-VM slice 5a — VMM confinement — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Confine the Firecracker VMM (the `kastellan-microvm-run` launcher and the `firecracker` process it spawns) inside the project's existing unprivileged `bwrap` jail + a `systemd-run --user --scope` cgroup, with no root, default-ON via opt-out — closing the missing host-side cgroup gap for FC workers.

**Architecture:** The sandbox backend prepends `systemd-run --user --scope … -- bwrap <vmm-jail binds> … -- <launcher>` to the launcher spawn. `firecracker`, spawned as the launcher's child, inherits the namespaces and cgroup for free; only stdio crosses the jail. A `VmmConfinement` enum is the dispatch seam (`None` = today's bare spawn, `BwrapCgroup` = new default) with a documented `Jailer` future sibling for a privileged tier.

**Tech Stack:** Rust (rustc 1.96.0), `kastellan-sandbox` crate (`linux_firecracker` module, `#[cfg(target_os="linux")]`), `kastellan-microvm-run` launcher crate (pure-std), `bwrap`, `systemd-run --user`, Firecracker v1.16.0.

## Global Constraints

- **AGPL-3.0; AGPL-compatible deps only.** No new crate dependency is introduced by this slice (std + existing only).
- **Cross-platform invariant.** All new code is `#[cfg(target_os="linux")]`-gated inside `linux_firecracker`; macOS keeps Seatbelt + `MacosContainer`. The launcher arg-parse change is pure-std (compiles on macOS).
- **Non-root, never self-escalate.** No root, no setuid, no new uid. The confinement uses unprivileged bwrap (user namespaces) + `systemd-run --user`.
- **Fail-closed, no false greens.** When confinement is enabled (the default) and bwrap/cgroup are unavailable, the VM worker **refuses to spawn** — never a silent bare-spawn fallback.
- **Default-ON via opt-out.** Env flag `KASTELLAN_MICROVM_CONFINE_VMM`: unset/`1`/`true` → confine; `0`/`false`/`no`/`off` → bare spawn. The `None` path stays **byte-identical** to today.
- **Verification environments.** `linux_firecracker` is Linux-gated: its unit tests **run on the DGX** (`ssh dgx '<cmd>'`, native aarch64 + real KVM) and are **cross-clippy compile-checked on the Mac** (`cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets -D warnings`). The launcher (`kastellan-microvm-run`) unit tests run natively on the Mac. The e2e (Task 7) is DGX-only / `#[ignore]`.
- **DGX e2e prerequisites** (Task 7): `export PATH=$HOME/.local/bin:$PATH` (firecracker is off the ssh PATH); rebuild the **release** launcher (`cargo build --release -p kastellan-microvm-run`) — a stale `target/release` launcher silently shadows source changes.
- **Merge gate.** The PR merges only once the Task-7 DGX confined-boot e2e is green (proving `/dev/kvm` + `/dev/vhost-vsock` survive the bwrap user namespace). If that interaction proves unworkable, ship the flag default-OFF (a one-line change in Task 1's `confinement_from_env` default) — documented fallback, not a redesign.
- **Spec:** `docs/superpowers/specs/2026-06-29-firecracker-microvm-slice5a-vmm-confinement-design.md`.

---

## File structure

- **Create** `sandbox/src/linux_firecracker/confine.rs` — the whole confinement unit: `VmmConfinement` enum, `confinement_from_env`, `find_executable`, `build_vmm_jail_argv`, `build_confined_spawn_argv`, and their unit tests. One file, one responsibility (≈250 LOC incl. tests).
- **Modify** `sandbox/src/linux_firecracker.rs` — `mod confine; pub use …`; call the confinement seam in `spawn_under_policy`.
- **Modify** `sandbox/src/linux_firecracker/probe.rs` — add `confine_vmm` + `vmm_confine_usable` bits, gate them in `probe_report`, gather them in `probe`.
- **Modify** `workers/microvm-run/src/boot.rs` — `firecracker_argv` takes the firecracker binary path.
- **Modify** `workers/microvm-run/src/main.rs` — parse optional `--firecracker-bin` (default `firecracker`).
- **Create** `core/tests/firecracker_vmm_confinement_e2e.rs` — DGX-only confined-boot + opt-out no-regression.

---

### Task 1: `VmmConfinement` enum + `confinement_from_env`

**Files:**
- Create: `sandbox/src/linux_firecracker/confine.rs`
- Modify: `sandbox/src/linux_firecracker.rs` (add `mod confine;` + `pub use`)

**Interfaces:**
- Produces: `pub enum VmmConfinement { None, BwrapCgroup }` and
  `pub fn confinement_from_env(flag: Option<&str>) -> VmmConfinement`.

- [ ] **Step 1: Write the failing tests** — create `sandbox/src/linux_firecracker/confine.rs` with only the module doc, a stub, and the test module:

```rust
//! Unprivileged VMM confinement (slice 5a): wrap the launcher + firecracker in
//! the existing bwrap jail + systemd-run cgroup. The `Jailer` strategy (a
//! privileged root chroot + uid-drop sibling) is a documented future addition —
//! the `VmmConfinement` enum is the seam where it would slot in.

use std::path::{Path, PathBuf};

use crate::linux_firecracker::plan::FirecrackerLaunchPlan;
use crate::SandboxError;

/// How the VMM (launcher + firecracker) is confined on the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmmConfinement {
    /// Bare launcher spawn — today's behaviour. Selected by the explicit opt-out.
    None,
    /// `systemd-run --user --scope` cgroup + an unprivileged `bwrap` jail. Default.
    BwrapCgroup,
    // Future: `Jailer` — firecracker's root jailer (chroot + uid-drop + cgroup +
    // netns) for a privileged/system deployment tier. An additive sibling; not
    // built in slice 5a. The match arms below are where it would dispatch.
}

/// Decide the confinement strategy from the `KASTELLAN_MICROVM_CONFINE_VMM` flag
/// value. Default-ON: only a clear opt-out (`0`/`false`/`no`/`off`, case- and
/// whitespace-insensitive) disables it; absent or any other value confines (the
/// secure default — a malformed flag must not silently drop containment).
pub fn confinement_from_env(flag: Option<&str>) -> VmmConfinement {
    match flag.map(|s| s.trim().to_ascii_lowercase()) {
        Some(v) if v == "0" || v == "false" || v == "no" || v == "off" => VmmConfinement::None,
        _ => VmmConfinement::BwrapCgroup,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_bwrap_cgroup_when_unset() {
        assert_eq!(confinement_from_env(None), VmmConfinement::BwrapCgroup);
    }

    #[test]
    fn explicit_opt_out_values_disable() {
        for v in ["0", "false", "no", "off", " OFF ", "False"] {
            assert_eq!(confinement_from_env(Some(v)), VmmConfinement::None, "value {v:?}");
        }
    }

    #[test]
    fn enabled_values_and_garbage_confine() {
        for v in ["1", "true", "yes", "on", "", "garbage"] {
            assert_eq!(confinement_from_env(Some(v)), VmmConfinement::BwrapCgroup, "value {v:?}");
        }
    }
}
```

(The `Path`/`PathBuf`/`FirecrackerLaunchPlan`/`SandboxError` imports are unused until Task 2/3 — add `#[allow(unused_imports)]` on them for this task only, removed in Task 2.)

- [ ] **Step 2: Wire the module in** — in `sandbox/src/linux_firecracker.rs`, after the existing `mod cleanup;` / `pub use cleanup::…` block (around line 27-31), add:

```rust
mod confine;
pub use confine::{confinement_from_env, VmmConfinement};
```

- [ ] **Step 3: Run the tests to verify they pass (DGX) / compile (Mac)**

DGX: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-sandbox confinement_from_env --lib'`
Expected: 3 passed (`default_is_bwrap_cgroup_when_unset`, `explicit_opt_out_values_disable`, `enabled_values_and_garbage_confine`).
Mac compile-check: `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets -D warnings`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add sandbox/src/linux_firecracker/confine.rs sandbox/src/linux_firecracker.rs
git commit -m "feat(sandbox): VmmConfinement enum + confinement_from_env (slice 5a)"
```

---

### Task 2: `find_executable` PATH resolver

The confined path must `--ro-bind` `firecracker` and `kastellan-microvm-run` by **absolute** path (bwrap execs the launcher by abs path; the launcher execs firecracker by abs path). Today the bare-name spawn relies on `$PATH`. Add a pure `$PATH` search.

**Files:**
- Modify: `sandbox/src/linux_firecracker/confine.rs`

**Interfaces:**
- Produces: `pub fn find_executable(name: &str, path_env: Option<&str>) -> Option<PathBuf>`.

- [ ] **Step 1: Write the failing tests** — append to the `tests` module in `confine.rs`:

```rust
    #[test]
    fn find_executable_returns_first_matching_dir() {
        // /usr/bin/true exists on the DGX; /nonexistent does not.
        let found = find_executable("true", Some("/nonexistent:/usr/bin"));
        assert_eq!(found, Some(PathBuf::from("/usr/bin/true")));
    }

    #[test]
    fn find_executable_none_when_absent_or_no_path() {
        assert_eq!(find_executable("definitely-not-a-binary-xyz", Some("/usr/bin")), None);
        assert_eq!(find_executable("true", None), None);
    }
```

- [ ] **Step 2: Run to verify failure** — DGX: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-sandbox find_executable --lib'`
Expected: FAIL — `cannot find function find_executable`.

- [ ] **Step 3: Implement** — add to `confine.rs` (above the test module), and remove the Task-1 `#[allow(unused_imports)]` on `PathBuf`:

```rust
/// Resolve `name` to an absolute path by scanning the dirs in `path_env`
/// (a `$PATH`-style `:`-joined string), returning the first that holds a file
/// of that name. Pure over the injected `path_env` so it is unit-testable; the
/// spawn site passes `std::env::var("PATH")`. Used only on the confined path,
/// where the binary must be bound into the jail by absolute path.
pub fn find_executable(name: &str, path_env: Option<&str>) -> Option<PathBuf> {
    let path_env = path_env?;
    path_env
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|dir| Path::new(dir).join(name))
        .find(|p| p.is_file())
}
```

- [ ] **Step 4: Run to verify pass** — DGX: same command as Step 2. Expected: 2 passed.
Mac compile-check: `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets -D warnings` — clean.

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/linux_firecracker/confine.rs
git commit -m "feat(sandbox): find_executable PATH resolver for VMM-jail binds (slice 5a)"
```

---

### Task 3: `build_vmm_jail_argv` — the bwrap jail builder

**Files:**
- Modify: `sandbox/src/linux_firecracker/confine.rs`

**Interfaces:**
- Consumes: `FirecrackerLaunchPlan` (Task-existing struct; reads `kernel_path`, `rootfs_path`, `egress_host_uds`).
- Produces: `pub fn build_vmm_jail_argv(plan: &FirecrackerLaunchPlan, run_dir: &Path, firecracker_bin: &Path, launcher_bin: &Path) -> Result<Vec<String>, SandboxError>` — returns the `bwrap …` argv ending with `--` (the caller appends the launcher argv).

- [ ] **Step 1: Write the failing tests** — append to the `tests` module. (Helper builds a minimal plan via the crate's `build_launch_plan`.)

```rust
    use crate::linux_firecracker::plan::build_launch_plan;
    use crate::linux_firecracker::FirecrackerImage;
    use crate::{Net, SandboxPolicy};

    fn deny_plan() -> FirecrackerLaunchPlan {
        build_launch_plan(
            &SandboxPolicy::default(),
            &FirecrackerImage { kernel_path: "/img/vmlinux".into(), rootfs_path: "/img/python-exec.ext4".into() },
            "/w", &[],
        ).unwrap()
    }

    fn jail(plan: &FirecrackerLaunchPlan) -> Vec<String> {
        build_vmm_jail_argv(plan, Path::new("/run/x"), Path::new("/home/u/.local/bin/firecracker"),
                            Path::new("/usr/local/bin/kastellan-microvm-run")).unwrap()
    }

    #[test]
    fn jail_starts_with_bwrap_and_core_isolation_flags() {
        let a = jail(&deny_plan());
        assert_eq!(a[0], "bwrap");
        for f in ["--unshare-all", "--die-with-parent", "--new-session", "--as-pid-1", "--clearenv"] {
            assert!(a.contains(&f.to_string()), "missing {f}: {a:?}");
        }
        assert_eq!(a.last().map(String::as_str), Some("--"));
    }

    #[test]
    fn jail_dev_binds_kvm_and_vsock_after_dev() {
        let a = jail(&deny_plan());
        let j = a.join(" ");
        assert!(j.contains("--dev /dev"), "needs a fresh minimal /dev: {j}");
        assert!(j.contains("--dev-bind /dev/kvm /dev/kvm"), "{j}");
        assert!(j.contains("--dev-bind /dev/vhost-vsock /dev/vhost-vsock"), "{j}");
        // --dev must precede the device binds or it shadows them.
        let dev = a.iter().position(|s| s == "--dev").unwrap();
        let kvm = a.iter().position(|s| s == "/dev/kvm").unwrap();
        assert!(dev < kvm, "--dev must come before --dev-bind /dev/kvm: {a:?}");
    }

    #[test]
    fn jail_ro_binds_kernel_rootfs_and_both_binaries() {
        let a = jail(&deny_plan());
        let j = a.join(" ");
        assert!(j.contains("--ro-bind /img/vmlinux /img/vmlinux"), "{j}");
        assert!(j.contains("--ro-bind /img/python-exec.ext4 /img/python-exec.ext4"), "{j}");
        assert!(j.contains("--ro-bind /home/u/.local/bin/firecracker /home/u/.local/bin/firecracker"), "{j}");
        assert!(j.contains("--ro-bind /usr/local/bin/kastellan-microvm-run /usr/local/bin/kastellan-microvm-run"), "{j}");
    }

    #[test]
    fn jail_rw_binds_the_run_dir() {
        let a = jail(&deny_plan());
        assert!(a.join(" ").contains("--bind /run/x /run/x"), "run dir must be writable: {a:?}");
    }

    #[test]
    fn jail_binds_egress_uds_only_when_force_routed() {
        // Net::Deny → no egress bind.
        assert!(!jail(&deny_plan()).iter().any(|s| s == "/scratch/egress.sock"));
        // Force-routed → bind the host proxy UDS rw.
        let policy = SandboxPolicy {
            net: Net::Allowlist(vec!["h:443".into()]),
            proxy_uds: Some("/scratch/egress.sock".into()),
            ..Default::default()
        };
        let plan = build_launch_plan(
            &policy,
            &FirecrackerImage { kernel_path: "/img/vmlinux".into(), rootfs_path: "/img/python-exec.ext4".into() },
            "/w", &[],
        ).unwrap();
        assert!(jail(&plan).join(" ").contains("--bind /scratch/egress.sock /scratch/egress.sock"));
    }

    #[test]
    fn jail_rejects_relative_run_dir() {
        let e = build_vmm_jail_argv(&deny_plan(), Path::new("rel/dir"),
            Path::new("/fc"), Path::new("/l")).unwrap_err();
        assert!(format!("{e}").contains("absolute"));
    }
```

- [ ] **Step 2: Run to verify failure** — DGX: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-sandbox jail_ --lib'`
Expected: FAIL — `cannot find function build_vmm_jail_argv`.

- [ ] **Step 3: Implement** — add to `confine.rs`:

```rust
/// Build the `bwrap` argv that jails the launcher + firecracker (slice 5a),
/// ending with `--` so the caller appends the launcher invocation. Binds ONLY
/// what the VMM tooling touches — NOT the worker's `fs_read`/`fs_write` (those
/// are the guest's, delivered as ext4 drives). Mirrors `linux_bwrap::build_argv`
/// invariants (`--unshare-all`/`--die-with-parent`/`--new-session`/`--as-pid-1`/
/// `--clearenv`). The launcher reads its config from argv, and the guest worker
/// env rides the kernel cmdline (in `fc.json`), so the jail forwards no env.
pub fn build_vmm_jail_argv(
    plan: &FirecrackerLaunchPlan,
    run_dir: &Path,
    firecracker_bin: &Path,
    launcher_bin: &Path,
) -> Result<Vec<String>, SandboxError> {
    if !run_dir.is_absolute() {
        return Err(SandboxError::Backend(format!(
            "vmm jail run_dir must be absolute, got {run_dir:?}"
        )));
    }
    let ro = |argv: &mut Vec<String>, p: &Path| {
        let s = p.display().to_string();
        argv.push("--ro-bind".into());
        argv.push(s.clone());
        argv.push(s);
    };

    let mut a: Vec<String> = Vec::with_capacity(48);
    a.push("bwrap".into());
    a.push("--unshare-all".into()); // user/ipc/pid/uts/cgroup/net ns; egress rides vsock, no host net
    a.push("--die-with-parent".into());
    a.push("--new-session".into());
    a.push("--as-pid-1".into());
    a.push("--clearenv".into());

    a.extend(["--proc".into(), "/proc".into()]);
    // Fresh minimal /dev FIRST, then bind the two devices into it (order matters:
    // `--dev /dev` after a `--dev-bind` would shadow it).
    a.extend(["--dev".into(), "/dev".into()]);
    a.extend(["--dev-bind".into(), "/dev/kvm".into(), "/dev/kvm".into()]);
    a.extend(["--dev-bind".into(), "/dev/vhost-vsock".into(), "/dev/vhost-vsock".into()]);
    a.extend(["--tmpfs".into(), "/tmp".into()]);

    // /usr + the merged-/usr symlinks + ld.so.cache so firecracker's and the
    // launcher's dynamic loader resolves (same set as linux_bwrap::build_argv).
    a.extend(["--ro-bind".into(), "/usr".into(), "/usr".into()]);
    a.extend(["--symlink".into(), "usr/bin".into(), "/bin".into()]);
    a.extend(["--symlink".into(), "usr/sbin".into(), "/sbin".into()]);
    a.extend(["--symlink".into(), "usr/lib".into(), "/lib".into()]);
    a.extend(["--symlink".into(), "usr/lib64".into(), "/lib64".into()]);
    a.extend(["--ro-bind-try".into(), "/etc/ld.so.cache".into(), "/etc/ld.so.cache".into()]);

    // Read-only: the guest kernel, the rootfs (drive is_read_only=true), and the
    // two host binaries. The per-spawn RO/RW share ext4 images live inside run_dir
    // and are covered by the rw run_dir bind below.
    ro(&mut a, &plan.kernel_path);
    ro(&mut a, &plan.rootfs_path);
    ro(&mut a, firecracker_bin);
    ro(&mut a, launcher_bin);

    // Writable: the per-spawn run dir (firecracker writes vsock.sock + fc.log;
    // the rw-scratch ext4 image lives here).
    let rd = run_dir.display().to_string();
    a.extend(["--bind".into(), rd.clone(), rd]);

    // Force-routed net worker: bind the host egress-proxy UDS rw so the launcher's
    // reverse-relay can reach it. Egress rides the vsock relay, so --unshare-all's
    // private netns is unaffected.
    if let Some(uds) = &plan.egress_host_uds {
        let s = uds.display().to_string();
        a.extend(["--bind".into(), s.clone(), s]);
    }

    a.push("--".into());
    Ok(a)
}
```

- [ ] **Step 4: Run to verify pass** — DGX: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-sandbox jail_ --lib'`
Expected: 6 passed.
Mac compile-check: `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets -D warnings` — clean.

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/linux_firecracker/confine.rs
git commit -m "feat(sandbox): build_vmm_jail_argv — unprivileged bwrap jail for the VMM (slice 5a)"
```

---

### Task 4: Launcher accepts `--firecracker-bin`

So the confined path can hand the launcher firecracker's absolute in-jail path (the jail has no `$PATH`).

**Files:**
- Modify: `workers/microvm-run/src/boot.rs` (`firecracker_argv`)
- Modify: `workers/microvm-run/src/main.rs` (arg parse + call site)

**Interfaces:**
- Produces: `firecracker_argv(fc_bin: &str, config_path: &str, log_path: &str) -> Vec<String>`; launcher CLI gains optional `--firecracker-bin <path>` (default `"firecracker"`).

- [ ] **Step 1: Write the failing test** — in `workers/microvm-run/src/boot.rs`, update/add the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firecracker_argv_uses_given_binary_path() {
        let a = firecracker_argv("/abs/firecracker", "/run/fc.json", "/run/fc.log");
        assert_eq!(a[0], "/abs/firecracker");
        assert!(a.windows(2).any(|w| w[0] == "--config-file" && w[1] == "/run/fc.json"));
        assert!(a.windows(2).any(|w| w[0] == "--log-path" && w[1] == "/run/fc.log"));
    }

    #[test]
    fn firecracker_argv_defaults_to_bare_name() {
        let a = firecracker_argv("firecracker", "/c", "/l");
        assert_eq!(a[0], "firecracker");
    }
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p kastellan-microvm-run firecracker_argv`
Expected: FAIL — `firecracker_argv` takes 2 args, not 3.

- [ ] **Step 3: Implement** — change `firecracker_argv` in `boot.rs`:

```rust
/// Build the firecracker argv. `fc_bin` is argv[0] — `"firecracker"` (resolved
/// via $PATH) on the bare path, or an absolute path on the confined path (the
/// bwrap jail has no $PATH, so the backend resolves + binds it and passes it here).
pub fn firecracker_argv(fc_bin: &str, config_path: &str, log_path: &str) -> Vec<String> {
    vec![
        fc_bin.into(),
        "--no-api".into(),
        "--config-file".into(), config_path.into(),
        "--log-path".into(), log_path.into(),
        "--level".into(), "Warn".into(),
    ]
}
```

- [ ] **Step 4: Update the call site in `workers/microvm-run/src/main.rs`** — parse the optional flag (default `"firecracker"`) alongside the existing flags, and thread it into the `firecracker_argv` call (replacing `boot::firecracker_argv(&config, &log)` with the binary-aware form):

```rust
// Among the existing arg parsing (--config-file/--vsock-uds/--run-dir/…), add:
let mut firecracker_bin = String::from("firecracker");
// ... in the arg loop, alongside the other "--flag value" arms:
//     "--firecracker-bin" => firecracker_bin = next_value()?,
// (Match the file's existing parsing style; if it uses a manual index loop,
//  add the arm there. If it already collects into a struct, add the field.)

// at the spawn site:
let fc_argv = boot::firecracker_argv(&firecracker_bin, &config, &log);
```

(Read `main.rs`'s existing arg-parse block first and mirror its exact style — manual `while`/`match` over `args`. The flag is optional; absence keeps `"firecracker"`, so the non-confined path is byte-identical.)

- [ ] **Step 5: Run to verify pass** — `cargo test -p kastellan-microvm-run` (runs natively on the Mac).
Expected: the 2 new tests pass; existing launcher tests still pass.

- [ ] **Step 6: Commit**

```bash
git add workers/microvm-run/src/boot.rs workers/microvm-run/src/main.rs
git commit -m "feat(microvm-run): optional --firecracker-bin for the confined jail (slice 5a)"
```

---

### Task 5: `build_confined_spawn_argv` + wire into `spawn_under_policy`

Compose `systemd-run … -- bwrap … -- <launcher abs> … --firecracker-bin <fc abs>` as a pure function, then dispatch on the strategy in the backend's spawn.

**Files:**
- Modify: `sandbox/src/linux_firecracker/confine.rs` (`build_confined_spawn_argv`)
- Modify: `sandbox/src/linux_firecracker.rs` (`spawn_under_policy` dispatch)

**Interfaces:**
- Consumes: `build_systemd_run_argv` (`crate::linux_cgroup`), `build_vmm_jail_argv` (Task 3), `launcher_argv` + `MICROVM_RUN_BIN` (`crate::linux_firecracker`).
- Produces: `pub fn build_confined_spawn_argv(policy: &SandboxPolicy, plan: &FirecrackerLaunchPlan, run_dir: &Path, firecracker_bin: &Path, launcher_bin: &Path, config_path: &str, log_path: &str) -> Result<Vec<String>, SandboxError>`.

- [ ] **Step 1: Write the failing tests** — append to `confine.rs` tests:

```rust
    #[test]
    fn confined_argv_is_systemd_then_bwrap_then_launcher() {
        let plan = deny_plan();
        let argv = build_confined_spawn_argv(
            &SandboxPolicy { mem_mb: 512, ..Default::default() },
            &plan, Path::new("/run/x"),
            Path::new("/fc/firecracker"), Path::new("/bin/kastellan-microvm-run"),
            "/run/x/fc.json", "/run/x/fc.log",
        ).unwrap();
        assert_eq!(argv[0], "systemd-run");
        // exactly two `--` separators: systemd-run|bwrap and bwrap|launcher
        assert_eq!(argv.iter().filter(|s| *s == "--").count(), 2);
        // launcher invoked by ABSOLUTE path (jail has no $PATH), not the bare name
        assert!(argv.contains(&"/bin/kastellan-microvm-run".to_string()));
        assert!(!argv.contains(&"kastellan-microvm-run".to_string()));
        // firecracker abs path handed to the launcher
        assert!(argv.windows(2).any(|w| w[0] == "--firecracker-bin" && w[1] == "/fc/firecracker"));
        // cgroup cap from the policy is present (proves systemd-run saw mem_mb)
        assert!(argv.join(" ").contains("MemoryMax=512M"));
    }

    #[test]
    fn confined_argv_orders_bwrap_between_separators() {
        let plan = deny_plan();
        let argv = build_confined_spawn_argv(
            &SandboxPolicy::default(), &plan, Path::new("/run/x"),
            Path::new("/fc"), Path::new("/l"), "/run/x/fc.json", "/run/x/fc.log",
        ).unwrap();
        let first_dd = argv.iter().position(|s| s == "--").unwrap();
        assert_eq!(argv[first_dd + 1], "bwrap", "bwrap must follow the systemd-run `--`");
    }
```

- [ ] **Step 2: Run to verify failure** — DGX: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-sandbox confined_argv --lib'`
Expected: FAIL — `cannot find function build_confined_spawn_argv`.

- [ ] **Step 3: Implement** — add to `confine.rs`:

```rust
use crate::linux_cgroup::build_systemd_run_argv;
use crate::linux_firecracker::launcher_argv;
use crate::SandboxPolicy;

/// Compose the full confined spawn argv:
///   systemd-run --user --scope … -- bwrap <vmm jail> -- <launcher abs> … --firecracker-bin <fc abs>
/// The launcher's argv[0] is rewritten to its absolute path (the jail has no
/// $PATH) and `--firecracker-bin <fc abs>` is appended so the in-jail launcher
/// execs firecracker by absolute path. Pure — unit-testable without spawning.
pub fn build_confined_spawn_argv(
    policy: &SandboxPolicy,
    plan: &FirecrackerLaunchPlan,
    run_dir: &Path,
    firecracker_bin: &Path,
    launcher_bin: &Path,
    config_path: &str,
    log_path: &str,
) -> Result<Vec<String>, SandboxError> {
    let mut argv = build_systemd_run_argv(policy); // ends with `--`
    argv.extend(build_vmm_jail_argv(plan, run_dir, firecracker_bin, launcher_bin)?); // ends with `--`

    let mut largv = launcher_argv(plan, config_path, log_path, &run_dir.display().to_string());
    largv[0] = launcher_bin.display().to_string(); // abs path, not MICROVM_RUN_BIN bare name
    largv.push("--firecracker-bin".into());
    largv.push(firecracker_bin.display().to_string());

    argv.extend(largv);
    Ok(argv)
}
```

Then export it from `sandbox/src/linux_firecracker.rs`:

```rust
pub use confine::{build_confined_spawn_argv, confinement_from_env, VmmConfinement};
```

- [ ] **Step 4: Run to verify pass** — DGX: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-sandbox confined_argv --lib'`
Expected: 2 passed.

- [ ] **Step 5: Wire into `spawn_under_policy`** — in `sandbox/src/linux_firecracker.rs`, replace the bare-spawn block (the `let argv = launcher_argv(…); let child = Command::new(&argv[0])…` section, ~lines 184-196) with strategy dispatch:

```rust
        let confine = confinement_from_env(
            std::env::var("KASTELLAN_MICROVM_CONFINE_VMM").ok().as_deref(),
        );
        let config_s = config_path.to_string_lossy().into_owned();
        let log_s = log_path.to_string_lossy().into_owned();
        let run_s = run_dir.to_string_lossy().into_owned();

        let argv = match confine {
            VmmConfinement::None => launcher_argv(&plan, &config_s, &log_s, &run_s),
            VmmConfinement::BwrapCgroup => {
                // Resolve the two binaries to absolute paths so they can be bound
                // into the jail (which has no $PATH). Fail closed: a missing
                // binary under the (default) confined strategy refuses to spawn —
                // never a silent bare-spawn fallback.
                let path_env = std::env::var("PATH").ok();
                let fc = confine::find_executable("firecracker", path_env.as_deref()).ok_or_else(|| {
                    SandboxError::Backend(
                        "VMM confinement on but firecracker not found on $PATH to bind into the \
                         jail (set KASTELLAN_MICROVM_CONFINE_VMM=0 to disable, or fix $PATH)".into(),
                    )
                })?;
                let launcher = confine::find_executable(MICROVM_RUN_BIN, path_env.as_deref()).ok_or_else(|| {
                    SandboxError::Backend(format!(
                        "VMM confinement on but {MICROVM_RUN_BIN} not found on $PATH to bind into \
                         the jail (set KASTELLAN_MICROVM_CONFINE_VMM=0 to disable, or fix $PATH)"
                    ))
                })?;
                build_confined_spawn_argv(policy, &plan, &run_dir, &fc, &launcher, &config_s, &log_s)?
            }
        };
        let child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| SandboxError::Backend(format!("microvm-run spawn failed: {e}")))?;
```

(Add `use confine;` is unnecessary — it's a sibling `mod`; reference as `confine::find_executable`. `build_confined_spawn_argv`/`confinement_from_env`/`VmmConfinement` are already `pub use`d into scope.)

- [ ] **Step 6: Verify the `None` path is byte-identical** — add to `confine.rs` tests a guard that the opt-out argv equals today's `launcher_argv`:

```rust
    #[test]
    fn none_strategy_matches_bare_launcher_argv() {
        let plan = deny_plan();
        let bare = launcher_argv(&plan, "/run/x/fc.json", "/run/x/fc.log", "/run/x");
        // The None arm calls launcher_argv with identical args — assert the
        // helper output is what we expect the bare spawn to use.
        assert_eq!(bare[0], crate::linux_firecracker::MICROVM_RUN_BIN);
        assert!(!bare.iter().any(|s| s == "--firecracker-bin"));
    }
```

- [ ] **Step 7: Run + compile-check** — DGX: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-sandbox --lib'` (all sandbox lib tests).
Expected: all pass (existing + new).
Mac: `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets -D warnings` — clean.

- [ ] **Step 8: Commit**

```bash
git add sandbox/src/linux_firecracker/confine.rs sandbox/src/linux_firecracker.rs
git commit -m "feat(sandbox): dispatch VMM confinement in spawn_under_policy, fail-closed (slice 5a)"
```

---

### Task 6: Probe gating for the confined path

When confinement is on (default), `probe()` must also verify bwrap + cgroup are usable, fail-closed, so a misconfigured host is rejected at probe time with a clear fix rather than at spawn.

**Files:**
- Modify: `sandbox/src/linux_firecracker/probe.rs`

**Interfaces:**
- `ProbeInputs` gains `confine_vmm: bool` and `vmm_confine_usable: bool`.
- `probe_report` errors on `confine_vmm && !vmm_confine_usable`.

- [ ] **Step 1: Write the failing tests** — in `probe.rs` tests, update the `ok()` helper and add cases:

```rust
    // update ok() to include the two new bits:
    //   confine_vmm: true, vmm_confine_usable: true,

    #[test]
    fn confine_on_but_unusable_names_both_fixes() {
        let err = probe_report(&ProbeInputs {
            confine_vmm: true, vmm_confine_usable: false, ..ok()
        }).unwrap_err();
        let m = format!("{err}");
        assert!(m.contains("KASTELLAN_MICROVM_CONFINE_VMM"), "names the opt-out: {m}");
        assert!(m.contains("install-bwrap-apparmor-profile.sh") || m.contains("systemd"), "names a fix: {m}");
    }

    #[test]
    fn confine_off_skips_the_check() {
        // confinement opted out → bwrap/cgroup not required → still Ok.
        assert!(probe_report(&ProbeInputs { confine_vmm: false, vmm_confine_usable: false, ..ok() }).is_ok());
    }
```

- [ ] **Step 2: Run to verify failure** — DGX: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-sandbox probe --lib'`
Expected: FAIL — `ProbeInputs` has no field `confine_vmm`.

- [ ] **Step 3: Implement** — in `probe.rs`: add the two fields to `ProbeInputs`; append the gated check at the end of `probe_report` (after the `mkfs_ext4` check, before `Ok(())`); and gather the bits in `probe`:

```rust
// in struct ProbeInputs { … add: }
    /// Whether VMM confinement is enabled (default-ON). When true, bwrap + the
    /// user cgroup are hard requirements (slice 5a).
    pub confine_vmm: bool,
    /// Whether the bwrap jail + systemd-run cgroup are usable. Only consulted
    /// when `confine_vmm` is true.
    pub vmm_confine_usable: bool,

// at the end of probe_report, before Ok(()):
    if inputs.confine_vmm && !inputs.vmm_confine_usable {
        return Err(SandboxError::Backend(
            "VMM confinement is enabled (KASTELLAN_MICROVM_CONFINE_VMM, default on) but the \
             bwrap jail + user cgroup are not usable: install the unprivileged-userns AppArmor \
             profile (`sudo scripts/linux/install-bwrap-apparmor-profile.sh`) and ensure a \
             `systemd --user` session is running (`loginctl enable-linger $USER`). To run VMs \
             WITHOUT host-side VMM confinement, set KASTELLAN_MICROVM_CONFINE_VMM=0"
                .into(),
        ));
    }

// in impl LinuxFirecracker::probe, extend the gathered inputs:
        let confine_vmm = matches!(
            super::confinement_from_env(std::env::var("KASTELLAN_MICROVM_CONFINE_VMM").ok().as_deref()),
            super::VmmConfinement::BwrapCgroup
        );
        let inputs = ProbeInputs {
            // … existing six bits …
            confine_vmm,
            // LinuxBwrap::probe() already verifies bwrap-userns AND the user cgroup
            // (it calls cgroup_probe internally) — exactly the two confinement deps.
            vmm_confine_usable: !confine_vmm || super::super::linux_bwrap::LinuxBwrap::probe().is_ok(),
        };
```

(The `!confine_vmm ||` short-circuit avoids spawning the bwrap probe when confinement is off. Confirm the path to `LinuxBwrap` from `probe.rs` — it is `crate::linux_bwrap::LinuxBwrap`; use that exact path.)

- [ ] **Step 4: Run to verify pass** — DGX: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-sandbox probe --lib'`
Expected: all probe tests pass.
Mac: `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets -D warnings` — clean.

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/linux_firecracker/probe.rs
git commit -m "feat(sandbox): fail-closed probe gating for VMM confinement (slice 5a)"
```

---

### Task 7: DGX confined-boot e2e (merge gate)

**Files:**
- Create: `core/tests/firecracker_vmm_confinement_e2e.rs`

**Interfaces:**
- Consumes: `kastellan_core::tool_host::{spawn_worker, WorkerSpec}`, `kastellan_core::workers::python_exec::firecracker_mode_entry`, `kastellan_sandbox::linux_firecracker::{FirecrackerImage, LinuxFirecracker}`, `SandboxBackends` — mirror `core/tests/python_exec_firecracker_e2e.rs` (read it first for the exact `spawn_worker`/`WorkerSpec` wiring + the `skip_if_no_microvm` helper).

- [ ] **Step 1: Write the e2e** (DGX-only, `#![cfg(target_os="linux")]`, `#[ignore]`). Mirror `python_exec_firecracker_e2e.rs`'s setup helpers (`image_dir`, `firecracker_image`, `locate_microvm_run`, `skip_if_no_microvm`). Two tests:

```rust
#![cfg(target_os = "linux")]
//! Slice 5a e2e: the FC VMM runs inside the unprivileged bwrap jail + systemd-run
//! cgroup (default-ON) and a python-exec VM still boots + computes. Proves
//! /dev/kvm + /dev/vhost-vsock survive the bwrap user namespace — the merge gate.
//!
//! DGX-only / #[ignore]: needs /dev/kvm + /dev/vhost-vsock + a built rootfs +
//! the RELEASE launcher on $PATH. Run:
//!     export PATH=$HOME/.local/bin:$PATH
//!     cargo build --release -p kastellan-microvm-run
//!     cargo test -p kastellan-core --test firecracker_vmm_confinement_e2e -- --ignored --nocapture

// … mirror the imports + helpers from python_exec_firecracker_e2e.rs …

/// Default path (confinement ON): a python-exec VM boots confined and runs 6*7.
#[test]
#[ignore = "DGX-only: real KVM + vsock + built rootfs"]
fn confined_python_exec_boots_and_computes() {
    // Ensure the default (do NOT set the opt-out).
    std::env::remove_var("KASTELLAN_MICROVM_CONFINE_VMM");
    if skip_if_no_microvm() { return; }
    // … build the firecracker_mode_entry python-exec worker, spawn_worker, send
    //   {"code":"print(6*7)"} python.exec, assert stdout contains "42" …
}

/// Opt-out no-regression: KASTELLAN_MICROVM_CONFINE_VMM=0 boots bare and computes.
#[test]
#[ignore = "DGX-only: real KVM + vsock + built rootfs"]
fn opt_out_bare_boot_still_computes() {
    std::env::set_var("KASTELLAN_MICROVM_CONFINE_VMM", "0");
    if skip_if_no_microvm() { std::env::remove_var("KASTELLAN_MICROVM_CONFINE_VMM"); return; }
    // … same spawn + 6*7 assertion; the bare path proves the None strategy intact …
    std::env::remove_var("KASTELLAN_MICROVM_CONFINE_VMM");
}
```

(Fill the `…` bodies by copying the spawn+assert flow from `confined`-less `python_exec_firecracker_e2e.rs::*` verbatim — same `WorkerSpec`, same `python.exec` request, same stdout assertion. The ONLY difference between the two tests here is the env flag.)

- [ ] **Step 2: Run on the DGX**

```
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && export PATH=$HOME/.local/bin:$PATH && \
  cargo build --release -p kastellan-microvm-run && \
  cargo test -p kastellan-core --test firecracker_vmm_confinement_e2e -- --ignored --nocapture'
```

Expected: both tests pass — `confined_python_exec_boots_and_computes` prints `42` (the merge gate), `opt_out_bare_boot_still_computes` prints `42`.
**If the confined test fails on a missing `/sys`, a missing firecracker shared lib, or a device-permission error:** that is the expected iteration point — add the needed `--ro-bind /sys /sys` or the firecracker `ldd` closure bind to `build_vmm_jail_argv` (Task 3) and re-run. If `/dev/kvm` itself is denied through the userns (no bind fixes it), invoke the documented fallback: flip `confinement_from_env`'s default to `None` and ship the mechanism opt-IN (`KASTELLAN_MICROVM_CONFINE_VMM=1` to enable), recording the decision in the spec + HANDOVER.

- [ ] **Step 3: No-regression sweep on the DGX**

```
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && export PATH=$HOME/.local/bin:$PATH && \
  cargo test -p kastellan-core --test python_exec_firecracker_e2e --test python_exec_firecracker_warm_idle_e2e \
    --test python_exec_firecracker_hostdir_e2e --test web_fetch_firecracker_egress_e2e -- --ignored --nocapture'
```

Expected: the existing FC suites still pass under the new default-ON confinement (they don't set the opt-out, so they now boot confined — this is the intended new default and the broad proof the confinement doesn't regress slices 1–4b).

- [ ] **Step 4: Commit**

```bash
git add core/tests/firecracker_vmm_confinement_e2e.rs
git commit -m "test(core): DGX e2e — confined VMM boot + opt-out no-regression (slice 5a)"
```

---

### Task 8: Docs — operator note + handover/roadmap

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`
- Modify: the FC operator runbook (the slice that documents `install-firecracker-vsock.sh` — locate via `grep -rl firecracker-vsock docs/`), adding the `KASTELLAN_MICROVM_CONFINE_VMM` toggle + the bwrap-AppArmor-profile dependency for confined VMs.

- [ ] **Step 1: Update the runbook** — document that VM workers now boot under host-side bwrap+cgroup confinement by default; hosts must have the unprivileged-userns AppArmor profile (`install-bwrap-apparmor-profile.sh`) and a `systemd --user` session, or set `KASTELLAN_MICROVM_CONFINE_VMM=0`.

- [ ] **Step 2: Update HANDOVER.md** — move slice 5a from "Next TODO" to a "Recently completed" entry (files, the default-ON/opt-out decision, the merge-gate result, test-count delta); refresh "Working state" for `linux_firecracker::confine`; write a fresh "Next TODO" leading with **slice 5b (long-lived/channel-worker-in-VM)** + the deferred true-`Jailer` strategy. Bump the `Last updated` + commit-hash + session-end test-count header fields.

- [ ] **Step 3: Tick ROADMAP.md** — mark slice 5a (VMM confinement) done with the commit hash; note 5b + true-jailer remain.

- [ ] **Step 4: Commit**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md docs/<runbook>.md
git commit -m "docs(handover,roadmap): firecracker slice 5a VMM confinement shipped"
```

---

## Self-review

**1. Spec coverage:**
- VMM-jail bwrap policy (spec §2) → Task 3 (`build_vmm_jail_argv`) ✓
- Confinement seam `{None, BwrapCgroup}` + documented `Jailer` (spec §3) → Task 1 ✓
- Default-ON / opt-out / fail-closed (spec §4) → Task 1 (`confinement_from_env`) + Task 5 (fail-closed binary resolution) + Task 6 (fail-closed probe) ✓
- Probe gating (spec §3) → Task 6 ✓
- Launcher inherits the jail; firecracker abs path (spec §1, "no $PATH in jail") → Task 4 + Task 5 ✓
- Testing: pure Mac/DGX unit + DGX e2e (spec §5) → Tasks 1-6 unit + Task 7 e2e ✓
- Merge gate + fallback to default-OFF (spec §4) → Task 7 Step 2 ✓
- Out-of-scope items (true jailer, 5b) → not implemented; referenced in Task 8 Next-TODO ✓

**2. Placeholder scan:** The `…` in Task 7 are explicit "copy the body verbatim from the named existing test" instructions with the exact source file — not vague TODOs. Task 4 Step 4 says "mirror main.rs's existing arg-parse style" with the exact arm to add. No `TBD`/`handle edge cases`/`add validation` placeholders.

**3. Type consistency:** `VmmConfinement`/`confinement_from_env`/`find_executable`/`build_vmm_jail_argv`/`build_confined_spawn_argv` signatures match across Tasks 1-3-5; `firecracker_argv(fc_bin, config, log)` 3-arg form matches between Task 4 def and Task 5's launcher path (Task 5 appends `--firecracker-bin`, consumed by Task 4's launcher parse); `ProbeInputs` field names (`confine_vmm`, `vmm_confine_usable`) match between Task 6 struct + tests + gatherer.

**Process note:** TDD red/green for the Linux-gated sandbox tests happens on the DGX (`ssh dgx`); the Mac does cross-clippy compile-checks. The launcher (Task 4) and the pure logic run/compile both places. Commit after each task; the whole branch verifies on the DGX before the PR.
