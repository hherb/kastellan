//! Does each report reach someone: the delivery check answers for the
//! emitter's own target, at every arm and for every shipping emitter.
//! The fd-writability probe's tests are in [`fd`].

use super::super::shared::STDERR_FALLBACK_MARKERS;

mod fd;

/// A stand-in emitter living in its **own module**, exactly as the four
/// real ones do.
///
/// The point of the nesting: this module's path differs from
/// `…::report::delivery`, where a "simplified" shared check would live. A
/// filter that enables one and not the other therefore tells the two
/// designs apart, which is what [`the_check_answers_for_the_emitters_own_target`]
/// does.
mod pretend_emitter {
    pub fn emit(line: &str) -> bool {
        super::super::warn_and_fall_back!("[worker-failed]", line)
    }

    pub fn emit_with_label(line: &str, label: &str) -> bool {
        super::super::warn_and_fall_back!("[worker-failed]", line, label = label)
    }

    pub fn emit_error_with_label(line: &str, label: &str) -> bool {
        super::super::warn_and_fall_back!("[worker-failed]", line, label = label, level = ERROR)
    }

    pub fn emit_info_with_label(line: &str, label: &str) -> bool {
        super::super::warn_and_fall_back!("[worker-failed]", line, label = label, level = INFO)
    }

    /// This module's own path, so a test can build a directive naming it
    /// without hardcoding a string that silently rots if the module moves.
    pub fn target() -> &'static str {
        module_path!()
    }
}

/// Run `f` with a subscriber built from `directive`, and report both what
/// the emitter decided and what the subscriber actually **recorded**.
///
/// Recording the events rather than trusting the predicate is the whole
/// design: the claim under test is "the fallback fires exactly when
/// `tracing` did not carry the report", and a test that asked
/// `event_enabled!` itself would be checking the implementation against
/// itself.
fn under(directive: &str, f: impl FnOnce() -> bool) -> (bool, bool) {
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("sink mutex").extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
        type Writer = Sink;
        fn make_writer(&'a self) -> Sink {
            self.clone()
        }
    }

    let sink = Sink::default();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(directive))
        .with_writer(sink.clone())
        .with_ansi(false)
        .finish();
    // Scoped, not global: these tests run on libtest's threads alongside
    // everything else in this binary, and a global install would leak into
    // every later test in the process.
    let fell_back = tracing::subscriber::with_default(subscriber, f);
    let recorded = String::from_utf8_lossy(&sink.0.lock().expect("sink mutex")).into_owned();
    (fell_back, recorded.contains(PROBE_LINE))
}

/// The text each macro invocation carries, distinctive enough that finding
/// it in the sink cannot be an accident.
const PROBE_LINE: &str = "kastellan-test: delivery probe";

#[test]
fn the_check_answers_for_the_emitters_own_target() {
    // The regression this pins: someone "simplifies" the macro away into a
    // helper function in THIS module. `event_enabled!` would then carry
    // `…::report::delivery` as its target instead of the emitter's, and a
    // directive that silences the emitter would make the check say
    // "delivered" for an event `EnvFilter` dropped — #734 reproduced
    // inside its own fix.
    //
    // ⚠️ **It has to be `warn,<emitter>=off` and not `<this module>=warn`.**
    // `EnvFilter` matches targets by **PREFIX**, so a directive naming an
    // ancestor enables every descendant — the first draft of this test
    // named this module and silently enabled the emitter nested inside it,
    // which the positive control below caught. Only a directive that
    // enables everything EXCEPT the emitter's own path can tell a check at
    // the call site from one in any enclosing module.
    let directive = format!("warn,{}=off", pretend_emitter::target());
    let (fell_back, recorded) = under(&directive, || pretend_emitter::emit(PROBE_LINE));
    assert!(
        !recorded,
        "POSITIVE CONTROL: `{directive}` must DROP the emitter's own warn, or this test \
         cannot tell the two designs apart"
    );
    assert!(
        fell_back,
        "the delivery check must answer for the EMITTER's target ({}), not for whichever \
         module the check happens to be written in. Under `{directive}` the report reached \
         NOBODY — but every enclosing module is still enabled, so a check written in `{}` \
         would have called it delivered and stayed quiet. Keep the check expanding at the \
         call site: that is the entire reason `warn_and_fall_back!` is a macro.",
        pretend_emitter::target(),
        module_path!()
    );
}

#[test]
fn a_recorded_event_does_not_also_fall_back() {
    // The other direction, and the reason the fix is not just "always
    // eprintln!": a daemon whose subscriber DOES record the warn must not
    // get every worker report twice in its log.
    let directive = format!("{}=warn", pretend_emitter::target());
    let (fell_back, recorded) = under(&directive, || pretend_emitter::emit(PROBE_LINE));
    assert!(
        recorded,
        "POSITIVE CONTROL: a directive naming the emitter's own target must enable it, or \
         the assertion below passes vacuously. Directive: {directive}"
    );
    assert!(
        !fell_back,
        "an event `tracing` DID record must not also take the stderr fallback, or every \
         report is doubled in the daemon's log"
    );
}

#[test]
fn the_operators_scheduler_directive_still_gets_the_report() {
    // #734's own scenario, at unit scale: the directive names a real target
    // that is not this one. Both channels were silent before the fix.
    let (fell_back, recorded) = under("kastellan_core::scheduler=debug", || {
        pretend_emitter::emit(PROBE_LINE)
    });
    assert!(!recorded, "the scheduler directive must not enable the emitter's target");
    assert!(
        fell_back,
        "a target-scoped `RUST_LOG` is operator-settable through the `kastellan.env.local` \
         overlay, and before #734 it silenced both channels at once"
    );
}

#[test]
fn a_field_scoped_off_directive_does_not_fool_the_labelled_check() {
    // Measured, and the reason the labelled arm repeats `label` in its
    // `event_enabled!`: `EnvFilter` matches field-carrying events with a
    // different set of directives than bare ones, so a BARE check is
    // fail-OPEN here — it reports `true` for an event this directive drops.
    let directive = format!("warn,{}[{{label}}]=off", pretend_emitter::target());
    // Both labelled arms: the ERROR one (#783) repeats the rule at its own
    // level, so it can get it wrong on its own.
    for (arm, fired) in [
        (
            "labelled WARN",
            Box::new(|| pretend_emitter::emit_with_label(PROBE_LINE, "matrix"))
                as Box<dyn Fn() -> bool>,
        ),
        (
            "labelled ERROR",
            Box::new(|| pretend_emitter::emit_error_with_label(PROBE_LINE, "matrix")),
        ),
        (
            "labelled INFO",
            Box::new(|| pretend_emitter::emit_info_with_label(PROBE_LINE, "matrix")),
        ),
    ] {
        let (fell_back, recorded) = under(&directive, fired);
        assert!(
            !recorded,
            "POSITIVE CONTROL for the {arm} arm: a field-scoped `off` must drop the \
             labelled event, or this test is not exercising the disagreement it exists \
             for. Directive: {directive}"
        );
        assert!(
            fell_back,
            "the {arm} arm's check must declare the same `label` field its event does. A \
             bare `event_enabled!` answers `true` under `{directive}` while the event is \
             dropped — silence, which is the failure mode this module exists to remove."
        );
    }
}

#[test]
fn the_error_arm_checks_at_its_own_level() {
    // An `error!` checked at WARN answers for the wrong directive set: under
    // `<target>=warn` both levels are on, so only a directive that keeps
    // ERROR and drops WARN tells them apart — and in that one the ERROR
    // report IS recorded, so it must not also fall back.
    let directive = format!("{}=error", pretend_emitter::target());
    let (fell_back, recorded) =
        under(&directive, || pretend_emitter::emit_error_with_label(PROBE_LINE, "matrix"));
    assert!(recorded, "POSITIVE CONTROL: `{directive}` must record an ERROR event");
    assert!(!fell_back, "a recorded ERROR report must not also take the stderr fallback");
    // And a plain round trip: with the target off, the report falls back.
    let directive = format!("{}=off", pretend_emitter::target());
    let (fell_back, recorded) =
        under(&directive, || pretend_emitter::emit_error_with_label(PROBE_LINE, "matrix"));
    assert!(!recorded, "POSITIVE CONTROL: `{directive}` must drop it");
    assert!(fell_back, "a dropped ERROR report must fall back");
}

#[test]
fn the_info_arm_checks_at_its_own_level() {
    // #788's recovery line. The mirror image of the ERROR case: an `info!`
    // checked at WARN answers "recorded" under a WARN-only directive that
    // drops it, so the line reaches nobody. That directive is the test.
    let directive = format!("{}=warn", pretend_emitter::target());
    let (fell_back, recorded) =
        under(&directive, || pretend_emitter::emit_info_with_label(PROBE_LINE, "matrix"));
    assert!(!recorded, "POSITIVE CONTROL: `{directive}` must drop an INFO event");
    assert!(fell_back, "a dropped INFO report must fall back, not be checked at WARN");
    // And the recorded direction: no second copy on stderr.
    let directive = format!("{}=info", pretend_emitter::target());
    let (fell_back, recorded) =
        under(&directive, || pretend_emitter::emit_info_with_label(PROBE_LINE, "matrix"));
    assert!(recorded, "POSITIVE CONTROL: `{directive}` must record an INFO event");
    assert!(!fell_back, "a recorded INFO report must not also take the stderr fallback");
}

#[test]
fn a_message_scoped_off_directive_fools_neither_arm() {
    // The hole #745's own review found in #745's fix, and the reason this
    // test covers BOTH arms where the `label` one covers only the second.
    //
    // A `warn!("{line}")` carries an implicit `message` field. The checks
    // originally declared `label` (second arm) or nothing (first arm), so
    // under a `[{message}]` predicate `event_enabled!` answered `true`
    // while `EnvFilter` dropped the event — both channels silent, which is
    // #734 verbatim, one field further along than the version the macro's
    // doc already tabulated.
    // The base is `info`, not `warn`, so the INFO arm's event is enabled by
    // everything but the `message` predicate — otherwise it would fall back
    // for its level alone and the row would test nothing.
    let directive = format!("info,{}[{{message}}]=off", pretend_emitter::target());
    for (arm, fired) in [
        ("bare", Box::new(|| pretend_emitter::emit(PROBE_LINE)) as Box<dyn Fn() -> bool>),
        (
            "labelled",
            Box::new(|| pretend_emitter::emit_with_label(PROBE_LINE, "matrix")),
        ),
        (
            "labelled ERROR",
            Box::new(|| pretend_emitter::emit_error_with_label(PROBE_LINE, "matrix")),
        ),
    ] {
        let (fell_back, recorded) = under(&directive, fired);
        assert!(
            !recorded,
            "POSITIVE CONTROL for the {arm} arm: `{directive}` must DROP the warn, or this \
             test is not exercising the disagreement it exists for"
        );
        assert!(
            fell_back,
            "the {arm} arm's check must declare the implicit `message` field its `warn!` \
             carries. Without it `event_enabled!` answers `true` under `{directive}` while \
             the event is dropped, and the report reaches NOBODY. Every field the `warn!` \
             carries must be named in the check — `message` included."
        );
    }
}


/// Every emitter that actually ships, with the module its `warn!` is
/// written in.
///
/// ⚠️ **A new emitter must join this array.** Nothing forces it — a fifth
/// `emit_*` that called a plain function instead of `warn_and_fall_back!`
/// would be silently un-guarded, which is the whole failure this array
/// exists to prevent. [`every_shipping_emitter_is_covered`] is the only
/// thing that notices, and it can only count what it is given.
#[allow(clippy::type_complexity)]
fn shipping_emitters() -> Vec<(&'static str, &'static str, Box<dyn Fn() -> bool>)> {
    const TOOL_WORKER: &str = "kastellan_core::worker_stderr::report::tool_worker";
    const PERSISTENT: &str = "kastellan_core::worker_stderr::report::persistent";
    const REFUSAL: &str = "kastellan_core::worker_stderr::report::refusal";
    vec![
        (
            "emit_worker_failure_report",
            TOOL_WORKER,
            Box::new(|| crate::worker_stderr::emit_worker_failure_report(PROBE_LINE)),
        ),
        (
            "emit_persistent_death_report",
            PERSISTENT,
            Box::new(|| crate::worker_stderr::emit_persistent_death_report("matrix", PROBE_LINE)),
        ),
        (
            "emit_persistent_down_report",
            PERSISTENT,
            Box::new(|| crate::worker_stderr::emit_persistent_down_report("matrix", PROBE_LINE)),
        ),
        // The ERROR severity: the credential line, the one that names an
        // operator action. The WARN and INFO severities are driven by
        // `every_refusal_severity_checks_delivery_at_its_own_callsite`,
        // because this array holds one row per marker.
        (
            "emit_worker_refusal_report",
            REFUSAL,
            Box::new(|| {
                crate::worker_stderr::emit_worker_refusal_report(
                    "matrix",
                    PROBE_LINE,
                    crate::worker_stderr::RefusalSeverity::Error,
                )
            }),
        ),
    ]
}

#[test]
fn every_refusal_severity_checks_delivery_at_its_own_callsite() {
    use crate::worker_stderr::{emit_worker_refusal_report, RefusalSeverity};
    const REFUSAL: &str = "kastellan_core::worker_stderr::report::refusal";
    for severity in [RefusalSeverity::Info, RefusalSeverity::Warn, RefusalSeverity::Error] {
        let emit = || emit_worker_refusal_report("matrix", PROBE_LINE, severity);
        let (fell_back, recorded) = under(&format!("info,{REFUSAL}=off"), emit);
        assert!(!recorded, "POSITIVE CONTROL for {severity:?}: the directive must drop it");
        assert!(fell_back, "{severity:?}: a dropped refusal report must fall back to stderr");
        let (fell_back, recorded) = under(&format!("{REFUSAL}=info"), emit);
        assert!(recorded, "POSITIVE CONTROL for {severity:?}: the directive must record it");
        assert!(!fell_back, "{severity:?}: a recorded refusal report must not also fall back");
    }
}

#[test]
fn every_shipping_emitter_checks_delivery_at_its_own_callsite() {
    // The blind spot this closes: every other test in this file drives
    // `pretend_emitter`, which proves the MACRO works and nothing about the
    // four functions that actually ship. A refactor replacing the macro in
    // `persistent.rs` with a helper call would pass all of them.
    for (name, target, emit) in shipping_emitters() {
        // Enable everything EXCEPT this emitter's own module. A check at
        // the call site sees `false`; a check written in any enclosing
        // module — or in `delivery` — sees `true` and stays silent.
        let directive = format!("warn,{target}=off");
        let (fell_back, recorded) = under(&directive, emit);
        assert!(
            !recorded,
            "POSITIVE CONTROL for {name}: `{directive}` must drop its warn. If it did not, \
             `{target}` is no longer the module the `warn!` is written in and this test is \
             checking nothing — update the const beside it."
        );
        assert!(
            fell_back,
            "{name} must fall back to stderr when its own event is filtered away. It did \
             not, which means its delivery check is answering for some module other than \
             `{target}` — the #734 fail-open, reintroduced. Keep it on \
             `warn_and_fall_back!`, which expands the check at the call site."
        );
    }
}

#[test]
fn every_shipping_emitter_is_covered() {
    // Without this, deleting a row above would make the test pass while
    // silently dropping an emitter from the guard.
    assert_eq!(
        shipping_emitters().len(),
        STDERR_FALLBACK_MARKERS.len(),
        "there is exactly one shipping emitter per fallback marker; if a fifth marker was \
         added, its emitter must join `shipping_emitters` or it is unguarded"
    );
    // ⚠️ **Counts alone are not the census.** A count check is satisfied by
    // two emitters sharing one marker, or by the same emitter listed
    // twice — both of which leave a marker with no emitter actually
    // driven. Comparing the marker each emitter PRODUCES against the
    // declared set closes that, which is the difference between a guard
    // and a tally [[guard-shares-the-census-blind-spot]].
    let mut produced: Vec<String> = shipping_emitters()
        .into_iter()
        .map(|(name, _, emit)| {
            // No subscriber in scope ⇒ the fallback fires ⇒ the marked
            // line is written. Capturing it is not possible here (libtest
            // swallows a passing test's stderr), so the marker is taken
            // from the renderer the emitter is required to use.
            let (fell_back, _) = under("off", &emit);
            assert!(fell_back, "{name} must fall back under an `off` directive");
            marker_of(name).to_string()
        })
        .collect();
    produced.sort();
    produced.dedup();
    let mut declared: Vec<String> =
        STDERR_FALLBACK_MARKERS.iter().map(|m| m.to_string()).collect();
    declared.sort();
    assert_eq!(
        produced, declared,
        "every declared fallback marker must have exactly one emitter in \
         `shipping_emitters`, and no two emitters may share one. Counts agreeing is not \
         enough: two rows carrying the same marker pass a length check while leaving a \
         marker undriven"
    );
}

/// The marker each shipping emitter is required to write, named beside the
/// emitter rather than derived from it — a census the test can disagree
/// with is a census worth having.
fn marker_of(emitter: &str) -> &'static str {
    match emitter {
        "emit_worker_failure_report" => crate::worker_stderr::WORKER_FAILED_STDERR_MARKER,
        "emit_persistent_death_report" => crate::worker_stderr::WORKER_DEATH_STDERR_MARKER,
        "emit_persistent_down_report" => crate::worker_stderr::WORKER_DOWN_STDERR_MARKER,
        "emit_worker_refusal_report" => crate::worker_stderr::WORKER_REFUSAL_STDERR_MARKER,
        other => panic!(
            "a new shipping emitter `{other}` must name the marker it writes here, or \
             `every_shipping_emitter_is_covered` cannot tell whether it is guarded"
        ),
    }
}

#[test]
fn a_shipping_emitter_recorded_by_tracing_does_not_also_fall_back() {
    // The other direction for the real functions: no double-reporting in a
    // daemon whose subscriber does carry the event.
    for (name, target, emit) in shipping_emitters() {
        let directive = format!("{target}=warn");
        let (fell_back, recorded) = under(&directive, emit);
        assert!(recorded, "POSITIVE CONTROL for {name}: `{directive}` must enable its warn");
        assert!(
            !fell_back,
            "{name} reported on BOTH channels; the daemon's log would carry every worker \
             report twice"
        );
    }
}
