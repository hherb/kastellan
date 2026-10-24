//! Tests for the pure readers of `launchctl`'s answers (`launchctl.rs`).
//!
//! Lifted out of `launchd_agents/tests.rs` when that file neared the
//! 500-LOC cap. The measured fixture `NOT_LOADED` and the `backend` helper
//! are shared with the driver tests there, hence their `pub(in …)` visibility.

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
    let timed_out = backend(&timed_out_message(&["bootout", "gui/501/svc"]));
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
    assert_eq!(
        status_from_print("gui/501/svc", running).unwrap(),
        (ServiceStatus::Active, Some("running".into()))
    );
    let not_running = Ok("\tstate = not running\n".to_string());
    assert_eq!(
        status_from_print("gui/501/svc", not_running).unwrap(),
        (ServiceStatus::Inactive, Some("not running".into()))
    );
}

#[test]
fn status_from_print_reads_not_loaded_as_inactive() {
    let not_loaded = backend(PRINT_NOT_FOUND);
    // launchd gives no word here, so the reader names it (#900).
    assert_eq!(
        status_from_print("gui/501/svc", not_loaded).unwrap(),
        (ServiceStatus::Inactive, Some("not loaded".into()))
    );
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
    assert!(loaded_from_print(backend(&timed_out_message(&["print", "gui/501/svc"]))).is_err());
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
    let timed_out = backend(&timed_out_message(&["print", "gui/501/svc"]));
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

// ---------- #898: no GUI domain, every launchd state word ----------

/// What `launchctl bootout` says when there is no `gui/<uid>` domain at all
/// — no console login, e.g. over SSH (measured on macOS 27: exit 112).
const BOOTOUT_NO_DOMAIN: &str =
    "launchctl bootout gui/99999/svc (exit 112): Could not find domain for user gui: 99999";

/// `launchctl print`'s answer for the same case (measured: exit 112).
const PRINT_NO_DOMAIN: &str =
    "launchctl print gui/99999/svc (exit 112): Bad request. Could not find domain for user gui: 99999";

/// No domain means no agent can be loaded in it, so the bootout's job is
/// done and `uninstall` may remove the plist.
#[test]
fn bootout_outcome_reads_a_missing_domain_as_not_loaded() {
    assert!(bootout_outcome(backend(BOOTOUT_NO_DOMAIN)).is_ok());
}

/// `unload_wait`'s probe agrees, so a headless `run_uninstall` does not warn
/// that the agent never left a domain that does not exist.
#[test]
fn loaded_from_print_reads_a_missing_domain_as_not_loaded() {
    assert!(!loaded_from_print(backend(PRINT_NO_DOMAIN)).unwrap());
}

/// `status` keeps the error: install's health wait should stop at once and
/// say there is no GUI session, not spin 90 s on `Inactive`.
#[test]
fn status_from_print_keeps_a_missing_domain_as_an_error() {
    assert!(status_from_print("gui/99999/svc", backend(PRINT_NO_DOMAIN)).is_err());
}

/// The timeout's real text (built by `run_launchctl`) is never a
/// "not loaded" answer to any reader.
#[test]
fn the_real_timeout_message_is_not_a_not_loaded_answer() {
    let msg = timed_out_message(&["bootout", "gui/501/svc"]);
    assert!(!is_not_loaded_answer(&msg), "{msg}");
    assert!(!is_no_such_service_error(&msg), "{msg}");
}

fn read(word: &str) -> (ServiceStatus, Option<String>) {
    status_from_print("gui/501/svc", Ok(format!("\tstate = {word}\n"))).unwrap()
}

/// Only `running` is `Active`. The words of a spawn in progress or a kill
/// not yet finished are `Transitioning` (#902) — a wait for `Inactive` must
/// not end on `SIGTERMed` — and the settled not-running words are `Inactive`.
/// Every word comes back verbatim for the timeout text (#900).
#[test]
fn status_from_print_maps_every_measured_state_word() {
    assert_eq!(read("running"), (ServiceStatus::Active, Some("running".into())));
    for word in ["spawn scheduled", "spawning", "xpcproxy", "SIGTERMed", "SIGKILLed"] {
        assert_eq!(read(word), (ServiceStatus::Transitioning, Some(word.into())), "{word}");
    }
    for word in ["not running", "languishing", "exited"] {
        assert_eq!(read(word), (ServiceStatus::Inactive, Some(word.into())), "{word}");
    }
}

/// An unlisted word is `Transitioning`, NOT an error (#898, measured) and
/// NOT `Inactive`: no list is provably complete — the binary's own table
/// missed `xpcproxy` — an error fails every health wait on a transient state
/// at once, and `Inactive` would end a stop-wait on a still-live process.
#[test]
fn status_from_print_reads_an_unlisted_state_word_as_transitioning() {
    assert_eq!(read("teleporting"), (ServiceStatus::Transitioning, Some("teleporting".into())));
}

/// `sudo -E` keeps the user's `$HOME` but runs as another uid: refused, so
/// `uninstall` keeps the user's plists instead of reading that uid's missing
/// domain as "not loaded" (#899 review).
#[test]
fn ensure_home_owned_by_accepts_only_the_owner() {
    let home = std::env::temp_dir().join(format!("kastellan-home-owner-{}", std::process::id()));
    fs::create_dir_all(&home).expect("create home");
    let uid = current_uid();
    let own = ensure_home_owned_by(Some(&home), uid);
    let other = ensure_home_owned_by(Some(&home), uid.wrapping_add(1));
    let _ = fs::remove_dir(&home);
    own.expect("own $HOME accepted");
    let err = other.expect_err("another uid's $HOME refused");
    assert!(matches!(&err, SupervisorError::Backend(m) if m.contains("sudo")), "{err}");
    assert!(!is_not_loaded_answer(&err.to_string()), "{err}");
}

#[test]
fn ensure_home_owned_by_skips_an_unset_or_missing_home() {
    assert!(ensure_home_owned_by(None, 0).is_ok());
    assert!(ensure_home_owned_by(Some(Path::new("/nonexistent/kastellan-no-home")), 0).is_ok());
}

// ---------- evidence_from_print (#904): the crash-loop evidence ----------

/// `launchctl print` for an agent that has run before, as macOS 27 lays it
/// out (measured 2026-10-23): top-level keys one tab deep, nested dicts two.
/// `resource coalition`'s nested `runs = 7` is what pins the depth rule;
/// environment entries use ` => `, so they never match a ` = ` key anyway.
const PRINT_RAN_BEFORE: &str = "gui/501/svc = {\n\tactive count = 1\n\tstate = running\n\
     \tenvironment = {\n\t\truns => 99\n\t}\n\n\tminimum runtime = 10\n\truns = 14\n\
     \tpid = 87791\n\tlast exit code = 0\n\n\tresource coalition = {\n\t\truns = 7\n\t}\n}\n";

const SVC: &str = "gui/501/svc";

#[test]
fn evidence_from_print_names_the_runs_and_the_last_exit() {
    assert_eq!(
        evidence_from_print(SVC, Ok(PRINT_RAN_BEFORE.into())).unwrap().as_deref(),
        Some("runs = 14, last exit code = 0")
    );
}

/// Every `last exit …`/`last terminating …` line is kept, whatever launchd
/// calls it — a jetsam'd agent says `last exit reason` (measured), a crashed
/// one its signal. Not a closed list, for the reason #898 gave.
#[test]
fn evidence_from_print_keeps_every_last_exit_line() {
    let print = "\tstate = not running\n\truns = 3\n\tlast exit reason = JETSAM_REASON_MEMORY_IDLE_EXIT\n\
                 \tlast terminating signal = Killed: 9\n";
    assert_eq!(
        evidence_from_print(SVC, Ok(print.into())).unwrap().as_deref(),
        Some("runs = 3, last exit reason = JETSAM_REASON_MEMORY_IDLE_EXIT, last terminating signal = Killed: 9")
    );
    // A loaded agent that never ran, verbatim (measured 2026-10-24).
    let never = "\tstate = not running\n\truns = 0\n\tlast exit code = (never exited)\n";
    assert_eq!(
        evidence_from_print(SVC, Ok(never.into())).unwrap().as_deref(),
        Some("runs = 0, last exit code = (never exited)")
    );
}

/// Only the counter's own key: `runs` exactly, not a key that starts with it.
#[test]
fn evidence_from_print_reads_the_runs_key_exactly() {
    let print = "\truns = 2\n\truns total = 9\n\tlast exitcode = 4\n";
    assert_eq!(evidence_from_print(SVC, Ok(print.into())).unwrap().as_deref(), Some("runs = 2"));
}

#[test]
fn evidence_from_print_has_nothing_for_an_agent_that_is_not_loaded() {
    assert_eq!(evidence_from_print(SVC, backend(PRINT_NOT_FOUND)).unwrap(), None);
}

/// A failed read is an error the caller names, never "no evidence" — and so
/// is a `print` with no `runs` line, which every loaded agent has (#895).
#[test]
fn evidence_from_print_names_a_failed_or_runless_read() {
    let timed_out = backend(&timed_out_message(&["print", SVC]));
    assert!(evidence_from_print(SVC, timed_out).is_err());
    let err = evidence_from_print(SVC, Ok("\tstate = running\n\tlast exit code = 0\n".into())).unwrap_err();
    assert!(err.to_string().contains("without a `runs = …` line"), "{err}");
}
