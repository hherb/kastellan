//! Slice-3 host-dir-share value types + the `kastellan.mounts` manifest encoder.
//! Split out of `plan.rs` to keep it under the 500-LOC guideline. Pure — no KVM,
//! no spawn; unit-tested without root.

use std::path::PathBuf;

/// A read-only host-dir share: the absolute `fs_read` roots exposed inside the
/// guest at their original paths, plus the guest device node the RO ext4 will
/// appear as. The image is built per-spawn into the run dir (see the backend).
#[derive(Clone, Debug, PartialEq)]
pub struct RoShare {
    pub sources: Vec<PathBuf>,
    pub guest_dev: String,
}

/// A writable, disk-backed scratch drive mounted in-guest at `mountpoint`.
/// Ephemeral — discarded with the run dir on teardown (no host write-back).
#[derive(Clone, Debug, PartialEq)]
pub struct RwScratch {
    pub mountpoint: PathBuf,
    pub guest_dev: String,
}

/// Reserved rootfs top-level dirs an `fs_read` path may not live under: mounting
/// a tmpfs anchor over one of these would shadow the worker's own files. Returns
/// the offending first component if reserved, else `None`.
pub fn reserved_top_level(path: &std::path::Path) -> Option<&str> {
    const RESERVED: &[&str] =
        &["usr", "bin", "lib", "lib64", "etc", "sbin", "proc", "sys", "dev", "boot", "root"];
    let first = path
        .components()
        .find_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })?;
    RESERVED.iter().copied().find(|&r| r == first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_top_level_flags_system_dirs_only() {
        use std::path::Path;
        assert_eq!(reserved_top_level(Path::new("/usr/lib/foo")), Some("usr"));
        assert_eq!(reserved_top_level(Path::new("/etc/passwd")), Some("etc"));
        assert_eq!(reserved_top_level(Path::new("/opt/venv")), None);
        assert_eq!(reserved_top_level(Path::new("/data/x")), None);
    }
}
