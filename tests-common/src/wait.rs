//! Poll-loop helpers for waiting on service state, listening sockets,
//! and log-file content.
//!
//! All three sleep 50 ms between polls — sub-second resolution without
//! burning the CPU. Timeouts return `Err(String)` so the caller can
//! `.expect("<context>")` with a useful message.

use std::path::Path;
use std::time::{Duration, Instant};

use kastellan_supervisor::{ServiceStatus, Supervisor};

/// Block until `predicate(status)` returns `true` or `timeout`
/// elapses. The supervisor is polled every 50 ms via `status(name)`;
/// a `status` error short-circuits with `Err`.
pub fn wait_for_status<F: Fn(ServiceStatus) -> bool>(
    sup: &dyn Supervisor,
    name: &str,
    predicate: F,
    timeout: Duration,
) -> Result<ServiceStatus, String> {
    let start = Instant::now();
    let mut last = sup.status(name).map_err(|e| format!("status: {e}"))?;
    loop {
        if predicate(last) {
            return Ok(last);
        }
        if start.elapsed() > timeout {
            return Err(format!(
                "timed out after {:?} waiting for status; last={:?}",
                timeout, last
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
        last = sup.status(name).map_err(|e| format!("status: {e}"))?;
    }
}

/// Block until Postgres creates `<socket_dir>/.s.PGSQL.5432` (its
/// "ready to accept connections" signal) or `timeout` elapses.
///
/// This is more reliable than `psql` retry loops because it doesn't
/// require a successful UDS connect to detect "not ready yet".
/// Postgres creates the socket file only after `pg_ctl` has finished
/// initialising — racing it produces flaky "could not connect" errors
/// that obscure the real failure.
pub fn wait_for_socket(socket_dir: &Path, timeout: Duration) -> Result<(), String> {
    let target = socket_dir.join(".s.PGSQL.5432");
    let start = Instant::now();
    loop {
        if target.exists() {
            return Ok(());
        }
        if start.elapsed() > timeout {
            return Err(format!(
                "timed out after {:?} waiting for {} to appear",
                timeout,
                target.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Tail a log file until any **single line** matches `predicate(line)`
/// or `timeout` elapses. On match, returns the matching line (without
/// trailing newline). On timeout, returns an error containing the
/// full file content captured so far for triage.
///
/// # Predicate contract
///
/// The predicate is invoked once **per line**, not once per file
/// body. Use this for substring/regex checks that fit within a single
/// log line (`s.contains("scheduler spawned")`). If you need a
/// body-spanning match (e.g. line A *and* line B both present in any
/// order), read the file yourself and poll on the combined condition
/// — this helper deliberately does not give you the whole-body view,
/// because every current caller is a single-line check and the
/// per-line contract gives a tighter, less-ambiguous return value
/// (the matching line, not the entire body up to the match).
///
/// The file is opened fresh on every poll — handles the case where
/// the writer rotates or truncates between polls.
pub fn wait_for_log_match<F: Fn(&str) -> bool>(
    path: &Path,
    predicate: F,
    timeout: Duration,
) -> Result<String, String> {
    let start = Instant::now();
    loop {
        if let Ok(contents) = std::fs::read_to_string(path) {
            for line in contents.lines() {
                if predicate(line) {
                    return Ok(line.to_string());
                }
            }
            if start.elapsed() > timeout {
                return Err(format!(
                    "timed out after {:?} waiting for log match in {}; \
                     full content was:\n{}",
                    timeout,
                    path.display(),
                    contents
                ));
            }
        } else if start.elapsed() > timeout {
            return Err(format!(
                "timed out after {:?}; log file {} never appeared",
                timeout,
                path.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::wait_for_log_match;
    use std::time::Duration;

    fn temp_log(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "kastellan-wait-selftest-{}-{}.log",
            tag,
            crate::temp::unique_suffix()
        ))
    }

    /// A file that never appears must time out with the dedicated
    /// "never appeared" message, not hang or panic.
    #[test]
    fn errors_when_file_never_appears() {
        let path = temp_log("absent");
        let err = wait_for_log_match(&path, |_| true, Duration::from_millis(80)).unwrap_err();
        assert!(err.contains("never appeared"), "got: {err}");
    }

    /// A present file with no matching line times out and surfaces the
    /// captured body for triage.
    #[test]
    fn errors_with_body_when_no_line_matches() {
        let path = temp_log("nomatch");
        std::fs::write(&path, "line one\nline two\n").unwrap();
        let err = wait_for_log_match(&path, |l| l.contains("absent-marker"), Duration::from_millis(80))
            .unwrap_err();
        let _ = std::fs::remove_file(&path);
        assert!(err.contains("full content was"), "got: {err}");
        assert!(err.contains("line one"), "error should echo the body, got: {err}");
    }

    /// The happy path returns the matching line (newline stripped).
    #[test]
    fn returns_the_matching_line() {
        let path = temp_log("match");
        std::fs::write(&path, "noise\nthe MARKER line\nmore\n").unwrap();
        let got = wait_for_log_match(&path, |l| l.contains("MARKER"), Duration::from_secs(1)).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(got, "the MARKER line");
    }
}
