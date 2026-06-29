//! Unprivileged VMM confinement (slice 5a): wrap the launcher + firecracker in
//! the existing bwrap jail + systemd-run cgroup. The `Jailer` strategy (a
//! privileged root chroot + uid-drop sibling) is a documented future addition —
//! the `VmmConfinement` enum is the seam where it would slot in.

use std::path::{Path, PathBuf};

use crate::linux_cgroup::build_systemd_run_argv;
use crate::linux_firecracker::launcher_argv;
use crate::linux_firecracker::plan::FirecrackerLaunchPlan;
use crate::SandboxError;
use crate::SandboxPolicy;

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

    #[test]
    fn none_strategy_matches_bare_launcher_argv() {
        let plan = deny_plan();
        let bare = launcher_argv(&plan, "/run/x/fc.json", "/run/x/fc.log", "/run/x");
        // The None arm calls launcher_argv with identical args — assert the
        // helper output is what we expect the bare spawn to use.
        assert_eq!(bare[0], crate::linux_firecracker::MICROVM_RUN_BIN);
        assert!(!bare.iter().any(|s| s == "--firecracker-bin"));
    }
}
