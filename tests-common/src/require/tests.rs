use super::*;

const TEST_KNOB: RequireKnob = RequireKnob::new("KASTELLAN_TEST_REQUIRE_E2E", "test-tier");

/// The dialect, in both directions. A strict `Some("1")` rule is the skew
/// #654 was filed about, so `true`/`YES`/` on ` must all demand.
#[test]
fn the_flag_dialect_decides_whether_a_run_is_demanded() {
    for truthy in ["1", "true", "TRUE", "yes", "on", "  on  "] {
        assert_eq!(
            unmet_action(Some(truthy.to_string())),
            UnmetAction::Fail,
            "{truthy:?} must demand a real run"
        );
    }
    for falsey in ["0", "false", "no", "off", ""] {
        assert_eq!(
            unmet_action(Some(falsey.to_string())),
            UnmetAction::Skip,
            "{falsey:?} must not demand"
        );
    }
    assert_eq!(unmet_action(None), UnmetAction::Skip, "unset is the default");
}

/// An out-of-dialect value is operator error, and silently treating it as
/// unset hands back the green run the operator was ruling out.
#[test]
fn an_out_of_dialect_value_warns_and_names_the_knob() {
    let mut sink = Vec::new();
    warn_if_out_of_dialect("KASTELLAN_X_REQUIRE_E2E", Some("y"), &mut sink);
    let got = String::from_utf8(sink).expect("utf8");
    assert!(got.contains("[WARN]"), "must warn: {got:?}");
    assert!(got.contains("KASTELLAN_X_REQUIRE_E2E"), "must name the knob: {got:?}");
    assert!(got.contains("will NOT become failures"), "must name the consequence: {got:?}");
}

/// ...but a deliberate opt-out is not a typo, and warning on it would train
/// the operator to ignore the warning.
#[test]
fn a_deliberate_opt_out_does_not_warn() {
    for quiet in [Some("0"), Some("false"), Some("OFF"), Some(""), Some("   "), None] {
        let mut sink = Vec::new();
        warn_if_out_of_dialect("KASTELLAN_X_REQUIRE_E2E", quiet, &mut sink);
        assert!(sink.is_empty(), "{quiet:?} must be silent, got {sink:?}");
    }
}

/// **The wiring, not the rule.** `action_reporting_to` must actually CALL
/// the dialect warning — deleting that call leaves every unit test above
/// green while restoring the silent skip.
#[test]
fn the_skip_arm_emits_the_dialect_warning_through_the_knob() {
    let mut sink = Vec::new();
    let action = TEST_KNOB.action_reporting_to(Some("y".into()), &mut sink);
    assert_eq!(action, UnmetAction::Skip, "an out-of-dialect value does not demand");
    let got = String::from_utf8(sink).expect("utf8");
    assert!(got.contains("KASTELLAN_TEST_REQUIRE_E2E"), "the knob warns by name: {got:?}");
}

/// A demanded run must NOT warn — there is nothing out of dialect, and a
/// warning on the success path is noise in exactly the log a gate reads.
#[test]
fn a_demanded_run_emits_no_dialect_warning() {
    let mut sink = Vec::new();
    let action = TEST_KNOB.action_reporting_to(Some("1".into()), &mut sink);
    assert_eq!(action, UnmetAction::Fail);
    assert!(sink.is_empty(), "no warning on the demanded path: {sink:?}");
}

/// The Skip arm **emits** a `[SKIP]` line, not merely renders one. A
/// dropped `eprint!` here would make a misleading run look clean.
#[test]
fn the_skip_arm_emits_a_skip_line_carrying_the_reason() {
    let mut sink = Vec::new();
    let got: Option<()> =
        TEST_KNOB.report_unmet_to(UnmetAction::Skip, "no cluster at /nowhere", &mut sink);
    assert!(got.is_none(), "the skip arm yields None so the caller returns");
    let rendered = String::from_utf8(sink).expect("utf8");
    assert!(rendered.contains("[SKIP]"), "must be greppable: {rendered:?}");
    assert!(rendered.contains("no cluster at /nowhere"), "carries the reason: {rendered:?}");
}

/// The Fail arm names **both** the knob and the reason. Naming only the
/// knob sends the operator hunting; naming only the reason reads as a
/// regression in whatever change is in flight.
#[test]
fn the_fail_arm_panics_naming_the_knob_the_tier_and_the_reason() {
    let err = std::panic::catch_unwind(|| {
        let _: Option<()> = TEST_KNOB.report_unmet(UnmetAction::Fail, "no cluster at /nowhere");
    })
    .expect_err("Fail must panic");
    let msg = err
        .downcast_ref::<String>()
        .map(String::as_str)
        .expect("panic payload is a String");
    assert!(msg.contains("KASTELLAN_TEST_REQUIRE_E2E"), "names the knob: {msg}");
    assert!(msg.contains("test-tier"), "names the tier: {msg}");
    assert!(msg.contains("no cluster at /nowhere"), "names the reason: {msg}");
}

/// A probe reason may span lines (both supervisor backends embed a `\n\n`
/// operator hint). A panic whose first line stops before the reason is the
/// archaeology the knob exists to spare the operator.
#[test]
fn the_fail_arm_flattens_a_multi_line_reason() {
    let err = std::panic::catch_unwind(|| {
        let _: Option<()> =
            TEST_KNOB.report_unmet(UnmetAction::Fail, "probe failed\n\n   start the manager");
    })
    .expect_err("Fail must panic");
    let msg = err.downcast_ref::<String>().map(String::as_str).expect("String payload");
    assert!(
        msg.contains("probe failed start the manager"),
        "the whole reason is on one line: {msg}"
    );
}

/// **The positive control.** A demanded run emits an `[E2E]` line naming
/// the tier and what was resolved, so a gate can assert a COUNT rather than
/// infer from the absence of `[SKIP]`.
#[test]
fn a_demanded_run_announces_itself_with_a_greppable_e2e_line() {
    let mut sink = Vec::new();
    TEST_KNOB.announce_to(UnmetAction::Fail, "cluster at /tmp/pg", &mut sink);
    let got = String::from_utf8(sink).expect("utf8");
    assert!(got.starts_with("\n[E2E] "), "must be its own greppable line: {got:?}");
    assert!(got.contains("test-tier"), "names the tier: {got:?}");
    assert!(got.contains("cluster at /tmp/pg"), "names what was resolved: {got:?}");
}

/// ...and a run nobody demanded stays quiet, so every `[E2E]` line in a
/// gate log means one thing: a **demanded** precondition was met here.
/// Without this, a plain `cargo test --workspace` would emit hundreds of
/// lines and the count would stop being evidence of anything.
#[test]
fn an_undemanded_run_announces_nothing() {
    let mut sink = Vec::new();
    TEST_KNOB.announce_to(UnmetAction::Skip, "cluster at /tmp/pg", &mut sink);
    assert!(sink.is_empty(), "no [E2E] line without the knob: {sink:?}");
}

/// **The success-path read.** `announce_demanded` exists for cascades whose
/// success arm never computed an action, so it must reach the environment
/// itself — a constant here would make every micro-VM `[E2E]` line either
/// unconditional or absent.
#[test]
fn announce_demanded_reads_the_environment_not_a_constant() {
    let _lock = crate::env::env_lock();

    let mut demanded = Vec::new();
    {
        let _set = crate::env::EnvVarGuard::set("KASTELLAN_TEST_REQUIRE_E2E", "1");
        TEST_KNOB.announce_demanded_to("fixture staged", &mut demanded);
    }
    let got = String::from_utf8(demanded).expect("utf8");
    assert!(got.contains("[E2E]"), "a demanded run announces: {got:?}");
    assert!(got.contains("fixture staged"), "carries the detail: {got:?}");

    let mut undemanded = Vec::new();
    {
        let _unset = crate::env::EnvVarGuard::unset("KASTELLAN_TEST_REQUIRE_E2E");
        TEST_KNOB.announce_demanded_to("fixture staged", &mut undemanded);
    }
    assert!(undemanded.is_empty(), "no knob, no line: {undemanded:?}");
}

/// ...and it must NOT warn on the success path. The dialect warning belongs
/// to the decision; repeated once per precondition per test it would drown
/// the log a gate greps.
#[test]
fn announce_demanded_does_not_emit_the_dialect_warning() {
    let _lock = crate::env::env_lock();
    let _set = crate::env::EnvVarGuard::set("KASTELLAN_TEST_REQUIRE_E2E", "y");

    let mut sink = Vec::new();
    TEST_KNOB.announce_demanded_to("fixture staged", &mut sink);
    assert!(sink.is_empty(), "out-of-dialect is not demanded, and does not warn here: {sink:?}");
}

/// A knob set to a non-UTF-8 value must not read as *unset*. `.ok()` would
/// discard it, and `warn_if_out_of_dialect` returns early on `None`, so the
/// operator would get a silent skip from a knob they demonstrably set.
#[test]
fn a_non_utf8_knob_value_warns_rather_than_reading_as_unset() {
    let _lock = crate::env::env_lock();

    let mut sink = Vec::new();
    let action = TEST_KNOB.action_reporting_to(Some("\u{fffd}".into()), &mut sink);
    assert_eq!(action, UnmetAction::Skip, "a non-UTF-8 value cannot be truthy");
    let got = String::from_utf8(sink).expect("utf8");
    assert!(got.contains("[WARN]"), "it must leave a trace: {got:?}");
}

/// The vocabulary is what `scripts/run-e2e-gate.sh` is pinned against, so a
/// stray name or a duplicated tier would weaken that check silently.
#[test]
fn the_knob_vocabulary_is_well_formed() {
    assert!(!KNOBS.is_empty(), "an empty vocabulary would make every check over it vacuous");
    for knob in KNOBS {
        assert!(knob.env().starts_with("KASTELLAN_"), "{} is not one of ours", knob.env());
        assert!(knob.env().ends_with("_REQUIRE_E2E"), "{} is not a REQUIRE knob", knob.env());
        assert!(!knob.tier().is_empty(), "{} has no tier phrase", knob.env());
    }

    // Tiers must be distinct: the gate script counts `[E2E] <tier>:` lines
    // per tier, so two knobs sharing a phrase would make one floor
    // satisfiable by the other's evidence — the very substitution the
    // per-tier floors exist to prevent.
    let mut tiers: Vec<&str> = KNOBS.iter().map(RequireKnob::tier).collect();
    tiers.sort_unstable();
    let before = tiers.len();
    tiers.dedup();
    assert_eq!(before, tiers.len(), "two knobs share a tier phrase: {tiers:?}");
}

/// `[E2E]`, `[SKIP]` and `[WARN]` are counted separately when a run is
/// audited, so none may be mistakable for another. A positive control that
/// inflated the skip count would misattribute a test that actually ran.
#[test]
fn the_three_evidence_markers_are_mutually_distinguishable() {
    let e2e = e2e_line("tier", "detail");
    assert!(!e2e.contains("[SKIP]"), "a success is not a skip: {e2e:?}");
    assert!(!e2e.contains("[WARN]"), "a success is not a warning: {e2e:?}");
    assert!(!skip_line("r").contains("[E2E]"), "a skip is not a success");
    assert!(!warn_line("r").contains("[E2E]"), "a warning is not a success");
}

// ---------------------------------------------------------------------------
// #755: every emitter writes its marker as ONE framed write.
//
// The renderers (`skip_line`, `warn_line`, `e2e_line`) already frame their
// lines, and `skip.rs` pins that. What these pin is the EMITTER: that it hands
// the framed string over in a single write, rather than, say, `writeln!` of a
// trimmed copy — two writes, the second of which another thread can precede.
// ---------------------------------------------------------------------------

use crate::write_recorder::WriteRecorder;

#[test]
fn the_dialect_warning_is_one_framed_write() {
    let mut out = WriteRecorder::default();
    warn_if_out_of_dialect("KASTELLAN_TEST_REQUIRE_E2E", Some("y"), &mut out);
    out.assert_one_framed_line("[WARN]");
}

#[test]
fn the_skip_line_is_one_framed_write() {
    let mut out = WriteRecorder::default();
    let _: Option<()> = TEST_KNOB.report_unmet_to(UnmetAction::Skip, "fixture absent", &mut out);
    out.assert_one_framed_line("[SKIP]");
}

#[test]
fn the_e2e_announcement_is_one_framed_write() {
    let mut out = WriteRecorder::default();
    TEST_KNOB.announce_to(UnmetAction::Fail, "fixture staged", &mut out);
    out.assert_one_framed_line("[E2E]");
}

#[test]
fn the_microvm_caveat_is_one_framed_write() {
    let _lock = crate::env::env_lock();
    let _unset = crate::env::EnvVarGuard::unset(crate::microvm::REQUIRE_ENV);
    let mut out = WriteRecorder::default();
    crate::microvm::report_caveat_microvm_to("image freshness unknown", &mut out);
    out.assert_one_framed_line("[WARN]");
}

/// The micro-VM tier reads its knob itself (`require_action_to`), not through
/// [`RequireKnob::action`], so it must not bring back the lossy read `raw()`
/// was written to retire: `std::env::var(..).ok()` turns a non-UTF-8 value
/// into `None`, which skips with no `[WARN]` — a knob the operator set, and no
/// trace that it was ignored.
#[cfg(unix)]
#[test]
fn a_non_utf8_microvm_knob_warns_rather_than_reading_as_unset() {
    use std::os::unix::ffi::OsStrExt;
    let _lock = crate::env::env_lock();
    let _restore = crate::env::EnvVarGuard::unset(crate::microvm::REQUIRE_ENV);
    std::env::set_var(crate::microvm::REQUIRE_ENV, std::ffi::OsStr::from_bytes(b"\xff"));

    let mut out = WriteRecorder::default();
    let action = crate::microvm::require_action_to(&mut out);
    assert_eq!(action, UnmetAction::Skip, "a non-UTF-8 value cannot be truthy");
    out.assert_one_framed_line("[WARN]");
}
