//! #880: the gate reads libtest's `N filtered out` and caps it per profile.
//!
//! `MAX_IGNORED` (#876, `ignored.rs`) is blind on the profiles that pass
//! `--ignored` (`microvm`, `mail-live`): there libtest runs only the ignored
//! tests and counts the rest as `filtered out`. So a Firecracker test that LOST
//! its `#[ignore]` stopped running under the demanded knobs, and every floor was
//! still met by the tests that kept theirs. The same count also catches an
//! operator name filter passed after the profile name, which narrows any
//! profile silently.
//!
//! Like `MAX_IGNORED`, the cap is a MEASURED number: `microvm` carries 4 plain
//! plan tests that are deliberately not `#[ignore]`d; every other profile is 0.

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

/// [`PROFILE`]'s committed cap, which these tests assume is 0.
#[test]
fn the_profile_these_tests_drive_caps_filtered_at_zero() {
    let p = profiles().into_iter().find(|p| p.name == PROFILE).expect("PROFILE row");
    assert_eq!(p.max_filtered, "0", "the tests below assume `{PROFILE}` caps MAX_FILTERED at 0");
}

#[test]
fn one_filtered_out_test_fails_a_profile_capped_at_zero() {
    // A test that lost its `#[ignore]` on an `--ignored` profile, or a name
    // filter: refused, the field named, and the offending summary line shown.
    let out = run_gate_with(PROFILE, "filtered-one", &log_with_filtered(&[0, 1]), 0, &GateEdits::default());
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("1 test(s) were `filtered out`, max is 0 (MAX_FILTERED)"),
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
fn filtered_counts_are_summed_across_binaries_and_the_cap_is_inclusive() {
    // A cap of 4 (microvm's) on a copy: 1 + 3 passes, 2 + 3 is refused.
    let edits = GateEdits { max_filtered: Some((PROFILE, "4")), ..Default::default() };
    let at = run_gate_with(PROFILE, "filtered-at-cap", &log_with_filtered(&[1, 3]), 0, &edits);
    assert!(at.status.success(), "a cap of 4 must allow 4\n{}", describe(&at));
    let over = run_gate_with(PROFILE, "filtered-over", &log_with_filtered(&[2, 3]), 0, &edits);
    assert_eq!(over.status.code(), Some(1), "a cap of 4 must refuse 5\n{}", describe(&over));
    assert!(
        String::from_utf8_lossy(&over.stdout).contains("5 test(s) were `filtered out`, max is 4"),
        "{}",
        describe(&over)
    );
}

#[test]
fn a_non_numeric_max_filtered_is_refused_before_anything_runs() {
    for value in ["any", "four"] {
        let edits = GateEdits { max_filtered: Some((PROFILE, value)), ..Default::default() };
        let out = run_gate_with(PROFILE, "filtered-malformed", &log_with_filtered(&[0]), 0, &edits);
        assert_eq!(out.status.code(), Some(2), "{value}: {}", describe(&out));
        assert!(
            String::from_utf8_lossy(&out.stderr)
                .contains(&format!("profile '{PROFILE}' MAX_FILTERED must be a number")),
            "{value}: {}",
            describe(&out)
        );
        assert!(!String::from_utf8_lossy(&out.stdout).contains("==> evidence"), "{}", describe(&out));
    }
}

#[test]
fn an_empty_max_filtered_is_refused_before_anything_runs() {
    // `[ "$n" -gt "" ]` errors, which `if` reads as "within the cap".
    let edits = GateEdits { max_filtered: Some((PROFILE, "")), ..Default::default() };
    let out = run_gate_with(PROFILE, "filtered-empty", &log_with_filtered(&[5]), 0, &edits);
    assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains(&format!("profile '{PROFILE}' has an empty 'max_filtered' field")),
        "{}",
        describe(&out)
    );
}

#[test]
fn a_result_line_cut_before_filtered_out_is_refused_as_torn() {
    // The whole-line check now reaches `filtered out`, so a line that stops
    // after `ignored; ` cannot hide its `3 filtered out` from the sum.
    let mut log = e2e_evidence_for(PROFILE);
    log.push_str(&hooked_binary("torn_e2e"));
    log.push_str("test result: ok. 4 passed; 0 failed; 0 ignored; \n");
    log.push_str("0 measured; 3 filtered out; finished in 0.01s\n");
    let out = run_gate_with(PROFILE, "filtered-torn", &log, 0, &GateEdits::default());
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("1 `test result:` line(s) are not whole"),
        "{}",
        describe(&out)
    );
}
