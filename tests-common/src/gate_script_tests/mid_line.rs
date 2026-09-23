//! #755: an evidence marker that lands MID-LINE is refused, not silently
//! uncounted.
//!
//! Under `--nocapture` libtest writes `test <name> ... ` to stdout with no
//! newline yet, and the gate merges stdout and stderr into one pipe. A marker
//! written to stderr at that moment lands after the prefix, on the same line:
//!
//! ```text
//! test some_test ... [WARN] KASTELLAN_PG_REQUIRE_E2E="y" is not in the flag dialect …
//! ```
//!
//! Every count in the gate is anchored at column 0, so that line is invisible
//! to it. For `[WARN]` (a hard zero), a capped `[SKIP]` and a `[panic]` under
//! `MAX_PANIC` that is a false GREEN; for `[E2E]` a false red, and for
//! `[panic-hook]` a binary reported unhooked when it was not. Anchoring is
//! deliberate — a neutralised hostile payload legitimately carries `[WARN]`
//! mid-line — so the fix is not to unanchor the counts but to refuse the
//! shape only libtest produces: a counted marker as the first `[` on a
//! `test <name> ... ` line.
//!
//! Every emitter in the suites the profiles select puts the leading `\n` and
//! its marker in the same write, so today this check should never fire. The
//! renderers (`skip_line`, `warn_line`, `e2e_line`, `panic_hook::own_line`)
//! are printed whole in one write; a hand-written `eprintln!("\n[SKIP] …: {e}")`
//! is several writes, but its first literal piece carries both the `\n` and
//! the marker, which is the part that matters. It is not true of the whole tree —
//! #718's hand-written `[SKIP]`s include unframed ones outside every profile —
//! and the check is here for the day one of those joins a profile: fail-closed
//! on a shape rather than trusting a census of emitters.

use super::run::{
    describe, healthy_log_for, run_gate, run_gate_bytes, run_gate_with, GateEdits, PROFILE,
};
use crate::panic_hook::{install_line, PANIC_MARKER};
use crate::skip::{e2e_line, skip_line, warn_line};

/// A libtest line whose result was overwritten by `marker_line` — what an
/// unframed emitter produces under `--nocapture`.
fn landed_mid_line(marker_line: &str) -> String {
    format!("test fake_suite::a_test ... {}\n", marker_line.trim())
}

/// Every marker the gate counts, rendered by the real renderer where one
/// exists — so a renamed marker cannot leave these tests checking a stale copy.
fn counted_marker_lines() -> Vec<String> {
    vec![
        skip_line("a precondition was unmet"),
        warn_line("KASTELLAN_PG_REQUIRE_E2E=\"y\" is not in the flag dialect"),
        e2e_line("gliner-relex", "precondition met (fake cargo)"),
        format!("{PANIC_MARKER} thread 'x' panicked at src/lib.rs:1:1: boom"),
        install_line(),
    ]
}

#[test]
fn every_counted_marker_landing_mid_line_is_refused() {
    for marker_line in counted_marker_lines() {
        let log = format!("{}{}", landed_mid_line(&marker_line), healthy_log_for(PROFILE));
        let out = run_gate("mid-line", &log, 0);
        assert_eq!(
            out.status.code(),
            Some(1),
            "a mid-line {:?} is uncounted by every anchored grep, so the run's totals are \
             wrong — the gate must refuse it\n{}",
            marker_line.trim(),
            describe(&out)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("landed mid-line"),
            "the gate must refuse for the stated reason\n{}",
            describe(&out)
        );
        assert!(
            stdout.contains(marker_line.trim()),
            "the refusal must quote the offending line\n{}",
            describe(&out)
        );
    }
}

#[test]
fn a_marker_landing_after_the_result_word_is_refused() {
    // libtest writes the result word and the line's `\n` as SEPARATE flushed
    // writes, so an unframed marker can also land between them — after `ok`,
    // `FAILED`, or an `ignored, <message>`. Under a multithreaded run (every
    // profile) the name is written at completion, so this gap is no rarer
    // than the one after `... `.
    let warn = warn_line("KASTELLAN_PG_REQUIRE_E2E=\"y\" is not in the flag dialect");
    for result in ["ok", "FAILED", "ignored, inner fixture: run by its parent"] {
        let log = format!(
            "test fake_suite::a_test ... {result}{}\n{}",
            warn.trim(),
            healthy_log_for(PROFILE)
        );
        let out = run_gate("after-result", &log, 0);
        assert_eq!(out.status.code(), Some(1), "after {result:?}\n{}", describe(&out));
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("landed mid-line"),
            "after {result:?}\n{}",
            describe(&out)
        );
    }
}

#[test]
fn a_framed_marker_after_a_partial_libtest_line_passes() {
    // What a correct emitter produces: the leading `\n` ends libtest's partial
    // line, so the marker owns column 0 and the prefix line stands alone.
    let framed = e2e_line("gliner-relex", "precondition met (fake cargo)");
    let log = format!("test fake_suite::a_test ... {framed}ok\n{}", healthy_log_for(PROFILE));
    let out = run_gate("framed", &log, 0);
    assert!(out.status.success(), "a framed marker is the healthy shape\n{}", describe(&out));
}

#[test]
fn a_neutralised_payload_carrying_a_marker_mid_line_is_not_refused() {
    // The worker-report suites print hostile payloads whose control characters
    // were escaped, so `[WARN]` sits mid-line on a `[worker-failed]` line —
    // after libtest's prefix, too, if that emitter is ever unframed. That line
    // is inert, and refusing it would make the `worker-report` profile red on
    // every run. Only a counted marker as the FIRST `[` on a libtest line is
    // the mid-line shape.
    //
    // The payloads also spell the prefix themselves — `a ... [WARN]` and
    // `test forged ... [WARN]` — so a pattern that let the name span a `[`, or that
    // was not anchored at column 0, would match them and fail this test.
    let log = format!(
        "test fake_suite::a_test ... [worker-failed] method=\"anything\\u{{1b}}[31m\\n\
         [WARN] kastellan-test: FORGED-GATE-LINE a ... [WARN] forged\"\n\
         [worker-failed] method=\"test forged ... [WARN] forged\"\n{}",
        healthy_log_for(PROFILE)
    );
    let out = run_gate("neutralised", &log, 0);
    assert!(out.status.success(), "{}", describe(&out));
}

#[test]
fn a_failing_mid_line_scan_refuses_a_verdict() {
    // grep prints nothing both when nothing matched and when it failed, and
    // "nothing" is exactly what a clean log looks like. Only the exit status
    // (2, not 1) tells them apart, so a failed scan must refuse rather than pass.
    let edits = GateEdits { tools: &[("grep", "exit 2")], ..Default::default() };
    let out = run_gate_with(PROFILE, "grep-fails", &healthy_log_for(PROFILE), 0, &edits);
    assert_eq!(out.status.code(), Some(3), "{}", describe(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("grep failed (2) scanning"),
        "{}",
        describe(&out)
    );
}

/// Assert the gate refused `log` for a mid-line marker, quoting `quoted`.
fn assert_refused_mid_line(tag: &str, log: &[u8], quoted: &str, env: &[(&str, &str)]) {
    let edits = GateEdits { env, ..Default::default() };
    let out = run_gate_bytes(PROFILE, tag, log, 0, &edits);
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("landed mid-line"), "{}", describe(&out));
    assert!(
        stdout.contains(quoted),
        "the refusal must quote the offending line, not a grep notice\n{}",
        describe(&out)
    );
}

#[test]
fn a_nul_byte_elsewhere_in_the_log_does_not_blind_the_scan() {
    // Measured on GNU grep 3.11 (the DGX): one NUL anywhere makes the log
    // "binary", grep prints NOTHING and exits 0 — "matched" by status, "clean"
    // by output. A verdict read off the output passed a stranded `[WARN]`.
    // BSD grep prints `Binary file … matches` instead, which refused only by
    // accident, quoting the notice rather than the line.
    let warn = warn_line("KASTELLAN_PG_REQUIRE_E2E=\"y\" is not in the flag dialect");
    let mut log = landed_mid_line(&warn).into_bytes();
    log.extend_from_slice(b"a worker echoed a raw \0 byte\n");
    log.extend_from_slice(healthy_log_for(PROFILE).as_bytes());
    assert_refused_mid_line("nul", &log, warn.trim(), &[]);
}

#[test]
fn an_invalid_utf8_byte_on_the_stranded_line_does_not_hide_it() {
    // Under a UTF-8 locale `[^[]*` does not match a byte that is not a
    // character, so the line never matches (GNU grep: exit 1, "clean"). The
    // scan must read bytes whatever the operator's locale.
    let warn = warn_line("KASTELLAN_PG_REQUIRE_E2E=\"y\" is not in the flag dialect");
    let mut log = b"test fake_suite::a_test ... ignored, \xffx".to_vec();
    log.extend_from_slice(warn.trim().as_bytes());
    log.push(b'\n');
    log.extend_from_slice(healthy_log_for(PROFILE).as_bytes());
    let utf8 = [("LC_ALL", "en_US.UTF-8"), ("LANG", "en_US.UTF-8")];
    assert_refused_mid_line("invalid-utf8", &log, warn.trim(), &utf8);
}

#[test]
fn a_stray_non_utf8_byte_does_not_fail_a_healthy_run() {
    // The same byte on an ordinary line must not turn the gate red: under a
    // UTF-8 locale macOS awk dies on it ("towc: multibyte conversion
    // failure"), which refused every run whose log held one — a false red, the
    // kind that gets a gate switched off.
    let mut log = b"a worker echoed \xff raw\n".to_vec();
    log.extend_from_slice(healthy_log_for(PROFILE).as_bytes());
    let env = [("LC_ALL", "en_US.UTF-8"), ("LANG", "en_US.UTF-8")];
    let edits = GateEdits { env: &env, ..Default::default() };
    let out = run_gate_bytes(PROFILE, "stray-byte", &log, 0, &edits);
    assert!(out.status.success(), "{}", describe(&out));
}
