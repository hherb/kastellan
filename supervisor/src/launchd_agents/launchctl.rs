//! The `launchctl` invocations and the pure readers of their answers.
//!
//! Lifted out of `launchd_agents.rs` when that file outgrew the 500-LOC
//! cap. Everything here is `pub(super)`: private to the `launchd_agents`
//! module tree.

use std::process::Command;

use super::LAUNCHCTL_TIMEOUT;
use crate::bounded_command::{run_capped, CappedOutcome};
use crate::{ServiceStatus, SupervisorError};

/// Build the `gui/<uid>` domain target string used by `launchctl`.
///
/// The UID comes from `getuid(2)`; the value is whatever the calling
/// process happens to run as. We do not honour `$SUDO_UID` or similar
/// because the supervisor crate is *user-level only* (running it as
/// root would put the agent into `gui/0`, which is not a regular user
/// session).
pub(super) fn user_domain_target() -> Result<String, SupervisorError> {
    Ok(format!("gui/{}", current_uid()))
}

/// Current real UID via `libc::getuid()`.
///
/// `getuid` is async-signal-safe and infallible, so no error path is
/// possible here. Wrapped in a function so the type stays `u32` at
/// the call sites without forcing every caller to write a libc cast.
#[cfg(target_os = "macos")]
pub(super) fn current_uid() -> u32 {
    // SAFETY: `getuid` is documented as always-succeeding and
    // async-signal-safe; it has no preconditions.
    unsafe { libc::getuid() as u32 }
}

/// Run `launchctl <args>` with stdio captured, bounded by
/// [`LAUNCHCTL_TIMEOUT`].
///
/// A non-zero exit is [`SupervisorError::Backend`]
/// `launchctl <args> (exit N): <stderr> <stdout>`; a timeout is also
/// `Backend` (its message never matches [`is_no_such_service_error`]); a
/// spawn or read failure is [`SupervisorError::Io`]. The readers below
/// depend on that split.
pub(super) fn run_launchctl(args: &[&str]) -> Result<String, SupervisorError> {
    // Bounded so a hung `bootstrap`/`bootout` against a churn-degraded
    // GUI domain fails fast instead of wedging the thread at 0 % CPU.
    // `run_capped` sets the stdio (stdin null, stdout/stderr piped) for us.
    let mut cmd = Command::new("launchctl");
    cmd.args(args);
    let out = match run_capped(&mut cmd, LAUNCHCTL_TIMEOUT)
        .map_err(|e| SupervisorError::Io(format!("run launchctl: {e}")))?
    {
        CappedOutcome::Completed(out) => out,
        CappedOutcome::TimedOut => {
            return Err(SupervisorError::Backend(format!(
                "launchctl {} timed out after {}s (the gui/<uid> launchd domain may be \
                 churn-degraded under heavy service load — see issue #130)",
                args.join(" "),
                LAUNCHCTL_TIMEOUT.as_secs()
            )));
        }
    };
    if !out.status.success() {
        // `launchctl` writes most diagnostics to stdout (yes, stdout)
        // and only some to stderr. Fold both so the error message
        // surfaces whichever the tool happened to use.
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let combined = format!("{} {}", stderr.trim(), stdout.trim()).trim().to_string();
        let exit = out
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());
        return Err(SupervisorError::Backend(format!(
            "launchctl {} (exit {exit}): {combined}",
            args.join(" ")
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parse the `state = <word>` line out of `launchctl print` output.
///
/// `launchctl print` emits a verbose nested dict; the `state` line
/// looks like `state = running`, indented by some whitespace. This
/// helper finds the first such line and returns the word — `running`,
/// `not running`, `waiting`, etc. Returns `None` if no `state` line
/// is present.
pub(super) fn parse_print_state(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(val) = trimmed.strip_prefix("state = ") {
            return Some(val.trim().to_string());
        }
    }
    None
}

/// Is the service loaded into the given launchd domain?
///
/// `launchctl print <target>` succeeds when the service is bootstrapped
/// (whether running, exited, waiting or transitional); only launchd's own
/// "no such service" answer means not loaded. A timeout, a spawn failure or
/// any other error is returned as `Err` — never read as "not loaded", which
/// would send `start` into a `bootstrap` that fails with a misleading error,
/// and `unload_wait` into calling a still-loaded agent gone.
pub(super) fn is_loaded_in_domain(target: &str) -> Result<bool, SupervisorError> {
    loaded_from_print(run_launchctl(&["print", target]))
}

/// Read `launchctl print`'s result as "is it loaded?" (see
/// [`is_loaded_in_domain`]).
pub(super) fn loaded_from_print(
    print: Result<String, SupervisorError>,
) -> Result<bool, SupervisorError> {
    match print {
        Ok(_) => Ok(true),
        Err(SupervisorError::Backend(msg)) if is_no_such_service_error(&msg) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Heuristic: does `msg` look like the "no such service" error
/// `launchctl bootout` emits when the agent isn't in the domain?
/// Used to make `stop` idempotent.
pub(super) fn is_no_such_service_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("could not find service")
        || m.contains("no such process")
        || m.contains("could not find specified service")
}

/// Read `launchctl bootout`'s result: `Ok` iff the agent is out of the
/// domain — it was booted out, or launchd says it was never loaded.
///
/// Anything else (a timeout against a churn-degraded domain, an answer
/// that could not be read, a refusal) leaves the agent possibly still
/// loaded and is returned as the error it is (#894).
pub(super) fn bootout_outcome(
    bootout: Result<String, SupervisorError>,
) -> Result<(), SupervisorError> {
    match bootout {
        Ok(_) => Ok(()),
        Err(SupervisorError::Backend(msg)) if is_no_such_service_error(&msg) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Read `launchctl print <target>`'s result into a status for an agent
/// whose plist is on disk.
///
/// launchd's "no such service" answer ([`is_no_such_service_error`]) means
/// installed but not loaded → `Inactive`.
/// A successful `print` is read from its `state = …` line; one with no
/// such line (a format change, a cut-short answer) is an error, never a
/// confident `Inactive` — a failed read is not an answer (#895).
pub(super) fn status_from_print(
    target: &str,
    print: Result<String, SupervisorError>,
) -> Result<ServiceStatus, SupervisorError> {
    let stdout = match print {
        Ok(stdout) => stdout,
        Err(SupervisorError::Backend(msg)) if is_no_such_service_error(&msg) => {
            return Ok(ServiceStatus::Inactive)
        }
        Err(e) => return Err(e),
    };
    match parse_print_state(&stdout).as_deref() {
        Some("running") => Ok(ServiceStatus::Active),
        // Loaded-but-not-running variants: `not running`, `waiting`,
        // `exited`, `spawn scheduled` (transitional). All map to Inactive,
        // matching the Linux backend's liberal mapping.
        Some(_) => Ok(ServiceStatus::Inactive),
        None => Err(SupervisorError::Backend(format!(
            "launchctl print {target} answered without a `state = …` line, so its state \
             is unknown (has launchctl's output format changed?)"
        ))),
    }
}
