//! Per-spawn writable scratch for sandboxed workers (macOS parity, #283).
//!
//! On Linux every writable-scratch worker gets a fresh ephemeral `/tmp` tmpfs
//! from bwrap (#89); macOS Seatbelt has no tmpfs, so the host must create a
//! per-spawn dir, grant it via `fs_write`, tell the worker where it is, and
//! clean it up. This module is that mechanism; it is composed around
//! `spawn_worker` by the cold-spawn sites (and the python-exec e2e harness).

use std::path::{Path, PathBuf};

use kastellan_sandbox::SandboxPolicy;

use crate::tool_host::ToolHostError;

/// Env var carrying the per-spawn scratch dir to a worker process. The worker
/// uses it for `TMPDIR`/`HOME`/cwd, falling back to `/tmp` when unset (the
/// Linux tmpfs path). **Keep in sync** with the worker-side constant
/// `kastellan_worker_python_exec::exec::WORKER_SCRATCH_ENV`.
pub const ENV_WORKER_SCRATCH: &str = "KASTELLAN_WORKER_SCRATCH";

use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic per-process counter so two scratch dirs spawned in the same
/// millisecond by the same pid still get distinct names.
static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

/// Build the per-spawn scratch path `<root>/pyexec-<pid>-<seq>`. Pure (no I/O).
pub fn scratch_subdir(root: &Path, pid: u32, seq: u64) -> PathBuf {
    root.join(format!("pyexec-{pid}-{seq}"))
}

/// Grant `dir` to the worker: a writable `fs_write` entry (→ Seatbelt
/// `(allow file-read* file-write* (subpath ...))`) and the
/// [`ENV_WORKER_SCRATCH`] env entry telling the worker where it is. Pure.
pub fn apply_scratch(policy: &mut SandboxPolicy, dir: &Path) {
    policy.fs_write.push(dir.to_path_buf());
    policy
        .env
        .push((ENV_WORKER_SCRATCH.to_string(), dir.to_string_lossy().into_owned()));
}

/// Env var naming the per-task durable output dir for a worker that opts into
/// artifact output (mirrors [`ENV_WORKER_SCRATCH`]). Unlike the scratch env,
/// this dir is **task-scoped** and **durable**: the lane runner creates it under
/// the artifacts root (`<artifacts_root>/<task_id>/`), so a file written here
/// survives the task and the path the worker returns is where the file actually
/// is. The runner prunes it after the task only if it is empty (see
/// `scheduler::runner`); retention of delivered files is an operator concern.
pub const ENV_WORKER_OUT: &str = "KASTELLAN_WORKER_OUT";

/// Bind a per-task `out/` directory into a worker policy: a writable `fs_write`
/// entry (→ the worker-side Landlock filter via `derive_lockdown_env`, so host
/// and worker agree) plus the [`ENV_WORKER_OUT`] env pointer telling the worker
/// where to write. Pure. Mirrors [`apply_scratch`] but for durable task output.
///
/// **Per-spawn vs. per-worker-lifetime hazard** (same class as
/// `ToolEntry.ephemeral_scratch`): this binds ONE task's `out/` into a policy
/// clone consulted only on a COLD spawn. A warm-reusable (`Lifecycle::IdleTimeout`)
/// worker would keep the first task's `out/` for every later task — and that dir
/// is wiped at the first task's finalize. So a tool that opts in via
/// `wants_workspace_out` MUST be `Lifecycle::SingleUse`; `apply_task_out`
/// debug-asserts this. No warm-reusable worker opts in today (mail is SingleUse);
/// revisit before the first one does.
pub fn apply_workspace_out(policy: &mut SandboxPolicy, out_dir: &Path) {
    policy.fs_write.push(out_dir.to_path_buf());
    policy
        .env
        .push((ENV_WORKER_OUT.to_string(), out_dir.to_string_lossy().into_owned()));
}

/// RAII owner of a host-created per-spawn scratch dir. `Drop` best-effort
/// removes the whole subtree — mirrors `crate::egress::net_worker`'s scratch
/// cleanup. Held inside `SupervisedWorker` so the dir outlives the worker
/// exactly and no longer.
#[must_use = "dropping the guard immediately removes the scratch dir before the worker can use it; bind it to the worker via SupervisedWorker::with_scratch"]
pub struct EphemeralScratch {
    dir: PathBuf,
}

impl EphemeralScratch {
    /// The granted scratch directory.
    pub fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for EphemeralScratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Create + grant a per-spawn scratch dir when a worker requests one, returning
/// the RAII guard the caller must hold for the worker's lifetime.
///
/// * **macOS, `ephemeral == true`:** create `<temp_dir>/pyexec-<pid>-<seq>`,
///   [`apply_scratch`] it onto `policy`, return `Some(guard)`. Fail-closed: any
///   create error aborts the spawn.
/// * **Otherwise** (off macOS, or `ephemeral == false`): `Ok(None)` — Linux's
///   bwrap tmpfs already provides per-spawn `/tmp`, so the host creates nothing.
///
/// The dir is created with [`std::fs::create_dir`] (exclusive — fails if the
/// path already exists), **not** `create_dir_all`: `<temp_dir>` always exists,
/// so no recursion is needed, and exclusivity means a name collision with a
/// dir leaked by a crashed prior run (reused pid + reset seq) aborts the spawn
/// fail-closed rather than silently handing the worker another spawn's stale
/// contents — preserving the per-spawn-isolation guarantee. (Crash-time leak
/// sweep is tracked in #251.)
///
/// Cross-platform-callable (runtime `cfg!`) so there is no dead code on Linux.
pub fn prepare_ephemeral_scratch(
    policy: &mut SandboxPolicy,
    ephemeral: bool,
) -> Result<Option<EphemeralScratch>, ToolHostError> {
    if !ephemeral || !cfg!(target_os = "macos") {
        return Ok(None);
    }
    let seq = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = scratch_subdir(&std::env::temp_dir(), std::process::id(), seq);
    std::fs::create_dir(&dir).map_err(ToolHostError::Io)?;
    apply_scratch(policy, &dir);
    Ok(Some(EphemeralScratch { dir }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_subdir_is_pid_seq_named_under_root() {
        let d = scratch_subdir(Path::new("/var/tmp"), 1234, 7);
        assert_eq!(d, PathBuf::from("/var/tmp/pyexec-1234-7"));
    }

    #[test]
    fn scratch_subdir_distinct_for_distinct_seq() {
        let a = scratch_subdir(Path::new("/r"), 9, 1);
        let b = scratch_subdir(Path::new("/r"), 9, 2);
        assert_ne!(a, b);
    }

    #[test]
    fn apply_scratch_adds_fs_write_and_env() {
        let mut p = SandboxPolicy::default();
        apply_scratch(&mut p, Path::new("/var/tmp/pyexec-1-1"));
        assert!(p.fs_write.contains(&PathBuf::from("/var/tmp/pyexec-1-1")));
        let hits: Vec<_> = p.env.iter().filter(|(k, _)| k == ENV_WORKER_SCRATCH).collect();
        assert_eq!(hits.len(), 1, "exactly one scratch env entry");
        assert_eq!(hits[0].1, "/var/tmp/pyexec-1-1");
    }

    #[test]
    fn apply_workspace_out_pushes_fs_write_and_env() {
        let mut p = SandboxPolicy::default();
        let dir = Path::new("/tmp/ws/out");
        apply_workspace_out(&mut p, dir);
        assert!(p.fs_write.contains(&PathBuf::from("/tmp/ws/out")), "out dir must be writable");
        let hits: Vec<_> = p.env.iter().filter(|(k, _)| k == ENV_WORKER_OUT).collect();
        assert_eq!(hits.len(), 1, "exactly one out env entry");
        assert_eq!(hits[0].1, "/tmp/ws/out");
    }

    #[test]
    fn ephemeral_scratch_drop_removes_the_dir() {
        let root = std::env::temp_dir();
        let dir = scratch_subdir(&root, std::process::id(), 424_242);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(dir.exists());
        {
            let guard = EphemeralScratch { dir: dir.clone() };
            assert_eq!(guard.path(), dir);
        } // drop here
        assert!(!dir.exists(), "Drop must remove the scratch dir");
    }

    #[test]
    fn prepare_returns_none_when_not_requested() {
        let mut p = SandboxPolicy::default();
        let before = p.clone();
        let got = prepare_ephemeral_scratch(&mut p, false).unwrap();
        assert!(got.is_none());
        assert_eq!(p.fs_write, before.fs_write, "policy untouched when flag off");
        assert_eq!(p.env.len(), before.env.len());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn prepare_is_noop_on_non_macos_even_when_requested() {
        // Linux already has the bwrap tmpfs; the host creates nothing.
        let mut p = SandboxPolicy::default();
        let got = prepare_ephemeral_scratch(&mut p, true).unwrap();
        assert!(got.is_none(), "no host scratch off macOS");
        assert!(p.fs_write.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn prepare_creates_grants_and_cleans_on_macos() {
        let mut p = SandboxPolicy::default();
        let guard = prepare_ephemeral_scratch(&mut p, true).unwrap().expect("Some on macOS");
        let dir = guard.path().to_path_buf();
        assert!(dir.exists(), "dir created on disk");
        assert!(p.fs_write.contains(&dir), "granted via fs_write");
        assert!(p.env.iter().any(|(k, v)| k == ENV_WORKER_SCRATCH && Path::new(v) == dir));
        drop(guard);
        assert!(!dir.exists(), "cleaned on drop");
    }
}
