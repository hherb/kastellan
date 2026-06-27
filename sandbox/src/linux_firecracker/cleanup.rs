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

#[cfg(test)]
mod tests {
    use super::*;

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
}
