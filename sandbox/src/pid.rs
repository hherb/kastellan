//! Cross-platform pid-liveness check, shared by every orphan-reclaim sweep
//! (the Firecracker run-dir sweep in [`crate::linux_firecracker`] and the
//! egress scratch-dir sweep in `kastellan-core`). One implementation so the
//! conservatism guarantees ("a reused pid reads as alive → keep") cannot
//! drift between sweepers.

/// Liveness via `kill(pid, 0)` — identical semantics on Linux and macOS.
///
/// Signal `0` performs the existence + permission check without delivering a
/// signal: `0` → the process exists (alive); `ESRCH` → no such process (dead);
/// `EPERM` → the process exists but we may not signal it (still alive). A
/// reused pid reads as alive, so a caller keying cleanup off this check
/// conservatively keeps that pid's resources — a safe missed cleanup.
///
/// A pid that does not fit a positive `pid_t` (`0` or `> i32::MAX`) returns
/// `false`: `kill(2)` treats `0`/`-1`/negative pids as process-group or
/// broadcast targets, which would be the wrong question (and, for `-1`,
/// dangerous). **Callers driving deletion must not treat that `false` as
/// "proven dead"** — no real process ever has such a pid, so a name that
/// parses to one was not written by us and should be rejected *before* the
/// liveness check (see `parse_daemon_pid` in `kastellan-core`'s
/// `egress::scratch_sweep` for the pattern).
pub fn pid_is_alive(pid: u32) -> bool {
    let signed = pid as libc::pid_t;
    if signed <= 0 {
        return false;
    }
    // SAFETY: `kill(2)` with signal 0 and a positive `pid_t` has no memory
    // effects; it only inspects process-table state.
    let r = unsafe { libc::kill(signed, 0) };
    if r == 0 {
        return true;
    }
    // `-1`: alive iff the errno is EPERM (exists, not signalable); ESRCH → dead.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alive_for_self() {
        assert!(pid_is_alive(std::process::id()), "our own pid is alive");
    }

    #[test]
    fn not_alive_for_reserved_and_uncastable() {
        // 0 is the process-group sentinel; > i32::MAX cannot be a real pid.
        assert!(!pid_is_alive(0));
        assert!(!pid_is_alive(u32::MAX));
    }

    #[test]
    fn alive_for_pid_1_via_eperm_branch() {
        // pid 1 (init/launchd) always exists; we typically can't signal it,
        // so this exercises the EPERM → alive branch on real systems.
        assert!(pid_is_alive(1), "pid 1 exists on any Unix host");
    }
}
