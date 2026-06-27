//! Best-effort cleanup of orphaned per-spawn micro-VM run directories.
//!
//! Each micro-VM spawn gets a temp run-dir (`kastellan-microvm-<pid>-<seq>`)
//! holding `fc.json`, `fc.log`, and the per-spawn vsock UDS. The launcher
//! (`kastellan-microvm-run`) removes its own run-dir on every graceful/panic
//! exit (see `workers/microvm-run`). This module is the BACKSTOP for the one
//! case the launcher cannot self-clean: a launcher killed by SIGKILL (the
//! wall-clock watchdog, OOM, or PDEATHSIG when the daemon dies) never runs its
//! teardown, leaking its run-dir.
//!
//! The backstop is keyed on the launcher's OWN pid, written into
//! `<run_dir>/launcher.pid` by the backend right after spawn. The dir-NAME pid
//! is the daemon's pid (shared by every run-dir from one daemon), so it is
//! useless as a per-VM liveness signal; the pidfile is the authoritative one.

/// Filename of the per-run pidfile each run-dir carries: the
/// `kastellan-microvm-run` launcher's PID, written by the backend after spawn.
pub const LAUNCHER_PID_FILE: &str = "launcher.pid";

/// Name prefix of every per-spawn run-dir under the system temp dir.
/// Kept in sync with `make_spawn_dir` in the parent module.
pub const RUN_DIR_PREFIX: &str = "kastellan-microvm-";

/// Pure decision: should an orphan sweep remove a run-dir, given the contents of
/// its pidfile (if any) and a liveness predicate?
///
/// Returns `true` ONLY when the pidfile is present AND parses to a PID the
/// `alive` predicate reports as dead. Every uncertain case returns `false` —
/// the sweep must never delete a dir it cannot prove belongs to a dead launcher:
/// - `None` (no pidfile yet — a dir still mid-spawn) → keep
/// - unparseable / whitespace-only contents → keep
/// - a live PID → keep
///
/// This conservatism is what makes the sweep safe to run concurrently with live
/// spawns: a false negative is a missed cleanup (caught next sweep); a false
/// positive would delete a running VM's dir, which this rules out.
pub fn orphaned_run_dir_should_remove(pidfile: Option<String>, alive: impl Fn(u32) -> bool) -> bool {
    match pidfile
        .as_deref()
        .map(str::trim)
        .and_then(|s| s.parse::<u32>().ok())
    {
        Some(pid) => !alive(pid),
        None => false,
    }
}

/// I/O: scan `temp_dir` for orphaned `kastellan-microvm-*` run-dirs and remove
/// them. A dir is orphaned when its `launcher.pid` names a dead PID (see
/// [`orphaned_run_dir_should_remove`]). Best-effort throughout: an unreadable
/// entry or a failed removal is skipped, never propagated. Returns the number of
/// dirs actually removed.
///
/// Called at the top of `spawn_under_policy` (before this spawn creates its own
/// dir), so it is naturally rate-matched to micro-VM use and never sees the
/// in-flight spawn's not-yet-created dir.
pub fn sweep_orphaned_run_dirs(temp_dir: &std::path::Path, alive: impl Fn(u32) -> bool) -> usize {
    let entries = match std::fs::read_dir(temp_dir) {
        Ok(e) => e,
        Err(_) => return 0, // temp dir unreadable/absent → nothing to do.
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        // Cheap name check first (no syscall): skip the `is_dir` stat for the
        // vast majority of `/tmp` entries that aren't ours.
        let is_run_dir = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(RUN_DIR_PREFIX));
        if !is_run_dir {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let pidfile = std::fs::read_to_string(path.join(LAUNCHER_PID_FILE)).ok();
        if orphaned_run_dir_should_remove(pidfile, &alive) && std::fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Linux liveness check via `/proc/<pid>` existence. No external dependency.
/// A reused pid (a dead launcher's pid now held by an unrelated process) reads
/// as alive → that dir is conservatively kept (a safe missed-cleanup).
pub fn pid_is_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn removes_when_pidfile_names_a_dead_pid() {
        assert!(orphaned_run_dir_should_remove(Some("999".into()), |_| false));
    }

    #[test]
    fn keeps_when_pidfile_names_a_live_pid() {
        assert!(!orphaned_run_dir_should_remove(Some("999".into()), |_| true));
    }

    #[test]
    fn keeps_when_no_pidfile() {
        // A dir still mid-spawn (created, pidfile not yet written) must survive.
        assert!(!orphaned_run_dir_should_remove(None, |_| false));
    }

    #[test]
    fn keeps_when_pidfile_is_garbage() {
        assert!(!orphaned_run_dir_should_remove(Some("not-a-pid".into()), |_| false));
    }

    #[test]
    fn parses_pidfile_with_trailing_whitespace() {
        // Dead pid with a trailing newline (how the backend writes it) → remove.
        assert!(orphaned_run_dir_should_remove(Some("123\n".into()), |p| {
            assert_eq!(p, 123, "whitespace must be trimmed before parse");
            false
        }));
    }

    // Unique temp root per test so parallel runs don't collide.
    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);
    fn fresh_temp_root() -> std::path::PathBuf {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "kastellan-sweeptest-{}-{}",
            std::process::id(),
            seq
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn make_run_dir(root: &Path, suffix: &str, pidfile: Option<&str>) -> std::path::PathBuf {
        let dir = root.join(format!("{RUN_DIR_PREFIX}{suffix}"));
        fs::create_dir_all(&dir).unwrap();
        if let Some(contents) = pidfile {
            fs::write(dir.join(LAUNCHER_PID_FILE), contents).unwrap();
        }
        dir
    }

    #[test]
    fn sweep_removes_dead_pid_dir_keeps_live_and_pidfileless() {
        let root = fresh_temp_root();
        let dead = make_run_dir(&root, "1-0", Some("100\n")); // dead
        let live = make_run_dir(&root, "1-1", Some("200\n")); // live
        let young = make_run_dir(&root, "1-2", None); // mid-spawn, no pidfile
        let other = root.join("unrelated-dir");
        fs::create_dir_all(&other).unwrap();

        // alive(): only pid 200 is "alive".
        let removed = sweep_orphaned_run_dirs(&root, |p| p == 200);

        assert_eq!(removed, 1, "exactly the dead-pid dir is removed");
        assert!(!dead.exists(), "dead-pid run-dir removed");
        assert!(live.exists(), "live-pid run-dir kept");
        assert!(young.exists(), "pidfile-less (mid-spawn) run-dir kept");
        assert!(other.exists(), "non-matching dir untouched");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn sweep_on_missing_dir_returns_zero() {
        let missing = std::env::temp_dir().join("kastellan-sweeptest-does-not-exist-xyz");
        assert_eq!(sweep_orphaned_run_dirs(&missing, |_| false), 0);
    }

    #[test]
    fn pid_is_alive_true_for_self_false_for_unused() {
        // Our own pid is alive; pid 0 is never a normal process under /proc.
        assert!(pid_is_alive(std::process::id()));
        assert!(!pid_is_alive(0));
    }
}
