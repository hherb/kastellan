//! Tests of the pure readers of `systemctl --user`'s answers (#896).
//!
//! Every fixture is systemd's own wording, measured on the DGX (systemd
//! 255, 2026-10-23) — never a paraphrase, so a reader that matches the
//! wrong phrase fails here rather than on a live host.

use super::*;

/// `systemctl --user stop x.service` for a unit the manager has not loaded
/// (exit 5). The real error as `run_systemctl_user` renders it.
pub(in crate::systemd_user) const NOT_LOADED: &str = "systemctl --user stop x.service (exit 5): \
     Failed to stop x.service: Unit x.service not loaded.";

/// `systemctl --user disable x.service` once the unit file is gone (exit 1).
const NO_UNIT_FILE: &str = "systemctl --user disable x.service (exit 1): \
     Failed to disable unit: Unit file x.service does not exist.";

/// Any `systemctl --user` call with no reachable user bus (exit 1, empty
/// stdout) — e.g. `XDG_RUNTIME_DIR` unset in a headless shell.
const NO_BUS: &str = "systemctl --user stop x.service (exit 1): \
     Failed to connect to bus: No medium found";

pub(in crate::systemd_user) fn backend(msg: &str) -> Result<String, SupervisorError> {
    Err(SupervisorError::Backend(msg.into()))
}

#[test]
fn stop_outcome_accepts_success_and_not_loaded_only() {
    stop_outcome(Ok(String::new())).expect("a clean stop");
    stop_outcome(backend(NOT_LOADED)).expect("not loaded = already stopped");
    for failure in [
        backend(NO_BUS),
        backend(&timed_out_message(&["stop", "x.service"])),
        Err(SupervisorError::Io("run systemctl: Broken pipe".into())),
    ] {
        stop_outcome(failure).expect_err("the unit may still be running");
    }
}

#[test]
fn disable_outcome_accepts_success_and_no_unit_file_only() {
    disable_outcome(Ok(String::new())).expect("a clean disable");
    disable_outcome(backend(NO_UNIT_FILE)).expect("no file = nothing to unlink");
    disable_outcome(backend(NO_BUS)).expect_err("the boot link may remain");
    disable_outcome(backend(&timed_out_message(&["disable", "x.service"])))
        .expect_err("a timeout is no answer");
}

/// The two benign answers are not interchangeable: a stop that says "does
/// not exist" or a disable that says "not loaded" is not what was measured.
#[test]
fn the_benign_answers_do_not_cross_over() {
    stop_outcome(backend(NO_UNIT_FILE)).expect_err("not a stop answer");
    disable_outcome(backend(NOT_LOADED)).expect_err("not a disable answer");
}

/// If the timeout text ever gained a benign phrase, a hung manager would read
/// as "already stopped" and `uninstall` would delete a running unit's file.
#[test]
fn the_timeout_text_is_never_a_benign_answer() {
    let msg = timed_out_message(&["stop", "x.service"]);
    assert!(!is_not_loaded_answer(&msg) && !is_no_unit_file_answer(&msg), "{msg}");
    assert!(msg.contains("systemctl --user stop x.service timed out after"), "{msg}");
}

#[test]
fn status_from_is_active_maps_the_three_settled_states() {
    assert_eq!(status_from_is_active("x.service", "active\n", "").unwrap(), ServiceStatus::Active);
    assert_eq!(
        status_from_is_active("x.service", "inactive\n", "").unwrap(),
        ServiceStatus::Inactive
    );
    assert_eq!(status_from_is_active("x.service", "failed\n", "").unwrap(), ServiceStatus::Failed);
}

/// Transitional and unforeseen words stay `Inactive` — deliberately NOT a
/// closed list: the launchd twin's closed list missed `xpcproxy` and failed
/// two PG bring-ups seconds from `running` (#898).
#[test]
fn status_from_is_active_reads_any_other_word_as_inactive() {
    for word in ["activating", "deactivating", "reloading", "maintenance", "refreshing", "a-new-word"] {
        assert_eq!(
            status_from_is_active("x.service", &format!("{word}\n"), "").unwrap(),
            ServiceStatus::Inactive,
            "{word}"
        );
    }
}

/// #896: no word at all (the user bus unreachable) is a failed read, not a
/// confident `Inactive` — and the error carries systemctl's own reason.
#[test]
fn status_from_is_active_refuses_an_empty_answer() {
    for stdout in ["", "\n", "  \n"] {
        let err = status_from_is_active("x.service", stdout, "Failed to connect to bus: No medium found\n")
            .expect_err("no state word");
        let msg = err.to_string();
        assert!(msg.contains("x.service") && msg.contains("Failed to connect to bus"), "{msg}");
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
