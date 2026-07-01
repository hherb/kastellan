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

/// A persistent, host-backed RW drive mounted in-guest at `mountpoint`. Unlike
/// [`RwScratch`] its backing image is reused across spawns (contents survive).
#[derive(Clone, Debug, PartialEq)]
pub struct PersistentMount {
    pub mountpoint: PathBuf,
    pub guest_dev: String,
}

/// Top-level rootfs dirs an `fs_read`/`fs_write` path may be anchored under in
/// the micro-VM. These are exactly the empty anchor dirs `build-rootfs.sh`
/// pre-creates (the guest init tmpfs-mounts the anchor so a bind/mount target is
/// creatable on the otherwise read-only root), plus `/tmp` (already a tmpfs the
/// init mounts at boot). A share under any other top-level cannot be mounted
/// in-guest — the anchor dir does not exist on the read-only rootfs, so the
/// tmpfs and the bind both fail — so such paths must be rejected up front in
/// `build_launch_plan` rather than silently failing to appear inside the guest.
/// Keep this list in lockstep with `build-rootfs.sh`'s anchor `mkdir`.
const SHARE_ANCHORS: &[&str] = &["opt", "data", "srv", "mnt", "work", "tmp"];

/// Returns the offending first path component when `path`'s top-level is not a
/// permitted share anchor (see [`SHARE_ANCHORS`]); `None` when it is anchorable.
/// This is an *allowlist*: it rejects not only rootfs system dirs (`/usr`,
/// `/etc`, …) but any top-level (e.g. `/home`, `/var`) the rootfs has no anchor
/// for, since those would silently fail to mount inside the guest.
pub fn non_anchor_top_level(path: &std::path::Path) -> Option<&str> {
    let first = path
        .components()
        .find_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })?;
    if SHARE_ANCHORS.contains(&first) {
        None
    } else {
        Some(first)
    }
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
    persistent: Option<&PersistentMount>,
) -> Result<Option<String>, SandboxError> {
    if ro.is_none() && rw.is_none() && persistent.is_none() {
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
    if let Some(ps) = persistent {
        let mp = ps.mountpoint.to_string_lossy();
        guard(&mp)?;
        lines.push(format!("rw\t{}\t{}", ps.guest_dev, mp));
    }
    let block = lines.join("\n");
    Ok(Some(format!(" {MOUNTS_CMDLINE_KEY}={}", hex_encode(block.as_bytes()))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_anchor_top_level_allowlists_share_anchors() {
        use std::path::Path;
        // Rootfs system dirs are rejected (offending component returned).
        assert_eq!(non_anchor_top_level(Path::new("/usr/lib/foo")), Some("usr"));
        assert_eq!(non_anchor_top_level(Path::new("/etc/passwd")), Some("etc"));
        // Non-system dirs the rootfs has no anchor for are ALSO rejected — they
        // would silently fail to mount in-guest (the bug the allowlist closes).
        assert_eq!(non_anchor_top_level(Path::new("/home/user/x")), Some("home"));
        assert_eq!(non_anchor_top_level(Path::new("/var/lib/x")), Some("var"));
        // The pre-created share anchors (+ /tmp) are accepted.
        assert_eq!(non_anchor_top_level(Path::new("/opt/venv")), None);
        assert_eq!(non_anchor_top_level(Path::new("/data/x")), None);
        assert_eq!(non_anchor_top_level(Path::new("/work/scratch")), None);
        assert_eq!(non_anchor_top_level(Path::new("/tmp/x")), None);
    }

    #[test]
    fn encode_mount_manifest_none_when_empty() {
        assert_eq!(encode_mount_manifest(None, None, None).unwrap(), None);
    }

    #[test]
    fn encode_mount_manifest_ro_only_fixture() {
        // Cross-crate sync guard: kastellan-microvm-init decodes this exact hex.
        // Block "ro\t/dev/vdb\t/opt/a" =
        //   72 6f 09 2f 64 65 76 2f 76 64 62 09 2f 6f 70 74 2f 61
        let ro = RoShare { sources: vec![PathBuf::from("/opt/a")], guest_dev: "/dev/vdb".into() };
        assert_eq!(
            encode_mount_manifest(Some(&ro), None, None).unwrap().unwrap(),
            " kastellan.mounts=726f092f6465762f766462092f6f70742f61"
        );
    }

    #[test]
    fn encode_mount_manifest_ro_and_rw() {
        let ro = RoShare { sources: vec![PathBuf::from("/opt/a")], guest_dev: "/dev/vdb".into() };
        let rw = RwScratch { mountpoint: PathBuf::from("/tmp/s"), guest_dev: "/dev/vdc".into() };
        let suffix = encode_mount_manifest(Some(&ro), Some(&rw), None).unwrap().unwrap();
        assert!(suffix.starts_with(" kastellan.mounts="));
        // Single whitespace-free token.
        assert_eq!(suffix.split_whitespace().count(), 1);
    }

    #[test]
    fn encode_mount_manifest_rejects_tab_and_newline_in_paths() {
        let ro = RoShare { sources: vec![PathBuf::from("/opt/a\tb")], guest_dev: "/dev/vdb".into() };
        assert!(encode_mount_manifest(Some(&ro), None, None).is_err());
        let ro2 = RoShare { sources: vec![PathBuf::from("/opt/a\nb")], guest_dev: "/dev/vdb".into() };
        assert!(encode_mount_manifest(Some(&ro2), None, None).is_err());
    }

    #[test]
    fn encode_includes_persistent_rw_line_after_scratch() {
        let rw = RwScratch { mountpoint: PathBuf::from("/tmp"), guest_dev: "/dev/vdc".into() };
        let ps = PersistentMount { mountpoint: PathBuf::from("/data"), guest_dev: "/dev/vdd".into() };
        let suffix = encode_mount_manifest(None, Some(&rw), Some(&ps)).unwrap().unwrap();
        // hex-decode the value after "kastellan.mounts="
        let hex = suffix.trim().strip_prefix("kastellan.mounts=").unwrap();
        let bytes = (0..hex.len()).step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect::<Vec<u8>>();
        let decoded = String::from_utf8(bytes).unwrap();
        assert!(decoded.contains("rw\t/dev/vdc\t/tmp"));
        assert!(decoded.contains("rw\t/dev/vdd\t/data"));
    }
}
