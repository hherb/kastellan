//! Fail-closed readiness probe for the Firecracker backend. The decision is a
//! pure fn over injected capability bits; the real device/binary checks are a
//! thin gatherer so the logic is testable without KVM.

use std::path::Path;

use crate::SandboxError;

use super::FirecrackerImage;

/// Capability bits the probe checks. Each false → a specific operator fix.
pub struct ProbeInputs {
    pub firecracker_on_path: bool,
    pub kvm_rw: bool,
    pub vhost_vsock_rw: bool,
    pub kernel_present: bool,
    pub rootfs_present: bool,
    /// `mkfs.ext4` (e2fsprogs) on `$PATH` — needed to build per-spawn host-dir
    /// share images (slice 3).
    pub mkfs_ext4_on_path: bool,
    /// Whether VMM confinement is enabled (default-ON). When true, bwrap + the
    /// user cgroup are hard requirements (slice 5a).
    pub confine_vmm: bool,
    /// Whether the bwrap jail + systemd-run cgroup are usable. Only consulted
    /// when `confine_vmm` is true.
    pub vmm_confine_usable: bool,
}

/// Pure: turn capability bits into an Ok or a fail-closed error naming the fix.
pub fn probe_report(inputs: &ProbeInputs) -> Result<(), SandboxError> {
    if !inputs.firecracker_on_path {
        return Err(SandboxError::Backend(
            "firecracker binary not on $PATH — install the pinned v1.16.0 release \
             (scripts/workers/microvm/install-firecracker.sh)"
                .into(),
        ));
    }
    if !inputs.kvm_rw {
        return Err(SandboxError::Backend(
            "/dev/kvm not readable+writable by this user — run the one-time host setup: \
             `sudo scripts/linux/install-firecracker-vsock.sh --kvm`"
                .into(),
        ));
    }
    if !inputs.vhost_vsock_rw {
        return Err(SandboxError::Backend(
            "/dev/vhost-vsock not accessible — run the one-time host setup: \
             `sudo scripts/linux/install-firecracker-vsock.sh` (loads + persists vhost_vsock \
             and ACL-grants this user)"
                .into(),
        ));
    }
    if !inputs.kernel_present {
        return Err(SandboxError::Backend(
            "guest kernel image missing — run scripts/workers/microvm/build-rootfs.sh".into(),
        ));
    }
    if !inputs.rootfs_present {
        return Err(SandboxError::Backend(
            "guest rootfs image missing — run scripts/workers/microvm/build-rootfs.sh".into(),
        ));
    }
    if !inputs.mkfs_ext4_on_path {
        return Err(SandboxError::Backend(
            "mkfs.ext4 not on $PATH — install e2fsprogs (Ubuntu: `sudo apt-get install \
             e2fsprogs`); required to build per-spawn host-dir share images"
                .into(),
        ));
    }
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
    Ok(())
}

/// True iff `path` is openable read+write by the current user.
fn dev_rw(path: &str) -> bool {
    use std::fs::OpenOptions;
    OpenOptions::new().read(true).write(true).open(path).is_ok()
}

impl super::LinuxFirecracker {
    /// Gather real capability bits and delegate to [`probe_report`].
    pub fn probe(image: &FirecrackerImage) -> Result<(), SandboxError> {
        let confine_vmm = matches!(
            super::confinement_from_env(std::env::var("KASTELLAN_MICROVM_CONFINE_VMM").ok().as_deref()),
            super::VmmConfinement::BwrapCgroup
        );
        let inputs = ProbeInputs {
            firecracker_on_path: which_on_path("firecracker"),
            kvm_rw: dev_rw("/dev/kvm"),
            vhost_vsock_rw: dev_rw("/dev/vhost-vsock"),
            kernel_present: Path::new(&image.kernel_path).exists(),
            rootfs_present: Path::new(&image.rootfs_path).exists(),
            mkfs_ext4_on_path: which_on_path("mkfs.ext4"),
            confine_vmm,
            // LinuxBwrap::probe() already verifies bwrap-userns AND the user cgroup
            // (it calls cgroup_probe internally) — exactly the two confinement deps.
            // The `!confine_vmm ||` short-circuit avoids spawning the bwrap probe
            // when confinement is off.
            vmm_confine_usable: !confine_vmm || crate::linux_bwrap::LinuxBwrap::probe().is_ok(),
        };
        probe_report(&inputs)
    }
}

/// Cheap `$PATH` lookup for `bin` (no spawn).
fn which_on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok() -> ProbeInputs {
        ProbeInputs {
            firecracker_on_path: true,
            kvm_rw: true,
            vhost_vsock_rw: true,
            kernel_present: true,
            rootfs_present: true,
            mkfs_ext4_on_path: true,
            confine_vmm: true,
            vmm_confine_usable: true,
        }
    }

    #[test]
    fn all_present_is_ok() {
        assert!(probe_report(&ok()).is_ok());
    }

    #[test]
    fn missing_firecracker_names_fix() {
        let err = probe_report(&ProbeInputs {
            firecracker_on_path: false,
            ..ok()
        })
        .unwrap_err();
        assert!(format!("{err}").contains("firecracker"));
    }

    #[test]
    fn missing_vsock_names_setup_script() {
        let err = probe_report(&ProbeInputs {
            vhost_vsock_rw: false,
            ..ok()
        })
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("vhost_vsock") && msg.contains("install-firecracker-vsock.sh"));
    }

    #[test]
    fn missing_kvm_names_fix() {
        let err = probe_report(&ProbeInputs {
            kvm_rw: false,
            ..ok()
        })
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("/dev/kvm") && msg.contains("install-firecracker-vsock.sh"));
    }

    #[test]
    fn missing_rootfs_names_build_script() {
        let err = probe_report(&ProbeInputs {
            rootfs_present: false,
            ..ok()
        })
        .unwrap_err();
        assert!(format!("{err}").contains("build-rootfs.sh"));
    }

    #[test]
    fn missing_mkfs_names_e2fsprogs() {
        let err = probe_report(&ProbeInputs { mkfs_ext4_on_path: false, ..ok() }).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("mkfs.ext4") && msg.contains("e2fsprogs"));
    }

    #[test]
    fn confine_on_but_unusable_names_both_fixes() {
        let err = probe_report(&ProbeInputs {
            confine_vmm: true,
            vmm_confine_usable: false,
            ..ok()
        })
        .unwrap_err();
        let m = format!("{err}");
        assert!(m.contains("KASTELLAN_MICROVM_CONFINE_VMM"), "names the opt-out: {m}");
        assert!(
            m.contains("install-bwrap-apparmor-profile.sh") || m.contains("systemd"),
            "names a fix: {m}"
        );
    }

    #[test]
    fn confine_off_skips_the_check() {
        // confinement opted out → bwrap/cgroup not required → still Ok.
        assert!(
            probe_report(&ProbeInputs {
                confine_vmm: false,
                vmm_confine_usable: false,
                ..ok()
            })
            .is_ok()
        );
    }
}
