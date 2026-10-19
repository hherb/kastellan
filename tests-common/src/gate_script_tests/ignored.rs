//! #876: the gate reads libtest's `N ignored` and caps it per profile.
//!
//! Before this, nothing read that field. `#[ignore]` every test but one in each
//! binary of a profile, and the gate still printed ✅: the one survivor per
//! binary cleared `MIN_PASSED`, announced its `[E2E]` line and reached the
//! panic hook, while the rest — containment tests among them — stopped running
//! unseen.
//!
//! The cap is a MEASURED number per profile, not a blanket zero: the
//! `worker-report` suites carry `#[ignore]`d *inner fixtures* that their parent
//! tests re-run in a child process, and a zero there would make a healthy gate
//! red on every host. Profiles that pass `--ignored` (micro-VM, mail-live)
//! report 0 ignored by construction — libtest counts the non-ignored tests as
//! `filtered out` — so a cap of 0 holds for them too.

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

/// [`PROFILE`]'s committed cap, which these tests assume is 0.
#[test]
fn the_profile_these_tests_drive_caps_ignored_at_zero() {
    let p = profiles().into_iter().find(|p| p.name == PROFILE).expect("PROFILE row");
    assert_eq!(p.max_ignored, "0", "the tests below assume `{PROFILE}` caps MAX_IGNORED at 0");
}

#[test]
fn one_ignored_test_fails_a_profile_capped_at_zero() {
    // The acceptance test the issue names: `1 ignored`, refused, and the
    // refusal names the field rather than crashing.
    let out = run_gate_with(PROFILE, "ignored-one", &log_with_ignored(&[1]), 0, &GateEdits::default());
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("1 test(s) reported `ignored`, max is 0"),
        "{}",
        describe(&out)
    );
}

#[test]
fn ignored_counts_are_summed_across_binaries_and_the_cap_is_inclusive() {
    // A cap of 2 on a copy: 1 + 1 across two binaries passes, 1 + 2 is refused.
    // A per-binary check, or `-ge` for `-gt`, fails one of the two halves.
    let edits = GateEdits { max_ignored: Some((PROFILE, "2")), ..Default::default() };
    let at = run_gate_with(PROFILE, "ignored-at-cap", &log_with_ignored(&[1, 1]), 0, &edits);
    assert!(at.status.success(), "a cap of 2 must allow 2\n{}", describe(&at));
    let over = run_gate_with(PROFILE, "ignored-over", &log_with_ignored(&[1, 2]), 0, &edits);
    assert_eq!(over.status.code(), Some(1), "a cap of 2 must refuse 3\n{}", describe(&over));
    assert!(
        String::from_utf8_lossy(&over.stdout).contains("3 test(s) reported `ignored`, max is 2"),
        "{}",
        describe(&over)
    );
}

#[test]
fn a_non_numeric_max_ignored_is_refused_before_anything_runs() {
    // `any` too: an uncapped ignored count is the hole this column closes.
    for value in ["any", "zero"] {
        let edits = GateEdits { max_ignored: Some((PROFILE, value)), ..Default::default() };
        let out = run_gate_with(PROFILE, "ignored-malformed", &log_with_ignored(&[0]), 0, &edits);
        assert_eq!(out.status.code(), Some(2), "{value}: {}", describe(&out));
        assert!(
            String::from_utf8_lossy(&out.stderr)
                .contains(&format!("profile '{PROFILE}' MAX_IGNORED must be a number")),
            "{value}: {}",
            describe(&out)
        );
        assert!(!String::from_utf8_lossy(&out.stdout).contains("==> evidence"), "{}", describe(&out));
    }
}
