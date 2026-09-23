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

/// Marker prefixing the ONE line [`install_once`] prints when it installs the
/// hook ([#748](https://github.com/hherb/kastellan/issues/748)).
///
/// # Why installing the hook announces itself
///
/// `scripts/run-e2e-gate.sh` checks that **every test binary a profile runs
/// reached this hook**. That cannot be read off the source: suites reach it
/// indirectly, through a dozen `tests_common` helpers that all funnel into
/// [`crate::require::RequireKnob::action_reporting_to`], so a static scan would
/// need a list of helper names — a census, and a stale one the day a helper is
/// added. The announcement measures what actually happened instead: the gate
/// splits its log at cargo's `Running …` lines and refuses any binary whose
/// section carries no `[panic-hook]` line.
///
/// That check is also what makes the gate's `[panic]` cap mean anything. A
/// cap of 0 is satisfied just as well by a binary whose panics went through
/// the **default** hook — no marker, nothing counted — as by one that never
/// panicked. Proving the hook was installed is what separates the two.
///
/// ⚠️ **Disjoint from every counted marker, in both directions** — asserted
/// in the tests below. It is printed with `eprintln!`, so an ordinary
/// capturing `cargo test` swallows it; only `--nocapture` runs (every gate
/// profile) show it.
pub const HOOK_INSTALLED_MARKER: &str = "[panic-hook]";

/// The announcement [`install_once`] prints, as a pure function so its shape
/// (one line, marker at column 0) can be unit-tested.
pub fn install_line() -> String {
    format!("{HOOK_INSTALLED_MARKER} neutralising panic hook installed in this process")
}

/// `line`, framed so it starts at column 0 whatever else shares the stream.
///
/// ⚠️ **Measured, and it is why this exists (#748).** Under `--nocapture`
/// libtest writes `test <name> ... ` to stdout with no newline yet, and a gate
/// log merges stdout and stderr into one pipe. An unframed line printed at
/// that moment lands mid-line, and the gate's anchored grep does not see it:
/// the hook check refused a binary that had installed the hook, and a
/// `[panic]` line would have slipped under the cap — a false green.
///
/// The frame is `\n<line>\n`, emitted as ONE write (see [`emit_own_line`]).
/// The leading newline ends any partial line; a single `write` of at most
/// `PIPE_BUF` bytes (POSIX minimum 512) to a pipe is atomic, so nothing else
/// can land inside it. The cost is an occasional blank line.
pub fn own_line(line: &str) -> String {
    format!("\n{line}\n")
}

/// Print [`own_line`]`(line)` to stderr as a single `write_str`.
///
/// `eprint!("{s}")` with one argument and no literal pieces formats to one
/// `write_str` call — unlike `eprintln!("\n{line}")`, whose literal `\n` and
/// argument are separate writes that something else could land between.
/// Still a `print` MACRO, so libtest's capture sees it.
fn emit_own_line(line: &str) {
    let framed = own_line(line);
    eprint!("{framed}");
}

static INSTALLED: Once = Once::new();

/// Whether [`install_once`] has run in this process.
///
/// For tests only: `knob_reads_install_the_panic_hook_e2e` asserts, in a fresh
/// child per door, that the hook goes from absent to present across ONE knob
/// read. Hidden rather than private because an integration test is another
/// crate; it is not an API.
#[doc(hidden)]
pub fn is_installed() -> bool {
    INSTALLED.is_completed()
}

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
/// the property that decays. Every gated tier reads its knob, and a knob is
/// read from the environment through exactly two doors — `RequireKnob::raw()`
/// and `RequireKnob::action_reporting_to` — both of which install this.
///
/// ⚠️ **This paragraph used to name ONE door, and it was wrong from the day
/// it was written** (#745 → #748). A healthy micro-VM host meets every
/// precondition, so it never reaches `action_reporting_to`; it reads the knob
/// only to announce `[E2E]`, through `raw()`. #748's run-time gate check found
/// it on its first real run — all 15 micro-VM binaries refused while printing
/// 65 knob-gated lines. A static reading of the call sites would not have: it
/// is exactly the census that argued for one chokepoint. The gate's per-binary
/// `[panic-hook]` check, not this comment, is what now keeps it true.
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
            // The `eprint!` macro, not `writeln!(std::io::stderr(), …)`:
            // libtest captures through `std::io::set_output_capture`, which
            // only the `print!`/`eprint!` macros consult. A panic message
            // written through the handle would not appear beneath the failing
            // test that needs it — the same property `worker_stderr` depends
            // on. Framed onto its own line so the gate's `[panic]` count sees
            // it (#748); a panic longer than PIPE_BUF loses the atomicity half
            // of that guarantee, but not the leading newline.
            emit_own_line(&line);
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
        // Announce the install (#748) — AFTER `set_hook`, so the line is only
        // ever printed by a process that really has the hook. Inside the
        // `Once`, so the chokepoint's many calls produce one line per process.
        //
        // Guarded by the same probe as the hook, for the same reason:
        // `eprintln!` panics on a failed write, and a knob read in a process
        // whose stderr is a dead pipe must not turn into a test panic here.
        // Skipping the line is the safe direction — the gate then reports that
        // binary as unreached, a false RED, never a false green.
        if kastellan_core::worker_stderr::stderr_is_writable() {
            emit_own_line(&install_line());
        }
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
mod tests;
