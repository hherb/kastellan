//! The `systemctl --user` invocations.
//!
//! Lifted out of `systemd_user.rs` when the #896 fix would have taken it
//! past the 500-LOC cap — the Linux twin of `launchd_agents/launchctl.rs`.
//! [`probe`] is re-exported by the parent; the rest is `pub(super)`.

use std::process::{Command, Stdio};

use crate::SupervisorError;

/// Probe whether `systemctl --user` can talk to a live user manager.
///
/// Mirrors `sandbox::linux_cgroup::cgroup_probe`: succeed silently or
/// return a structured error with a hint pointing at the most common
/// recovery (`loginctl enable-linger $USER` for headless sessions).
///
/// Used by callers that want fail-closed behaviour at startup — if
/// the supervisor cannot reach the user manager, every lifecycle
/// call would fail anyway, so failing once up front is friendlier.
pub fn probe() -> Result<(), SupervisorError> {
    let out = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| SupervisorError::Probe(format!("spawn systemctl: {e}")))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let hint = if stderr.contains("Failed to connect") || stderr.contains("No such file") {
        "\n\nThe per-user systemd manager does not appear to be running. \
         On a normal desktop session it starts automatically; on headless \
         hosts run `loginctl enable-linger $USER` and re-login."
    } else {
        ""
    };
    Err(SupervisorError::Probe(format!(
        "systemctl --user show-environment failed: {}{hint}",
        stderr.trim()
    )))
}

/// Run `systemctl --user <args>` with stdio captured. Maps non-zero
/// exits to [`SupervisorError::Backend`] with the trimmed stderr.
pub(super) fn run_systemctl_user(args: &[&str]) -> Result<String, SupervisorError> {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| SupervisorError::Io(format!("spawn systemctl: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(SupervisorError::Backend(format!(
            "systemctl --user {}: {stderr}",
            args.join(" ")
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
