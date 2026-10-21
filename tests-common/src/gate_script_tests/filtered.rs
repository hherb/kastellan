//! #880: the gate reads libtest's `N filtered out` and demands an exact,
//! per-profile count of it.
//!
//! `IGNORED` (#876, `ignored.rs`) is blind on the profiles that pass
//! `--ignored` (`microvm`, `mail-live`): there libtest runs only the ignored
//! tests and counts the rest as `filtered out`. So a Firecracker test that LOST
//! its `#[ignore]` stopped running under the demanded knobs, and every floor was
//! still met by the tests that kept theirs. The same count also fails an
//! operator name filter passed after the profile name, which narrows any
//! profile silently.
//!
//! Like `IGNORED`, it is a MEASURED number (see FILTERED_OUT in the
//! script's profile table) and EXACT, not a cap: under `≤`, deleting one
//! deliberate plain test would leave room for one lost `#[ignore]`.

use super::profiles;
use super::run::{
    describe, e2e_evidence_for, hooked_binary, run_gate_with, GateEdits, PROFILE,
};

/// One binary's libtest summary with `filtered` tests in it.
fn summary(passed: u32, filtered: u32) -> String {
    format!(
        "test result: ok. {passed} passed; 0 failed; 0 ignored; 0 measured; \
         {filtered} filtered out; finished in 0.01s\n"
    )
}

/// A healthy run of [`PROFILE`] whose binaries report these `filtered out` counts.
fn log_with_filtered(per_binary: &[u32]) -> String {
    let mut log = e2e_evidence_for(PROFILE);
    for (i, filtered) in per_binary.iter().enumerate() {
        log.push_str(&hooked_binary(&format!("suite_{i}_e2e")));
        log.push_str(&summary(5, *filtered));
    }
    log
}

/// [`PROFILE`]'s committed count, which these tests assume is 0.
#[test]
fn the_profile_these_tests_drive_expects_zero_filtered() {
    let p = profiles().into_iter().find(|p| p.name == PROFILE).expect("PROFILE row");
    assert_eq!(p.filtered_out, "0", "the tests below assume `{PROFILE}` expects FILTERED_OUT = 0");
}

#[test]
fn one_filtered_out_test_fails_a_profile_expecting_zero() {
    // A test that lost its `#[ignore]` on an `--ignored` profile, or a name
    // filter: refused, the field named, and the offending summary line shown.
    let out = run_gate_with(PROFILE, "filtered-one", &log_with_filtered(&[0, 1]), 0, &GateEdits::default());
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("1 test(s) were `filtered out`, expected exactly 0 (FILTERED_OUT)"),
        "{}",
        describe(&out)
    );
    // Named by binary: libtest never names a filtered-out test.
    assert!(
        stdout.contains(
            "     tests/suite_1_e2e.rs: test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out;"
        ),
        "{}",
        describe(&out)
    );
    assert!(!stdout.contains("suite_0_e2e.rs: test result"), "a 0-filtered binary is not listed\n{}", describe(&out));
}

#[test]
fn filtered_counts_are_summed_across_binaries_and_must_match_exactly() {
    // A count of 4 on a copy: 1 + 3 passes, 2 + 3 and 1 + 2 are both refused.
    let edits = GateEdits { filtered_out: Some((PROFILE, "4")), ..Default::default() };
    let at = run_gate_with(PROFILE, "filtered-exact", &log_with_filtered(&[1, 3]), 0, &edits);
    assert!(at.status.success(), "a count of 4 must allow 4\n{}", describe(&at));
    let over = run_gate_with(PROFILE, "filtered-over", &log_with_filtered(&[2, 3]), 0, &edits);
    assert_eq!(over.status.code(), Some(1), "a count of 4 must refuse 5\n{}", describe(&over));
    assert!(
        String::from_utf8_lossy(&over.stdout).contains("5 test(s) were `filtered out`, expected exactly 4"),
        "{}",
        describe(&over)
    );
    // Below: a deliberate plain test went away, leaving room for a lost #[ignore].
    let under = run_gate_with(PROFILE, "filtered-under", &log_with_filtered(&[1, 2]), 0, &edits);
    assert_eq!(under.status.code(), Some(1), "a count of 4 must refuse 3\n{}", describe(&under));
    let stdout = String::from_utf8_lossy(&under.stdout);
    assert!(stdout.contains("only 3 test(s) were `filtered out`, expected exactly 4"), "{}", describe(&under));
    assert!(
        stdout.contains("     tests/suite_0_e2e.rs: test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out;"),
        "every suite's summary is listed\n{}",
        describe(&under)
    );
}

#[test]
fn a_non_numeric_filtered_out_is_refused_before_anything_runs() {
    for value in ["any", "four"] {
        let edits = GateEdits { filtered_out: Some((PROFILE, value)), ..Default::default() };
        let out = run_gate_with(PROFILE, "filtered-malformed", &log_with_filtered(&[0]), 0, &edits);
        assert_eq!(out.status.code(), Some(2), "{value}: {}", describe(&out));
        assert!(
            String::from_utf8_lossy(&out.stderr)
                .contains(&format!("profile '{PROFILE}' FILTERED_OUT must be a number")),
            "{value}: {}",
            describe(&out)
        );
        assert!(!String::from_utf8_lossy(&out.stdout).contains("==> evidence"), "{}", describe(&out));
    }
}

#[test]
fn an_empty_filtered_out_is_refused_before_anything_runs() {
    // `-gt ""` and `-lt ""` both error, which `if`/`elif` read as exact.
    let edits = GateEdits { filtered_out: Some((PROFILE, "")), ..Default::default() };
    let out = run_gate_with(PROFILE, "filtered-empty", &log_with_filtered(&[5]), 0, &edits);
    assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains(&format!("profile '{PROFILE}' has an empty 'filtered_out' field")),
        "{}",
        describe(&out)
    );
}

#[test]
fn a_result_line_cut_before_finished_in_is_refused_as_torn() {
    // libtest's LAST split: `. N passed; … N filtered out` and `; finished in`
    // are separate writes. A framed stderr line between them leaves a summary
    // ending `filtered out`, which the sum (it needs `out;`) reads as 0.
    let mut log = e2e_evidence_for(PROFILE);
    log.push_str(&hooked_binary("torn_e2e"));
    log.push_str("test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out\n");
    log.push_str("[panic-hook] a late line from a lingering thread\n");
    log.push_str("; finished in 0.01s\n");
    let out = run_gate_with(PROFILE, "filtered-torn", &log, 0, &GateEdits::default());
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("1 `test result:` line(s) are not whole"),
        "{}",
        describe(&out)
    );
}

#[test]
fn a_binary_with_no_summary_line_is_refused() {
    // Exited inside a test: no `test result:` at all, so its 9 filtered tests
    // reach no sum and no torn-line check.
    let mut log = log_with_filtered(&[0]);
    log.push_str(&hooked_binary("silent_e2e"));
    log.push_str("\nrunning 9 tests\n");
    let out = run_gate_with(PROFILE, "filtered-no-summary", &log, 0, &GateEdits::default());
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("2 test binaries ran but the log has 1 whole `test result:`"), "{}", describe(&out));
    assert!(stdout.contains("     tests/silent_e2e.rs\n"), "the silent suite is named\n{}", describe(&out));
    assert!(!stdout.contains("     tests/suite_0_e2e.rs\n"), "a whole suite is not named\n{}", describe(&out));
}

#[test]
fn a_summary_with_output_in_front_of_it_is_refused() {
    // The `\n` and `test result: ` are separate writes, so unframed stderr can
    // land BEFORE the summary — where no `^test result:` anchor reads it.
    let mut log = log_with_filtered(&[0]);
    log.push_str(&hooked_binary("prefixed_e2e"));
    log.push_str("XYZ");
    log.push_str(&summary(0, 9));
    let out = run_gate_with(PROFILE, "filtered-prefixed", &log, 0, &GateEdits::default());
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("     tests/prefixed_e2e.rs\n"),
        "{}",
        describe(&out)
    );
}

#[test]
fn a_binary_that_ran_zero_tests_is_refused_even_when_the_others_clear_every_floor() {
    // #823: the floors are run-wide, so one suite that ran nothing (a whole-file
    // `cfg`, every test `#[ignore]`d or renamed away) hid behind the others.
    // Hooked on purpose: the per-binary hook check must not be what proves a
    // suite ran, since a hook install outside a test would reopen the gap.
    let mut log = log_with_filtered(&[0]);
    log.push_str(&hooked_binary("empty_e2e"));
    log.push_str(&summary(0, 0));
    let out = run_gate_with(PROFILE, "zero-test-binary", &log, 0, &GateEdits::default());
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("1 test binary(ies) ran zero tests"), "{}", describe(&out));
    assert!(stdout.contains("     tests/empty_e2e.rs\n"), "the empty suite is named\n{}", describe(&out));
    assert!(!stdout.contains("     tests/suite_0_e2e.rs\n"), "a suite that ran is not named\n{}", describe(&out));
}

#[test]
fn a_binary_whose_only_tests_failed_is_not_called_empty() {
    // `0 passed; 2 failed` ran tests; the red exit already fails it, and calling
    // it empty would send the reader after the wrong cause.
    let mut log = log_with_filtered(&[0]);
    log.push_str(&hooked_binary("red_e2e"));
    log.push_str("test result: FAILED. 0 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n");
    let out = run_gate_with(PROFILE, "red-not-empty", &log, 101, &GateEdits::default());
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("ran zero tests"), "{}", describe(&out));
}
