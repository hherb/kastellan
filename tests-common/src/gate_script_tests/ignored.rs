//! #876: the gate reads libtest's `N ignored` and demands an exact, per-profile
//! count of it (a cap until #882).
//!
//! Before this, nothing read that field. `#[ignore]` every test but one in each
//! binary of a profile, and the gate still printed ✅: the survivors cleared
//! `MIN_PASSED`, each announced its `[E2E]` line and reached the panic hook,
//! while the rest — containment tests among them — stopped running unseen.
//!
//! The count is a MEASURED number per profile, not a blanket zero: the
//! `worker-report` suites carry `#[ignore]`d *inner fixtures* that their parent
//! tests re-run in a child process, and a zero there would make a healthy gate
//! red on every host. ⚠️ Exact, not a cap (#882): under `≤`, deleting one inner
//! fixture left room for one real test to gain an `#[ignore]` unseen. Profiles
//! that pass `--ignored` (`microvm`, `mail-live`) report 0 ignored by
//! construction — libtest counts the non-ignored tests as `filtered out` — and
//! #880's `FILTERED_OUT` (`filtered.rs`) is what gates them.

use super::profiles;
use super::run::{
    describe, e2e_evidence_for, hooked_binary, run_gate_with, GateEdits, PROFILE,
};

/// One binary's libtest summary with `ignored` tests in it.
fn summary(passed: u32, ignored: u32) -> String {
    format!(
        "test result: ok. {passed} passed; 0 failed; {ignored} ignored; 0 measured; \
         0 filtered out; finished in 0.01s\n"
    )
}

/// A healthy run of [`PROFILE`] whose binaries report these `ignored` counts.
fn log_with_ignored(per_binary: &[u32]) -> String {
    let mut log = e2e_evidence_for(PROFILE);
    for (i, ignored) in per_binary.iter().enumerate() {
        log.push_str(&hooked_binary(&format!("suite_{i}_e2e")));
        log.push_str(&summary(5, *ignored));
    }
    log
}

/// [`PROFILE`]'s committed count, which these tests assume is 0.
#[test]
fn the_profile_these_tests_drive_expects_zero_ignored() {
    let p = profiles().into_iter().find(|p| p.name == PROFILE).expect("PROFILE row");
    assert_eq!(p.ignored, "0", "the tests below assume `{PROFILE}` expects IGNORED = 0");
}

#[test]
fn one_ignored_test_fails_a_profile_expecting_zero() {
    // The acceptance test the issue names: `1 ignored`, refused, and the
    // refusal names the field rather than crashing.
    let out = run_gate_with(PROFILE, "ignored-one", &log_with_ignored(&[1]), 0, &GateEdits::default());
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .contains("1 test(s) reported `ignored`, expected exactly 0 (IGNORED)"),
        "{}",
        describe(&out)
    );
}

#[test]
fn ignored_counts_are_summed_across_binaries_and_must_match_exactly() {
    // A count of 2 on a copy: 1 + 1 across two binaries passes; 1 + 2 and
    // 0 + 1 are both refused. A per-binary check fails the first half.
    let edits = GateEdits { ignored: Some((PROFILE, "2")), ..Default::default() };
    let at = run_gate_with(PROFILE, "ignored-exact", &log_with_ignored(&[1, 1]), 0, &edits);
    assert!(at.status.success(), "a count of 2 must allow 2\n{}", describe(&at));
    let over = run_gate_with(PROFILE, "ignored-over", &log_with_ignored(&[1, 2]), 0, &edits);
    assert_eq!(over.status.code(), Some(1), "a count of 2 must refuse 3\n{}", describe(&over));
    assert!(
        String::from_utf8_lossy(&over.stdout).contains("3 test(s) reported `ignored`, expected exactly 2"),
        "{}",
        describe(&over)
    );
    // Below (#882): an inner fixture went away, leaving room for a real test
    // to gain an `#[ignore]` with the sum back at 2.
    let under = run_gate_with(PROFILE, "ignored-under", &log_with_ignored(&[0, 1]), 0, &edits);
    assert_eq!(under.status.code(), Some(1), "a count of 2 must refuse 1\n{}", describe(&under));
    assert!(
        String::from_utf8_lossy(&under.stdout)
            .contains("only 1 test(s) reported `ignored`, expected exactly 2 (IGNORED)"),
        "{}",
        describe(&under)
    );
}

#[test]
fn a_non_numeric_ignored_is_refused_before_anything_runs() {
    // `any` too: an unchecked ignored count is the hole this column closes.
    for value in ["any", "zero"] {
        let edits = GateEdits { ignored: Some((PROFILE, value)), ..Default::default() };
        let out = run_gate_with(PROFILE, "ignored-malformed", &log_with_ignored(&[0]), 0, &edits);
        assert_eq!(out.status.code(), Some(2), "{value}: {}", describe(&out));
        assert!(
            String::from_utf8_lossy(&out.stderr)
                .contains(&format!("profile '{PROFILE}' IGNORED must be a number")),
            "{value}: {}",
            describe(&out)
        );
        assert!(!String::from_utf8_lossy(&out.stdout).contains("==> evidence"), "{}", describe(&out));
    }
}

#[test]
fn an_empty_ignored_is_refused_before_anything_runs() {
    // An empty count makes `[ "$n" -gt "" ]` error, which `if` reads as "not
    // over" — the empty-operand false green this check exists to refuse.
    let edits = GateEdits { ignored: Some((PROFILE, "")), ..Default::default() };
    let out = run_gate_with(PROFILE, "ignored-empty", &log_with_ignored(&[5]), 0, &edits);
    assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains(&format!("profile '{PROFILE}' has an empty 'ignored' field")),
        "{}",
        describe(&out)
    );
}

#[test]
fn a_torn_test_result_line_is_refused() {
    // libtest flushes `\ntest result: `, `ok` and `. N passed; …` separately,
    // so stderr between them pushes `3 ignored` onto a line the sum never reads.
    // Without the whole-line check this log reads as 0 ignored and passes.
    let mut log = e2e_evidence_for(PROFILE);
    log.push_str(&hooked_binary("torn_e2e"));
    log.push_str("test result: worker exited: status 0\n");
    log.push_str("ok. 4 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out; finished in 0.01s\n");
    log.push_str(&hooked_binary("whole_e2e"));
    log.push_str(&summary(5, 0));
    let out = run_gate_with(PROFILE, "torn-result", &log, 0, &GateEdits::default());
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("1 `test result:` line(s) are not whole"),
        "{}",
        describe(&out)
    );
}
