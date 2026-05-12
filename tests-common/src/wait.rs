//! Poll-loop helpers for waiting on service state, listening sockets,
//! and log-file content.
//!
//! All three sleep 50 ms between polls — sub-second resolution without
//! burning the CPU. Timeouts return `Err(String)` so the caller can
//! `.expect("<context>")` with a useful message.

use std::path::Path;
use std::time::{Duration, Instant};

use hhagent_supervisor::{ServiceStatus, Supervisor};

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

/// Tail a log file until any line matches `predicate(line)` or
/// `timeout` elapses. On match, returns the matching line (without
/// trailing newline). On timeout, returns an error containing the
/// full file content captured so far for triage.
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
