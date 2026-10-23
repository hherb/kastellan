//! Tests of the pure readers of `systemctl --user`'s answers (#896).
//!
//! Every fixture is systemd's own wording, measured on the DGX (systemd
//! 255, 2026-10-23) — never a paraphrase, so a reader that matches the
//! wrong phrase fails here rather than on a live host.

use super::*;
use std::os::unix::process::ExitStatusExt;

/// The unit every fixture names.
pub(in crate::systemd_user) const UNIT: &str = "x.service";

/// `systemctl --user stop x.service` for a unit the manager has not loaded
/// (exit 5). The real error as `run_systemctl_user` renders it.
pub(in crate::systemd_user) const NOT_LOADED: &str = "systemctl --user stop x.service (exit 5): \
     Failed to stop x.service: Unit x.service not loaded.";

/// `systemctl --user disable x.service` once the unit file is gone (exit 1).
const NO_UNIT_FILE: &str = "systemctl --user disable x.service (exit 1): \
     Failed to disable unit: Unit file x.service does not exist.";

/// Any `systemctl --user` call with no reachable user bus (exit 1, empty
/// stdout) — e.g. `XDG_RUNTIME_DIR` unset in a headless shell.
pub(in crate::systemd_user) const NO_BUS: &str = "systemctl --user stop x.service (exit 1): \
     Failed to connect to bus: No medium found";

pub(in crate::systemd_user) fn backend(msg: &str) -> Result<String, SupervisorError> {
    Err(SupervisorError::Backend(msg.into()))
}

#[test]
fn stop_outcome_accepts_success_and_not_loaded_only() {
    stop_outcome(UNIT, Ok(String::new())).expect("a clean stop");
    stop_outcome(UNIT, backend(NOT_LOADED)).expect("not loaded = already stopped");
    for failure in [
        backend(NO_BUS),
        backend(&timed_out_message(&["stop", UNIT])),
        Err(SupervisorError::Io("run systemctl: Broken pipe".into())),
    ] {
        stop_outcome(UNIT, failure).expect_err("the unit may still be running");
    }
}

#[test]
fn disable_outcome_accepts_success_and_no_unit_file_only() {
    disable_outcome(UNIT, Ok(String::new())).expect("a clean disable");
    disable_outcome(UNIT, backend(NO_UNIT_FILE)).expect("no file = nothing to unlink");
    for failure in [
        backend(NO_BUS),
        backend(&timed_out_message(&["disable", UNIT])),
        // Each half of the answer alone is not the answer.
        backend("systemctl --user disable x.service (exit 1): Directory /x does not exist."),
        backend("systemctl --user disable x.service (exit 1): Unit file x.service is masked."),
        Err(SupervisorError::Io("run systemctl: Broken pipe".into())),
    ] {
        disable_outcome(UNIT, failure).expect_err("the boot link may remain");
    }
}

/// The two benign answers are not interchangeable: a stop that says "does
/// not exist" or a disable that says "not loaded" is not what was measured.
#[test]
fn the_benign_answers_do_not_cross_over() {
    stop_outcome(UNIT, backend(NO_UNIT_FILE)).expect_err("not a stop answer");
    disable_outcome(UNIT, backend(NOT_LOADED)).expect_err("not a disable answer");
}

/// A benign answer about some other unit says nothing about this one.
#[test]
fn the_benign_answers_are_about_this_unit_only() {
    stop_outcome("y.service", backend(NOT_LOADED)).expect_err("x was not loaded, not y");
    disable_outcome("y.service", backend(NO_UNIT_FILE)).expect_err("x has no file, not y");
}

/// If the timeout text ever gained a benign phrase, a hung manager would read
/// as "already stopped" and `uninstall` would delete a running unit's file.
#[test]
fn the_timeout_text_is_never_a_benign_answer() {
    let msg = timed_out_message(&["stop", UNIT]);
    assert!(!is_not_loaded_answer(&msg, UNIT) && !is_no_unit_file_answer(&msg, UNIT), "{msg}");
    assert!(msg.contains("systemctl --user stop x.service did not finish within"), "{msg}");
}

/// The stop-then-disable wiring, fed systemd's answers: which calls it makes
/// and what it concludes.
#[test]
fn stop_and_disable_disables_only_after_a_clean_stop() {
    let run = |stop: Result<String, SupervisorError>, disable: Result<String, SupervisorError>| {
        let mut calls = Vec::new();
        let (mut stop, mut disable) = (Some(stop), Some(disable));
        let res = stop_and_disable_with(UNIT, |args: &[&str]| {
            calls.push(args.join(" "));
            match args[0] {
                "stop" => stop.take().expect("one stop"),
                _ => disable.take().expect("one disable"),
            }
        });
        (res, calls)
    };
    let both = vec![format!("stop {UNIT}"), format!("disable {UNIT}")];

    let (res, calls) = run(backend(NO_BUS), Ok(String::new()));
    assert!(res.is_err(), "a failed stop is a failure even if disable would succeed");
    assert_eq!(calls, vec![format!("stop {UNIT}")], "no disable after a failed stop");

    let (res, calls) = run(Ok(String::new()), backend(NO_BUS));
    assert!(res.is_err(), "a failed disable leaves the boot link");
    assert_eq!(calls, both);

    let (res, calls) = run(backend(NOT_LOADED), backend(NO_UNIT_FILE));
    assert!(res.is_ok(), "a retry after everything was removed: {res:?}");
    assert_eq!(calls, both);
}

/// `is-active`'s answer as `run_user` returns it.
fn answer(stdout: &str, stderr: &str, code: i32) -> Output {
    Output {
        status: ExitStatus::from_raw(code << 8),
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

/// The word, as `status_from_is_active` reads it, with the word itself
/// handed back verbatim for the timeout text (#900).
fn read(word: &str, code: i32) -> ServiceStatus {
    let (status, back) = status_from_is_active(UNIT, &answer(&format!("{word}\n"), "", code)).unwrap();
    assert_eq!(back.as_deref(), Some(word), "the word comes back verbatim");
    status
}

#[test]
fn status_from_is_active_maps_the_three_settled_states() {
    assert_eq!(read("active", 0), ServiceStatus::Active);
    assert_eq!(read("inactive", 3), ServiceStatus::Inactive);
    assert_eq!(read("failed", 3), ServiceStatus::Failed);
}

/// `reloading`/`refreshing` are up and serving → `Active`; `activating`/
/// `deactivating` may still have a process → `Transitioning`, so a wait for
/// `Inactive` after a stop does not end inside the stop timeout (#902).
/// `maintenance` (the service's own process is gone; `systemctl clean` is
/// removing its directories) → `Inactive`.
#[test]
fn status_from_is_active_maps_the_moving_states() {
    assert_eq!(read("reloading", 0), ServiceStatus::Active);
    assert_eq!(read("refreshing", 0), ServiceStatus::Active);
    assert_eq!(read("activating", 3), ServiceStatus::Transitioning);
    assert_eq!(read("deactivating", 3), ServiceStatus::Transitioning);
    assert_eq!(read("maintenance", 3), ServiceStatus::Inactive);
}

/// An unforeseen word is `Transitioning` — deliberately NOT a closed list
/// that errors (the launchd twin's missed `xpcproxy` failed two PG bring-ups
/// seconds from `running`, #898), and NOT `Inactive`, which would end a
/// stop-wait on a word nobody has read the meaning of.
#[test]
fn status_from_is_active_reads_an_unforeseen_word_as_transitioning() {
    assert_eq!(read("a-new-word", 3), ServiceStatus::Transitioning);
}

/// #896: no word at all (the user bus unreachable) is a failed read, not a
/// confident `Inactive` — and the error carries systemctl's own reason and
/// exit code, so even a silent failure says something.
#[test]
fn status_from_is_active_refuses_an_empty_answer() {
    for stdout in ["", "\n", "  \n"] {
        let out = answer(stdout, "Failed to connect to bus: No medium found\n", 1);
        let msg = status_from_is_active(UNIT, &out).expect_err("no state word").to_string();
        assert!(msg.contains(UNIT) && msg.contains("Failed to connect to bus"), "{msg}");
        assert!(msg.contains("(exit 1)"), "{msg}");
    }
}

/// The bounded runner itself: a real `systemctl` is not needed to prove a
/// spawn failure is `Io` (no binary named this exists).
#[test]
fn run_bounded_reports_a_missing_binary_as_io() {
    let err = run_bounded("kastellan-no-such-systemctl-896", &["--user", "is-active", "x"])
        .expect_err("no such binary");
    assert!(matches!(err, SupervisorError::Io(_)), "{err:?}");
}

/// #904: `systemctl --user show -p NRestarts,Result,ExecMainStatus` for a
/// crash-looping unit, verbatim (DGX, systemd 255, 2026-10-23 — the live
/// core refusing to boot without its guard backend). systemd picks the order.
#[test]
fn evidence_from_show_names_the_restarts_and_the_last_exit() {
    let show = "Result=exit-code\nNRestarts=1355\nExecMainStatus=1\n";
    assert_eq!(
        evidence_from_show(show).as_deref(),
        Some("Result=exit-code, NRestarts=1355, ExecMainStatus=1")
    );
    assert_eq!(evidence_from_show(""), None);
    assert_eq!(evidence_from_show("\n"), None);
}
