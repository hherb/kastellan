//! Unit tests for [`super`], split out of `panic_hook.rs` to keep it under
//! the 500-line cap (#748). Movement only.

use super::*;

/// The payload from #742's own reproduction: an ANSI escape and a newline
/// followed by a forged evidence marker.
const HOSTILE: &str = "fixture death\u{1b}[31m\n[WARN] FORGED-PANIC-LINE";

#[test]
fn a_hostile_payload_cannot_forge_a_column_zero_line() {
    let rendered = render_panic_line(Some("worker-2"), Some("core/tests/x.rs:647:9"), HOSTILE);
    // The property, stated the way the gate reads it: `grep -c '^\[WARN\]'`
    // over these bytes must be 0.
    let forged: Vec<&str> = rendered
        .lines()
        .filter(|l| l.starts_with("[WARN]") || l.starts_with("[SKIP]") || l.starts_with("[E2E]"))
        .collect();
    assert!(
        forged.is_empty(),
        "a panic payload forged a column-0 evidence line, which \
         `scripts/run-e2e-gate.sh` counts: {forged:?}\nrendered: {rendered:?}"
    );
    // The general form of the same property, and the one that still holds
    // now the renderer keeps a payload's own line breaks: EVERY line after
    // the first begins with the indent, so none of them is at column 0 —
    // whatever it says. Checking only the three known markers above would
    // let a fourth marker added to the gate script sail through.
    let unindented: Vec<&str> = rendered
        .lines()
        .skip(1)
        .filter(|l| !l.starts_with(CONTINUATION_INDENT))
        .collect();
    assert!(
        unindented.is_empty(),
        "every continuation line must be INDENTED — an unindented one sits at column 0, \
         where `run-e2e-gate.sh`'s anchored greps count it, and is where a forgery hides. \
         Offenders: {unindented:?}\nrendered: {rendered:?}"
    );
}

#[test]
fn a_multi_line_message_keeps_its_line_breaks() {
    // ⚠️ REGRESSION GUARD for the fix's own cost. The first version mapped
    // every `\n` to a space, which is safe and unreadable: 59 assertion
    // messages in `core/tests` interpolate a whole child-process
    // transcript, and flattening them turns each failure into one
    // multi-kilobyte line. The anti-forgery property only needs column 0
    // kept clear, which an indent does.
    let payload = "first assertion line\nsecond line\nthird line";
    let rendered = render_panic_line(Some("t"), Some("f.rs:1:1"), payload);
    assert_eq!(
        rendered.lines().count(),
        3,
        "a three-line panic message must still render as three lines, or the transcript \
         this tree's assertions carry is unreadable: {rendered:?}"
    );
    for fragment in ["first assertion line", "second line", "third line"] {
        assert!(rendered.contains(fragment), "line lost: {fragment:?} in {rendered:?}");
    }
    // And the property that makes keeping them safe.
    assert!(
        rendered.lines().skip(1).all(|l| l.starts_with(CONTINUATION_INDENT)),
        "kept line breaks are only safe while every continuation line is indented: \
         {rendered:?}"
    );
}

#[test]
fn the_continuation_indent_is_not_empty() {
    // Without this the multi-line rendering is a forgery vector again: an
    // empty indent makes `starts_with(CONTINUATION_INDENT)` true of every
    // line, including one at column 0, so the assertions above would pass
    // while the gate became forgeable.
    assert!(
        !CONTINUATION_INDENT.is_empty()
            && CONTINUATION_INDENT.chars().all(char::is_whitespace),
        "the continuation indent must be non-empty whitespace: {CONTINUATION_INDENT:?}"
    );
}

#[test]
fn the_payload_survives_as_readable_text() {
    // POSITIVE CONTROL. Without it, a renderer that dropped the payload
    // entirely — or replaced it with a fixed string — would satisfy every
    // assertion above while destroying the diagnostic the line exists for.
    let rendered = render_panic_line(Some("worker-2"), Some("core/tests/x.rs:647:9"), HOSTILE);
    assert!(rendered.contains("fixture death"), "payload text lost: {rendered}");
    assert!(
        rendered.contains("FORGED-PANIC-LINE"),
        "the forged TEXT must survive — neutralisation moves it off column 0, it does not \
         censor it. Asserting its absence would pass just as well if the payload never \
         reached the line: {rendered}"
    );
    assert!(rendered.contains("worker-2"), "thread name lost: {rendered}");
    assert!(rendered.contains("core/tests/x.rs:647:9"), "location lost: {rendered}");
}

#[test]
fn the_escape_never_reaches_the_readers_terminal() {
    let rendered = render_panic_line(None, None, HOSTILE);
    assert!(
        !rendered.contains('\u{1b}'),
        "an ESC in a panic payload is an ANSI sequence executing in the terminal of \
         whoever reads the failure: {rendered:?}"
    );
}

#[test]
fn a_hostile_thread_name_cannot_forge_a_line_either() {
    // `std::thread::Builder::name` takes an arbitrary String, so the thread
    // name is an interpolated input like any other. Checked separately
    // because a renderer that neutralised only the payload would pass every
    // test above.
    //
    // ⚠️ Unlike the payload, a thread name's `\n` is still FLATTENED to a
    // space: only the payload's own breaks are kept, because only the
    // payload is the diagnostic a reader needs laid out. A thread name
    // that could open a line would be a forgery vector with no upside.
    let rendered = render_panic_line(Some("t\n[WARN] FORGED-VIA-THREAD-NAME"), None, "boom");
    assert_eq!(
        rendered.lines().count(),
        1,
        "a thread name is interpolated too and must be FLATTENED, not line-broken: \
         {rendered:?}"
    );
    assert!(rendered.contains("FORGED-VIA-THREAD-NAME"), "positive control: {rendered}");
}

#[test]
fn a_hostile_location_cannot_forge_a_line_either() {
    // Defence in depth: today a location comes from the compiler and is
    // safe. It is neutralised anyway, because the column-0 property should
    // belong to this function rather than to an audit of its inputs — the
    // same argument `format_stderr_fallback` makes. Flattened like the
    // thread name, for the same reason: only the payload earns line breaks.
    let rendered = render_panic_line(None, Some("f.rs:1:1\n[WARN] FORGED-VIA-LOCATION"), "boom");
    assert_eq!(rendered.lines().count(), 1, "location must be neutralised: {rendered:?}");
}

#[test]
fn the_marker_is_not_a_gate_evidence_marker() {
    // The same rule `worker_stderr::report::shared` asserts over its three
    // fallback markers. `starts_with`, not equality: the gate's greps are
    // anchored at line start, so a marker of `"[WARN] panic"` would pass an
    // inequality check and still be counted.
    for evidence in ["[SKIP]", "[WARN]", "[E2E]"] {
        assert!(
            !PANIC_MARKER.starts_with(evidence),
            "the panic marker must not BEGIN with {evidence}: the gate greps them anchored \
             at line start"
        );
    }
    // Without this the check above is vacuous — `""` satisfies no
    // `starts_with` of a non-empty string, and `contains("")` is true of
    // any output at all.
    assert!(
        PANIC_MARKER.starts_with('[') && PANIC_MARKER.ends_with(']') && PANIC_MARKER.len() > 2,
        "the marker must be a non-trivial bracketed token: {PANIC_MARKER:?}"
    );
}

#[test]
fn the_install_marker_is_disjoint_from_every_marker_the_gate_counts() {
    // The gate counts `^\[panic-hook\]` per test binary (#748) and ALSO
    // counts `^\[panic\]`, `^\[SKIP\]`, `^\[WARN\]` and `^\[E2E\]`. If
    // either marker were a prefix of the other, one grep would count the
    // other's lines: an install announcement would trip the `[panic]` cap
    // in every healthy run, or a panic line would satisfy the hook check.
    // Both directions, because `starts_with` is not symmetric.
    for counted in ["[SKIP]", "[WARN]", "[E2E]", PANIC_MARKER] {
        assert!(
            !HOOK_INSTALLED_MARKER.starts_with(counted),
            "the install marker must not BEGIN with {counted}"
        );
        assert!(
            !counted.starts_with(HOOK_INSTALLED_MARKER),
            "{counted} must not begin with the install marker"
        );
    }
    // Non-vacuity, as for PANIC_MARKER above.
    assert!(
        HOOK_INSTALLED_MARKER.starts_with('[')
            && HOOK_INSTALLED_MARKER.ends_with(']')
            && HOOK_INSTALLED_MARKER.len() > 2,
        "the marker must be a non-trivial bracketed token: {HOOK_INSTALLED_MARKER:?}"
    );
}

#[test]
fn the_install_line_starts_at_column_zero_with_its_marker_and_is_one_line() {
    // The gate's grep is anchored at line start, so a leading space or a
    // second line would make the announcement invisible to it.
    let line = install_line();
    assert!(line.starts_with(HOOK_INSTALLED_MARKER), "{line:?}");
    assert_eq!(line.lines().count(), 1, "{line:?}");
}

#[test]
fn an_emitted_line_owns_its_own_column_zero() {
    // Measured, not hypothetical (#748): under `--nocapture` libtest writes
    // `test <name> ... ` to STDOUT with no newline yet, and our STDERR line
    // shares the same merged pipe in a gate log. An unprefixed line landed
    // mid-line there, the anchored grep missed it, and the hook check
    // refused a binary that HAD installed the hook. For `[panic]` the same
    // interleaving is a false GREEN against the cap.
    //
    // So every line is emitted as `\n<line>\n`: the leading newline ends
    // any partial line, and one write under PIPE_BUF is atomic on a pipe.
    for line in [install_line(), render_panic_line(None, None, "boom")] {
        let emitted = own_line(&line);
        assert!(emitted.starts_with('\n'), "must end any partial line first: {emitted:?}");
        assert!(emitted.ends_with('\n'), "{emitted:?}");
        assert_eq!(emitted.trim_matches('\n'), line, "the line itself must be unchanged");
        // Atomicity is only guaranteed up to PIPE_BUF, whose POSIX minimum
        // is 512 bytes. The install line is fixed; a panic line is not, so
        // this pins only the one whose length we control.
    }
    assert!(own_line(&install_line()).len() <= 512, "the announcement must fit one atomic pipe write");
}

#[test]
fn installing_twice_is_harmless() {
    // The knob-read doors call this on every knob read, which is many times per
    // binary. `Once` is what makes that free; without it each call would
    // leak a boxed closure.
    install_once();
    install_once();
}

/// #755: the hook's own lines reach stderr as ONE framed write. The renderer
/// test above pins the frame; this pins that the emitter does not split it —
/// `eprintln!("\n{line}")` is the two-write spelling that lets another
/// thread's output land between the newline and the marker.
#[test]
fn the_hook_emits_its_own_line_as_one_framed_write() {
    let mut out = crate::write_recorder::WriteRecorder::default();
    emit_own_line_to(&install_line(), &mut out);
    out.assert_one_framed_line(HOOK_INSTALLED_MARKER);
}
