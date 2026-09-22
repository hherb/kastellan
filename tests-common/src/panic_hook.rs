//! A panic hook that cannot forge a gate evidence line
//! ([#742](https://github.com/hherb/kastellan/issues/742)).
//!
//! # The hole this closes
//!
//! `scripts/run-e2e-gate.sh` greps `^\[WARN\]` and asserts **zero** matches,
//! and every profile passes `--nocapture`. This tree therefore takes care that
//! nothing it prints can forge a column-0 line: `worker_stderr`'s three
//! emitters run their text through `neutralise_controls`, and the three
//! fallback markers are deliberately none of `[SKIP]`/`[WARN]`/`[E2E]`.
//!
//! Rust's **default panic hook** is covered by none of that. It prints the
//! payload verbatim, so a `panic!` carrying a newline puts whatever follows it
//! at column 0:
//!
//! ```text
//! thread '<unnamed>' panicked at core/tests/…:647:9:
//! fixture death<ESC>[31m
//! [WARN] FORGED-PANIC-LINE
//! ```
//!
//! That third line is counted. It is not hypothetical — it is what made the
//! first draft of #743's `#739` parent test fail, and the assertion there had
//! to be narrowed to our own report line as a result.
//!
//! # Severity, stated plainly rather than oversold
//!
//! A panic payload is attacker-influenced only where a `panic!` interpolates
//! untrusted text, and the one such path in the tree today is
//! `PersistentTransport` implementors, all of which are in-tree. A test that
//! panics already fails, so the gate is red either way. **The bad case is
//! narrower:** a panic inside a *passing* test — a caught panic, or a panic on
//! a non-test thread — whose payload lands in a profile log and turns a
//! *different* assertion red, pointing at nothing.
//!
//! The ANSI escape is the more everyday win: unneutralised, it executes in the
//! terminal of whoever reads the failure.
//!
//! # Why this hook does not delegate to the default
//!
//! The issue suggests neutralising "before delegating to the default". **Measured:
//! that cannot work.** `std::panic::set_hook` gives the next hook a
//! `PanicHookInfo` borrowed from the runtime, which no caller can construct or
//! modify — so delegating hands the default the *original* payload and it
//! prints it verbatim, forged line and all. The hook has to render the panic
//! itself.
//!
//! The obvious worry about doing that is libtest, which is how a failing test
//! gets its name and message into `test result: FAILED`. **Measured, against a
//! child process running a deliberately panicking fixture:**
//!
//! | observation | with this hook |
//! | --- | --- |
//! | column-0 `[WARN]` lines forged | **0** (was 1) |
//! | `test result: FAILED. 0 passed; 1 failed;` still printed | yes |
//! | the payload text still readable | yes |
//! | ESC reaching the reader's terminal | no (was yes) |
//!
//! libtest accounts for a failure through `catch_unwind`'s return value, not
//! through the hook, so replacing the hook costs its reporting nothing.

use std::sync::Once;

/// Marker prefixing every line this hook writes.
///
/// ⚠️ **Deliberately not `[SKIP]`, `[WARN]` or `[E2E]`**, and for the same
/// reason `worker_stderr`'s three fallback markers are not: the gate greps
/// those anchored at line start and asserts a count over them, so a hook that
/// borrowed one would turn profiles red from any panicking fixture. Asserted
/// in the tests below rather than left to whoever edits this string.
pub const PANIC_MARKER: &str = "[panic]";

static INSTALLED: Once = Once::new();

/// Install the neutralising panic hook, at most once per process.
///
/// Idempotent by [`Once`], because the chokepoint that calls it
/// ([`crate::require::RequireKnob::action_reporting_to`]) runs many times per
/// binary and a hook that reinstalled itself on every knob read would leak a
/// boxed closure per call.
///
/// ## Why it is called from the REQUIRE-knob path rather than by each suite
///
/// There is no way to run code automatically at the start of every test
/// binary, so *something* has to call this — and "every suite remembers to" is
/// the property that decays. Every gated tier reads its knob to decide whether
/// to skip, and every path that reads a knob funnels through
/// `action_reporting_to`, so a suite cannot be in a gate profile and bypass
/// this.
///
/// ⚠️ **It is installed from `action_reporting_to` and NOT from `action`,
/// because `action` is not the only door.** The first version used `action`
/// and the whole **`microvm` profile** went round it —
/// `microvm::skip_unless_ready` reaches the knob through `require_action_to`.
/// It was covered only when a co-set knob happened to be read first, which is
/// coverage by accident dressed as coverage by construction
/// [[guard-shares-the-census-blind-spot]]. Moving one level down fixed it;
/// picking the chokepoint by reading one call site is what did not.
///
/// ⚠️ **It is a side effect of a knob read, which is surprising, and that is
/// the trade.** The alternative — an explicit `install()` in each of ~30
/// `tests/*.rs` files — is a census, and a census that one new file forgets to
/// join is exactly the blind spot this tree keeps paying for. Coverage by
/// construction beats coverage by discipline; this comment is the price.
///
/// ⚠️ **Suites with no REQUIRE knob are NOT covered.** That is honest rather
/// than complete: they are also not in any gate profile, which is where the
/// forged line does damage. A suite that joins a profile gains a knob and
/// gains this in the same change.
///
/// ⚠️ **The guarantee is "installed before the first knob read", not
/// "installed for the whole binary".** libtest runs tests in parallel, so a
/// test that panics *before* any knob has been read anywhere in the process
/// is still rendered by the **default** hook. Every suite in a gate profile
/// reads its knob in its first fixture, so the window is small — but it is a
/// window, and a suite whose knob read is buried behind a slow probe widens
/// it. This is the residual limitation; it is not closed.
pub fn install_once() {
    INSTALLED.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            // ⚠️ **The guard comes first, and it is not defensive tidiness**
            // (#749). Everything below writes with `eprintln!`, which PANICS
            // when the write fails — and a panic inside a panic hook is a
            // panic while panicking, which aborts the process immediately with
            // **no output on any stream** — including libtest's own
            // `test result: FAILED` line, because the process never reaches it.
            // That is strictly worse than the #733 case this borrows the probe
            // from: there a report was lost, here the whole ACCOUNT of the
            // failure is.
            //
            // Measured on this Mac, a child whose fd 2 is the write end of a
            // pipe with no reader:
            //
            // | hook | outcome |
            // | --- | --- |
            // | unguarded | **signal 6 (SIGABRT)**, nothing on any stream |
            // | guarded | exit 101, libtest still reports the failure |
            //
            // ⚠️ **The panic TEXT is dropped either way** — on this fd nothing
            // can be written, and returning early is what the guard is for.
            // What it buys is the process surviving to be accounted for. Do
            // not read the row above as "the message survives"; #750's review
            // found that claim in three places and it was wrong in all three.
            //
            // ⚠️ A merely CLOSED fd 2 (`2>&-`) is harmless — `std` swallows
            // `EBADF` on stdio via `handle_ebadf` — so the fixture needs a
            // broken pipe, measured rather than assumed. `EPIPE` is the
            // motivating errno but not the only one: a pty slave whose master
            // has closed returns `EIO`, and `eprintln!` panics on any write
            // error, so the probe (not the errno) is what this turns on.
            //
            // The probe is `kastellan-core`'s, not a copy: it has already
            // needed one host-specific correction (macOS sets `POLLNVAL` on
            // live character devices) that a second copy would not have
            // received.
            if !kastellan_core::worker_stderr::stderr_is_writable() {
                return;
            }
            let line = render_panic_line(
                std::thread::current().name(),
                info.location().map(|l| l.to_string()).as_deref(),
                payload_of(info.payload()),
            );
            // `eprintln!`, not `writeln!(std::io::stderr(), …)`: libtest
            // captures through `std::io::set_output_capture`, which only the
            // `print!`/`eprint!` macros consult. A panic message written
            // through the handle would not appear beneath the failing test
            // that needs it — the same property `worker_stderr` depends on.
            eprintln!("{line}");
            // The default hook's backtrace behaviour, preserved. Indented
            // frames cannot forge a column-0 marker, so this needs no
            // neutralisation of its own.
            //
            // ⚠️ `RUST_BACKTRACE=0` and *unset* share an arm because the real
            // default hook prints the note for BOTH — measured on this
            // toolchain, against a child that simply panics. #749 records this
            // as a divergence from the default; it is not one, and "fixing" it
            // to print nothing at `=0` would CREATE the divergence.
            match std::env::var("RUST_BACKTRACE").as_deref() {
                Ok("0") | Err(_) => {
                    eprintln!("note: run with `RUST_BACKTRACE=1` for a backtrace");
                }
                Ok(_) => eprintln!("{}", std::backtrace::Backtrace::force_capture()),
            }
        }));
    });
}

/// The panic payload as a string, for the two types `panic!` can produce.
///
/// A `panic!("literal")` yields `&str`; a `panic!("{x}")` or
/// `std::panic::panic_any(String)` yields `String`. Anything else is a
/// `panic_any` of some other type, which has no text to show.
///
/// ⚠️ **Takes the payload, not the `PanicHookInfo`, and that is an MSRV
/// constraint rather than taste.** `std::panic::PanicHookInfo` is stable only
/// since 1.81 and this workspace's `rust-version` is 1.78, so naming it is a
/// `clippy::incompatible_msrv` error. Its predecessor `PanicInfo` is deprecated
/// on current toolchains, which `-D warnings` would equally reject — so the
/// type is not named at all. The hook's closure infers it from `set_hook`.
fn payload_of(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("<panic payload was not a string>")
}

/// Pure: what this hook prints for one panic.
///
/// Pure and separate from the hook so the rendering can be pinned without
/// panicking a test process — the same reason [`crate::skip::skip_line`] is a
/// renderer rather than a printer.
///
/// ⚠️ **The property is "no line begins at column 0 but the first", NOT "one
/// line" — and the difference is a diagnostic this tree depends on.** The
/// first version flattened the whole payload with `neutralise_controls`,
/// mapping every `\n` to a space. That is safe but expensive: **59** assertion
/// messages in `core/tests` alone interpolate a whole child-process transcript
/// (`\n{both}`), and once the hook is installed in every gated suite, each of
/// those failures renders as a single unbroken multi-kilobyte line. The arc
/// this hook belongs to exists to make worker failures *readable*; flattening
/// the messages that report them pays for the gate with the thing the gate is
/// protecting.
///
/// So a payload's own newlines are kept as line breaks and every continuation
/// line is **indented**. `scripts/run-e2e-gate.sh` greps its markers anchored
/// at line start (`^\[WARN\]`), so an indented line cannot be counted no
/// matter what it says — while the reader gets the transcript back.
///
/// Every *other* control character still maps to a space, and every
/// interpolated part goes through that — payload, location, and the thread
/// name, which `std::thread::Builder::name` lets a caller choose. Replacing
/// rather than deleting is deliberate: deleting would silently join the tokens
/// on either side, and a panic message is evidence.
pub fn render_panic_line(thread: Option<&str>, location: Option<&str>, payload: &str) -> String {
    let clean = |s: &str| kastellan_core::untrusted_text::neutralise_controls(s);
    let thread = clean(thread.unwrap_or("<unnamed>"));
    let location = clean(location.unwrap_or("<unknown location>"));
    // `lines()` splits on `\n` and strips a trailing `\r`; every remaining
    // control character (including the `\u{2028}`-class separators, which
    // `lines()` does NOT split on) is neutralised to a space per line. So the
    // only line breaks in the output are ones this function put there, each
    // followed by an indent.
    let mut out = payload.lines().map(clean);
    let first = out.next().unwrap_or_default();
    let mut rendered =
        format!("{PANIC_MARKER} thread '{thread}' panicked at {location}: {first}");
    for line in out {
        rendered.push_str(&format!("\n{CONTINUATION_INDENT}{line}"));
    }
    rendered
}

/// What every line of a rendered panic after the first begins with.
///
/// ⚠️ **It must be non-empty, and a test asserts that.** The entire
/// anti-forgery property of a multi-line rendering is that a continuation line
/// does not start at column 0; an empty indent silently gives the payload back
/// the forgery it just lost.
pub const CONTINUATION_INDENT: &str = "    ";

#[cfg(test)]
mod tests {
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
    fn installing_twice_is_harmless() {
        // The chokepoint calls this on every knob read, which is many times per
        // binary. `Once` is what makes that free; without it each call would
        // leak a boxed closure.
        install_once();
        install_once();
    }
}
