//! Best-effort startup sweep of orphaned per-worker egress scratch dirs (#251).
//!
//! Force-routing gives each `Net::Allowlist` worker a per-worker scratch subdir
//! `egress-<pid>-<seq>` under `scratch_root` (see
//! [`super::net_worker`]) to hold its sidecar UDS. Steady-state cleanup is RAII
//! via `EgressSidecar::drop`. The one case RAII cannot cover is a daemon
//! **crash / SIGKILL**: `Drop` never runs, so the dir leaks. Because the name
//! embeds the *creating daemon's* pid, the next daemon (a different pid) never
//! reclaims it — it only gets cleared by whatever reaps the OS temp dir, which
//! for a non-temp `KASTELLAN_EGRESS_SCRATCH_DIR` override is nothing.
//!
//! This module reclaims those husks at startup. It is a **leak fix, never a
//! safety mechanism** — egress is gated by the OS netns/Seatbelt barrier, not by
//! scratch hygiene. The sweep is conservative: it removes a dir only when it can
//! prove the pid that owns it is neither our own nor a live process, so it is
//! safe to run concurrently with another daemon that legitimately owns its own
//! pid's dirs (issue #251's guard).

use std::path::Path;

/// Name prefix of every per-worker scratch dir. Kept in sync with
/// `make_worker_scratch_dir` in [`super::net_worker`], which formats
/// `"{SCRATCH_DIR_PREFIX}{pid}-{seq}"`.
pub(crate) const SCRATCH_DIR_PREFIX: &str = "egress-";

/// Parse the creating-daemon pid out of an `egress-<pid>-<seq>` dir name.
///
/// Returns `None` for anything that is not our exact shape — no prefix, a
/// non-numeric pid field, or a missing `-<seq>` tail (so a bare `egress-123`
/// that isn't one of ours is left alone). Pure.
pub fn parse_daemon_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix(SCRATCH_DIR_PREFIX)?;
    let (pid_str, seq) = rest.split_once('-')?;
    if seq.is_empty() {
        return None;
    }
    pid_str.parse::<u32>().ok()
}

/// Pure decision: should the startup sweep remove a scratch dir named `name`,
/// given our own pid and a pid-liveness predicate?
///
/// Removes only when the name parses to a pid that is **both** not our own pid
/// **and** reported dead by `alive`. Every other case is kept:
/// - an unparseable name → keep (not one of ours, or malformed);
/// - our own pid → keep (a dir this daemon legitimately owns right now);
/// - a live foreign pid → keep (a concurrent daemon owns it).
///
/// The conservatism is deliberate: a false negative is a missed cleanup (caught
/// on the next startup); a false positive would delete a running daemon's live
/// sidecar dir, which this rules out.
pub fn orphaned_scratch_should_remove(
    name: &str,
    our_pid: u32,
    alive: impl Fn(u32) -> bool,
) -> bool {
    match parse_daemon_pid(name) {
        Some(pid) if pid != our_pid => !alive(pid),
        _ => false,
    }
}

/// I/O: scan `scratch_root` for orphaned `egress-<pid>-<seq>` dirs and remove
/// them (see [`orphaned_scratch_should_remove`]). Best-effort throughout: an
/// unreadable root, an unreadable entry, or a failed removal is skipped, never
/// propagated. Returns the number of dirs actually removed.
pub fn sweep_orphaned_scratch_dirs(
    scratch_root: &Path,
    our_pid: u32,
    alive: impl Fn(u32) -> bool,
) -> usize {
    let entries = match std::fs::read_dir(scratch_root) {
        Ok(e) => e,
        Err(_) => return 0, // root unreadable/absent → nothing to do.
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        // Cheap name check first (no stat) for the common case of unrelated
        // entries sharing the temp dir.
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue, // non-UTF-8 name can't be one of ours.
        };
        if !name.starts_with(SCRATCH_DIR_PREFIX) {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if orphaned_scratch_should_remove(&name, our_pid, &alive)
            && std::fs::remove_dir_all(&path).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

/// Cross-platform pid-liveness check via `kill(pid, 0)` (Linux + macOS).
///
/// Signal `0` performs the existence + permission check without delivering a
/// signal: `0` → the process exists (alive); `ESRCH` → no such process (dead);
/// `EPERM` → the process exists but we may not signal it (still alive). A reused
/// pid reads as alive, so its dir is conservatively kept — a safe missed
/// cleanup.
pub fn pid_is_alive(pid: u32) -> bool {
    // Never pass 0 or a value that casts to a non-positive `pid_t`: `kill(2)`
    // treats `0`/`-1`/negative pids as process-group or broadcast targets, which
    // would both be the wrong question and (for -1) dangerous. `std::process::id`
    // is always a positive pid, so a rejected value here is only ever a
    // malformed/hostile dir name → treat as "not alive" (keep-vs-remove is then
    // decided by the `!= our_pid` guard in the caller).
    let signed = pid as libc::pid_t;
    if signed <= 0 {
        return false;
    }
    // SAFETY: `kill(2)` with signal 0 and a positive `pid_t` has no memory
    // effects; it only inspects process-table state. Mirrors the existing
    // `libc::kill` use in `tool_host::watchdog::send_sigkill`.
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
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn parses_pid_from_well_formed_name() {
        assert_eq!(parse_daemon_pid("egress-12345-0"), Some(12345));
        assert_eq!(parse_daemon_pid("egress-1-9999"), Some(1));
    }

    #[test]
    fn rejects_malformed_names() {
        assert_eq!(parse_daemon_pid("egress-abc-0"), None); // non-numeric pid
        assert_eq!(parse_daemon_pid("egress-123"), None); // no -<seq> tail
        assert_eq!(parse_daemon_pid("egress-123-"), None); // empty seq
        assert_eq!(parse_daemon_pid("other-123-0"), None); // wrong prefix
        assert_eq!(parse_daemon_pid("egress--0"), None); // empty pid
    }

    #[test]
    fn removes_dead_foreign_pid() {
        assert!(orphaned_scratch_should_remove("egress-100-0", 999, |_| false));
    }

    #[test]
    fn keeps_live_foreign_pid() {
        assert!(!orphaned_scratch_should_remove("egress-100-0", 999, |_| true));
    }

    #[test]
    fn keeps_our_own_pid_even_if_alive_says_dead() {
        // Our own dirs are in use right now; never sweep them regardless of the
        // liveness predicate.
        assert!(!orphaned_scratch_should_remove("egress-777-0", 777, |_| false));
    }

    #[test]
    fn keeps_unparseable_name() {
        assert!(!orphaned_scratch_should_remove("egress-nope", 1, |_| false));
        assert!(!orphaned_scratch_should_remove("unrelated", 1, |_| false));
    }

    // Unique temp root per test so parallel runs don't collide.
    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);
    fn fresh_temp_root() -> PathBuf {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join(format!("kastellan-egress-sweeptest-{}-{}", std::process::id(), seq));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn make_scratch_dir(root: &Path, suffix: &str) -> PathBuf {
        let dir = root.join(format!("{SCRATCH_DIR_PREFIX}{suffix}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sweep_removes_dead_foreign_keeps_live_self_and_unrelated() {
        let root = fresh_temp_root();
        let dead = make_scratch_dir(&root, "100-0"); // foreign, dead
        let live = make_scratch_dir(&root, "200-0"); // foreign, live
        let ours = make_scratch_dir(&root, "300-0"); // our own pid
        let unrelated = root.join("some-other-dir");
        fs::create_dir_all(&unrelated).unwrap();

        // our pid = 300; only pid 200 is "alive".
        let removed = sweep_orphaned_scratch_dirs(&root, 300, |p| p == 200);

        assert_eq!(removed, 1, "exactly the dead foreign dir is removed");
        assert!(!dead.exists(), "dead foreign scratch dir removed");
        assert!(live.exists(), "live foreign scratch dir kept");
        assert!(ours.exists(), "our own scratch dir kept");
        assert!(unrelated.exists(), "non-matching dir untouched");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn sweep_on_missing_root_returns_zero() {
        let missing =
            std::env::temp_dir().join("kastellan-egress-sweeptest-does-not-exist-xyz");
        assert_eq!(sweep_orphaned_scratch_dirs(&missing, 1, |_| false), 0);
    }

    #[test]
    fn pid_is_alive_true_for_self_false_for_reserved() {
        assert!(pid_is_alive(std::process::id()), "our own pid is alive");
        // 0 is the process-group/broadcast sentinel, guarded to "not alive".
        assert!(!pid_is_alive(0));
    }
}
