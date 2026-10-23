//! The `systemctl --user` invocations and the pure readers of their answers.
//!
//! Lifted out of `systemd_user.rs` when the #896 fix would have taken it
//! past the 500-LOC cap — the Linux twin of `launchd_agents/launchctl.rs`.
//! [`probe`] is re-exported by the parent; the rest is `pub(super)`.
//!
//! Every call goes through [`run_bounded`], so a wedged user manager fails
//! a lifecycle call after [`SYSTEMCTL_TIMEOUT`] instead of hanging it (#896).

use std::process::{Command, Output};
use std::time::Duration;

use crate::bounded_command::{run_capped, CappedOutcome};
use crate::{ServiceStatus, SupervisorError};

/// Wall-clock cap on every `systemctl --user` call.
///
/// `stop` waits for the unit to exit, and every unit we generate has
/// `TimeoutStopSec=10` before systemd SIGKILLs it; `start` of a
/// `Type=simple` unit returns once it is forked. 30 s is well above both,
/// so only a manager that has stopped answering reaches it.
pub(super) const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(30);

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
    let out = match run_bounded("systemctl", &["--user", "show-environment"])
        .map_err(|e| SupervisorError::Probe(e.to_string()))?
    {
        CappedOutcome::Completed(out) => out,
        CappedOutcome::TimedOut => {
            return Err(SupervisorError::Probe(timed_out_message(&["show-environment"])))
        }
    };
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
/// exits to [`SupervisorError::Backend`]
/// `systemctl --user <args> (exit N): <stderr>`; a timeout is `Backend`
/// too ([`timed_out_message`]); a spawn or read failure is `Io`.
pub(super) fn run_systemctl_user(args: &[&str]) -> Result<String, SupervisorError> {
    let out = run_user(args)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let exit = out
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());
        return Err(SupervisorError::Backend(format!(
            "systemctl --user {} (exit {exit}): {stderr}",
            args.join(" ")
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run `systemctl --user <args>` bounded, returning its output whatever the
/// exit code — for `is-active`, whose non-zero exits are answers. A timeout
/// is `Backend` ([`timed_out_message`]).
pub(super) fn run_user(args: &[&str]) -> Result<Output, SupervisorError> {
    let mut full = vec!["--user"];
    full.extend_from_slice(args);
    match run_bounded("systemctl", &full)? {
        CappedOutcome::Completed(out) => Ok(out),
        CappedOutcome::TimedOut => Err(SupervisorError::Backend(timed_out_message(args))),
    }
}

/// Run `program <args>` under [`SYSTEMCTL_TIMEOUT`]; a spawn or read failure
/// is `Io`. Takes the program name so a test can prove the `Io` arm without a
/// real `systemctl`.
pub(super) fn run_bounded(program: &str, args: &[&str]) -> Result<CappedOutcome, SupervisorError> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    run_capped(&mut cmd, SYSTEMCTL_TIMEOUT)
        .map_err(|e| SupervisorError::Io(format!("run {program}: {e}")))
}

/// The error text of a `systemctl --user <args>` call cut off at
/// [`SYSTEMCTL_TIMEOUT`]. One function so the tests read the real wording:
/// if it ever gained a benign phrase, a hung manager would read as "already
/// stopped".
pub(super) fn timed_out_message(args: &[&str]) -> String {
    format!(
        "systemctl --user {} timed out after {}s (the per-user systemd manager \
         is not answering)",
        args.join(" "),
        SYSTEMCTL_TIMEOUT.as_secs()
    )
}

/// Does `msg` carry systemd's answer to `stop` for a unit it has not loaded
/// (`Unit x.service not loaded.`, exit 5 — measured)? Nothing is running.
pub(super) fn is_not_loaded_answer(msg: &str) -> bool {
    msg.contains(" not loaded.")
}

/// Does `msg` carry systemd's answer to `disable` once the unit file is gone
/// (`Unit file x.service does not exist.`, exit 1 — measured)? No boot link
/// can point at a file that is not there.
pub(super) fn is_no_unit_file_answer(msg: &str) -> bool {
    msg.contains("Unit file ") && msg.contains(" does not exist.")
}

/// Read `systemctl --user stop`'s result: `Ok` iff nothing is left running —
/// it stopped, or systemd says the unit is not loaded.
///
/// Anything else (no user bus, a timeout, a refusal) leaves the unit
/// possibly running and is returned as the error it is (#896).
pub(super) fn stop_outcome(stop: Result<String, SupervisorError>) -> Result<(), SupervisorError> {
    match stop {
        Ok(_) => Ok(()),
        Err(SupervisorError::Backend(msg)) if is_not_loaded_answer(&msg) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Read `systemctl --user disable`'s result: `Ok` iff no boot link is left —
/// it was removed, or there is no unit file to resolve one from.
///
/// A failed disable keeps the `default.target.wants/` link, which `disable`
/// can only find again while the unit file is still on disk.
pub(super) fn disable_outcome(
    disable: Result<String, SupervisorError>,
) -> Result<(), SupervisorError> {
    match disable {
        Ok(_) => Ok(()),
        Err(SupervisorError::Backend(msg)) if is_no_unit_file_answer(&msg) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Stop `unit`, then drop its boot link: `Ok` only when neither left anything
/// behind ([`stop_outcome`], [`disable_outcome`]).
pub(super) fn stop_and_disable(unit: &str) -> Result<(), SupervisorError> {
    stop_outcome(run_systemctl_user(&["stop", unit]))?;
    disable_outcome(run_systemctl_user(&["disable", unit]))
}

/// Read `systemctl --user is-active <unit>`'s answer for a unit whose file is
/// on disk. Its exit code is not read: 3 and 4 both say `inactive`.
///
/// `active` → `Active`, `failed` → `Failed`, any other word → `Inactive` —
/// deliberately not a closed list (the launchd twin's missed `xpcproxy`,
/// #898). No word at all is a failed read (the user bus unreachable prints
/// its reason on stderr and nothing on stdout), never a confident
/// `Inactive` (#896).
pub(super) fn status_from_is_active(
    unit: &str,
    stdout: &str,
    stderr: &str,
) -> Result<ServiceStatus, SupervisorError> {
    match stdout.trim() {
        "active" => Ok(ServiceStatus::Active),
        "failed" => Ok(ServiceStatus::Failed),
        "" => Err(SupervisorError::Backend(format!(
            "systemctl --user is-active {unit} answered no state, so its state is unknown: {}",
            stderr.trim()
        ))),
        _ => Ok(ServiceStatus::Inactive),
    }
}

#[cfg(test)]
pub(super) mod tests;
