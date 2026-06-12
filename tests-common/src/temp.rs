//! Per-test uniqueness helpers.
//!
//! `unique_suffix` and `unique_temp_root` are used to construct paths
//! that won't collide between concurrent test runs on the same host —
//! both pid (distinguishes parallel `cargo test` invocations) and
//! nanos (distinguishes successive runs within one process) feed in.
//!
//! `current_username` is the OS username, used to set up peer-auth
//! Postgres roles (the cluster bootstrap superuser is named after the
//! OS user so `psql -h <socket_dir> -U $USER` connects without a
//! password prompt).

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// Process-global call counter feeding [`unique_suffix`]; guarantees
/// distinctness even when two calls land on the same clock reading.
static SUFFIX_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `{pid}-{nanos_since_epoch}-{counter}`. Distinct on every call: pid
/// separates concurrent `cargo test` processes, nanos separates
/// successive runs of the same test binary, and the atomic counter
/// separates concurrent calls *within* one process — macOS's
/// `CLOCK_REALTIME` only ticks at ~microsecond resolution, so two
/// parallel test threads can read the identical nanos value (observed
/// 2026-06-13: two `python_exec_e2e` tests computed the same PG data
/// dir and destroyed each other's initdb).
pub fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = SUFFIX_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{}-{}", std::process::id(), nanos, n)
}

/// A fresh directory under a short-enough temp root.
///
/// **macOS uses `/tmp` unconditionally instead of `std::env::temp_dir()`** —
/// macOS's default `TMPDIR` (`/var/folders/<hash>/T/`, ~49 chars) leaves
/// only ~54 bytes for `<label>-<pid>-<nanos>-<n>/data/sockets/.s.PGSQL.5432`
/// before hitting the platform's `sockaddr_un.sun_path` 103-byte cap.
/// `<pid>` is 5-6 digits, `<nanos>` is 19 digits, `<n>` is 1-4 digits in
/// practice, the trailing literal is 27 chars — even a 1-char label
/// overflows. `/tmp` keeps everything safe and is the convention macOS
/// dev tools (Docker, Postgres.app, etc.) already use for
/// socket-sensitive work.
///
/// Linux uses `std::env::temp_dir()` (typically `/tmp` already; its
/// `sockaddr_un.sun_path` cap is 108 bytes — slightly more generous).
///
/// The label is the human-readable middle segment of the path (e.g.
/// `"disp-d"`); keep it short anyway as defense in depth — the resulting
/// socket path `<temp>/<label>-<pid>-<nanos>-<n>/data/sockets/.s.PGSQL.5432`
/// must still fit within the platform's `sockaddr_un.sun_path` cap.
pub fn unique_temp_root(label: &str) -> PathBuf {
    #[cfg(target_os = "macos")]
    let base = PathBuf::from("/tmp");
    #[cfg(not(target_os = "macos"))]
    let base = std::env::temp_dir();
    base.join(format!("kastellan-{}-{}", label, unique_suffix()))
}

/// OS username via `$USER` with a `whoami` fallback. Used to name the
/// Postgres bootstrap superuser; the role must match the OS uid for
/// peer auth to succeed.
///
/// The `is_empty()` guard handles the case where `$USER` is set but
/// empty (e.g. `env -u USER` inherited downstream).
pub fn current_username() -> String {
    if let Some(u) = std::env::var_os("USER") {
        let s = u.to_string_lossy().into_owned();
        if !s.is_empty() {
            return s;
        }
    }
    if let Ok(out) = Command::new("whoami").output() {
        if out.status.success() {
            return String::from_utf8_lossy(&out.stdout).trim().to_string();
        }
    }
    "kastellan".into()
}

#[cfg(test)]
mod tests {
    use super::unique_suffix;
    use std::collections::HashSet;

    /// Two concurrent (or rapid back-to-back) calls must never return the
    /// same suffix. The pid+nanos scheme alone collides on macOS, where
    /// `CLOCK_REALTIME` has only ~microsecond resolution — two parallel
    /// test threads bringing up PG clusters got the identical data dir
    /// and destroyed each other's initdb (observed 2026-06-13 in
    /// `python_exec_e2e`). The counter component makes uniqueness
    /// clock-independent within one process.
    #[test]
    fn unique_suffix_never_collides_within_one_process() {
        let handles: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(|| (0..1000).map(|_| unique_suffix()).collect::<Vec<_>>()))
            .collect();
        let mut seen = HashSet::new();
        for h in handles {
            for s in h.join().expect("suffix thread") {
                assert!(seen.insert(s.clone()), "duplicate suffix generated: {s}");
            }
        }
    }
}
