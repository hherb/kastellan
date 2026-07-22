//! Harvest a task's workspace `out/` deliverables into a durable artifacts
//! dir before the ephemeral workspace is wiped.
//!
//! The per-task [`crate::workspace::Workspace`] `Drop` recursively wipes the
//! whole `<root>/<task_id>` tree, so anything a worker wrote to `out/` would be
//! lost. The lane runner calls [`harvest_outputs`] at task finalize — after the
//! last dispatch, before the `Workspace` drops — to move those files somewhere
//! durable the agent/user can retrieve.

use std::path::{Path, PathBuf};

/// Move every entry directly under `out_dir` into `<artifacts_root>/<task_id>/`,
/// returning the destination paths. Rename first (same-filesystem, atomic);
/// fall back to copy+remove across filesystems. Best-effort: a file that cannot
/// be moved is logged and skipped, never fatal. An absent/empty/unreadable
/// `out_dir` yields an empty `Vec`.
pub(super) fn harvest_outputs(out_dir: &Path, artifacts_root: &Path, task_id: i64) -> Vec<PathBuf> {
    let dest_dir = artifacts_root.join(task_id.to_string());
    let mut harvested = Vec::new();

    let entries = match std::fs::read_dir(out_dir) {
        Ok(e) => e,
        Err(_) => return harvested, // out dir absent/unreadable → nothing to harvest
    };
    let mut created_dest = false;
    for entry in entries.flatten() {
        let src = entry.path();
        let Some(name) = src.file_name() else { continue };
        if !created_dest {
            if let Err(e) = std::fs::create_dir_all(&dest_dir) {
                tracing::warn!(task_id, error = %e, dir = ?dest_dir, "harvest: create artifacts dir failed");
                return harvested;
            }
            created_dest = true;
        }
        let dst = dest_dir.join(name);
        match std::fs::rename(&src, &dst) {
            Ok(()) => harvested.push(dst),
            Err(_) => match copy_then_remove(&src, &dst) {
                Ok(()) => harvested.push(dst),
                Err(e) => {
                    tracing::warn!(task_id, error = %e, src = ?src, "harvest: move failed, skipped")
                }
            },
        }
    }
    harvested
}

fn copy_then_remove(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::copy(src, dst)?;
    std::fs::remove_file(src)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harvest_moves_files_and_returns_dest_paths() {
        let base = std::env::temp_dir().join(format!("kastellan-harvest-{}", std::process::id()));
        let out = base.join("out");
        let artifacts = base.join("artifacts");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("booking.pdf"), b"%PDF-1.7 fake").unwrap();

        let dests = harvest_outputs(&out, &artifacts, 42);

        assert_eq!(dests.len(), 1);
        let moved = artifacts.join("42").join("booking.pdf");
        assert!(moved.exists(), "file harvested to artifacts/<task_id>/");
        assert_eq!(std::fs::read(&moved).unwrap(), b"%PDF-1.7 fake");
        assert!(!out.join("booking.pdf").exists(), "source moved, not copied");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn harvest_empty_out_is_empty() {
        let base =
            std::env::temp_dir().join(format!("kastellan-harvest-empty-{}", std::process::id()));
        let out = base.join("out");
        std::fs::create_dir_all(&out).unwrap();
        assert!(harvest_outputs(&out, &base.join("artifacts"), 1).is_empty());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn harvest_absent_out_dir_is_empty_not_panic() {
        let base = std::env::temp_dir().join(format!("kastellan-harvest-absent-{}", std::process::id()));
        assert!(harvest_outputs(&base.join("nope"), &base.join("artifacts"), 9).is_empty());
    }
}
