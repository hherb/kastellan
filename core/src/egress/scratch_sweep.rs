//! Best-effort startup sweep of orphaned per-worker egress scratch dirs (#251).
//!
//! Force-routing gives each `Net::Allowlist` worker a per-worker scratch subdir
//! under `scratch_root` to hold its sidecar UDS: `egress-<pid>-<seq>` for
//! tool workers (see [`super::net_worker`]) and `matrix-<pid>-<seq>` for the
//! Matrix channel worker (see `crate::channel::matrix`). Steady-state cleanup
//! is RAII via `EgressSidecar::drop`. The one case RAII cannot cover is a
//! daemon **crash / SIGKILL**: `Drop` never runs, so the dir leaks. Because the
//! name embeds the *creating daemon's* pid, the next daemon (a different pid)
//! never reclaims it — it only gets cleared by whatever reaps the OS temp dir,
//! which for a non-temp `KASTELLAN_EGRESS_SCRATCH_DIR` override is nothing.
//!
//! This module reclaims those husks at startup. It is a **leak fix, never a
//! safety mechanism** — egress is gated by the OS netns/Seatbelt barrier, not by
//! scratch hygiene. The sweep is conservative: it removes a dir only when its
//! name round-trips through our own producer grammar **and** the embedded pid
//! is neither our own nor a live process, so it is safe to run concurrently
//! with another daemon that legitimately owns its own pid's dirs (issue #251's
//! guard).
//!
//! Known residual (accepted tradeoff): the sweep runs behind the
//! force-routing gate in `main.rs`, because `scratch_root` is only resolved
//! when force-routing is enabled. Husks under a persistent
//! `KASTELLAN_EGRESS_SCRATCH_DIR` override therefore survive any boots where
//! the operator has turned `KASTELLAN_EGRESS_FORCE_ROUTING` off, and are
//! reclaimed the next time it is on. No *new* dirs accumulate while it is off.
//! The macOS `pyexec-<pid>-<seq>` python-exec scratch dirs
//! (`crate::tool_host::scratch`) are a separate sweep tracked in #251's
//! follow-up.

use std::path::Path;

pub use kastellan_sandbox::pid::pid_is_alive;

/// Name prefix of every per-worker *egress* scratch dir. Kept in sync with
/// `make_worker_scratch_dir` in [`super::net_worker`], which formats
/// `"{EGRESS_SCRATCH_DIR_PREFIX}{pid}-{seq}"`.
pub(crate) const EGRESS_SCRATCH_DIR_PREFIX: &str = "egress-";

/// Name prefix of the Matrix channel worker's per-spawn scratch dir. Kept in
/// sync with the producer in `crate::channel::matrix`, which formats
/// `"{MATRIX_SCRATCH_DIR_PREFIX}{pid}-{seq}"` under the same `scratch_root`.
pub(crate) const MATRIX_SCRATCH_DIR_PREFIX: &str = "matrix-";

/// Name prefix of the per-worker embed-broker sidecar scratch dir (Slice B).
/// Kept in sync with `BrokerKind::Embed.scratch_prefix()` (the producer in
/// `crate::broker::spawn`), which formats `"{EMBED_SCRATCH_DIR_PREFIX}{pid}-{seq}"`
/// under the same `scratch_root` to hold the broker's `embed.sock`. Same
/// crash-leak class as the egress sidecar.
pub(crate) const EMBED_SCRATCH_DIR_PREFIX: &str = "embed-";

/// Name prefix of the per-worker search-broker sidecar scratch dir. Kept in sync
/// with `BrokerKind::Search.scratch_prefix()`; holds the broker's `search.sock`.
pub(crate) const SEARCH_SCRATCH_DIR_PREFIX: &str = "search-";

/// Every per-worker scratch-dir prefix the startup sweep reclaims. Add a new
/// producer's prefix here (and a round-trip test below) when a new worker
/// family gets its own scratch dirs under `scratch_root`.
pub(crate) const SCRATCH_DIR_PREFIXES: &[&str] = &[
    EGRESS_SCRATCH_DIR_PREFIX,
    MATRIX_SCRATCH_DIR_PREFIX,
    EMBED_SCRATCH_DIR_PREFIX,
    SEARCH_SCRATCH_DIR_PREFIX,
];

/// Parse the creating-daemon pid out of a `<prefix><pid>-<seq>` dir name.
///
/// Returns `None` for anything that is not **exactly** our producers' shape:
/// no known prefix, a pid field that is not all ASCII digits, a pid of `0` or
/// one too large for a positive `pid_t` (no real process can have it — such a
/// name was not written by us), a missing `-<seq>` tail, or a seq that is not
/// all ASCII digits parsing as `u64` (producers emit an `AtomicU64` counter).
/// The strictness is load-bearing: a parsed name is what authorizes
/// `remove_dir_all` in the sweep, and the default `scratch_root` is the shared
/// OS temp dir — anything not provably ours must be left alone. Pure.
pub fn parse_daemon_pid(name: &str) -> Option<u32> {
    let rest = SCRATCH_DIR_PREFIXES.iter().find_map(|p| name.strip_prefix(p))?;
    let (pid_str, seq_str) = rest.split_once('-')?;
    // `str::parse` alone would admit a leading `+`; producers only ever emit
    // plain digits, so require exactly that on both fields.
    if pid_str.is_empty() || !pid_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if seq_str.is_empty() || !seq_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    seq_str.parse::<u64>().ok()?;
    let pid = pid_str.parse::<u32>().ok()?;
    // A pid outside 1..=i32::MAX cannot fit a positive `pid_t`, so no process
    // ever had it and no kastellan daemon ever wrote it. Rejecting here keeps
    // the dir (conservative); passing it on would let `pid_is_alive`'s
    // "uncastable → false" answer be misread as "proven dead → remove".
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    Some(pid)
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

/// I/O: scan `scratch_root` for orphaned per-worker scratch dirs (any prefix
/// in [`SCRATCH_DIR_PREFIXES`]) and remove them (see
/// [`orphaned_scratch_should_remove`]). Best-effort throughout: an unreadable
/// root, an unreadable entry, or a failed removal is skipped, never
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
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue, // non-UTF-8 name can't be one of ours.
        };
        // Full name check first (pure, no stat) so the common case of
        // unrelated entries sharing the temp dir costs no syscall.
        if !orphaned_scratch_should_remove(&name, our_pid, &alive) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() && std::fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn parses_pid_from_well_formed_name() {
        assert_eq!(parse_daemon_pid("egress-12345-0"), Some(12345));
        assert_eq!(parse_daemon_pid("egress-1-9999"), Some(1));
        assert_eq!(parse_daemon_pid("matrix-12345-0"), Some(12345));
    }

    /// Both broker kinds' scratch dirs are swept — an `embed-<pid>-<seq>` and a
    /// `search-<pid>-<seq>` husk each round-trip to their creating-daemon pid.
    #[test]
    fn parses_pid_from_broker_scratch_names() {
        assert_eq!(parse_daemon_pid("embed-12345-0"), Some(12345));
        assert_eq!(parse_daemon_pid("search-12345-0"), Some(12345));
    }

    /// The parser must accept exactly what the producers emit — pin the
    /// round-trip against the same `"{prefix}{pid}-{seq}"` grammar
    /// `make_worker_scratch_dir` and the matrix spawn use.
    #[test]
    fn parser_round_trips_producer_grammar() {
        for prefix in SCRATCH_DIR_PREFIXES {
            for (pid, seq) in [(1u32, 0u64), (4242, 17), (i32::MAX as u32, u64::MAX)] {
                let name = format!("{prefix}{pid}-{seq}");
                assert_eq!(parse_daemon_pid(&name), Some(pid), "round-trip failed for {name}");
            }
        }
    }

    #[test]
    fn rejects_malformed_names() {
        assert_eq!(parse_daemon_pid("egress-abc-0"), None); // non-numeric pid
        assert_eq!(parse_daemon_pid("egress-123"), None); // no -<seq> tail
        assert_eq!(parse_daemon_pid("egress-123-"), None); // empty seq
        assert_eq!(parse_daemon_pid("other-123-0"), None); // wrong prefix
        assert_eq!(parse_daemon_pid("egress--0"), None); // empty pid
    }

    /// A numeric-pid name with a non-numeric tail is NOT ours — the producers
    /// always emit a numeric `AtomicU64` seq. Treating it as ours would
    /// `remove_dir_all` a third party's data in the shared temp dir.
    #[test]
    fn rejects_non_numeric_seq_tail() {
        assert_eq!(parse_daemon_pid("egress-2024-backup"), None);
        assert_eq!(parse_daemon_pid("egress-2024-07-04"), None); // date-shaped
        assert_eq!(parse_daemon_pid("egress-123-0x1f"), None);
        assert_eq!(parse_daemon_pid("matrix-123-cache"), None);
        assert_eq!(parse_daemon_pid("egress-123-+1"), None); // parse::<u64> would take "+1"
    }

    /// A pid that can't fit a positive `pid_t` was never a real process, so
    /// the name was not written by us → keep, never remove. (Previously such
    /// names fell through to `pid_is_alive`, whose `false` was misread as
    /// "proven dead" and the dir was deleted.)
    #[test]
    fn rejects_pid_outside_pid_t_range() {
        assert_eq!(parse_daemon_pid("egress-0-0"), None);
        assert_eq!(parse_daemon_pid(&format!("egress-{}-0", u32::MAX)), None);
        assert_eq!(parse_daemon_pid(&format!("egress-{}-0", i32::MAX as u32 + 1)), None);
        assert_eq!(parse_daemon_pid("egress-+123-0"), None); // parse::<u32> would take "+123"
        // Boundary: i32::MAX itself is a valid pid_t value.
        assert_eq!(parse_daemon_pid(&format!("egress-{}-0", i32::MAX)), Some(i32::MAX as u32));
    }

    #[test]
    fn removes_dead_foreign_pid() {
        assert!(orphaned_scratch_should_remove("egress-100-0", 999, |_| false));
        assert!(orphaned_scratch_should_remove("matrix-100-0", 999, |_| false));
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
        // Uncastable pid: `alive` says dead, but the name is not ours → keep.
        assert!(!orphaned_scratch_should_remove(
            &format!("egress-{}-0", u32::MAX),
            1,
            |_| false
        ));
    }

    fn make_scratch_dir(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sweep_removes_dead_foreign_keeps_live_self_and_unrelated() {
        let root = tempfile::tempdir().unwrap();
        let dead = make_scratch_dir(root.path(), "egress-100-0"); // foreign, dead
        let dead_matrix = make_scratch_dir(root.path(), "matrix-100-1"); // foreign, dead
        let live = make_scratch_dir(root.path(), "egress-200-0"); // foreign, live
        let ours = make_scratch_dir(root.path(), "egress-300-0"); // our own pid
        let foreign_shape = make_scratch_dir(root.path(), "egress-100-stuff"); // not our grammar
        let unrelated = make_scratch_dir(root.path(), "some-other-dir");

        // our pid = 300; only pid 200 is "alive".
        let removed = sweep_orphaned_scratch_dirs(root.path(), 300, |p| p == 200);

        assert_eq!(removed, 2, "exactly the dead foreign egress+matrix dirs are removed");
        assert!(!dead.exists(), "dead foreign egress scratch dir removed");
        assert!(!dead_matrix.exists(), "dead foreign matrix scratch dir removed");
        assert!(live.exists(), "live foreign scratch dir kept");
        assert!(ours.exists(), "our own scratch dir kept");
        assert!(foreign_shape.exists(), "non-grammar name untouched");
        assert!(unrelated.exists(), "non-matching dir untouched");
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
