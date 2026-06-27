//! Per-spawn ext4 image building for slice-3 host-dir sharing. Pure argv/path
//! helpers (unit-tested without root) + the I/O builder that stages fs_read
//! trees and runs `mkfs.ext4`. The images land in the spawn's run dir so the
//! launcher's RAII teardown (#362) reclaims them.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::plan::FirecrackerLaunchPlan;
use crate::SandboxError;

/// Default writable-scratch size. Disk-backed, so it does NOT consume the guest
/// `mem_size_mib` cap the way the existing tmpfs `/tmp` does.
pub const RW_SCRATCH_MIB_DEFAULT: u64 = 64;

/// Scratch size in MiB: `KASTELLAN_MICROVM_SCRATCH_MIB` if set+parseable, else
/// the default (fail-safe — a garbled value never aborts the boot).
pub fn rw_scratch_mib(env: &[(String, String)]) -> u64 {
    env.iter()
        .find(|(k, _)| k == "KASTELLAN_MICROVM_SCRATCH_MIB")
        .and_then(|(_, v)| v.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(RW_SCRATCH_MIB_DEFAULT)
}

/// Mirror an absolute `source` under `stage_root` so `mkfs.ext4 -d` reproduces
/// the absolute layout inside the image (e.g. `/opt/venv` → `<stage>/opt/venv`).
pub fn staged_path(stage_root: &Path, source: &Path) -> PathBuf {
    let rel = source.strip_prefix("/").unwrap_or(source);
    stage_root.join(rel)
}

/// `mkfs.ext4` argv that populates an image from a staged dir tree, journal-less
/// (a read-only ext4 that ever carried a journal needs recovery on RO mount —
/// the same reason the rootfs is built `-O ^has_journal`).
pub fn mkfs_populate_argv(stage_dir: &str, out_img: &str, size_mib: u64) -> Vec<String> {
    vec![
        "mkfs.ext4".into(),
        "-q".into(),
        "-F".into(),
        "-O".into(),
        "^has_journal".into(),
        "-d".into(),
        stage_dir.into(),
        out_img.into(),
        format!("{size_mib}M"),
    ]
}

/// `mkfs.ext4` argv for a blank writable image (no `-d`). Journalled is fine —
/// it is mounted read-write.
pub fn mkfs_blank_argv(out_img: &str, size_mib: u64) -> Vec<String> {
    vec![
        "mkfs.ext4".into(),
        "-q".into(),
        "-F".into(),
        out_img.into(),
        format!("{size_mib}M"),
    ]
}

/// Size the RO image to fit the staged tree with headroom (bytes → MiB, +16 MiB
/// slack, min 8 MiB). Keeps `mkfs.ext4` from rejecting a too-small size.
fn ro_image_mib(stage_root: &Path) -> u64 {
    fn dir_bytes(p: &Path) -> u64 {
        let mut total = 0;
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                let md = match e.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                total += if md.is_dir() { dir_bytes(&e.path()) } else { md.len() };
            }
        }
        total
    }
    (dir_bytes(stage_root) / (1024 * 1024) + 16).max(8)
}

/// Build the per-spawn share images into `run_dir`; set the plan's image paths.
/// Linux-only (runs `mkfs.ext4` + copies trees). No-op when no shares.
pub fn build_share_images(
    plan: &mut FirecrackerLaunchPlan,
    run_dir: &Path,
    env: &[(String, String)],
) -> Result<(), SandboxError> {
    let run = |argv: Vec<String>| -> Result<(), SandboxError> {
        let status = Command::new(&argv[0])
            .args(&argv[1..])
            .status()
            .map_err(|e| SandboxError::Backend(format!("spawn {}: {e}", argv[0])))?;
        if !status.success() {
            return Err(SandboxError::Backend(format!("{} failed: {status}", argv[0])));
        }
        Ok(())
    };

    if let Some(ro) = plan.ro_share.clone() {
        let stage_root = run_dir.join("ro-stage");
        for src in &ro.sources {
            let dest = staged_path(&stage_root, src);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| SandboxError::Backend(format!("stage mkdir {parent:?}: {e}")))?;
            }
            copy_tree(src, &dest)?;
        }
        let out = run_dir.join("ro-share.ext4");
        let mib = ro_image_mib(&stage_root);
        run(mkfs_populate_argv(
            &stage_root.to_string_lossy(),
            &out.to_string_lossy(),
            mib,
        ))?;
        plan.ro_image_path = Some(out);
    }

    if plan.rw_scratch.is_some() {
        let out = run_dir.join("rw-scratch.ext4");
        run(mkfs_blank_argv(&out.to_string_lossy(), rw_scratch_mib(env)))?;
        plan.rw_image_path = Some(out);
    }

    Ok(())
}

/// Recursively copy a host tree (dirs, files, symlinks-as-targets) into `dest`.
/// Plain `std` (no `fs_extra` dep).
fn copy_tree(src: &Path, dest: &Path) -> Result<(), SandboxError> {
    let md = std::fs::symlink_metadata(src)
        .map_err(|e| SandboxError::Backend(format!("stat {src:?}: {e}")))?;
    if md.is_dir() {
        std::fs::create_dir_all(dest)
            .map_err(|e| SandboxError::Backend(format!("mkdir {dest:?}: {e}")))?;
        for e in std::fs::read_dir(src)
            .map_err(|e| SandboxError::Backend(format!("read_dir {src:?}: {e}")))?
            .flatten()
        {
            copy_tree(&e.path(), &dest.join(e.file_name()))?;
        }
    } else {
        std::fs::copy(src, dest)
            .map_err(|e| SandboxError::Backend(format!("copy {src:?}->{dest:?}: {e}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn staged_path_mirrors_absolute_source() {
        assert_eq!(
            staged_path(Path::new("/run/x/ro-stage"), Path::new("/opt/venv")),
            PathBuf::from("/run/x/ro-stage/opt/venv")
        );
    }

    #[test]
    fn rw_scratch_mib_defaults_and_overrides() {
        assert_eq!(rw_scratch_mib(&[]), RW_SCRATCH_MIB_DEFAULT);
        let env = vec![("KASTELLAN_MICROVM_SCRATCH_MIB".to_string(), "256".to_string())];
        assert_eq!(rw_scratch_mib(&env), 256);
        // Garbage → fail-safe to default.
        let bad = vec![("KASTELLAN_MICROVM_SCRATCH_MIB".to_string(), "abc".to_string())];
        assert_eq!(rw_scratch_mib(&bad), RW_SCRATCH_MIB_DEFAULT);
    }

    #[test]
    fn mkfs_argv_shapes() {
        let pop = mkfs_populate_argv("/run/x/ro-stage", "/run/x/ro-share.ext4", 32);
        assert_eq!(pop[0], "mkfs.ext4");
        assert!(pop.windows(2).any(|w| w[0] == "-d" && w[1] == "/run/x/ro-stage"));
        assert!(pop.iter().any(|a| a == "^has_journal"));
        assert!(pop.iter().any(|a| a == "/run/x/ro-share.ext4"));
        assert!(pop.iter().any(|a| a == "32M"));
        let blank = mkfs_blank_argv("/run/x/rw-scratch.ext4", 64);
        assert_eq!(blank[0], "mkfs.ext4");
        assert!(blank.iter().any(|a| a == "/run/x/rw-scratch.ext4"));
        assert!(blank.iter().any(|a| a == "64M"));
        assert!(!blank.iter().any(|a| a == "-d"), "blank image has no -d source");
    }
}
