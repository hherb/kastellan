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

/// `{pid}-{nanos_since_epoch}`. Stable for the lifetime of one test
/// process; distinct between concurrent processes.
pub fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}", std::process::id(), nanos)
}

/// A fresh directory under a short-enough temp root.
///
/// **macOS uses `/tmp` unconditionally instead of `std::env::temp_dir()`** —
/// macOS's default `TMPDIR` (`/var/folders/<hash>/T/`, ~49 chars) leaves
/// only ~54 bytes for `<label>-<pid>-<nanos>/data/sockets/.s.PGSQL.5432`
/// before hitting the platform's `sockaddr_un.sun_path` 103-byte cap.
/// `<pid>` is 5-6 digits, `<nanos>` is 19 digits, the trailing literal is
/// 27 chars — even a 1-char label overflows. `/tmp` keeps everything safe
/// and is the convention macOS dev tools (Docker, Postgres.app, etc.)
/// already use for socket-sensitive work.
///
/// Linux uses `std::env::temp_dir()` (typically `/tmp` already; its
/// `sockaddr_un.sun_path` cap is 108 bytes — slightly more generous).
///
/// The label is the human-readable middle segment of the path (e.g.
/// `"disp-d"`); keep it short anyway as defense in depth — the resulting
/// socket path `<temp>/<label>-<pid>-<nanos>/data/sockets/.s.PGSQL.5432`
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
