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

/// Resolve `name` to an absolute path by scanning the dirs in `path_env`
/// (a `$PATH`-style `:`-joined string), returning the first that holds a file
/// of that name. Pure over the injected `path_env` so it is unit-testable; the
/// spawn site passes `std::env::var("PATH")`. Used only on the confined path,
/// where the binary must be bound into the jail by absolute path.
// Tasks 4-6 wire the call site; allow until then so the cross-clippy gate stays clean.
#[allow(dead_code)]
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
// Tasks 4-6 wire the call site; allow until then so the cross-clippy gate stays clean.
#[allow(dead_code)]
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
}
