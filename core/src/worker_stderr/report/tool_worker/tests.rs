use super::*;
use crate::worker_stderr::ReadFault;

/// Every cause, so a test can iterate rather than name four of five.
///
/// ⚠️ Its length is asserted in [`every_cause_is_covered`] below, which is
/// the positive control: without it, a sixth cause would be classified by
/// `from_client_error`, break nothing here, and quietly get no wording of
/// its own.
const ALL_CAUSES: [WorkerRetirementCause; 5] = [
    WorkerRetirementCause::ExitedBeforeResponding,
    WorkerRetirementCause::PipeBroke,
    WorkerRetirementCause::Undecodable,
    WorkerRetirementCause::IdMismatch,
    WorkerRetirementCause::ResponseTooLarge,
];

#[test]
fn every_cause_is_covered() {
    // `WorkerRetirementCause` has no `#[non_exhaustive]`, so adding a
    // variant breaks `happened_clause` and `silent_hint` (both matches are
    // exhaustive). Nothing would force THIS list to grow, though, and every
    // test below iterates it.
    assert_eq!(ALL_CAUSES.len(), 5);
}

#[test]
fn every_cause_names_the_worker_the_method_and_what_went_wrong() {
    // The invariant #737 is about: whichever of the five retired the
    // worker, the report has to be usable. Naming the program matters more
    // than it looks — on the micro-VM path the process that exits is
    // `kastellan-microvm-run` while the tool is whatever ran in the guest.
    for cause in ALL_CAUSES {
        let r = format_worker_failure_report(
            "kastellan-microvm-run",
            "python.exec",
            cause,
            Some(&CapturedTail::complete(vec!["microvm-init: relay UDS bind failed".to_string()])),
        );
        assert!(r.contains("kastellan-microvm-run"), "{cause:?}: {r}");
        assert!(r.contains("python.exec"), "{cause:?}: {r}");
        assert!(r.contains("relay UDS bind failed"), "{cause:?}: {r}");
    }
}

#[test]
fn a_partial_capture_never_claims_the_worker_was_silent() {
    // #732's headline defect. `silent_hint` is a DIAGNOSIS — "suspect a
    // kill (wall-clock/OOM/seccomp) or a sandbox that refused the spawn" —
    // and rendering it for a drain that merely timed out sends the operator
    // to audit cgroup limits and seccomp profiles for a fault that was in
    // the guest's Python. That is #719 reproduced with the report actively
    // pointing the wrong way.
    for cause in ALL_CAUSES {
        let partial = CapturedTail::partial(vec![]);
        let r = format_worker_failure_report("w", "m", cause, Some(&partial));
        assert!(
            !r.contains("wrote NOTHING"),
            "{cause:?}: a PARTIAL capture must not state the worker wrote nothing — we \
             stopped listening, which is a fact about us: {r}"
        );
        assert!(
            !r.contains(silent_hint(cause)),
            "{cause:?}: a PARTIAL capture must not carry the per-cause diagnosis; it is \
             only earned once the drain reached EOF: {r}"
        );
        assert!(
            r.contains("PARTIAL"),
            "{cause:?}: the report must SAY the capture was partial, or the reader cannot \
             tell this from a complete one — rendering the two identically is the whole \
             defect: {r}"
        );
        // POSITIVE CONTROL: the same cause with a COMPLETE empty tail must
        // still produce the diagnosis. Without this, a renderer that had
        // simply deleted `silent_hint` would satisfy every assertion above.
        let complete = CapturedTail::complete(vec![]);
        let known = format_worker_failure_report("w", "m", cause, Some(&complete));
        assert!(
            known.contains(silent_hint(cause)) && known.contains("wrote NOTHING"),
            "{cause:?}: a COMPLETE empty tail is the worker genuinely saying nothing, and \
             must still get its diagnosis — otherwise #732's fix has thrown away the \
             signal it was protecting: {known}"
        );
    }
}

#[test]
fn a_failed_drain_never_claims_the_worker_was_silent() {
    // #747. `mark_drained` after a failed read is deliberate — a waiter
    // must not burn the full 250 ms cap because the pipe broke — but
    // recording it as EOF handed an EMPTY tail the one sentence only a
    // KNOWN silence earns. A guest fd that returns EIO on teardown then
    // sent the operator to audit cgroup limits and seccomp profiles for a
    // worker that spoke fine into a pipe we could not read.
    for cause in ALL_CAUSES {
        let failed = CapturedTail::drain_failed(vec![], ReadFault::os(libc::EIO));
        let r = format_worker_failure_report("w", "m", cause, Some(&failed));
        assert!(
            !r.contains("wrote NOTHING"),
            "{cause:?}: a FAILED drain must not state the worker wrote nothing — the \
             emptiness is in our pipe: {r}"
        );
        assert!(
            !r.contains(silent_hint(cause)),
            "{cause:?}: a FAILED drain must not carry the per-cause diagnosis; it is only \
             earned once the drain reached EOF: {r}"
        );
        assert!(
            r.contains("FAILED"),
            "{cause:?}: the report must SAY the read failed, or the reader cannot tell \
             this from a worker that was genuinely quiet: {r}"
        );
        // POSITIVE CONTROL: the same cause with a COMPLETE empty tail must
        // still produce the diagnosis, or a renderer that simply deleted
        // `silent_hint` would satisfy every assertion above.
        let known = format_worker_failure_report("w", "m", cause, Some(&CapturedTail::complete(vec![])));
        assert!(
            known.contains(silent_hint(cause)) && known.contains("wrote NOTHING"),
            "{cause:?}: a COMPLETE empty tail is the worker genuinely saying nothing and \
             must still get its diagnosis: {known}"
        );
    }
}

#[test]
fn a_failed_drain_names_its_errno_and_guesses_only_where_the_errno_points() {
    // #752. Before it every read failure got the EIO sentence — "suspect
    // the transport (a torn-down guest's vsock/pty fd)" — so an EBADF,
    // which is a double-close in OUR fd handling, sent the operator to
    // audit a guest that was fine.
    let c = WorkerRetirementCause::ExitedBeforeResponding;
    let report = |errno| {
        format_worker_failure_report("w", "m", c, Some(&CapturedTail::drain_failed(vec![], ReadFault::os(errno))))
    };
    let (eio, ebadf, reset) = (report(libc::EIO), report(libc::EBADF), report(libc::ECONNRESET));
    assert!(eio.contains("vsock/pty"), "EIO is the torn-down-guest case: {eio}");
    assert!(
        ebadf.contains("fd handling") && !ebadf.contains("vsock/pty"),
        "EBADF is our own fault and must not point at the guest's transport: {ebadf}"
    );
    assert!(
        !reset.contains("suspect"),
        "an errno with no known meaning gets named, not guessed at: {reset}"
    );
    for (errno, r) in [(libc::EIO, &eio), (libc::EBADF, &ebadf), (libc::ECONNRESET, &reset)] {
        assert!(
            r.contains(&format!("os error {errno}")),
            "every failed-drain report names the errno, guess or no guess: {r}"
        );
    }
}

#[test]
fn a_failed_drain_does_not_call_its_lines_the_workers_last_words() {
    // The #747 sibling of the #732 test below: a read error leaves exactly
    // the same doubt a timeout does — the ring evicts OLDEST first, so
    // these may be boot lines — and must not be labelled the last words.
    let c = WorkerRetirementCause::ExitedBeforeResponding;
    let failed = CapturedTail::drain_failed(vec!["worker booting".into()], ReadFault::os(libc::EIO));
    let r = format_worker_failure_report("w", "m", c, Some(&failed));
    assert!(
        !r.contains("last words"),
        "a tail whose drain FAILED is not known to hold the worker's last words: {r}"
    );
    assert!(
        r.contains("worker booting"),
        "POSITIVE CONTROL: the lines must still be REPORTED — a failed drain is not a \
         reason to discard what did arrive: {r}"
    );
    assert!(
        r.contains("FAILED"),
        "the report must name the read failure, or the reader cannot tell why these lines \
         are unreliable: {r}"
    );
}

#[test]
fn a_partial_capture_does_not_call_its_lines_the_workers_last_words() {
    // The quieter half of #732. The ring evicts OLDEST first, so under a
    // complete drain the tail really is the last thing the worker said —
    // but under an incomplete one it is the FIRST thing, the boot lines. An
    // operator correlating "last words" against the moment of death then
    // reads startup noise.
    let c = WorkerRetirementCause::ExitedBeforeResponding;
    let partial = CapturedTail::partial(vec!["worker booting".into()]);
    let r = format_worker_failure_report("w", "m", c, Some(&partial));
    assert!(
        !r.contains("last words"),
        "an incomplete tail holds the worker's FIRST lines; calling them its last words \
         mislabels startup noise as the moment of death: {r}"
    );
    assert!(
        r.contains("worker booting"),
        "POSITIVE CONTROL: the lines must still be REPORTED — an incomplete drain is not \
         a reason to discard what did arrive: {r}"
    );
    assert!(
        r.contains("FIRST"),
        "the report must warn the reader which end of the stream these came from: {r}"
    );
    // And the complete case is unchanged, which is what makes the contrast
    // above a contrast rather than a blanket reword.
    let complete = CapturedTail::complete(vec!["worker booting".into()]);
    assert!(
        format_worker_failure_report("w", "m", c, Some(&complete)).contains("last words"),
        "a COMPLETE tail really is the worker's last words and must still say so"
    );
}

#[test]
fn the_seven_tail_states_all_read_differently() {
    // Seven states, seven sentences. Any two that render alike have
    // silently collapsed a distinction the caller went to trouble to
    // preserve — the same failure `no_two_causes_read_the_same` guards for
    // causes. Grew from five with #747's two drain-failure states.
    let c = WorkerRetirementCause::ExitedBeforeResponding;
    let lines = vec!["boom".to_string()];
    let rendered: Vec<String> = [
        Some(CapturedTail::complete(lines.clone())),
        Some(CapturedTail::complete(vec![])),
        Some(CapturedTail::partial(lines.clone())),
        Some(CapturedTail::partial(vec![])),
        Some(CapturedTail::drain_failed(lines, ReadFault::os(libc::EIO))),
        Some(CapturedTail::drain_failed(vec![], ReadFault::os(libc::EIO))),
        None,
    ]
    .iter()
    .map(|t| format_worker_failure_report("w", "m", c, t.as_ref()))
    .collect();
    let mut seen = rendered.clone();
    seen.sort();
    seen.dedup();
    assert_eq!(
        seen.len(),
        rendered.len(),
        "every tail state must read differently, or the report cannot tell a reader which \
         one happened: {rendered:#?}"
    );
}

#[test]
fn no_two_causes_read_the_same() {
    // A per-cause wording that is not actually per-cause is worse than one
    // generic sentence, because it LOOKS specific. This fails the moment
    // two arms are copy-pasted.
    let mut seen: Vec<String> = ALL_CAUSES
        .iter()
        .map(|c| happened_clause(*c, "w", "m"))
        .chain(ALL_CAUSES.iter().map(|c| silent_hint(*c).to_string()))
        .collect();
    let total = seen.len();
    seen.sort();
    seen.dedup();
    // The two *closing-pipe* causes deliberately SHARE a silent hint (a
    // worker that exited silently and one whose pipe broke point at the
    // same kill/OOM/seccomp page), so exactly one duplicate is expected.
    assert_eq!(seen.len(), total - 1, "expected exactly one shared hint: {seen:#?}");
}

#[test]
fn a_still_running_worker_is_not_described_as_having_exited() {
    // The concrete misreading the old wording would have produced for the
    // four variants #737 adds: `ResponseTooLarge` means the worker answered
    // and may still be running, so "exited before responding" would send a
    // reader looking for a corpse that is not there.
    for cause in [
        WorkerRetirementCause::Undecodable,
        WorkerRetirementCause::IdMismatch,
        WorkerRetirementCause::ResponseTooLarge,
    ] {
        let r = format_worker_failure_report("w", "m", cause, Some(&CapturedTail::complete(vec![])));
        assert!(!r.contains("exited"), "{cause:?} did not exit: {r}");
        assert!(r.contains("answered"), "{cause:?} must say it answered: {r}");
    }
}

#[test]
fn the_early_exit_wording_is_unchanged_by_the_generalisation() {
    // #737 rewrote three formatters into one. These three strings are what
    // the tree emitted before it, byte for byte — pinned so the
    // generalisation cannot quietly reword the one case that was already
    // in production and already read by a human in #666 and #719.
    let c = WorkerRetirementCause::ExitedBeforeResponding;
    assert_eq!(
        format_worker_failure_report("w", "m", c, Some(&CapturedTail::complete(vec!["boom".into()]))),
        "worker w exited before responding to m; its last words: boom"
    );
    assert_eq!(
        format_worker_failure_report("w", "m", c, Some(&CapturedTail::complete(vec![]))),
        "worker w exited before responding to m, and wrote NOTHING to stderr — suspect a \
         kill (wall-clock/OOM/seccomp) or a sandbox that refused the spawn before the \
         worker ran, rather than the worker's own logic"
    );
    assert_eq!(
        format_worker_failure_report("w", "m", c, None),
        "worker w exited before responding to m; its stderr was not piped, so there is \
         nothing to report"
    );
}

#[test]
fn the_three_headline_tail_states_are_three_different_statements() {
    // The three states that predate #732/#747, kept as their own focused
    // test: collapsing any two loses a diagnosis: "said nothing" is a kill
    // or a refused spawn, while "not piped" is a statement about OUR spawn
    // path and says nothing about the worker at all. The WHOLE space is
    // seven and is covered by `the_seven_tail_states_all_read_differently`
    // — this name says "headline" so it is not mistaken for the census.
    let c = WorkerRetirementCause::ExitedBeforeResponding;
    let spoke = format_worker_failure_report("w", "m", c, Some(&CapturedTail::complete(vec!["last words".into()])));
    let silent = format_worker_failure_report("w", "m", c, Some(&CapturedTail::complete(vec![])));
    let unpiped = format_worker_failure_report("w", "m", c, None);
    assert!(spoke.contains("last words"), "{spoke}");
    assert!(silent.contains("NOTHING"), "{silent}");
    assert!(silent.contains("kill"), "silence must name where to look: {silent}");
    assert!(unpiped.contains("not piped"), "it must say WHY there is no tail: {unpiped}");
    assert_ne!(spoke, silent);
    assert_ne!(silent, unpiped);
}

#[test]
fn the_stderr_fallback_line_is_marked_and_keeps_the_report_whole() {
    // The marker makes the fallback greppable and tells a reader which
    // channel they are looking at; the report must survive it intact,
    // because the report is the entire point of the line.
    let report = format_worker_failure_report(
        "w",
        "m",
        WorkerRetirementCause::ExitedBeforeResponding,
        Some(&CapturedTail::complete(vec!["last words".to_string()])),
    );
    let line = format_worker_failure_stderr_fallback(&report);
    assert!(
        line.starts_with(WORKER_FAILED_STDERR_MARKER),
        "the marker must lead, so a grep anchored at line start finds it: {line}"
    );
    assert!(line.contains(&report), "the report must not be altered: {line}");
}

#[test]
fn the_stderr_fallback_neutralises_a_model_authored_control_character() {
    // `program` and `method` are interpolated RAW by the formatters above
    // (tail lines are already stripped by `push_trimmed`), and `method` is
    // model-authored: `qualified_method` returns `None` outside the tool's
    // advertised set and the planner's string then goes through verbatim.
    //
    // This pins the property on the PUB formatter rather than on
    // `emit_worker_failure_report`'s call order, so a second producer cannot
    // render an un-neutralised line by reaching for this function. Those
    // producers now exist — the persistent-worker death report (#730) and
    // the down report (#738) — and the measurement is worth recording:
    // dropping the neutralisation from the SHARED renderer leaves the e2e
    // suites green, because each emitter neutralises its own line first and
    // masks it. Only these unit tests die. A `pub` renderer's guarantee is
    // reachable from a unit test and from nowhere else.
    //
    // ⚠️ **Say what that mutant is, plainly: near-equivalent today.** The
    // `format_*_stderr_fallback` wrappers have ZERO callers outside these
    // tests — `delivery::warn_and_fall_back!` expands to a
    // `format_stderr_fallback` call on the private renderer directly — so
    // the neutralisation these tests defend is redundant
    // belt-and-braces in every shipped binary. That makes this a test of a
    // `pub` API's contract for a future caller, which is worth having, and
    // NOT evidence that the e2e suites have a hole. Reading it the other
    // way would overstate what the pair proves.
    let report = format_worker_failure_report(
        "w",
        "m\u{1b}[31m\n[WARN] forged-gate-line",
        WorkerRetirementCause::ExitedBeforeResponding,
        Some(&CapturedTail::complete(vec!["last words".to_string()])),
    );
    let line = format_worker_failure_stderr_fallback(&report);
    assert!(
        !line.contains('\u{1b}'),
        "an ESC would be an ANSI sequence executing in the reader's terminal: {line:?}"
    );
    assert_eq!(
        line.lines().count(),
        1,
        "the fallback must be exactly ONE line — a `\\n` here forges a column-0 line that \
         `run-e2e-gate.sh`'s `^\\[WARN\\]` grep counts, failing an unrelated profile: \
         {line:?}"
    );
    // Neutralisation maps the class to a space, so the text must SURVIVE —
    // mid-line. A check that merely asserted its absence would also pass if
    // `method` stopped reaching the report at all.
    assert!(
        line.contains("forged-gate-line"),
        "the text must survive as text; only its line-forging effect is removed: {line:?}"
    );
}
