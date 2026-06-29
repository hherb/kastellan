//! Unprivileged VMM confinement (slice 5a): wrap the launcher + firecracker in
//! the existing bwrap jail + systemd-run cgroup. The `Jailer` strategy (a
//! privileged root chroot + uid-drop sibling) is a documented future addition —
//! the `VmmConfinement` enum is the seam where it would slot in.

use std::path::{Path, PathBuf};

#[allow(unused_imports)]
use crate::linux_firecracker::plan::FirecrackerLaunchPlan;
#[allow(unused_imports)]
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
#[allow(dead_code)]
pub fn find_executable(name: &str, path_env: Option<&str>) -> Option<PathBuf> {
    let path_env = path_env?;
    path_env
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|dir| Path::new(dir).join(name))
        .find(|p| p.is_file())
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
}
