//! Slice-3 host-dir-share value types + the `kastellan.mounts` manifest encoder.
//! Split out of `plan.rs` to keep it under the 500-LOC guideline. Pure — no KVM,
//! no spawn; unit-tested without root.

use std::path::PathBuf;

use crate::SandboxError;
use super::plan::hex_encode;

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

/// Cmdline token key carrying the hex-encoded mount manifest (slice 3). The guest
/// `kastellan-microvm-init` reads it from `/proc/cmdline`. Manually kept in sync
/// across the crate boundary (same constraint as `plan::ENV_CMDLINE_KEY`).
const MOUNTS_CMDLINE_KEY: &str = "kastellan.mounts";

/// Encode the derived host-dir shares as the ` kastellan.mounts=<hex>` cmdline
/// suffix. Block = one tab-separated line per drive (`ro\t<dev>\t<p1>\t<p2>…` /
/// `rw\t<dev>\t<mountpoint>`), lines joined by `\n`, hex-encoded. Returns
/// `Ok(None)` when both shares are absent so the cmdline stays byte-identical to
/// the pre-slice-3 baseline.
///
/// Fail closed if any path contains a `\t` (field separator) or `\n` (line
/// separator): such a path would silently shift the guest decoder's boundaries.
/// Absolute filesystem paths never legitimately contain these.
pub fn encode_mount_manifest(
    ro: Option<&RoShare>,
    rw: Option<&RwScratch>,
) -> Result<Option<String>, SandboxError> {
    if ro.is_none() && rw.is_none() {
        return Ok(None);
    }
    let mut lines: Vec<String> = Vec::new();
    let guard = |s: &str| -> Result<(), SandboxError> {
        if s.contains('\t') || s.contains('\n') {
            return Err(SandboxError::Backend(format!(
                "mount path {s:?} cannot be forwarded: it contains a tab or newline (the \
                 manifest's field/line separators)"
            )));
        }
        Ok(())
    };
    if let Some(ro) = ro {
        let mut fields = vec!["ro".to_string(), ro.guest_dev.clone()];
        for p in &ro.sources {
            let s = p.to_string_lossy();
            guard(&s)?;
            fields.push(s.into_owned());
        }
        lines.push(fields.join("\t"));
    }
    if let Some(rw) = rw {
        let mp = rw.mountpoint.to_string_lossy();
        guard(&mp)?;
        lines.push(format!("rw\t{}\t{}", rw.guest_dev, mp));
    }
    let block = lines.join("\n");
    Ok(Some(format!(" {MOUNTS_CMDLINE_KEY}={}", hex_encode(block.as_bytes()))))
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

    #[test]
    fn encode_mount_manifest_none_when_empty() {
        assert_eq!(encode_mount_manifest(None, None).unwrap(), None);
    }

    #[test]
    fn encode_mount_manifest_ro_only_fixture() {
        // Cross-crate sync guard: kastellan-microvm-init decodes this exact hex.
        // Block "ro\t/dev/vdb\t/opt/a" =
        //   72 6f 09 2f 64 65 76 2f 76 64 62 09 2f 6f 70 74 2f 61
        let ro = RoShare { sources: vec![PathBuf::from("/opt/a")], guest_dev: "/dev/vdb".into() };
        assert_eq!(
            encode_mount_manifest(Some(&ro), None).unwrap().unwrap(),
            " kastellan.mounts=726f092f6465762f766462092f6f70742f61"
        );
    }

    #[test]
    fn encode_mount_manifest_ro_and_rw() {
        let ro = RoShare { sources: vec![PathBuf::from("/opt/a")], guest_dev: "/dev/vdb".into() };
        let rw = RwScratch { mountpoint: PathBuf::from("/tmp/s"), guest_dev: "/dev/vdc".into() };
        let suffix = encode_mount_manifest(Some(&ro), Some(&rw)).unwrap().unwrap();
        assert!(suffix.starts_with(" kastellan.mounts="));
        // Single whitespace-free token.
        assert_eq!(suffix.split_whitespace().count(), 1);
    }

    #[test]
    fn encode_mount_manifest_rejects_tab_and_newline_in_paths() {
        let ro = RoShare { sources: vec![PathBuf::from("/opt/a\tb")], guest_dev: "/dev/vdb".into() };
        assert!(encode_mount_manifest(Some(&ro), None).is_err());
        let ro2 = RoShare { sources: vec![PathBuf::from("/opt/a\nb")], guest_dev: "/dev/vdb".into() };
        assert!(encode_mount_manifest(Some(&ro2), None).is_err());
    }
}
