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

/// A fresh directory under `std::env::temp_dir()`. The label is the
/// human-readable middle segment of the path (e.g. `"disp-d"`); keep
/// it short, because the resulting socket path
/// `<temp>/<label>-<pid>-<nanos>/data/sockets/.s.PGSQL.5432` must fit
/// in `sockaddr_un.sun_path` (108 bytes on Linux).
pub fn unique_temp_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hhagent-{}-{}", label, unique_suffix()))
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
    "hhagent".into()
}
