//! Unprivileged VMM confinement (slice 5a): wrap the launcher + firecracker in
//! the existing bwrap jail + systemd-run cgroup. The `Jailer` strategy (a
//! privileged root chroot + uid-drop sibling) is a documented future addition —
//! the `VmmConfinement` enum is the seam where it would slot in.

#[allow(unused_imports)]
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
