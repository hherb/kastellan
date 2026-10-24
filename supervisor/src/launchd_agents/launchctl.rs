//! The `launchctl` invocations and the pure readers of their answers.
//!
//! Lifted out of `launchd_agents.rs` when that file outgrew the 500-LOC
//! cap. Everything here is `pub(super)`: private to the `launchd_agents`
//! module tree.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
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
/// session). A `$HOME` owned by another uid is refused
/// ([`ensure_home_owned_by`]).
pub(super) fn user_domain_target() -> Result<String, SupervisorError> {
    let uid = current_uid();
    ensure_home_owned_by(std::env::var_os("HOME").map(PathBuf::from).as_deref(), uid)?;
    Ok(format!("gui/{uid}"))
}

/// Refuse to drive `gui/<uid>` when `$HOME` belongs to another user.
///
/// The plists live under `$HOME`; the domain comes from `getuid`. Under
/// `sudo -E` they disagree: `bootout` in the caller's domain can answer "no
/// such domain" (measured for a uid with no login; `gui/0` as root is not),
/// which reads as "not loaded" (#898), and `uninstall` would delete
/// the real user's plists while their agents keep running. A `$HOME` that is
/// unset or does not exist holds no plists, so only an existing one is checked.
pub(super) fn ensure_home_owned_by(home: Option<&Path>, uid: u32) -> Result<(), SupervisorError> {
    let Some(home) = home else { return Ok(()) };
    match fs::metadata(home) {
        Ok(m) if m.uid() == uid => Ok(()),
        Ok(m) => Err(SupervisorError::Backend(format!(
            "$HOME {} belongs to uid {}, but this process runs as uid {uid}; the LaunchAgents \
             supervisor is user-level only (run it as that user, not under sudo)",
            home.display(),
            m.uid()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(SupervisorError::Io(format!("stat {}: {e}", home.display()))),
    }
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
/// `Backend` ([`timed_out_message`], never a "not loaded" answer); a
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
            return Err(SupervisorError::Backend(timed_out_message(args)));
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

/// The error text of a `launchctl <args>` call cut off at
/// [`LAUNCHCTL_TIMEOUT`]. One function so the tests read the real wording:
/// if it ever gained a "not loaded" phrase, a timeout would read as "already
/// gone" (#898).
pub(super) fn timed_out_message(args: &[&str]) -> String {
    format!(
        "launchctl {} timed out after {}s (the gui/<uid> launchd domain may be \
         churn-degraded under heavy service load — see issue #130)",
        args.join(" "),
        LAUNCHCTL_TIMEOUT.as_secs()
    )
}

/// Parse the `state = <word>` line out of `launchctl print` output.
///
/// `launchctl print` emits a verbose nested dict; the `state` line
/// looks like `state = running`, indented by some whitespace. This
/// helper finds the first such line and returns the word — `running`,
/// `not running`, `spawn scheduled`, etc. Returns `None` if no `state`
/// line is present.
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
/// (whether running, exited or transitional); only launchd's own "no such
/// service" or "no such domain" answer ([`is_not_loaded_answer`]) means not
/// loaded. (With no domain, `start` then reaches `bootstrap`, which fails
/// with the same `Could not find domain for user gui: N`, exit 112 —
/// measured.) A timeout, a spawn failure or
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
        Err(SupervisorError::Backend(msg)) if is_not_loaded_answer(&msg) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Heuristic: does `msg` look like launchd's "no such service" answer
/// (from `bootout` or `print`) when the agent isn't in the domain?
/// [`status_from_print`] uses it alone; the other readers go through
/// [`is_not_loaded_answer`].
pub(super) fn is_no_such_service_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("could not find service")
        || m.contains("no such process")
        || m.contains("could not find specified service")
}

/// Heuristic: does `msg` say there is no `gui/<uid>` domain at all?
/// launchctl answers `Could not find domain for user gui: N` (exit 112) when
/// the user has no console login — e.g. a Mac reached only over SSH.
pub(super) fn is_no_such_domain_error(msg: &str) -> bool {
    msg.to_ascii_lowercase().contains("could not find domain")
}

/// Does `msg` say the agent is not in the domain — because launchd has no
/// such service there, or no such domain (no agent can be loaded in a domain
/// that does not exist)? A churn-degraded domain (#130) hangs instead, which
/// is a timeout, never this answer.
pub(super) fn is_not_loaded_answer(msg: &str) -> bool {
    is_no_such_service_error(msg) || is_no_such_domain_error(msg)
}

/// Read `launchctl bootout`'s result: `Ok` iff the agent is out of the
/// domain — it was booted out, or launchd says it is not loaded
/// ([`is_not_loaded_answer`], which includes "no such domain", #898).
///
/// Anything else (a timeout against a churn-degraded domain, an answer
/// that could not be read, a refusal) leaves the agent possibly still
/// loaded and is returned as the error it is (#894).
pub(super) fn bootout_outcome(
    bootout: Result<String, SupervisorError>,
) -> Result<(), SupervisorError> {
    match bootout {
        Ok(_) => Ok(()),
        Err(SupervisorError::Backend(msg)) if is_not_loaded_answer(&msg) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Read `launchctl print <target>`'s result into a status, plus launchd's
/// own state word for diagnostics (#900), for an agent whose plist is on disk.
///
/// launchd's "no such service" answer ([`is_no_such_service_error`]) means
/// installed but not loaded → `Inactive`, with the word `not loaded` — the
/// one case launchd gives no word, and the likeliest cause of a stuck
/// bring-up. "No such domain" stays an error on
/// purpose: install's health wait should stop at once and say there is no
/// GUI session, not spin on `Inactive`.
/// A successful `print` is read from its `state = …` line; one with no
/// such line (a format change, a cut-short answer) is an error, never a
/// confident `Inactive` — a failed read is not an answer (#895).
pub(super) fn status_from_print(
    target: &str,
    print: Result<String, SupervisorError>,
) -> Result<(ServiceStatus, Option<String>), SupervisorError> {
    let stdout = match print {
        Ok(stdout) => stdout,
        Err(SupervisorError::Backend(msg)) if is_no_such_service_error(&msg) => {
            return Ok((ServiceStatus::Inactive, Some("not loaded".into())))
        }
        Err(e) => return Err(e),
    };
    let Some(word) = parse_print_state(&stdout) else {
        return Err(SupervisorError::Backend(format!(
            "launchctl print {target} answered without a `state = …` line, so its state \
             is unknown (has launchctl's output format changed?)"
        )));
    };
    let status = match word.as_str() {
        "running" => ServiceStatus::Active,
        "not running" | "exited" | "languishing" => ServiceStatus::Inactive,
        // A spawn under way (`spawn scheduled`, `spawning`, `xpcproxy`) or a
        // kill launchd has sent but not seen finish (`SIGTERMed`,
        // `SIGKILLed`): the process may exist, so neither a start- nor a
        // stop-wait may end here (#902). Every unlisted word too — deliberately
        // NOT a closed list (#898): the words in the table after `service state: %s` in
        // `/sbin/launchd` miss `xpcproxy`, which a full sweep caught
        // mid-spawn — a closed list tried during #898 errored on it and
        // failed two PG bring-ups seconds from `running`.
        _ => ServiceStatus::Transitioning,
    };
    Ok((status, Some(word)))
}

/// Read `launchctl print`'s crash-loop evidence (#904): the agent's `runs`
/// count and every `last exit …`/`last terminating …` line, verbatim, as
/// `runs = 31, last exit code = 1`.
///
/// Only top-level keys (one tab deep) count: nested dicts such as
/// `resource coalition = { … }` reuse ` = ` keys, which must not pass for the
/// agent's own. The `last …`
/// keys are matched by prefix, not listed — launchd says `last exit code`,
/// `last exit reason` (a jetsam'd agent, measured) or names a signal, and no
/// list of its words has held (#898). Not loaded → `None`; a failed read is
/// the error it is, so the caller can name it — and so is a `print` with no
/// `runs` line: every loaded agent has one, `runs = 0` included (492 of 492,
/// measured), so its absence is a format change, not "nothing to say" (#895).
pub(super) fn evidence_from_print(
    target: &str,
    print: Result<String, SupervisorError>,
) -> Result<Option<String>, SupervisorError> {
    let stdout = match print {
        Ok(stdout) => stdout,
        Err(SupervisorError::Backend(msg)) if is_no_such_service_error(&msg) => return Ok(None),
        Err(e) => return Err(e),
    };
    let evidence: Vec<&str> = stdout
        .lines()
        // One tab stripped: a nested line's key still starts with a tab.
        .filter_map(|line| line.strip_prefix('\t'))
        .filter(|line| {
            line.split_once(" = ").is_some_and(|(key, _)| {
                key == "runs" || key.starts_with("last exit ") || key.starts_with("last terminating ")
            })
        })
        .collect();
    if !evidence.iter().any(|line| line.starts_with("runs = ")) {
        return Err(SupervisorError::Backend(format!(
            "launchctl print {target} answered without a `runs = …` line, so its crash \
             evidence is unknown (has launchctl's output format changed?)"
        )));
    }
    Ok(Some(evidence.join(", ")))
}

#[cfg(test)]
pub(super) mod tests;
