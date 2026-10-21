use super::*;
use crate::worker_stderr::ReadFault;

#[test]
fn a_partial_death_capture_does_not_claim_no_stderr() {
    // #732 for the persistent path — the one that runs the Matrix and
    // email channel workers. "no stderr captured" is a claim about the
    // WORKER; an empty tail whose drain timed out only supports a claim
    // about us.
    let r = format_death_report(Ok(None), &CapturedTail::partial(vec![]));
    assert!(
        !r.contains("no stderr captured"),
        "a PARTIAL capture must not report the worker as having written nothing: {r}"
    );
    assert!(
        r.contains("PARTIAL"),
        "the line must say the capture was partial, or a reader cannot tell it from a \
         complete one: {r}"
    );
    // POSITIVE CONTROL: the complete case must keep the original sentence,
    // or this test would also pass against a renderer that simply deleted
    // it.
    let known = format_death_report(Ok(None), &CapturedTail::complete(vec![]));
    assert!(
        known.contains("no stderr captured"),
        "a COMPLETE empty tail really is a silent worker and must still say so: {known}"
    );
}

#[test]
fn a_partial_death_capture_does_not_call_its_lines_recent() {
    // The ring evicts oldest first, so an incomplete tail is the FIRST
    // lines. "recent stderr" would point a reader correlating against the
    // crash at the worker's boot messages instead.
    let partial = CapturedTail::partial(vec!["worker booting".into()]);
    let r = format_death_report(Ok(None), &partial);
    assert!(
        !r.contains("recent stderr"),
        "an incomplete tail holds the FIRST lines, not the most recent ones: {r}"
    );
    assert!(
        r.contains("worker booting"),
        "POSITIVE CONTROL: whatever arrived must still be reported: {r}"
    );
    assert!(
        format_death_report(Ok(None), &CapturedTail::complete(vec!["boom".into()]))
            .contains("recent stderr"),
        "a COMPLETE tail really is the most recent output and must still say so"
    );
}

#[test]
fn a_failed_death_drain_does_not_claim_no_stderr() {
    // #747 for the persistent path. A Matrix or email worker whose stderr
    // fd dies on teardown must not be reported as having written nothing —
    // that sends an operator after a kill that never happened.
    let r = format_death_report(Ok(None), &CapturedTail::drain_failed(vec![], ReadFault::os(libc::EIO)));
    assert!(
        !r.contains("no stderr captured"),
        "a FAILED drain must not report the worker as having written nothing: {r}"
    );
    assert!(
        r.contains("FAILED"),
        "the line must say our read failed, or a reader cannot tell it from a genuinely \
         silent worker: {r}"
    );
    assert!(r.contains(&format!("os error {}", libc::EIO)), "and name which failure (#752): {r}");
    // With lines too, and a second errno so the fault is passed through,
    // not hard-coded — with the guess that errno earns.
    let words =
        format_death_report(Ok(None), &CapturedTail::drain_failed(vec!["boom".into()], ReadFault::os(libc::EBADF)));
    assert!(
        words.contains(&format!("os error {}", libc::EBADF)) && words.contains("fd handling"),
        "a failed drain with lines names its errno and guess as well: {words}"
    );
    // POSITIVE CONTROL, as above: the complete case keeps its sentence.
    assert!(
        format_death_report(Ok(None), &CapturedTail::complete(vec![])).contains("no stderr captured"),
        "a COMPLETE empty tail really is a silent worker and must still say so"
    );
}

#[test]
fn the_six_death_tail_states_all_read_differently() {
    // The persistent renderer's version of the tool-worker discipline:
    // every state the caller can hand us must produce a distinguishable
    // sentence, or the type's distinctions die at the last step.
    //
    // SIX, not seven, and the signature is why: this takes `&CapturedTail`
    // (the `Option` in it is on the exit STATUS), and
    // `PersistentTransport::death_report` returns `None` outright when
    // there is no tail, so it never calls this renderer at all. The "its
    // stderr was not piped" state cannot reach here — unlike the tool-worker
    // and sidecar renderers, whose censuses really are seven.
    let lines = vec!["boom".to_string()];
    let rendered: Vec<String> = [
        CapturedTail::complete(lines.clone()),
        CapturedTail::complete(vec![]),
        CapturedTail::partial(lines.clone()),
        CapturedTail::partial(vec![]),
        CapturedTail::drain_failed(lines, ReadFault::os(libc::EIO)),
        CapturedTail::drain_failed(vec![], ReadFault::os(libc::EIO)),
    ]
    .iter()
    .map(|t| format_death_report(Ok(None), t))
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
fn death_report_no_status_no_stderr() {
    let report = format_death_report(Ok(None), &CapturedTail::complete(vec![]));
    assert!(report.contains("exit status unknown"), "{report}");
    assert!(report.contains("no stderr captured"), "{report}");
}

#[test]
fn death_report_includes_status_and_stderr_tail() {
    // A real non-zero ExitStatus so the rendering (and signal-vs-exit
    // distinction the daemon log relies on) is exercised, not mocked.
    let status = std::process::Command::new("false")
        .status()
        .expect("spawn /usr/bin/false");
    let report = format_death_report(Ok(Some(status)), &CapturedTail::complete(vec!["boom".into(), "trace".into()]));
    assert!(report.contains("exit status"), "{report}");
    assert!(report.contains("boom | trace"), "{report}");
}

#[test]
fn a_failed_exit_poll_is_not_reported_as_an_unreaped_worker() {
    // #754: "not yet reaped" is a claim about the CHILD; a `try_wait` that
    // errored is a fact about OUR `waitpid`. Inside a report whose whole
    // purpose is keeping those apart, they must not render the same.
    let failed = format_death_report(
        Err(std::io::Error::from_raw_os_error(libc::ECHILD)),
        &CapturedTail::complete(vec![]),
    );
    assert!(!failed.contains("not yet reaped"), "{failed}");
    assert!(failed.contains("our waitpid failed"), "{failed}");
    assert!(
        failed.contains(&std::io::Error::from_raw_os_error(libc::ECHILD).to_string()),
        "the errno must be on the line: {failed}"
    );
    let unreaped = format_death_report(Ok(None), &CapturedTail::complete(vec![]));
    assert!(unreaped.contains("not yet reaped"), "{unreaped}");
}

#[test]
fn the_persistent_death_line_names_which_worker_died_and_carries_the_report() {
    // The fallback channel carries no `tracing` fields, so a label that lived
    // only in `%label` would leave the stderr line saying "persistent worker
    // died" without naming which — and the two production users are `matrix`
    // and `email`, i.e. exactly the distinction an operator needs first.
    let report = "worker exited (exit status: 1); recent stderr: boom";
    let line = format_persistent_death_line("matrix", report);
    assert!(line.contains("matrix"), "the line must name WHICH worker died: {line}");
    assert!(
        line.contains("boom"),
        "the report must survive intact — it is the point: {line}"
    );
    assert!(line.contains("died"), "the line must say what happened: {line}");
    // ⚠️ The SHAPE, not just the ingredients. Three `contains` checks pass
    // just as well on `"persistent worker {report} died: {label}"` — the
    // two interpolations swapped — which compiles (both are `&str`) and
    // renders an operator a line naming the exit status as the worker and
    // the worker as the cause. That mutant survived the whole suite until
    // the #735 review; this is what kills it.
    assert_eq!(
        line,
        format!("persistent worker matrix died: {report}"),
        "the label and the report must land in their own slots: {line}"
    );
}

#[test]
fn the_persistent_death_fallback_neutralises_a_hostile_report() {
    // ⚠️ Defence-in-depth at a PUBLIC TRAIT BOUNDARY, not a live hole — the
    // emitter's doc says so plainly and this test does not claim otherwise.
    // Nothing reaching this today is attacker-controlled (an `ExitStatus`, plus
    // a tail already stripped by `push_trimmed`), but
    // `PersistentTransport::death_report` is `pub` and `egress::persistent_net`
    // already delegates through it, so any implementor is a producer of this
    // string. Pinning the property on the PUB formatter — rather than on
    // `emit_persistent_death_report`'s call order — is what stops such an
    // implementor rendering an un-neutralised line by reaching for it directly.
    let line = format_persistent_death_stderr_fallback(
        "worker exited\u{1b}[31m\n[WARN] forged-gate-line",
    );
    assert!(
        line.starts_with(WORKER_DEATH_STDERR_MARKER),
        "the marker must lead, so a grep anchored at line start finds it: {line}"
    );
    assert!(
        !line.contains('\u{1b}'),
        "an ESC would be an ANSI sequence executing in the reader's terminal: {line:?}"
    );
    assert_eq!(
        line.lines().count(),
        1,
        "the fallback must be exactly ONE line — a `\\n` here forges a column-0 line that \
         `run-e2e-gate.sh`'s `^\\[WARN\\]` grep counts, failing an unrelated profile: {line:?}"
    );
    // Neutralisation maps the class to a space, so the text must SURVIVE —
    // mid-line. A check that merely asserted its absence would also pass if the
    // report stopped reaching the line at all.
    assert!(
        line.contains("forged-gate-line"),
        "the text must survive as text; only its line-forging effect is removed: {line:?}"
    );
}

#[test]
fn the_down_line_names_which_worker_and_carries_the_reason() {
    // Same argument as the death line's: the stderr fallback carries no
    // `tracing` fields, so a label kept only in a field leaves an operator
    // reading "a persistent worker is down" with `matrix` and `email` both
    // live.
    let line = format_persistent_down_line("matrix", "respawn attempt 3 failed: ENOENT");
    assert!(line.contains("matrix"), "{line}");
    assert!(line.contains("respawn attempt 3 failed: ENOENT"), "{line}");
}

#[test]
fn a_death_and_a_down_do_not_read_the_same() {
    // #738's whole argument in one assertion: "died once and came back" and
    // "is not coming back" are different events, and a reader must be able
    // to tell them apart from the line alone, not just from the marker.
    let death = format_persistent_death_line("matrix", "worker exited (signal: 9)");
    let down = format_persistent_down_line("matrix", "worker exited (signal: 9)");
    assert_ne!(death, down);
    assert!(
        format_persistent_down_stderr_fallback(&down)
            .starts_with(WORKER_DOWN_STDERR_MARKER),
        "the marker must lead so a grep anchored at line start finds it"
    );
    assert!(
        !format_persistent_down_stderr_fallback(&down)
            .starts_with(WORKER_DEATH_STDERR_MARKER),
        "a down line must not be collected as a death line"
    );
}

#[test]
fn the_down_fallback_neutralises_a_hostile_reason() {
    // The reason reaching `emit_persistent_down_report` includes a PANIC
    // PAYLOAD (#739's join path), which is arbitrary text from arbitrary
    // code — a genuinely untrusted input, unlike the death report's, whose
    // neutralisation is defence-in-depth at a trait boundary.
    let hostile = "boom\u{1b}[31m\n[WARN] forged-gate-line";
    let line = format_persistent_down_stderr_fallback(&format_persistent_down_line(
        "matrix", hostile,
    ));
    assert!(!line.contains('\u{1b}'), "{line:?}");
    assert_eq!(
        line.lines().count(),
        1,
        "a `\\n` here forges a column-0 line that `run-e2e-gate.sh`'s `^\\[WARN\\]` grep \
         counts, failing an unrelated profile: {line:?}"
    );
    assert!(
        line.contains("forged-gate-line"),
        "the text must survive as text; only its line-forging effect is removed: {line:?}"
    );
}

