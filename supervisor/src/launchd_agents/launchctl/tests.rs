//! Tests for the pure readers of `launchctl`'s answers (`launchctl.rs`).
//!
//! Lifted out of `launchd_agents/tests.rs` when that file neared the
//! 500-LOC cap. The measured-wording fixtures (`NOT_LOADED`, `backend`) are
//! shared with the driver tests there, hence their `pub(in …)` visibility.

use super::*;

// ---------- launchctl-parsing helper tests ----------

#[test]
fn parse_print_state_finds_state_line_with_indentation() {
    let stdout = "gui/501/foo = {\n\ttype = LaunchAgent\n\tstate = running\n\tlast exit code = 0\n}";
    assert_eq!(parse_print_state(stdout), Some("running".into()));
}

#[test]
fn parse_print_state_returns_none_when_absent() {
    let stdout = "Could not find service \"foo\" in domain for login: 501";
    assert_eq!(parse_print_state(stdout), None);
}

#[test]
fn parse_print_state_handles_multi_word_state() {
    let stdout = "    state = not running\n";
    assert_eq!(parse_print_state(stdout), Some("not running".into()));
}

#[test]
fn is_no_such_service_error_recognises_known_phrases() {
    assert!(is_no_such_service_error(
        "Could not find service \"foo\" in domain"
    ));
    assert!(is_no_such_service_error("No such process"));
    assert!(!is_no_such_service_error("permission denied"));
}

/// What `launchctl bootout` says when the agent is not loaded (measured on
/// macOS: exit 3, ESRCH).
pub(in crate::launchd_agents) const NOT_LOADED: &str =
    "launchctl bootout gui/501/svc (exit 3): Boot-out failed: 3: No such process";

/// What `launchctl print` says for a service not in the domain (measured on
/// macOS: exit 113).
const PRINT_NOT_FOUND: &str =
    "launchctl print gui/501/svc (exit 113): Could not find service \"svc\" in domain for user gui: 501";

pub(in crate::launchd_agents) fn backend(msg: &str) -> Result<String, SupervisorError> {
    Err(SupervisorError::Backend(msg.into()))
}

// ---------- bootout_outcome (#894): only "not loaded" is benign ----------

#[test]
fn bootout_outcome_accepts_success_and_not_loaded() {
    assert!(bootout_outcome(Ok(String::new())).is_ok());
    assert!(bootout_outcome(backend(NOT_LOADED)).is_ok());
}

/// A wedged domain, an unreadable answer or a refusal leaves the agent
/// possibly still loaded — never read as "already gone".
#[test]
fn bootout_outcome_keeps_a_timeout_an_io_error_and_a_refusal() {
    let timed_out = backend("launchctl bootout gui/501/svc timed out after 20s");
    assert!(matches!(bootout_outcome(timed_out), Err(SupervisorError::Backend(_))));
    let unread = Err(SupervisorError::Io("run launchctl: Broken pipe".into()));
    assert!(matches!(bootout_outcome(unread), Err(SupervisorError::Io(_))));
    let refused = backend("launchctl bootout gui/501/svc (exit 1): Operation not permitted");
    assert!(bootout_outcome(refused).is_err());
}

// ---------- status_from_print (#895): an unreadable answer is not "inactive" ----------

#[test]
fn status_from_print_maps_the_state_line() {
    let running = Ok("gui/501/svc = {\n\tstate = running\n}".to_string());
    assert_eq!(status_from_print("gui/501/svc", running).unwrap(), ServiceStatus::Active);
    let waiting = Ok("\tstate = not running\n".to_string());
    assert_eq!(status_from_print("gui/501/svc", waiting).unwrap(), ServiceStatus::Inactive);
}

#[test]
fn status_from_print_reads_not_loaded_as_inactive() {
    let not_loaded = backend(PRINT_NOT_FOUND);
    assert_eq!(status_from_print("gui/501/svc", not_loaded).unwrap(), ServiceStatus::Inactive);
}

// ---------- loaded_from_print: `start`'s and `unload_wait`'s probe ----------

#[test]
fn loaded_from_print_reads_success_and_not_found() {
    assert!(loaded_from_print(Ok(String::new())).unwrap());
    assert!(!loaded_from_print(backend(PRINT_NOT_FOUND)).unwrap());
}

/// A failed probe is not "not loaded" — that would let `start` skip to a
/// misleading `bootstrap` error and `unload_wait` call a loaded agent gone.
#[test]
fn loaded_from_print_passes_through_a_timeout_and_an_io_error() {
    assert!(loaded_from_print(backend("launchctl print gui/501/svc timed out after 20s")).is_err());
    let unread = Err(SupervisorError::Io("run launchctl: Broken pipe".into()));
    assert!(matches!(loaded_from_print(unread), Err(SupervisorError::Io(_))));
}

/// A successful `print` with no `state = …` line (a format drift, a cut-short
/// answer) is an error naming the target — never a confident `Inactive`.
#[test]
fn status_from_print_refuses_an_answer_without_a_state_line() {
    let drifted = Ok("gui/501/svc = {\n\tstatus = running\n}".to_string());
    let err = status_from_print("gui/501/svc", drifted).expect_err("no state line");
    assert!(matches!(&err, SupervisorError::Backend(m) if m.contains("gui/501/svc")), "{err}");
}

#[test]
fn status_from_print_passes_through_a_timeout_and_an_io_error() {
    let timed_out = backend("launchctl print gui/501/svc timed out after 20s");
    assert!(status_from_print("gui/501/svc", timed_out).is_err());
    let unread = Err(SupervisorError::Io("run launchctl: Broken pipe".into()));
    assert!(matches!(status_from_print("gui/501/svc", unread), Err(SupervisorError::Io(_))));
}

#[test]
fn user_domain_target_starts_with_gui() {
    // We can't pin the UID (varies per host) but the prefix is
    // invariant.
    let t = user_domain_target().expect("uid resolves");
    assert!(t.starts_with("gui/"), "got: {t}");
    // Must be `gui/<digits>` and nothing else.
    let suffix = t.strip_prefix("gui/").unwrap();
    assert!(
        suffix.chars().all(|c| c.is_ascii_digit()),
        "uid suffix must be all digits, got: {t}"
    );
}
