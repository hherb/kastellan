//! The `systemctl --user` invocations and the pure readers of their answers.
//!
//! Lifted out of `systemd_user.rs` when the #896 fix would have taken it
//! past the 500-LOC cap — the Linux twin of `launchd_agents/launchctl.rs`.
//! [`probe`] is re-exported by the parent; the rest is `pub(super)`.
//!
//! Every call goes through [`run_bounded`], so a wedged user manager fails
//! a lifecycle call after [`SYSTEMCTL_TIMEOUT`] instead of hanging it (#896).

use std::process::{Command, ExitStatus, Output};
use std::time::Duration;

use crate::bounded_command::{run_capped, CappedOutcome};
use crate::{ServiceStatus, SupervisorError};

/// Wall-clock cap on every `systemctl --user` call.
///
/// `stop` waits for the unit to exit. Every unit we generate has
/// `TimeoutStopSec=10`, which systemd spends once before SIGKILL and again
/// after it, so one unit can take 20 s — and `stop kastellan.target` stops
/// its two members in sequence (core `After=` postgres): 40 s. `start` of a
/// `Type=simple` unit returns once it is forked. 60 s clears the slowest
/// healthy stop, so a refusal here is a manager that is not answering, not
/// a stop still in progress (a false refusal fails `run_install`, #870).
pub(super) const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(60);

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
    let out = match run_bounded("systemctl", &["--user", "show-environment"]).map_err(|e| {
        SupervisorError::Probe(match e {
            SupervisorError::Io(m) => m,
            other => other.to_string(),
        })
    })? {
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

/// Run `systemctl --user <args>` with stdio captured. A non-zero exit is
/// [`SupervisorError::Backend`] worded
/// `systemctl --user <args> (exit N|signal): <stderr>`; a timeout is
/// `Backend` too ([`timed_out_message`]); a spawn or read failure is `Io`.
pub(super) fn run_systemctl_user(args: &[&str]) -> Result<String, SupervisorError> {
    let out = run_user(args)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(SupervisorError::Backend(format!(
            "systemctl --user {} (exit {}): {stderr}",
            args.join(" "),
            exit_word(&out.status)
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

/// An exit status as error text: its code, or `signal` when it has none.
fn exit_word(status: &ExitStatus) -> String {
    status.code().map_or_else(|| "signal".to_string(), |c| c.to_string())
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
        "systemctl --user {} did not finish within {}s (the per-user systemd \
         manager is not answering, or the call is still in progress)",
        args.join(" "),
        SYSTEMCTL_TIMEOUT.as_secs()
    )
}

/// Does `msg` carry systemd's answer to `stop` for `unit` when it is not
/// loaded (`Unit x.service not loaded.`, exit 5 — measured)? Nothing is
/// running. Anchored on `unit`, so an answer about another unit is not it.
pub(super) fn is_not_loaded_answer(msg: &str, unit: &str) -> bool {
    msg.contains(&format!("Unit {unit} not loaded."))
}

/// Does `msg` carry systemd's answer to `disable` for `unit` once its file
/// is gone (`Unit file x.service does not exist.`, exit 1 — measured)? With
/// no file `disable` has nothing to resolve links from, so a retry cannot
/// remove more; in the normal flow the link went with the earlier, clean
/// disable, and one left by an out-of-band delete dangles and starts nothing.
pub(super) fn is_no_unit_file_answer(msg: &str, unit: &str) -> bool {
    msg.contains(&format!("Unit file {unit} does not exist."))
}

/// Read `systemctl --user stop <unit>`'s result: `Ok` iff nothing is left
/// running — it stopped, or systemd says the unit is not loaded.
///
/// Anything else (no user bus, a timeout, a refusal) leaves the unit
/// possibly running and is returned as the error it is (#896).
pub(super) fn stop_outcome(
    unit: &str,
    stop: Result<String, SupervisorError>,
) -> Result<(), SupervisorError> {
    match stop {
        Ok(_) => Ok(()),
        Err(SupervisorError::Backend(msg)) if is_not_loaded_answer(&msg, unit) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Read `systemctl --user disable <unit>`'s result: `Ok` iff no boot link
/// is left — it was removed, or there is no unit file to resolve one from.
///
/// A failed disable keeps the `default.target.wants/` link, which `disable`
/// can only find again while the unit file is still on disk.
pub(super) fn disable_outcome(
    unit: &str,
    disable: Result<String, SupervisorError>,
) -> Result<(), SupervisorError> {
    match disable {
        Ok(_) => Ok(()),
        Err(SupervisorError::Backend(msg)) if is_no_unit_file_answer(&msg, unit) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Stop `unit`; only if nothing is left running, drop its boot link. `Ok`
/// only when neither left anything behind ([`stop_outcome`],
/// [`disable_outcome`]).
pub(super) fn stop_and_disable(unit: &str) -> Result<(), SupervisorError> {
    stop_and_disable_with(unit, run_systemctl_user)
}

/// [`stop_and_disable`] with the `systemctl --user` runner supplied, so a
/// test can feed it systemd's answers.
pub(super) fn stop_and_disable_with(
    unit: &str,
    mut systemctl: impl FnMut(&[&str]) -> Result<String, SupervisorError>,
) -> Result<(), SupervisorError> {
    stop_outcome(unit, systemctl(&["stop", unit]))?;
    disable_outcome(unit, systemctl(&["disable", unit]))
}

/// Read `systemctl --user is-active <unit>`'s answer for a unit whose file is
/// on disk into a status plus the word itself, for diagnostics (#900). Its
/// exit code is not read: 3 and 4 both say `inactive`.
///
/// `active`/`reloading`/`refreshing` (up and serving) → `Active`,
/// `inactive`/`maintenance` (the service's process is gone) → `Inactive`,
/// `failed` → `Failed`, `activating`/`deactivating` (a process may still
/// exist) and any other word → `Transitioning` (#902) — deliberately not a
/// closed list (the launchd twin's missed `xpcproxy`, #898), and an unknown
/// word must not end a stop-wait. No word at all is a failed read (the user
/// bus unreachable prints its reason on stderr and nothing on stdout), never
/// a confident `Inactive` (#896).
pub(super) fn status_from_is_active(
    unit: &str,
    out: &Output,
) -> Result<(ServiceStatus, Option<String>), SupervisorError> {
    let word = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let status = match word.as_str() {
        "active" | "reloading" | "refreshing" => ServiceStatus::Active,
        "inactive" | "maintenance" => ServiceStatus::Inactive,
        "failed" => ServiceStatus::Failed,
        "" => {
            return Err(SupervisorError::Backend(format!(
                "systemctl --user is-active {unit} (exit {}) answered no state, so its state \
                 is unknown: {}",
                exit_word(&out.status),
                String::from_utf8_lossy(&out.stderr).trim()
            )))
        }
        _ => ServiceStatus::Transitioning,
    };
    Ok((status, Some(word)))
}

/// The unit properties [`evidence_from_show`] reads: the restart count, how
/// the last run ended and its exit status (or signal number) (#904) — and
/// `LoadState`, because `show` answers for a unit the manager never loaded
/// too, exit 0, with clean defaults (`Result=success, NRestarts=0`, measured):
/// without `LoadState=not-found` beside them that reads as "never crashed".
pub(super) const CRASH_EVIDENCE_PROPS: &str = "LoadState,NRestarts,Result,ExecMainStatus";

/// `systemctl --user show -p` [`CRASH_EVIDENCE_PROPS`]'s answer as one line,
/// verbatim and in systemd's order: `Result=exit-code, NRestarts=31,
/// ExecMainStatus=1, LoadState=loaded`. No properties → `None`.
pub(super) fn evidence_from_show(stdout: &str) -> Option<String> {
    let props: Vec<&str> = stdout.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    (!props.is_empty()).then(|| props.join(", "))
}

#[cfg(test)]
pub(super) mod tests;
