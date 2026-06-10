//! Per-task scratch workspace with deterministic create-then-wipe lifecycle.
//!
//! Every task that needs writable scratch space asks for one [`Workspace`].
//! Creation lays down a fixed three-directory layout under
//! `<root>/<task_id>/{in,out,tmp}`, and the value's [`Drop`] impl recursively
//! removes the entire `<root>/<task_id>` tree. There is one owner, one
//! cleanup path, and no ambient state.
//!
//! Why a dedicated type instead of authoring `SandboxPolicy.fs_write` paths
//! by hand per worker:
//!
//! - **Single cleanup path.** RAII removes the dir even on panic or early
//!   return. The previous "caller authors fs_write paths" convention had no
//!   cleanup contract at all — workers wrote scratch dirs that nobody was
//!   responsible for removing.
//! - **Single owner.** [`Workspace::extend_policy`] is the canonical way to
//!   wire a workspace into a sandbox policy. The same set of paths flows to
//!   the bwrap/Seatbelt parent rules **and** to the worker-side Landlock
//!   filter (via `tool_host::derive_lockdown_env`), so the host and worker
//!   layers can never disagree.
//! - **No path-traversal in `task_id`.** Construction validates the task id
//!   against an explicit allow-list of characters, so a malicious or
//!   malformed task id can't escape the root.
//!
//! The default root is `$KASTELLAN_WORKSPACE_ROOT` if set, falling back to
//! `~/.kastellan/workspace`. Tests use [`Workspace::with_root`] to point at a
//! per-test temp directory and avoid global state.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use kastellan_sandbox::SandboxPolicy;

/// Env var name overriding the default workspace root. Useful for ops (e.g.
/// putting scratch on a tmpfs) and required for tests so they don't pollute
/// `~/.kastellan/`.
pub const ENV_WORKSPACE_ROOT: &str = "KASTELLAN_WORKSPACE_ROOT";

/// Errors from constructing a workspace. Keep small and explicit so callers
/// can turn each variant into a useful diagnostic.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    /// The task id contained characters outside `[A-Za-z0-9_-]`.
    /// Refused up-front to prevent path traversal and shell-quoting traps.
    #[error("invalid task id {0:?}: only [A-Za-z0-9_-] allowed")]
    InvalidTaskId(String),
    /// The home directory could not be determined while computing the
    /// default root. We do not silently fall back to `/tmp` — that would
    /// hide a real configuration error.
    #[error("cannot determine home directory for default workspace root; set {ENV_WORKSPACE_ROOT}")]
    NoHomeDir,
    /// Filesystem error during create or wipe. Wrapped to keep the cause
    /// path observable.
    #[error("io: {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// A per-task scratch directory rooted at `<root>/<task_id>/`, with the
/// fixed children `in/`, `out/`, `tmp/`. Dropping the value recursively
/// removes the per-task directory.
///
/// The three subdirs are intentional, not configurable: a stable contract
/// between core and workers means workers can hardcode "out is where I
/// write" without negotiating per call.
#[derive(Debug)]
pub struct Workspace {
    /// `<root>/<task_id>/`. The directory whose subtree is removed on drop.
    task_dir: PathBuf,
    inputs: PathBuf,
    outputs: PathBuf,
    tmp: PathBuf,
}

impl Workspace {
    /// Create a workspace under the default root (see module docs).
    ///
    /// `task_id` must be a non-empty string of `[A-Za-z0-9_-]`. Anything
    /// else is rejected with [`WorkspaceError::InvalidTaskId`] before any
    /// filesystem operation is attempted.
    pub fn new(task_id: &str) -> Result<Self, WorkspaceError> {
        let root = default_root()?;
        Self::with_root(&root, task_id)
    }

    /// Create a workspace under an explicit root directory. Used by tests
    /// (so each test can point at its own temp dir) and by ops paths that
    /// want to override the default location.
    ///
    /// The root is created if it does not already exist; the task subdir
    /// must not already exist (to avoid silently inheriting another task's
    /// state).
    pub fn with_root(root: &Path, task_id: &str) -> Result<Self, WorkspaceError> {
        validate_task_id(task_id)?;

        // Create the root if missing. We deliberately do *not* clean a
        // pre-existing root — only this workspace's own subtree is owned
        // by the [`Drop`] impl.
        fs::create_dir_all(root).map_err(|e| WorkspaceError::Io {
            path: root.to_path_buf(),
            source: e,
        })?;

        let task_dir = root.join(task_id);
        if task_dir.exists() {
            return Err(WorkspaceError::Io {
                path: task_dir,
                source: io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "task workspace already exists; refusing to inherit its state",
                ),
            });
        }

        let inputs = task_dir.join("in");
        let outputs = task_dir.join("out");
        let tmp = task_dir.join("tmp");

        for p in [&inputs, &outputs, &tmp] {
            fs::create_dir_all(p).map_err(|e| WorkspaceError::Io {
                path: p.clone(),
                source: e,
            })?;
        }

        Ok(Self {
            task_dir,
            inputs,
            outputs,
            tmp,
        })
    }

    /// Root of this workspace (`<root>/<task_id>`). Useful for diagnostics;
    /// callers should normally use [`inputs`](Self::inputs),
    /// [`outputs`](Self::outputs), or [`tmp`](Self::tmp) instead.
    pub fn root(&self) -> &Path {
        &self.task_dir
    }

    /// Read-only inputs the host stages for the worker.
    pub fn inputs(&self) -> &Path {
        &self.inputs
    }

    /// Worker-produced outputs the host harvests after the call.
    pub fn outputs(&self) -> &Path {
        &self.outputs
    }

    /// Worker-private scratch space; not harvested.
    pub fn tmp(&self) -> &Path {
        &self.tmp
    }

    /// The three directories the worker is permitted to write under.
    /// This is what flows into [`SandboxPolicy::fs_write`] (and from there,
    /// into the worker-side Landlock allow-list via
    /// `core::tool_host::derive_lockdown_env`).
    pub fn fs_write_paths(&self) -> Vec<PathBuf> {
        vec![self.inputs.clone(), self.outputs.clone(), self.tmp.clone()]
    }

    /// Append [`fs_write_paths`](Self::fs_write_paths) to a policy in-place.
    /// Intended as the canonical wiring point so a single call hooks the
    /// workspace up to both the parent-side sandbox rules and the
    /// worker-side Landlock filter.
    pub fn extend_policy(&self, policy: &mut SandboxPolicy) {
        policy.fs_write.extend(self.fs_write_paths());
    }
}

impl Drop for Workspace {
    /// Best-effort recursive wipe of the per-task directory.
    ///
    /// We cannot return an error from `Drop`. If the wipe fails (e.g.
    /// because a worker process is still holding a handle inside the dir),
    /// we log and move on; the supervisor will reap stale scratch dirs on
    /// next start. This is symmetric with how Linux's `bwrap --die-with-parent`
    /// handles cleanup of namespace state.
    fn drop(&mut self) {
        if let Err(e) = fs::remove_dir_all(&self.task_dir) {
            // `eprintln!` not `tracing` — the `Drop` impl runs even when
            // the tracing subscriber may not be installed yet (or has
            // already been torn down).
            eprintln!(
                "[kastellan-core] warning: failed to wipe workspace {}: {}",
                self.task_dir.display(),
                e
            );
        }
    }
}

/// Reject task ids that aren't strict `[A-Za-z0-9_-]+`. Anything containing
/// `/`, `..`, NUL, whitespace, or wildcard chars is rejected up-front.
fn validate_task_id(task_id: &str) -> Result<(), WorkspaceError> {
    if task_id.is_empty() {
        return Err(WorkspaceError::InvalidTaskId(task_id.to_string()));
    }
    let ok = task_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !ok {
        return Err(WorkspaceError::InvalidTaskId(task_id.to_string()));
    }
    Ok(())
}

/// `$KASTELLAN_WORKSPACE_ROOT` if set; otherwise `~/.kastellan/workspace`.
/// Errors only if neither is available.
fn default_root() -> Result<PathBuf, WorkspaceError> {
    if let Some(root) = std::env::var_os(ENV_WORKSPACE_ROOT) {
        return Ok(PathBuf::from(root));
    }
    let home = std::env::var_os("HOME").ok_or(WorkspaceError::NoHomeDir)?;
    Ok(PathBuf::from(home).join(".kastellan").join("workspace"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A tempdir helper that does *not* recursively delete on drop, so
    /// tests can assert on what `Workspace::Drop` did or didn't leave behind.
    /// Lives only for the test's duration; cleaned up explicitly at the
    /// end of each test.
    struct TestRoot(PathBuf);
    impl TestRoot {
        fn new(label: &str) -> Self {
            // Unique per process, per test, per call — pid + counter avoids
            // collisions when tests run in parallel within one binary.
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "kastellan-workspace-test-{}-{}-{}",
                std::process::id(),
                label,
                n
            ));
            // Ensure it doesn't already exist from a previous run.
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create test root");
            Self(path)
        }
    }
    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn creates_canonical_three_dir_layout() {
        let root = TestRoot::new("layout");
        let ws = Workspace::with_root(&root.0, "task-1").expect("create workspace");
        assert!(ws.root().is_dir());
        assert!(ws.inputs().is_dir());
        assert!(ws.outputs().is_dir());
        assert!(ws.tmp().is_dir());
        assert_eq!(ws.inputs(), root.0.join("task-1").join("in"));
        assert_eq!(ws.outputs(), root.0.join("task-1").join("out"));
        assert_eq!(ws.tmp(), root.0.join("task-1").join("tmp"));
    }

    #[test]
    fn drop_wipes_task_directory() {
        let root = TestRoot::new("drop");
        let task_dir;
        {
            let ws = Workspace::with_root(&root.0, "ephemeral").expect("create workspace");
            task_dir = ws.root().to_path_buf();
            // Place a file inside `out/` to verify recursive removal.
            fs::write(ws.outputs().join("artifact.txt"), b"hello").expect("write artifact");
            assert!(task_dir.exists());
        }
        assert!(
            !task_dir.exists(),
            "Workspace::Drop must remove the task directory recursively"
        );
        // The root itself is *not* this Workspace's responsibility.
        assert!(root.0.exists(), "Workspace::Drop must not touch the root");
    }

    #[test]
    fn fs_write_paths_returns_in_out_tmp_in_order() {
        let root = TestRoot::new("paths");
        let ws = Workspace::with_root(&root.0, "task").unwrap();
        let paths = ws.fs_write_paths();
        assert_eq!(paths.len(), 3);
        assert_eq!(paths[0], ws.inputs());
        assert_eq!(paths[1], ws.outputs());
        assert_eq!(paths[2], ws.tmp());
    }

    #[test]
    fn extend_policy_appends_three_paths_without_clobbering_existing() {
        use kastellan_sandbox::SandboxPolicy;
        let root = TestRoot::new("extend");
        let ws = Workspace::with_root(&root.0, "ext").unwrap();

        let mut policy = SandboxPolicy {
            fs_write: vec![PathBuf::from("/var/cache/kastellan")],
            ..SandboxPolicy::default()
        };
        ws.extend_policy(&mut policy);
        assert_eq!(policy.fs_write.len(), 4);
        assert_eq!(policy.fs_write[0], PathBuf::from("/var/cache/kastellan"));
        assert_eq!(policy.fs_write[1], ws.inputs());
        assert_eq!(policy.fs_write[2], ws.outputs());
        assert_eq!(policy.fs_write[3], ws.tmp());
    }

    #[test]
    fn task_id_with_path_separator_is_rejected() {
        let root = TestRoot::new("traversal");
        for bad in ["../etc", "a/b", "..", ".", "a b", "a\0b", "a\nb", ""] {
            let err = Workspace::with_root(&root.0, bad).expect_err(
                "task ids containing path separators or special chars must be rejected",
            );
            match err {
                WorkspaceError::InvalidTaskId(_) => {}
                other => panic!("expected InvalidTaskId for {bad:?}, got {other:?}"),
            }
        }
        // Validation must happen *before* any filesystem op, so the root
        // should still be empty after all the rejected attempts.
        let entries: Vec<_> = fs::read_dir(&root.0)
            .expect("read root")
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            entries.is_empty(),
            "rejected task ids must not create anything in the root; found: {:?}",
            entries.iter().map(|e| e.path()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn pre_existing_task_dir_is_refused() {
        let root = TestRoot::new("preexists");
        fs::create_dir_all(root.0.join("collide")).unwrap();
        let err = Workspace::with_root(&root.0, "collide")
            .expect_err("must refuse a pre-existing task dir");
        match err {
            WorkspaceError::Io { source, .. } => {
                assert_eq!(
                    source.kind(),
                    io::ErrorKind::AlreadyExists,
                    "expected AlreadyExists, got {source:?}"
                );
            }
            other => panic!("expected Io(AlreadyExists), got {other:?}"),
        }
    }

    #[test]
    fn root_is_created_if_missing() {
        let parent = TestRoot::new("nested");
        // The actual root we hand to `with_root` does not yet exist.
        let nested_root = parent.0.join("not").join("yet").join("there");
        assert!(!nested_root.exists());
        let ws = Workspace::with_root(&nested_root, "task").expect("auto-create root");
        assert!(nested_root.exists());
        assert!(ws.root().exists());
    }
}
